//! HTTP/2 协议实现的公共模块
//!
//! 本模块是 hyper 中 HTTP/2 协议实现的核心公共层，提供了客户端和服务端共享的基础设施。
//! 主要职责包括：
//!
//! - **连接头部清理**：根据 RFC 9110 规定，HTTP/2 中不允许使用连接级别的头部字段，
//!   本模块提供了 `strip_connection_headers` 函数来移除这些非法头部。
//! - **请求体到 HTTP/2 流的适配**：通过 `PipeToSendStream` 将 hyper 的 `Body` trait
//!   适配为 h2 库的 `SendStream`，实现请求/响应体数据的异步传输。
//! - **发送缓冲区抽象**：`SendBuf` 枚举封装了多种缓冲区类型，统一实现 `Buf` trait，
//!   为 h2 库提供灵活的数据源。
//!
//! 本模块还通过条件编译宏 `cfg_client!` 和 `cfg_server!` 分别导出客户端和服务端子模块。

// 标准库导入
use std::error::Error as StdError; // 标准错误 trait，用于错误类型约束
use std::future::Future; // Future trait，用于异步编程
use std::io::{Cursor, IoSlice}; // Cursor 用于包装字节切片实现 Buf，IoSlice 用于向量化写入
use std::pin::Pin; // Pin 类型，用于确保自引用类型的内存安全
use std::task::{Context, Poll}; // 异步任务上下文和轮询结果类型

// 第三方 crate 导入
use bytes::Buf; // bytes crate 的 Buf trait，提供高效的缓冲区读取接口
use futures_core::ready; // ready! 宏，用于简化 Poll::Ready 的匹配和传播
use h2::SendStream; // h2 crate 的发送流类型，用于向 HTTP/2 流中写入数据
use http::header::{HeaderName, CONNECTION, TE, TRANSFER_ENCODING, UPGRADE}; // HTTP 头部名称常量
use http::HeaderMap; // HTTP 头部映射类型
use pin_project_lite::pin_project; // 轻量级 pin projection 宏，用于安全地访问 Pin 结构体的字段

// 内部 crate 导入
use crate::body::Body; // hyper 的 Body trait，定义 HTTP 请求/响应体的抽象接口

/// ping 子模块，实现 HTTP/2 的 PING 帧机制，用于带宽检测（BDP）和连接保活（keep-alive）
pub(crate) mod ping;
/// upgrade 子模块，实现 HTTP/2 的连接升级（如 CONNECT 方法的隧道化）
pub(crate) mod upgrade;

// 通过条件编译宏导出客户端模块（仅在启用 client feature 时编译）
cfg_client! {
    pub(crate) mod client;
    pub(crate) use self::client::ClientTask;
}

// 通过条件编译宏导出服务端模块（仅在启用 server feature 时编译）
cfg_server! {
    pub(crate) mod server;
    pub(crate) use self::server::Server;
}

/// HTTP/2 规范定义的默认初始流窗口大小（65,535 字节）
///
/// 该值来自 RFC 9113，是 HTTP/2 流级别流量控制的默认窗口大小。
/// hyper 在自适应窗口模式下会使用此值作为初始值。
pub(crate) const SPEC_WINDOW_SIZE: u32 = 65_535;

// RFC 9110 第 7.6.1 节定义的连接级头部列表
//
// TE 头部在 HTTP/2 请求中只有值为 "trailers" 时才被允许，因此单独处理。
// 这些头部在 HTTP/1.1 中用于控制逐跳连接行为，但在 HTTP/2 中这些语义已被帧层接管。
static CONNECTION_HEADERS: [HeaderName; 4] = [
    HeaderName::from_static("keep-alive"),       // HTTP/1.1 保活头部
    HeaderName::from_static("proxy-connection"),  // 非标准代理连接头部
    TRANSFER_ENCODING,                            // 传输编码头部（HTTP/2 使用帧层处理）
    UPGRADE,                                      // 协议升级头部（HTTP/2 有自己的升级机制）
];

/// 从 HTTP 头部映射中移除 HTTP/2 中不允许的连接级头部
///
/// 根据 RFC 9110 第 7.6.1 节，HTTP/2 不允许使用连接特定的头部字段。
/// 此函数会移除所有连接级头部，并对 TE 和 Connection 头部进行特殊处理。
///
/// # 参数
/// - `headers`: 要处理的 HTTP 头部映射（会被就地修改）
/// - `is_request`: 是否为请求头部（影响 TE 头部的处理逻辑）
fn strip_connection_headers(headers: &mut HeaderMap, is_request: bool) {
    // 遍历并移除所有预定义的连接级头部
    for header in &CONNECTION_HEADERS {
        if headers.remove(header).is_some() {
            warn!("Connection header illegal in HTTP/2: {}", header.as_str());
        }
    }

    // 特殊处理 TE 头部：
    // - 对于请求：仅允许值为 "trailers" 的 TE 头部
    // - 对于响应：完全不允许 TE 头部
    if is_request {
        if headers
            .get(TE)
            .map_or(false, |te_header| te_header != "trailers")
        {
            warn!("TE headers not set to \"trailers\" are illegal in HTTP/2 requests");
            headers.remove(TE);
        }
    } else if headers.remove(TE).is_some() {
        warn!("TE headers illegal in HTTP/2 responses");
    }

    // 特殊处理 Connection 头部：
    // Connection 头部可能包含逗号分隔的其他头部名称列表，
    // 这些头部也是连接特定的，需要一并移除。
    if let Some(header) = headers.remove(CONNECTION) {
        warn!(
            "Connection header illegal in HTTP/2: {}",
            CONNECTION.as_str()
        );
        let header_contents = header.to_str().unwrap();

        // A `Connection` header may have a comma-separated list of names of other headers that
        // are meant for only this specific connection.
        //
        // Iterate these names and remove them as headers. Connection-specific headers are
        // forbidden in HTTP2, as that information has been moved into frame types of the h2
        // protocol.
        for name in header_contents.split(',') {
            let name = name.trim();
            headers.remove(name);
        }
    }
}

// 以下是客户端和服务端共用的请求体适配器

// 使用 pin_project_lite 宏定义 PipeToSendStream 结构体。
// pin_project 宏会自动为标记了 #[pin] 的字段生成安全的 pin projection 方法，
// 使得可以在 Pin<&mut Self> 上安全地访问内部字段。
pin_project! {
    /// 将 hyper `Body` 流桥接到 h2 `SendStream` 的适配器
    ///
    /// 此结构体实现了 `Future` trait，当被轮询时，它会从 `Body` 中读取数据帧，
    /// 并通过 h2 的 `SendStream` 发送出去。它处理了流量控制、数据帧和 trailer 帧的发送。
    ///
    /// 该类型同时被客户端和服务端的 HTTP/2 实现使用。
    pub(crate) struct PipeToSendStream<S>
    where
        S: Body,
    {
        // h2 发送流，用于将数据写入 HTTP/2 流
        body_tx: SendStream<SendBuf<S::Data>>,
        // 标记数据是否已全部发送完毕
        data_done: bool,
        // Body 流本身，使用 #[pin] 标注以支持 pin projection
        // 这是因为 Body 可能包含自引用的 Future，需要被 pin 住
        #[pin]
        stream: S,
    }
}

/// `PipeToSendStream` 的构造与基础方法实现
impl<S> PipeToSendStream<S>
where
    S: Body,
{
    /// 创建新的 `PipeToSendStream` 实例
    ///
    /// # 参数
    /// - `stream`: 要发送的 Body 流
    /// - `tx`: h2 的 SendStream，用于将数据写入 HTTP/2 连接
    fn new(stream: S, tx: SendStream<SendBuf<S::Data>>) -> PipeToSendStream<S> {
        PipeToSendStream {
            body_tx: tx,
            data_done: false,
            stream,
        }
    }
}

/// `PipeToSendStream` 的 Future 实现
///
/// 该 Future 在被轮询时执行以下步骤：
/// 1. 检查 h2 流的发送容量，必要时等待容量释放
/// 2. 从 Body 中读取下一个帧（数据帧或 trailer 帧）
/// 3. 将帧数据通过 SendStream 发送
/// 4. 当所有数据和 trailer 都发送完毕时返回 Ready
impl<S> Future for PipeToSendStream<S>
where
    S: Body,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type Output = crate::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 使用 pin projection 安全地获取各字段的可变引用
        let mut me = self.project();
        loop {
            // we don't have the next chunk of data yet, so just reserve 1 byte to make
            // sure there's some capacity available. h2 will handle the capacity management
            // for the actual body chunk.
            // 预留 1 字节容量以确保流处于活跃状态，h2 会自动管理实际的容量分配
            me.body_tx.reserve_capacity(1);

            // 检查当前可用容量
            if me.body_tx.capacity() == 0 {
                // 容量为 0 时，需要等待 h2 分配更多容量
                loop {
                    match ready!(me.body_tx.poll_capacity(cx)) {
                        Some(Ok(0)) => {} // 分配了 0 字节容量，继续等待
                        Some(Ok(_)) => break, // 获得了可用容量，跳出等待循环
                        Some(Err(e)) => return Poll::Ready(Err(crate::Error::new_body_write(e))),
                        None => {
                            // None means the stream is no longer in a
                            // streaming state, we either finished it
                            // somehow, or the remote reset us.
                            // None 表示流已不再处于可发送状态（可能已完成或被远端重置）
                            return Poll::Ready(Err(crate::Error::new_body_write(
                                "send stream capacity unexpectedly closed",
                            )));
                        }
                    }
                }
            } else if let Poll::Ready(reason) = me
                .body_tx
                .poll_reset(cx)
                .map_err(crate::Error::new_body_write)?
            {
                // 如果有容量但收到了 RST_STREAM，说明远端重置了该流
                debug!("stream received RST_STREAM: {:?}", reason);
                return Poll::Ready(Err(crate::Error::new_body_write(::h2::Error::from(reason))));
            }

            // 从 Body 中轮询下一个帧
            match ready!(me.stream.as_mut().poll_frame(cx)) {
                Some(Ok(frame)) => {
                    if frame.is_data() {
                        // 处理数据帧
                        let chunk = frame.into_data().unwrap_or_else(|_| unreachable!());
                        let is_eos = me.stream.is_end_stream();
                        trace!(
                            "send body chunk: {} bytes, eos={}",
                            chunk.remaining(),
                            is_eos,
                        );

                        // 将数据包装为 SendBuf 并发送
                        let buf = SendBuf::Buf(chunk);
                        me.body_tx
                            .send_data(buf, is_eos)
                            .map_err(crate::Error::new_body_write)?;

                        // 如果这是最后一帧数据，直接返回完成
                        if is_eos {
                            return Poll::Ready(Ok(()));
                        }
                    } else if frame.is_trailers() {
                        // 处理 trailer 帧（HTTP 尾部头部）
                        // no more DATA, so give any capacity back
                        // 不再需要数据容量，释放预留容量
                        me.body_tx.reserve_capacity(0);
                        me.body_tx
                            .send_trailers(frame.into_trailers().unwrap_or_else(|_| unreachable!()))
                            .map_err(crate::Error::new_body_write)?;
                        return Poll::Ready(Ok(()));
                    } else {
                        // 忽略未知类型的帧，继续循环
                        trace!("discarding unknown frame");
                        // loop again
                    }
                }
                Some(Err(e)) => return Poll::Ready(Err(me.body_tx.on_user_err(e))),
                None => {
                    // no more frames means we're done here
                    // but at this point, we haven't sent an EOS DATA, or
                    // any trailers, so send an empty EOS DATA.
                    // Body 没有更多帧了，但还未发送 EOS 标志，
                    // 发送一个空的带 EOS 标志的 DATA 帧来结束流
                    return Poll::Ready(me.body_tx.send_eos_frame());
                }
            }
        }
    }
}

/// `SendStream` 的扩展 trait，提供错误处理和结束流的辅助方法
///
/// 这是一个私有 trait，通过 extension trait 模式为 h2 的 `SendStream` 添加
/// hyper 特定的功能方法。
trait SendStreamExt {
    /// 处理用户 Body 流产生的错误
    ///
    /// 将用户错误转换为 hyper 错误，并向远端发送 RST_STREAM 帧
    fn on_user_err<E>(&mut self, err: E) -> crate::Error
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>;

    /// 发送一个空的结束流（EOS）数据帧
    fn send_eos_frame(&mut self) -> crate::Result<()>;
}

/// 为 `SendStream<SendBuf<B>>` 实现 `SendStreamExt` trait
impl<B: Buf> SendStreamExt for SendStream<SendBuf<B>> {
    fn on_user_err<E>(&mut self, err: E) -> crate::Error
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let err = crate::Error::new_user_body(err);
        debug!("send body user stream error: {}", err);
        // 向远端发送 RST_STREAM，使用错误对应的 h2 reason code
        self.send_reset(err.h2_reason());
        err
    }

    fn send_eos_frame(&mut self) -> crate::Result<()> {
        trace!("send body eos");
        // 发送一个空的 SendBuf::None 并设置 end_of_stream 为 true
        self.send_data(SendBuf::None, true)
            .map_err(crate::Error::new_body_write)
    }
}

/// 发送缓冲区枚举，封装了多种数据源类型
///
/// 该枚举用于统一 h2 `SendStream` 中的数据类型，使其能接受不同来源的数据：
/// - `Buf(B)`: 来自 Body trait 的数据块
/// - `Cursor(Cursor<Box<[u8]>>)`: 来自升级连接的数据（通过 Cursor 包装的字节切片）
/// - `None`: 空数据，用于发送 EOS（流结束）标志
///
/// `#[repr(usize)]` 确保枚举使用 usize 大小的判别符，有助于内存对齐优化。
#[repr(usize)]
enum SendBuf<B> {
    Buf(B),
    Cursor(Cursor<Box<[u8]>>),
    None,
}

/// 为 `SendBuf` 实现 `bytes::Buf` trait
///
/// 这使得 `SendBuf` 可以作为 h2 `SendStream` 的数据类型使用。
/// 每个方法都通过 match 委托给内部实际的缓冲区实现。
impl<B: Buf> Buf for SendBuf<B> {
    /// 返回缓冲区中剩余的可读字节数
    #[inline]
    fn remaining(&self) -> usize {
        match *self {
            Self::Buf(ref b) => b.remaining(),
            Self::Cursor(ref c) => Buf::remaining(c), // 使用完全限定语法避免歧义
            Self::None => 0,
        }
    }

    /// 返回从当前位置开始的连续字节切片
    #[inline]
    fn chunk(&self) -> &[u8] {
        match *self {
            Self::Buf(ref b) => b.chunk(),
            Self::Cursor(ref c) => c.chunk(),
            Self::None => &[], // 空缓冲区返回空切片
        }
    }

    /// 将内部游标向前推进 `cnt` 个字节
    #[inline]
    fn advance(&mut self, cnt: usize) {
        match *self {
            Self::Buf(ref mut b) => b.advance(cnt),
            Self::Cursor(ref mut c) => c.advance(cnt),
            Self::None => {} // 空缓冲区无需操作
        }
    }

    /// 将缓冲区数据填入多个 IoSlice 中，支持向量化 I/O（writev）
    ///
    /// 返回实际填入的 IoSlice 数量。向量化 I/O 可以减少系统调用次数，提高性能。
    fn chunks_vectored<'a>(&'a self, dst: &mut [IoSlice<'a>]) -> usize {
        match *self {
            Self::Buf(ref b) => b.chunks_vectored(dst),
            Self::Cursor(ref c) => c.chunks_vectored(dst),
            Self::None => 0,
        }
    }
}
