//! `Incoming` Body 类型的实现模块
//!
//! 本模块定义了 `Incoming` 结构体——hyper 中用于表示从网络接收到的 HTTP 消息体的核心类型。
//! 当 hyper 作为服务端时，`Incoming` 出现在请求的 body 中；当作为客户端时，它出现在响应的 body 中。
//!
//! 本模块还定义了与 `Incoming` 配对使用的 `Sender` 类型，它是通道的发送端，
//! 用于在 HTTP/1 场景下向 body 流中推送数据块（chunks）和 trailers。
//!
//! `Incoming` 内部通过 `Kind` 枚举来区分不同的数据来源：
//! - 空 body（`Empty`）
//! - HTTP/1 通道（`Chan`）——基于 `futures_channel` 的 mpsc 通道
//! - HTTP/2 接收流（`H2`）——基于 `h2` crate 的 `RecvStream`
//! - FFI 外部函数接口（`Ffi`）——用于 C-FFI 绑定场景

// --- 标准库导入 ---

/// 导入格式化 trait，用于实现 `Debug` 等格式化输出
use std::fmt;
/// 导入 `Future` trait，仅在 HTTP/1 客户端或服务端场景下用于异步操作
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
use std::future::Future;
/// 导入 `Pin` 类型，用于固定（pin）自引用类型，确保 `poll_*` 方法中的安全性
use std::pin::Pin;
/// 导入异步任务上下文 `Context` 和轮询结果 `Poll`
use std::task::{Context, Poll};

// --- 第三方 crate 导入 ---

/// `bytes::Bytes`：零拷贝的字节缓冲区类型，是 hyper body 数据块的基本单元
use bytes::Bytes;
/// `futures_channel::mpsc`：多生产者-单消费者异步通道，用于 HTTP/1 body 的数据传输
/// `futures_channel::oneshot`：一次性异步通道，用于传递 HTTP trailers
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
use futures_channel::{mpsc, oneshot};
/// `futures_core::ready` 宏：简化 `Poll::Ready` 的模式匹配，
/// 如果轮询结果为 `Pending` 则直接返回 `Pending`
#[cfg(all(
    any(feature = "http1", feature = "http2"),
    any(feature = "client", feature = "server")
))]
use futures_core::ready;
/// `FusedStream`：可检测是否已终止的 Stream trait
/// `Stream`：异步流 trait，这里用于 `mpsc::Receiver` 的轮询
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
use futures_core::{stream::FusedStream, Stream}; // for mpsc::Receiver
/// `http::HeaderMap`：HTTP 头部映射类型，用于表示 trailers
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
use http::HeaderMap;
/// 从 `http_body` crate 导入核心 trait 和类型：
/// - `Body`：HTTP 消息体的核心 trait
/// - `Frame`：body 流中的一帧（数据或 trailers）
/// - `SizeHint`：body 大小提示
use http_body::{Body, Frame, SizeHint};

// --- crate 内部导入 ---

/// 导入 `DecodedLength`：解码后的消息体长度表示（精确长度、分块传输、或连接关闭分隔）
#[cfg(all(
    any(feature = "http1", feature = "http2"),
    any(feature = "client", feature = "server")
))]
use super::DecodedLength;
/// 导入内部的 `watch` 模块：一个轻量级的通知机制，
/// 用于实现发送端（Sender）的就绪通知——即 "wanter" 模式
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
use crate::common::watch;
/// 导入 HTTP/2 ping 记录器，用于在接收 H2 body 数据时记录活动状态
#[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
use crate::proto::h2::ping;

// --- 类型别名 ---

/// Body 数据发送器的类型别名：通过 mpsc 通道发送 `Result<Bytes, Error>`
/// 使用 `Result` 是为了允许在数据流中传递错误（如中止信号）
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
type BodySender = mpsc::Sender<Result<Bytes, crate::Error>>;
/// Trailers 发送器的类型别名：通过 oneshot 通道发送 `HeaderMap`
/// 使用 oneshot 是因为 trailers 只能发送一次（在 body 数据结束之后）
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
type TrailersSender = oneshot::Sender<HeaderMap>;

/// 从网络接收到的 `Bytes` 流，用于表示 HTTP 请求或响应的消息体。
///
/// 注意：用户不应直接实例化此结构体。当使用 hyper 客户端时，
/// `Incoming` 会在响应中返回给你。类似地，当使用 hyper 服务端时，
/// 它会在请求中提供给你。
///
/// # 示例
///
/// ```rust,ignore
/// async fn echo(
///    req: Request<hyper::body::Incoming>,
/// ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
///    //Here, you can process `Incoming`
/// }
/// ```
// `#[must_use]` 属性：提醒用户流（stream）如果不被轮询（poll）则不会产生任何效果
#[must_use = "streams do nothing unless polled"]
pub struct Incoming {
    /// 内部使用枚举 `Kind` 来区分不同的 body 来源
    kind: Kind,
}

/// `Incoming` 的内部表示枚举。
///
/// 通过枚举的不同变体来支持多种 body 来源，这是 Rust 中常见的类型状态模式。
/// 每个变体对应不同的 HTTP 协议版本和数据来源方式。
enum Kind {
    /// 空 body，不包含任何数据
    Empty,
    /// HTTP/1 通道模式：通过 mpsc 通道接收数据块，通过 oneshot 通道接收 trailers
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    Chan {
        /// 已解码的 Content-Length，用于跟踪剩余数据量
        content_length: DecodedLength,
        /// watch 通道的发送端，用于通知 `Sender` 端当前 body 已被轮询（"wanter" 机制）
        want_tx: watch::Sender,
        /// mpsc 通道的接收端，用于接收 body 数据块
        data_rx: mpsc::Receiver<Result<Bytes, crate::Error>>,
        /// oneshot 通道的接收端，用于接收 HTTP trailers
        trailers_rx: oneshot::Receiver<HeaderMap>,
    },
    /// HTTP/2 接收流模式：直接从 h2 库的 `RecvStream` 中读取数据
    #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
    H2 {
        /// 已解码的 Content-Length
        content_length: DecodedLength,
        /// 标记数据帧是否已全部接收完毕，之后转为读取 trailers
        data_done: bool,
        /// HTTP/2 ping 记录器，用于跟踪连接活跃状态
        ping: ping::Recorder,
        /// h2 库的接收流，用于读取 HTTP/2 帧
        recv: h2::RecvStream,
    },
    /// FFI 模式：通过外部函数接口提供 body 数据
    #[cfg(feature = "ffi")]
    Ffi(crate::ffi::UserBody),
}

/// 通过 [`Body::channel()`] 创建的发送端。
///
/// 当需要从另一个线程流式发送数据块时非常有用。
///
/// ## Body 关闭行为
///
/// 注意：当 sender 被 drop 时，请求体会正常关闭（即向远端发送空的终止块）。
/// 如果你希望以不完整的响应关闭连接（例如在异步处理中遇到错误时），
/// 请调用 [`Sender::abort()`] 方法以异常方式中止 body。
///
/// [`Body::channel()`]: struct.Body.html#method.channel
/// [`Sender::abort()`]: struct.Sender.html#method.abort
// `#[must_use]` 属性：提醒用户 Sender 如果不使用则没有意义
#[must_use = "Sender does nothing unless sent on"]
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
pub(crate) struct Sender {
    /// watch 通道的接收端，用于检测 body 接收端是否已准备好接收数据（"wanter" 机制）
    want_rx: watch::Receiver,
    /// body 数据的 mpsc 发送端
    data_tx: BodySender,
    /// trailers 的 oneshot 发送端，使用 `Option` 包装是因为只能发送一次，
    /// 发送后通过 `take()` 消耗掉
    trailers_tx: Option<TrailersSender>,
}

// --- "wanter" 机制的常量 ---

/// 表示 body 接收端尚未轮询过数据，发送端应等待
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
const WANT_PENDING: usize = 1;
/// 表示 body 接收端已准备好接收数据，发送端可以发送
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
const WANT_READY: usize = 2;

/// `Incoming` 的构造方法和工厂方法实现块
impl Incoming {
    /// 创建一个带有关联发送端的 `Body` 流。
    ///
    /// 当需要从另一个线程流式发送数据块时非常有用。
    /// 此方法仅用于测试，内部调用 `new_channel` 并使用分块传输编码长度。
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[inline]
    #[cfg(test)]
    pub(crate) fn channel() -> (Sender, Incoming) {
        // 创建分块传输编码的通道，不启用 wanter 机制
        Self::new_channel(DecodedLength::CHUNKED, /*wanter =*/ false)
    }

    /// 创建一个新的 body 通道，可指定 content-length 和是否启用 wanter 机制。
    ///
    /// # 参数
    /// - `content_length`：解码后的消息体长度
    /// - `wanter`：若为 `true`，则 `Sender::poll_ready()` 在 body 被首次轮询前不会就绪
    ///
    /// # 返回值
    /// 返回 `(Sender, Incoming)` 元组，分别是发送端和接收端
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    pub(crate) fn new_channel(content_length: DecodedLength, wanter: bool) -> (Sender, Incoming) {
        // 创建容量为 0 的 mpsc 通道（即无缓冲通道，实际上 futures mpsc 会有 1 个缓冲槽位）
        let (data_tx, data_rx) = mpsc::channel(0);
        // 创建一次性通道用于传递 trailers
        let (trailers_tx, trailers_rx) = oneshot::channel();

        // 如果启用 wanter 模式，`Sender::poll_ready()` 在 `Body` 首次被轮询数据之前
        // 不会变为就绪状态——这实现了背压控制
        let want = if wanter { WANT_PENDING } else { WANT_READY };

        // 创建 watch 通道，用于在发送端和接收端之间同步就绪状态
        let (want_tx, want_rx) = watch::channel(want);

        let tx = Sender {
            want_rx,
            data_tx,
            trailers_tx: Some(trailers_tx),
        };
        let rx = Incoming::new(Kind::Chan {
            content_length,
            want_tx,
            data_rx,
            trailers_rx,
        });

        (tx, rx)
    }

    /// 从 `Kind` 枚举创建一个新的 `Incoming` 实例（内部构造函数）
    fn new(kind: Kind) -> Incoming {
        Incoming { kind }
    }

    /// 创建一个空的 `Incoming` body（不包含任何数据）
    #[allow(dead_code)]
    pub(crate) fn empty() -> Incoming {
        Incoming::new(Kind::Empty)
    }

    /// 创建一个 FFI 模式的 `Incoming` body，用于外部函数接口场景
    #[cfg(feature = "ffi")]
    pub(crate) fn ffi() -> Incoming {
        Incoming::new(Kind::Ffi(crate::ffi::UserBody::new()))
    }

    /// 从 HTTP/2 的 `RecvStream` 创建一个 `Incoming` body。
    ///
    /// # 参数
    /// - `recv`：h2 库的接收流
    /// - `content_length`：解码后的消息体长度
    /// - `ping`：HTTP/2 ping 记录器，用于跟踪连接活跃状态
    #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
    pub(crate) fn h2(
        recv: h2::RecvStream,
        mut content_length: DecodedLength,
        ping: ping::Recorder,
    ) -> Self {
        // 如果流已经是 EOS（End of Stream）状态，那么"未知长度"实际上就是零
        if !content_length.is_exact() && recv.is_end_stream() {
            content_length = DecodedLength::ZERO;
        }

        Incoming::new(Kind::H2 {
            data_done: false,
            ping,
            content_length,
            recv,
        })
    }

    /// 获取 FFI body 的可变引用。
    /// 如果当前 kind 不是 Ffi 变体，则先将其转换为 Ffi 变体。
    #[cfg(feature = "ffi")]
    pub(crate) fn as_ffi_mut(&mut self) -> &mut crate::ffi::UserBody {
        match self.kind {
            Kind::Ffi(ref mut body) => return body,
            _ => {
                // 将当前 kind 替换为新的 Ffi 变体
                self.kind = Kind::Ffi(crate::ffi::UserBody::new());
            }
        }

        // 经过上面的替换，此时一定是 Ffi 变体
        match self.kind {
            Kind::Ffi(ref mut body) => body,
            _ => unreachable!(),
        }
    }
}

/// 为 `Incoming` 实现 `http_body::Body` trait。
///
/// 这是 `Incoming` 的核心实现，通过 `poll_frame` 方法以异步轮询的方式
/// 从不同的数据来源（通道、H2 流等）读取数据帧和 trailers 帧。
impl Body for Incoming {
    /// 数据帧的类型：`bytes::Bytes`
    type Data = Bytes;
    /// 错误类型：hyper 自定义的错误类型
    type Error = crate::Error;

    /// 轮询获取 body 的下一帧。
    ///
    /// 返回 `Poll<Option<Result<Frame<Bytes>, Error>>>`：
    /// - `Poll::Ready(Some(Ok(frame)))`：成功获取到一帧数据或 trailers
    /// - `Poll::Ready(Some(Err(e)))`：发生错误
    /// - `Poll::Ready(None)`：body 已结束
    /// - `Poll::Pending`：当前没有数据可用，等待唤醒
    fn poll_frame(
        // 以下 cfg_attr 在未启用 HTTP 特性时允许 `self` 和 `cx` 未使用，避免编译警告
        #[cfg_attr(
            not(all(
                any(feature = "http1", feature = "http2"),
                any(feature = "client", feature = "server")
            )),
            allow(unused_mut)
        )]
        mut self: Pin<&mut Self>,
        #[cfg_attr(
            not(all(
                any(feature = "http1", feature = "http2"),
                any(feature = "client", feature = "server")
            )),
            allow(unused_variables)
        )]
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // 根据内部 Kind 变体进行分派
        match self.kind {
            // 空 body 直接返回 None，表示流已结束
            Kind::Empty => Poll::Ready(None),
            // HTTP/1 通道模式
            #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
            Kind::Chan {
                content_length: ref mut len,
                ref mut data_rx,
                ref mut want_tx,
                ref mut trailers_rx,
            } => {
                // 通知发送端：接收端已准备好接收数据（"wanter" 机制）
                want_tx.send(WANT_READY);

                // 先检查数据通道是否尚未终止
                if !data_rx.is_terminated() {
                    // ready! 宏：如果 poll_next 返回 Pending，则整个函数返回 Pending
                    // `?` 操作符：如果通道返回 Err，则传播错误
                    if let Some(chunk) = ready!(Pin::new(data_rx).poll_next(cx)?) {
                        // 从剩余长度中减去已接收的字节数
                        len.sub_if(chunk.len() as u64);
                        return Poll::Ready(Some(Ok(Frame::data(chunk))));
                    }
                }

                // 数据通道已终止后，检查 trailers
                match ready!(Pin::new(trailers_rx).poll(cx)) {
                    Ok(t) => Poll::Ready(Some(Ok(Frame::trailers(t)))),
                    // oneshot 通道的发送端被 drop（无 trailers），body 正常结束
                    Err(_) => Poll::Ready(None),
                }
            }
            // HTTP/2 接收流模式
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 {
                ref mut data_done,
                ref ping,
                recv: ref mut h2,
                content_length: ref mut len,
            } => {
                // 如果数据帧尚未全部接收
                if !*data_done {
                    match ready!(h2.poll_data(cx)) {
                        Some(Ok(bytes)) => {
                            // 释放 HTTP/2 流控（flow control）容量，允许对端发送更多数据
                            let _ = h2.flow_control().release_capacity(bytes.len());
                            // 从剩余长度中减去已接收的字节数
                            len.sub_if(bytes.len() as u64);
                            // 记录数据活动，用于 HTTP/2 ping/pong 保活机制
                            ping.record_data(bytes.len());
                            return Poll::Ready(Some(Ok(Frame::data(bytes))));
                        }
                        Some(Err(e)) => {
                            return match e.reason() {
                                // NO_ERROR 和 CANCEL 应该导致 body 读取停止，但不应视为失败
                                // 这与 `Read for H2Upgraded` 中的逻辑一致
                                Some(h2::Reason::NO_ERROR) | Some(h2::Reason::CANCEL) => {
                                    Poll::Ready(None)
                                }
                                _ => Poll::Ready(Some(Err(crate::Error::new_body(e)))),
                            };
                        }
                        None => {
                            // 数据帧全部接收完毕，标记后继续读取 trailers
                            *data_done = true;
                            // fall through to trailers
                        }
                    }
                }

                // 数据接收完毕后，检查 trailers
                match ready!(h2.poll_trailers(cx)) {
                    Ok(t) => {
                        // 记录非数据活动（trailers 也算连接活跃）
                        ping.record_non_data();
                        // 将 Option<HeaderMap> 转换为 Option<Frame>，再转为 Result
                        // `transpose()` 将 Result<Option<T>> 转换为 Option<Result<T>>
                        Poll::Ready(Ok(t.map(Frame::trailers)).transpose())
                    }
                    Err(e) => Poll::Ready(Some(Err(crate::Error::new_h2(e)))),
                }
            }

            // FFI 模式：委托给外部实现
            #[cfg(feature = "ffi")]
            Kind::Ffi(ref mut body) => body.poll_data(cx),
        }
    }

    /// 判断 body 流是否已到达结尾。
    ///
    /// 这是一个快速检查方法，不需要异步轮询就能判断 body 是否已结束。
    fn is_end_stream(&self) -> bool {
        match self.kind {
            Kind::Empty => true,
            #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
            Kind::Chan { content_length, .. } => content_length == DecodedLength::ZERO,
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 { recv: ref h2, .. } => h2.is_end_stream(),
            #[cfg(feature = "ffi")]
            Kind::Ffi(..) => false,
        }
    }

    /// 返回 body 的大小提示（size hint）。
    ///
    /// 如果 content-length 是已知的精确值，返回精确提示；
    /// 否则（如分块传输编码或连接关闭分隔）返回默认提示。
    fn size_hint(&self) -> SizeHint {
        /// 辅助函数：将 `DecodedLength` 转换为 `SizeHint`
        #[cfg(all(
            any(feature = "http1", feature = "http2"),
            any(feature = "client", feature = "server")
        ))]
        fn opt_len(decoded_length: DecodedLength) -> SizeHint {
            if let Some(content_length) = decoded_length.into_opt() {
                SizeHint::with_exact(content_length)
            } else {
                SizeHint::default()
            }
        }

        match self.kind {
            Kind::Empty => SizeHint::with_exact(0),
            #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
            Kind::Chan { content_length, .. } => opt_len(content_length),
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 { content_length, .. } => opt_len(content_length),
            #[cfg(feature = "ffi")]
            Kind::Ffi(..) => SizeHint::default(),
        }
    }
}

/// 为 `Incoming` 实现 `Debug` trait，提供可读的调试输出。
///
/// 出于安全考虑，不输出 body 的实际内容，仅显示状态信息（Empty 或 Streaming）。
impl fmt::Debug for Incoming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 定义辅助结构体用于 Debug 输出——这是 Rust 中常见的 Debug 实现模式，
        // 通过临时结构体来控制输出格式
        #[cfg(any(
            all(
                any(feature = "http1", feature = "http2"),
                any(feature = "client", feature = "server")
            ),
            feature = "ffi"
        ))]
        #[derive(Debug)]
        struct Streaming;
        #[derive(Debug)]
        struct Empty;

        let mut builder = f.debug_tuple("Body");
        match self.kind {
            Kind::Empty => builder.field(&Empty),
            // 所有非空变体都显示为 "Streaming"
            #[cfg(any(
                all(
                    any(feature = "http1", feature = "http2"),
                    any(feature = "client", feature = "server")
                ),
                feature = "ffi"
            ))]
            _ => builder.field(&Streaming),
        };

        builder.finish()
    }
}

/// `Sender` 的方法实现块。
///
/// `Sender` 是 HTTP/1 body 通道的发送端，负责向 `Incoming` 推送数据块、
/// trailers，以及处理错误和中止。
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
impl Sender {
    /// 检查此 `Sender` 是否可以发送更多数据。
    ///
    /// 此方法首先通过 "wanter" 机制检查接收端是否已轮询过 body，
    /// 然后检查 mpsc 通道是否有可用容量。
    pub(crate) fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        // 首先检查接收端是否已尝试轮询 body 数据
        ready!(self.poll_want(cx)?);
        self.data_tx
            .poll_ready(cx)
            .map_err(|_| crate::Error::new_closed())
    }

    /// 检查 "wanter" 状态：接收端是否准备好接收数据。
    ///
    /// - `WANT_READY`：接收端已就绪
    /// - `WANT_PENDING`：接收端尚未轮询，返回 Pending
    /// - `CLOSED`：接收端已关闭，返回错误
    fn poll_want(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        match self.want_rx.load(cx) {
            WANT_READY => Poll::Ready(Ok(())),
            WANT_PENDING => Poll::Pending,
            watch::CLOSED => Poll::Ready(Err(crate::Error::new_closed())),
            unexpected => unreachable!("want_rx value: {}", unexpected),
        }
    }

    /// 异步等待发送端就绪（测试用辅助方法）。
    ///
    /// 使用 `poll_fn` 将 `poll_ready` 转换为 `Future`。
    #[cfg(test)]
    async fn ready(&mut self) -> crate::Result<()> {
        futures_util::future::poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// 在数据通道就绪时发送数据（测试用方法）。
    ///
    /// 先等待通道就绪，然后尝试发送数据块。
    #[cfg(test)]
    #[allow(unused)]
    pub(crate) async fn send_data(&mut self, chunk: Bytes) -> crate::Result<()> {
        self.ready().await?;
        self.data_tx
            .try_send(Ok(chunk))
            .map_err(|_| crate::Error::new_closed())
    }

    /// 发送 HTTP trailers。
    ///
    /// Trailers 只能发送一次。如果已经发送过（`trailers_tx` 为 `None`），
    /// 则返回 "closed" 错误。
    #[allow(unused)]
    pub(crate) async fn send_trailers(&mut self, trailers: HeaderMap) -> crate::Result<()> {
        // 使用 take() 从 Option 中取出发送端，确保只能发送一次
        let tx = match self.trailers_tx.take() {
            Some(tx) => tx,
            None => return Err(crate::Error::new_closed()),
        };
        tx.send(trailers).map_err(|_| crate::Error::new_closed())
    }

    /// 尝试立即在此通道上发送数据（非异步版本）。
    ///
    /// # 错误
    ///
    /// 如果通道当前无法接受更多 `Bytes`，返回 `Err(Bytes)`。
    ///
    /// # 注意
    ///
    /// 这主要用于没有异步上下文的其他线程发送数据的场景。
    /// 如果在异步上下文中，优先使用 `send_data()` 方法。
    #[cfg(feature = "http1")]
    pub(crate) fn try_send_data(&mut self, chunk: Bytes) -> Result<(), Bytes> {
        self.data_tx
            .try_send(Ok(chunk))
            // 从发送错误中提取原始数据：因为我们刚发送的是 Ok(chunk)，
            // 所以 into_inner() 一定是 Ok，unwrap 是安全的
            .map_err(|err| err.into_inner().expect("just sent Ok"))
    }

    /// 尝试立即发送 trailers（非异步版本）。
    ///
    /// 返回 `Err(None)` 表示 trailers 已经被发送过，
    /// 返回 `Err(Some(HeaderMap))` 表示接收端已关闭。
    #[cfg(feature = "http1")]
    pub(crate) fn try_send_trailers(
        &mut self,
        trailers: HeaderMap,
    ) -> Result<(), Option<HeaderMap>> {
        let tx = match self.trailers_tx.take() {
            Some(tx) => tx,
            None => return Err(None),
        };

        tx.send(trailers).map_err(Some)
    }

    /// 中止 body 流（测试用方法）。
    ///
    /// 发送一个 "body write aborted" 错误到数据通道，
    /// 这会导致接收端在下次轮询时收到错误。
    #[cfg(test)]
    pub(crate) fn abort(mut self) {
        self.send_error(crate::Error::new_body_write_aborted());
    }

    /// 向数据通道发送错误。
    ///
    /// 通过 clone 发送端来确保即使缓冲区已满也能发送错误。
    /// 这是因为 mpsc 的 `try_send` 在缓冲区满时会失败，
    /// 而 clone 后的发送端可以绕过这个限制。
    pub(crate) fn send_error(&mut self, err: crate::Error) {
        let _ = self
            .data_tx
            // 克隆发送端以确保即使缓冲区已满也能发送错误
            .clone()
            .try_send(Err(err));
    }
}

/// 为 `Sender` 实现 `Debug` trait。
///
/// 通过检查 watch 通道的状态来判断发送端是 "Open" 还是 "Closed"。
#[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 定义辅助结构体用于 Debug 输出
        #[derive(Debug)]
        struct Open;
        #[derive(Debug)]
        struct Closed;

        let mut builder = f.debug_tuple("Sender");
        match self.want_rx.peek() {
            // 如果 watch 通道已关闭，显示 "Closed"
            watch::CLOSED => builder.field(&Closed),
            // 否则显示 "Open"
            _ => builder.field(&Open),
        };

        builder.finish()
    }
}

// ========== 测试模块 ==========

#[cfg(test)]
mod tests {
    /// 导入 `mem` 模块用于检查类型的内存大小
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    use std::mem;
    /// 导入 `Poll` 用于测试异步轮询结果
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    use std::task::Poll;

    /// 导入被测试的类型
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    use super::{Body, Incoming, SizeHint};
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    use super::{DecodedLength, Sender};
    /// `BodyExt` 提供 `frame()` 等便捷扩展方法
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    use http_body_util::BodyExt;

    /// 测试关键类型的内存大小。
    ///
    /// 这些断言主要用于防止意外增加类型的内存占用。
    /// 这是 hyper 中常见的优化守护测试模式。
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[test]
    fn test_size_of() {
        // These are mostly to help catch *accidentally* increasing
        // the size by too much.

        let body_size = mem::size_of::<Incoming>();
        let body_expected_size = mem::size_of::<u64>() * 5;
        assert!(
            body_size <= body_expected_size,
            "Body size = {} <= {}",
            body_size,
            body_expected_size,
        );

        //assert_eq!(body_size, mem::size_of::<Option<Incoming>>(), "Option<Incoming>");

        assert_eq!(
            mem::size_of::<Sender>(),
            mem::size_of::<usize>() * 5,
            "Sender"
        );

        // 验证 Option<Sender> 利用了空指针优化（niche optimization），
        // 与 Sender 本身大小相同
        assert_eq!(
            mem::size_of::<Sender>(),
            mem::size_of::<Option<Sender>>(),
            "Option<Sender>"
        );
    }

    /// 测试不同类型 `Incoming` 的 size_hint 返回值
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[test]
    fn size_hint() {
        fn eq(body: Incoming, b: SizeHint, note: &str) {
            let a = body.size_hint();
            assert_eq!(a.lower(), b.lower(), "lower for {:?}", note);
            assert_eq!(a.upper(), b.upper(), "upper for {:?}", note);
        }

        // 空 body 的 size_hint 应为精确的 0
        eq(Incoming::empty(), SizeHint::with_exact(0), "empty");

        // 分块传输通道的 size_hint 应为默认值（未知大小）
        eq(Incoming::channel().1, SizeHint::new(), "channel");

        // 带有精确长度的通道应返回精确的 size_hint
        eq(
            Incoming::new_channel(DecodedLength::new(4), /*wanter =*/ false).1,
            SizeHint::with_exact(4),
            "channel with length",
        );
    }

    /// 测试通道中止（abort）后接收端收到错误
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[cfg(not(miri))]
    #[tokio::test]
    async fn channel_abort() {
        let (tx, mut rx) = Incoming::channel();

        tx.abort();

        let err = rx.frame().await.unwrap().unwrap_err();
        assert!(err.is_body_write_aborted(), "{:?}", err);
    }

    /// 测试在缓冲区已满时中止通道的行为——先接收已缓冲的数据，再接收错误
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[cfg(all(not(miri), feature = "http1"))]
    #[tokio::test]
    async fn channel_abort_when_buffer_is_full() {
        let (mut tx, mut rx) = Incoming::channel();

        tx.try_send_data("chunk 1".into()).expect("send 1");
        // 缓冲区已满，但仍能发送中止信号（因为 abort 使用了 clone 技巧）
        tx.abort();

        let chunk1 = rx
            .frame()
            .await
            .expect("item 1")
            .expect("chunk 1")
            .into_data()
            .unwrap();
        assert_eq!(chunk1, "chunk 1");

        let err = rx.frame().await.unwrap().unwrap_err();
        assert!(err.is_body_write_aborted(), "{:?}", err);
    }

    /// 测试通道缓冲区容量限制——mpsc(0) 实际只缓冲 1 个元素
    #[cfg(feature = "http1")]
    #[test]
    fn channel_buffers_one() {
        let (mut tx, _rx) = Incoming::channel();

        tx.try_send_data("chunk 1".into()).expect("send 1");

        // 缓冲区现已满，第二次发送应失败
        let chunk2 = tx.try_send_data("chunk 2".into()).expect_err("send 2");
        assert_eq!(chunk2, "chunk 2");
    }

    /// 测试发送端被 drop 后接收端正常结束（返回 None）
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[cfg(not(miri))]
    #[tokio::test]
    async fn channel_empty() {
        let (_, mut rx) = Incoming::channel();

        assert!(rx.frame().await.is_none());
    }

    /// 测试不启用 wanter 时发送端立即就绪
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[test]
    fn channel_ready() {
        let (mut tx, _rx) = Incoming::new_channel(DecodedLength::CHUNKED, /*wanter = */ false);

        let mut tx_ready = tokio_test::task::spawn(tx.ready());

        assert!(tx_ready.poll().is_ready(), "tx is ready immediately");
    }

    /// 测试 wanter 机制：发送端在接收端首次轮询之前保持 Pending
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[test]
    fn channel_wanter() {
        let (mut tx, mut rx) =
            Incoming::new_channel(DecodedLength::CHUNKED, /*wanter = */ true);

        let mut tx_ready = tokio_test::task::spawn(tx.ready());
        let mut rx_data = tokio_test::task::spawn(rx.frame());

        assert!(
            tx_ready.poll().is_pending(),
            "tx isn't ready before rx has been polled"
        );

        assert!(rx_data.poll().is_pending(), "poll rx.data");
        assert!(tx_ready.is_woken(), "rx poll wakes tx");

        assert!(
            tx_ready.poll().is_ready(),
            "tx is ready after rx has been polled"
        );
    }

    /// 测试接收端被 drop 后发送端收到关闭通知
    #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
    #[test]
    fn channel_notices_closure() {
        let (mut tx, rx) = Incoming::new_channel(DecodedLength::CHUNKED, /*wanter = */ true);

        let mut tx_ready = tokio_test::task::spawn(tx.ready());

        assert!(
            tx_ready.poll().is_pending(),
            "tx isn't ready before rx has been polled"
        );

        // 丢弃接收端，模拟连接关闭
        drop(rx);
        assert!(tx_ready.is_woken(), "dropping rx wakes tx");

        match tx_ready.poll() {
            Poll::Ready(Err(ref e)) if e.is_closed() => (),
            unexpected => panic!("tx poll ready unexpected: {:?}", unexpected),
        }
    }
}
