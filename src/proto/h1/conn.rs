//! HTTP/1.1 连接状态机模块。
//!
//! 本模块实现了 HTTP/1.1 连接的核心状态管理，是 hyper HTTP/1.1 协议栈的关键组件。
//! `Conn` 结构体管理一个底层 IO 连接（如 TCP socket）上的完整 HTTP/1.1 生命周期，
//! 包括：
//!
//! - 消息边界检测：确定消息何时开始、何时结束
//! - Keep-alive 管理：决定连接是否可以复用
//! - 读写状态跟踪：通过 `Reading` 和 `Writing` 枚举追踪当前的读写阶段
//! - 协议版本协商：处理 HTTP/1.0 和 HTTP/1.1 之间的差异
//! - 协议升级支持：处理 WebSocket 等协议升级请求
//!
//! 状态机的核心设计：连接在 `Init -> Body -> KeepAlive -> Init` 的循环中运转，
//! 每次循环处理一个 HTTP 事务（请求-响应对）。

// 标准库导入
use std::fmt; // 格式化输出 trait
#[cfg(feature = "server")]
use std::future::Future; // Future trait，用于异步超时计时器
use std::io; // IO 错误类型
use std::marker::{PhantomData, Unpin}; // PhantomData 用于零大小类型标记，Unpin 用于安全 Pin 操作
use std::pin::Pin; // Pin 指针，用于固定异步 Future 在内存中的位置
use std::task::{Context, Poll}; // 异步任务的上下文和轮询结果类型
#[cfg(feature = "server")]
use std::time::Duration; // 时间间隔，用于头部读取超时

// hyper 自定义的 Read/Write trait（兼容 tokio 的异步 IO trait）
use crate::rt::{Read, Write};
// bytes crate: Buf trait 提供缓冲区读取抽象，Bytes 是不可变字节容器
use bytes::{Buf, Bytes};
// futures-core 的 ready! 宏，简化 Poll::Ready 的解包
use futures_core::ready;
// http crate 的头部相关类型
use http::header::{HeaderValue, CONNECTION, TE};
use http::{HeaderMap, Method, Version};
// http-body crate 的 Frame 类型，表示消息体的数据帧或 trailer 帧
use http_body::Frame;
// httparse 的解析器配置
use httparse::ParserConfig;

// 从本模块的 io 子模块导入带缓冲的 IO 层
use super::io::Buffered;
// 从父模块导入 HTTP/1.1 编解码相关类型
use super::{Decoder, Encode, EncodedBuf, Encoder, Http1Transaction, ParseContext, Wants};
// 解码后的消息体长度类型
use crate::body::DecodedLength;
// 时间管理工具（仅服务端）
#[cfg(feature = "server")]
use crate::common::time::Time;
// HTTP 头部解析辅助函数
use crate::headers;
// 从上层 proto 模块导入共享类型
use crate::proto::{BodyLength, MessageHead};
// 异步 Sleep trait（仅服务端，用于超时）
#[cfg(feature = "server")]
use crate::rt::Sleep;

/// HTTP/2 连接前言的固定字节序列。
/// 当服务端检测到客户端发送了 HTTP/2 前言时，会返回特殊错误。
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HTTP/1.1 连接状态机。
///
/// 管理一个已建立的 `Read + Write` IO 连接（如 TCP socket），
/// 在其上执行多个 HTTP 事务。连接负责：
/// - 检测消息边界（消息何时开始和结束）
/// - 管理 keep-alive 状态
/// - 协调读写操作
///
/// 泛型参数：
/// - `I`: 底层 IO 类型（实现 Read + Write + Unpin）
/// - `B`: 消息体数据块类型（实现 Buf trait）
/// - `T`: HTTP 事务类型（Client 或 Server，实现 Http1Transaction）
///
/// `PhantomData<fn(T)>` 使用函数指针类型来避免 T 的 drop check 问题，
/// 同时不影响 Conn 的 Send/Sync 属性。
pub(crate) struct Conn<I, B, T> {
    /// 带缓冲的 IO 层，处理底层的读写缓冲
    io: Buffered<I, EncodedBuf<B>>,
    /// 连接状态，包括读写阶段、keep-alive 状态等
    state: State,
    /// 幻象数据，标记事务类型 T，但不实际持有 T 的实例
    _marker: PhantomData<fn(T)>,
}

/// Conn 的主要实现块。
///
/// 要求底层 IO 实现 Read + Write + Unpin，
/// 数据块类型实现 Buf，事务类型实现 Http1Transaction。
impl<I, B, T> Conn<I, B, T>
where
    I: Read + Write + Unpin,
    B: Buf,
    T: Http1Transaction,
{
    /// 创建新的 HTTP/1.1 连接。
    ///
    /// 初始化所有状态为默认值。默认假设远端使用 HTTP/1.1，
    /// 如果远端声明使用 HTTP/1.0，后续会在 `read_head` 中降级。
    pub(crate) fn new(io: I) -> Conn<I, B, T> {
        Conn {
            io: Buffered::new(io),
            state: State {
                allow_half_close: false,
                cached_headers: None,
                error: None,
                keep_alive: KA::Busy,
                method: None,
                h1_parser_config: ParserConfig::default(),
                h1_max_headers: None,
                #[cfg(feature = "server")]
                h1_header_read_timeout: None,
                #[cfg(feature = "server")]
                h1_header_read_timeout_fut: None,
                #[cfg(feature = "server")]
                h1_header_read_timeout_running: false,
                #[cfg(feature = "server")]
                date_header: true,
                #[cfg(feature = "server")]
                timer: Time::Empty,
                preserve_header_case: false,
                #[cfg(feature = "ffi")]
                preserve_header_order: false,
                title_case_headers: false,
                h09_responses: false,
                #[cfg(feature = "client")]
                on_informational: None,
                notify_read: false,
                reading: Reading::Init,
                writing: Writing::Init,
                upgrade: None,
                // We assume a modern world where the remote speaks HTTP/1.1.
                // If they tell us otherwise, we'll downgrade in `read_head`.
                version: Version::HTTP_11,
                allow_trailer_fields: false,
            },
            _marker: PhantomData,
        }
    }

    /// 设置定时器实现（仅服务端）。
    /// 用于头部读取超时等定时功能。
    #[cfg(feature = "server")]
    pub(crate) fn set_timer(&mut self, timer: Time) {
        self.state.timer = timer;
    }

    /// 设置是否启用流水线刷新优化（仅服务端）。
    ///
    /// 启用后，在读缓冲区非空时（即有待处理的流水线请求），
    /// 可以跳过实际的 IO flush 操作以提升性能。
    #[cfg(feature = "server")]
    pub(crate) fn set_flush_pipeline(&mut self, enabled: bool) {
        self.io.set_flush_pipeline(enabled);
    }

    /// 设置写入策略为队列模式。
    ///
    /// 在队列模式下，多个写入缓冲区会被保持为独立的 buffer，
    /// 使用 vectored write (writev) 进行批量写入，减少系统调用次数。
    pub(crate) fn set_write_strategy_queue(&mut self) {
        self.io.set_write_strategy_queue();
    }

    /// 设置最大缓冲区大小限制。
    ///
    /// 当读缓冲区达到此大小但消息仍未解析完成时，会触发 TooLarge 错误。
    pub(crate) fn set_max_buf_size(&mut self, max: usize) {
        self.io.set_max_buf_size(max);
    }

    /// 设置读缓冲区的精确大小（仅客户端）。
    ///
    /// 用于客户端指定每次读取的精确缓冲区大小。
    #[cfg(feature = "client")]
    pub(crate) fn set_read_buf_exact_size(&mut self, sz: usize) {
        self.io.set_read_buf_exact_size(sz);
    }

    /// 设置写入策略为扁平化模式。
    ///
    /// 在扁平化模式下，所有待写入的数据会被拷贝到一个连续的缓冲区中，
    /// 适用于不支持 vectored write 的 IO 类型。
    pub(crate) fn set_write_strategy_flatten(&mut self) {
        self.io.set_write_strategy_flatten();
    }

    /// 设置 httparse 解析器配置。
    ///
    /// 允许自定义解析器行为，如是否允许请求行中的多余空格、
    /// 是否允许过时的多行头部折叠等。
    pub(crate) fn set_h1_parser_config(&mut self, parser_config: ParserConfig) {
        self.state.h1_parser_config = parser_config;
    }

    /// 启用头部字段名的 Title-Case 输出。
    ///
    /// 将 "content-type" 输出为 "Content-Type" 形式，
    /// 虽然 HTTP/1.1 头部名是大小写无关的，但某些客户端/服务端可能要求特定格式。
    pub(crate) fn set_title_case_headers(&mut self) {
        self.state.title_case_headers = true;
    }

    /// 启用保留原始头部字段名大小写。
    ///
    /// 解析时记录头部字段名的原始大小写，编码时恢复原样。
    pub(crate) fn set_preserve_header_case(&mut self) {
        self.state.preserve_header_case = true;
    }

    /// 启用保留原始头部字段顺序（仅 FFI feature）。
    #[cfg(feature = "ffi")]
    pub(crate) fn set_preserve_header_order(&mut self) {
        self.state.preserve_header_order = true;
    }

    /// 启用 HTTP/0.9 响应支持（仅客户端）。
    ///
    /// HTTP/0.9 是极简版本，响应没有状态行和头部，只有消息体。
    /// 这个选项仅对第一个响应有效，后续响应不再接受 HTTP/0.9。
    #[cfg(feature = "client")]
    pub(crate) fn set_h09_responses(&mut self) {
        self.state.h09_responses = true;
    }

    /// 设置最大允许的头部字段数量。
    pub(crate) fn set_http1_max_headers(&mut self, val: usize) {
        self.state.h1_max_headers = Some(val);
    }

    /// 设置 HTTP/1.1 头部读取超时时间（仅服务端）。
    ///
    /// 如果在指定时间内未能完整读取到请求头部，连接将被关闭。
    /// 这是防止慢速攻击（Slowloris）的重要安全措施。
    #[cfg(feature = "server")]
    pub(crate) fn set_http1_header_read_timeout(&mut self, val: Duration) {
        self.state.h1_header_read_timeout = Some(val);
    }

    /// 允许半关闭连接（仅服务端）。
    ///
    /// 启用后，当读端关闭时不会自动关闭写端，
    /// 允许在对端关闭发送方向后仍然发送响应。
    #[cfg(feature = "server")]
    pub(crate) fn set_allow_half_close(&mut self) {
        self.state.allow_half_close = true;
    }

    /// 禁用自动 Date 头部（仅服务端）。
    ///
    /// 默认情况下，服务端会自动在响应中添加 Date 头部。
    /// 调用此方法可以禁用该行为。
    #[cfg(feature = "server")]
    pub(crate) fn disable_date_header(&mut self) {
        self.state.date_header = false;
    }

    /// 消费连接，返回底层 IO 和剩余的读缓冲区数据。
    ///
    /// 用于协议升级场景，将底层连接的所有权转移给升级后的协议处理器。
    pub(crate) fn into_inner(self) -> (I, Bytes) {
        self.io.into_inner()
    }

    /// 取出挂起的协议升级对象。
    ///
    /// 如果当前连接有待处理的协议升级，返回 `Some(Pending)`；
    /// 否则返回 `None`。升级对象只能被取出一次。
    pub(crate) fn pending_upgrade(&mut self) -> Option<crate::upgrade::Pending> {
        self.state.upgrade.take()
    }

    /// 检查读端是否已关闭。
    pub(crate) fn is_read_closed(&self) -> bool {
        self.state.is_read_closed()
    }

    /// 检查写端是否已关闭。
    pub(crate) fn is_write_closed(&self) -> bool {
        self.state.is_write_closed()
    }

    /// 检查是否可以开始读取新的消息头部。
    ///
    /// 条件：
    /// 1. 当前读状态必须是 `Init`（没有正在读取的消息）
    /// 2. 对于服务端（应先读取），总是允许读取
    /// 3. 对于客户端，必须先发送了请求（writing 不在 Init 状态）
    pub(crate) fn can_read_head(&self) -> bool {
        if !matches!(self.state.reading, Reading::Init) {
            return false;
        }

        // 服务端应先读取请求，所以总是可以读
        if T::should_read_first() {
            return true;
        }

        // 客户端必须先发送了请求，才能读取响应
        !matches!(self.state.writing, Writing::Init)
    }

    /// 检查是否可以读取消息体。
    ///
    /// 读状态为 Body（正常读取消息体）或 Continue（等待 100-continue）时返回 true。
    pub(crate) fn can_read_body(&self) -> bool {
        matches!(
            self.state.reading,
            Reading::Body(..) | Reading::Continue(..)
        )
    }

    /// 检查连接是否处于初始读写状态且读缓冲区为空（仅服务端）。
    ///
    /// 用于判断连接是否在收到任何数据之前就被要求关闭。
    #[cfg(feature = "server")]
    pub(crate) fn has_initial_read_write_state(&self) -> bool {
        matches!(self.state.reading, Reading::Init)
            && matches!(self.state.writing, Writing::Init)
            && self.io.read_buf().is_empty()
    }

    /// 判断在遇到 EOF 时是否应该报告错误。
    ///
    /// 如果连接处于空闲状态，EOF 通常表示对端正常关闭连接；
    /// 如果正在传输消息，EOF 则是一个错误。
    fn should_error_on_eof(&self) -> bool {
        // If we're idle, it's probably just the connection closing gracefully.
        T::should_error_on_parse_eof() && !self.state.is_idle()
    }

    /// 检查读缓冲区是否以 HTTP/2 前言开头。
    ///
    /// 如果客户端尝试使用 HTTP/2 连接到 HTTP/1.1 服务端，
    /// 我们可以检测到这一点并返回更友好的错误。
    fn has_h2_prefix(&self) -> bool {
        let read_buf = self.io.read_buf();
        read_buf.len() >= 24 && read_buf[..24] == *H2_PREFACE
    }

    /// 异步轮询读取 HTTP 消息头部。
    ///
    /// 这是连接读取流程的核心方法。它负责：
    /// 1. 管理头部读取超时计时器（仅服务端）
    /// 2. 调用解析器解析字节流中的 HTTP 消息头部
    /// 3. 根据解析结果设置连接的读状态（Body/KeepAlive 等）
    /// 4. 处理 Expect: 100-continue 请求
    /// 5. 检测 TE: trailers 头部以允许 trailer 字段
    ///
    /// 返回值：
    /// - `Poll::Ready(Some(Ok((head, decode, wants))))`: 成功解析出消息头部
    /// - `Poll::Ready(Some(Err(e)))`: 解析错误
    /// - `Poll::Ready(None)`: 连接已关闭（EOF）
    /// - `Poll::Pending`: 等待更多数据
    pub(super) fn poll_read_head(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<crate::Result<(MessageHead<T::Incoming>, DecodedLength, Wants)>>> {
        debug_assert!(self.can_read_head());
        trace!("Conn::read_head");

        // 服务端：启动或重置头部读取超时计时器
        #[cfg(feature = "server")]
        if !self.state.h1_header_read_timeout_running {
            if let Some(h1_header_read_timeout) = self.state.h1_header_read_timeout {
                let deadline = self.state.timer.now() + h1_header_read_timeout;
                self.state.h1_header_read_timeout_running = true;
                match self.state.h1_header_read_timeout_fut {
                    Some(ref mut h1_header_read_timeout_fut) => {
                        trace!("resetting h1 header read timeout timer");
                        self.state.timer.reset(h1_header_read_timeout_fut, deadline);
                    }
                    None => {
                        trace!("setting h1 header read timeout timer");
                        self.state.h1_header_read_timeout_fut =
                            Some(self.state.timer.sleep_until(deadline));
                    }
                }
            }
        }

        // 调用 IO 层的解析方法，尝试从读缓冲区解析出 HTTP 消息
        let msg = match self.io.parse::<T>(
            cx,
            ParseContext {
                cached_headers: &mut self.state.cached_headers,
                req_method: &mut self.state.method,
                h1_parser_config: self.state.h1_parser_config.clone(),
                h1_max_headers: self.state.h1_max_headers,
                preserve_header_case: self.state.preserve_header_case,
                #[cfg(feature = "ffi")]
                preserve_header_order: self.state.preserve_header_order,
                h09_responses: self.state.h09_responses,
                #[cfg(feature = "client")]
                on_informational: &mut self.state.on_informational,
            },
        ) {
            Poll::Ready(Ok(msg)) => msg,
            Poll::Ready(Err(e)) => return self.on_read_head_error(e),
            Poll::Pending => {
                // 数据不够，检查是否超时（仅服务端）
                #[cfg(feature = "server")]
                if self.state.h1_header_read_timeout_running {
                    if let Some(ref mut h1_header_read_timeout_fut) =
                        self.state.h1_header_read_timeout_fut
                    {
                        if Pin::new(h1_header_read_timeout_fut).poll(cx).is_ready() {
                            self.state.h1_header_read_timeout_running = false;

                            warn!("read header from client timeout");
                            return Poll::Ready(Some(Err(crate::Error::new_header_timeout())));
                        }
                    }
                }

                return Poll::Pending;
            }
        };

        // 解析成功，重置超时状态（仅服务端）
        #[cfg(feature = "server")]
        {
            self.state.h1_header_read_timeout_running = false;
            self.state.h1_header_read_timeout_fut = None;
        }

        // Note: don't deconstruct `msg` into local variables, it appears
        // the optimizer doesn't remove the extra copies.

        debug!("incoming body is {}", msg.decode);

        // Prevent accepting HTTP/0.9 responses after the initial one, if any.
        // 仅允许第一个响应为 HTTP/0.9，后续响应必须是标准格式
        self.state.h09_responses = false;

        // Drop any OnInformational callbacks, we're done there!
        // 清理 1xx 信息性响应的回调
        #[cfg(feature = "client")]
        {
            self.state.on_informational = None;
        }

        // 将 keep-alive 状态设为 Busy，表示正在处理消息
        self.state.busy();
        // 使用位与操作更新 keep-alive 状态：如果消息不支持 keep-alive，则禁用
        self.state.keep_alive &= msg.keep_alive;
        // 记录对端的 HTTP 版本，后续编码响应时会据此调整
        self.state.version = msg.head.version;

        // 根据消息标志构建 Wants 对象
        let mut wants = if msg.wants_upgrade {
            Wants::UPGRADE
        } else {
            Wants::EMPTY
        };

        // 根据消息体长度设置读状态
        if msg.decode == DecodedLength::ZERO {
            // 没有消息体
            if msg.expect_continue {
                debug!("ignoring expect-continue since body is empty");
            }
            self.state.reading = Reading::KeepAlive;
            // 客户端（非先读取方）在消息体为空时尝试转入 keep-alive
            if !T::should_read_first() {
                self.try_keep_alive(cx);
            }
        } else if msg.expect_continue && msg.head.version.gt(&Version::HTTP_10) {
            // 有消息体且需要 100-Continue 确认（仅 HTTP/1.1+）
            let h1_max_header_size = None; // TODO: remove this when we land h1_max_header_size support
            self.state.reading = Reading::Continue(Decoder::new(
                msg.decode,
                self.state.h1_max_headers,
                h1_max_header_size,
            ));
            wants = wants.add(Wants::EXPECT);
        } else {
            // 有消息体，正常读取
            let h1_max_header_size = None; // TODO: remove this when we land h1_max_header_size support
            self.state.reading = Reading::Body(Decoder::new(
                msg.decode,
                self.state.h1_max_headers,
                h1_max_header_size,
            ));
        }

        // 检查 TE: trailers 头部，决定是否允许发送 trailer 字段
        self.state.allow_trailer_fields = msg
            .head
            .headers
            .get(TE)
            .map_or(false, |te_header| te_header == "trailers");

        Poll::Ready(Some(Ok((msg.head, msg.decode, wants))))
    }

    /// 处理消息头部读取错误。
    ///
    /// 根据错误类型和连接状态决定如何响应：
    /// - 如果是解析错误或数据不完整，尝试发送错误响应（服务端）或返回错误
    /// - 如果是正常的 EOF（连接关闭），返回 None
    fn on_read_head_error<Z>(&mut self, e: crate::Error) -> Poll<Option<crate::Result<Z>>> {
        // If we are currently waiting on a message, then an empty
        // message should be reported as an error. If not, it is just
        // the connection closing gracefully.
        let must_error = self.should_error_on_eof();
        self.close_read();
        // 消费读缓冲区中的前导空行（HTTP 允许在消息之间有空行）
        self.io.consume_leading_lines();
        let was_mid_parse = e.is_parse() || !self.io.read_buf().is_empty();
        if was_mid_parse || must_error {
            // We check if the buf contains the h2 Preface
            debug!(
                "parse error ({}) with {} bytes",
                e,
                self.io.read_buf().len()
            );
            match self.on_parse_error(e) {
                Ok(()) => Poll::Pending, // XXX: wat?
                Err(e) => Poll::Ready(Some(Err(e))),
            }
        } else {
            debug!("read eof");
            self.close_write();
            Poll::Ready(None)
        }
    }

    /// 异步轮询读取 HTTP 消息体。
    ///
    /// 从底层 IO 读取消息体数据，支持三种情况：
    /// 1. 正常的 Body 读取：通过 Decoder 解码数据帧和 trailer 帧
    /// 2. Continue 状态：先自动发送 100 Continue 响应，然后转入正常读取
    /// 3. 读取完成后尝试转入 keep-alive 状态
    pub(crate) fn poll_read_body(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Frame<Bytes>>>> {
        debug_assert!(self.can_read_body());

        let (reading, ret) = match self.state.reading {
            Reading::Body(ref mut decoder) => {
                match ready!(decoder.decode(cx, &mut self.io)) {
                    Ok(frame) => {
                        if frame.is_data() {
                            let slice = frame.data_ref().unwrap_or_else(|| unreachable!());
                            let (reading, maybe_frame) = if decoder.is_eof() {
                                debug!("incoming body completed");
                                (
                                    Reading::KeepAlive,
                                    if !slice.is_empty() {
                                        Some(Ok(frame))
                                    } else {
                                        None
                                    },
                                )
                            } else if slice.is_empty() {
                                error!("incoming body unexpectedly ended");
                                // This should be unreachable, since all 3 decoders
                                // either set eof=true or return an Err when reading
                                // an empty slice...
                                (Reading::Closed, None)
                            } else {
                                // 还有更多数据，直接返回当前帧
                                return Poll::Ready(Some(Ok(frame)));
                            };
                            (reading, Poll::Ready(maybe_frame))
                        } else if frame.is_trailers() {
                            // trailer 帧表示消息体结束
                            (Reading::Closed, Poll::Ready(Some(Ok(frame))))
                        } else {
                            trace!("discarding unknown frame");
                            (Reading::Closed, Poll::Ready(None))
                        }
                    }
                    Err(e) => {
                        debug!("incoming body decode error: {}", e);
                        (Reading::Closed, Poll::Ready(Some(Err(e))))
                    }
                }
            }
            Reading::Continue(ref decoder) => {
                // Write the 100 Continue if not already responded...
                // 如果服务端还没发送任何响应，自动发送 100 Continue
                if let Writing::Init = self.state.writing {
                    trace!("automatically sending 100 Continue");
                    let cont = b"HTTP/1.1 100 Continue\r\n\r\n";
                    self.io.headers_buf().extend_from_slice(cont);
                }

                // And now recurse once in the Reading::Body state...
                // 将状态从 Continue 转为 Body，然后递归调用自身
                self.state.reading = Reading::Body(decoder.clone());
                return self.poll_read_body(cx);
            }
            _ => unreachable!("poll_read_body invalid state: {:?}", self.state.reading),
        };

        self.state.reading = reading;
        self.try_keep_alive(cx);
        ret
    }

    /// 检查连接是否需要再次读取。
    ///
    /// 返回 `notify_read` 标志的当前值并将其重置为 false。
    /// 调度器使用此方法来决定是否需要再次调用 poll_read。
    pub(crate) fn wants_read_again(&mut self) -> bool {
        let ret = self.state.notify_read;
        self.state.notify_read = false;
        ret
    }

    /// 在无法读取头部或消息体时，轮询保持连接活跃。
    ///
    /// 处理两种情况：
    /// 1. 消息传输中途：检测 EOF 以提前发现连接断开
    /// 2. 空闲状态（客户端）：确保没有意外的数据到达
    pub(crate) fn poll_read_keep_alive(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        debug_assert!(!self.can_read_head() && !self.can_read_body());

        if self.is_read_closed() {
            Poll::Pending
        } else if self.is_mid_message() {
            self.mid_message_detect_eof(cx)
        } else {
            self.require_empty_read(cx)
        }
    }

    /// 检查连接是否处于消息传输中途。
    ///
    /// 只有当读和写都处于 Init 状态时，连接才不在消息传输中。
    fn is_mid_message(&self) -> bool {
        !matches!(
            (&self.state.reading, &self.state.writing),
            (&Reading::Init, &Writing::Init)
        )
    }

    // This will check to make sure the io object read is empty.
    //
    // This should only be called for Clients wanting to enter the idle
    // state.
    /// 要求读取为空（仅客户端空闲状态调用）。
    ///
    /// 客户端在进入空闲状态前需要确认没有意外的数据到达。
    /// 如果有数据到达，这通常意味着服务端在没有请求的情况下发送了消息，
    /// 这是一个协议错误。
    fn require_empty_read(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        debug_assert!(!self.can_read_head() && !self.can_read_body() && !self.is_read_closed());
        debug_assert!(!self.is_mid_message());
        debug_assert!(T::is_client());

        if !self.io.read_buf().is_empty() {
            debug!("received an unexpected {} bytes", self.io.read_buf().len());
            return Poll::Ready(Err(crate::Error::new_unexpected_message()));
        }

        let num_read = ready!(self.force_io_read(cx)).map_err(crate::Error::new_io)?;

        if num_read == 0 {
            let ret = if self.should_error_on_eof() {
                trace!("found unexpected EOF on busy connection: {:?}", self.state);
                Poll::Ready(Err(crate::Error::new_incomplete()))
            } else {
                trace!("found EOF on idle connection, closing");
                Poll::Ready(Ok(()))
            };

            // order is important: should_error needs state BEFORE close_read
            // 顺序很重要：should_error_on_eof 需要在 close_read 之前检查状态
            self.state.close_read();
            return ret;
        }

        debug!(
            "received unexpected {} bytes on an idle connection",
            num_read
        );
        Poll::Ready(Err(crate::Error::new_unexpected_message()))
    }

    /// 在消息传输中途检测 EOF。
    ///
    /// 如果在消息传输过程中对端关闭了连接，这通常是一个错误。
    /// 但如果启用了 `allow_half_close`，则允许读端先关闭。
    fn mid_message_detect_eof(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        debug_assert!(!self.can_read_head() && !self.can_read_body() && !self.is_read_closed());
        debug_assert!(self.is_mid_message());

        // 如果允许半关闭或读缓冲区非空，不需要检测 EOF
        if self.state.allow_half_close || !self.io.read_buf().is_empty() {
            return Poll::Pending;
        }

        let num_read = ready!(self.force_io_read(cx)).map_err(crate::Error::new_io)?;

        if num_read == 0 {
            trace!("found unexpected EOF on busy connection: {:?}", self.state);
            self.state.close_read();
            Poll::Ready(Err(crate::Error::new_incomplete()))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    /// 强制执行一次底层 IO 读取操作。
    ///
    /// 如果读取失败，关闭整个连接并返回错误。
    fn force_io_read(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        debug_assert!(!self.state.is_read_closed());

        let result = ready!(self.io.poll_read_from_io(cx));
        Poll::Ready(result.map_err(|e| {
            trace!(error = %e, "force_io_read; io error");
            self.state.close();
            e
        }))
    }

    /// 在适当时机通知调度器需要再次轮询读取。
    ///
    /// 当连接之前因为需要等待写入完成而暂停了读取时，
    /// 在写入完成后需要通知调度器重新尝试读取。
    ///
    /// 这比使用 `task::current()` + `notify()` 在流水线基准测试中明显更快。
    fn maybe_notify(&mut self, cx: &mut Context<'_>) {
        // its possible that we returned NotReady from poll() without having
        // exhausted the underlying Io. We would have done this when we
        // determined we couldn't keep reading until we knew how writing
        // would finish.

        match self.state.reading {
            Reading::Continue(..) | Reading::Body(..) | Reading::KeepAlive | Reading::Closed => {
                return
            }
            Reading::Init => (),
        };

        match self.state.writing {
            Writing::Body(..) => return,
            Writing::Init | Writing::KeepAlive | Writing::Closed => (),
        }

        // 如果底层 IO 没有被阻塞，尝试预读数据
        if !self.io.is_read_blocked() {
            if self.io.read_buf().is_empty() {
                match self.io.poll_read_from_io(cx) {
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            trace!("maybe_notify; read eof");
                            if self.state.is_idle() {
                                self.state.close();
                            } else {
                                self.close_read()
                            }
                            return;
                        }
                    }
                    Poll::Pending => {
                        trace!("maybe_notify; read_from_io blocked");
                        return;
                    }
                    Poll::Ready(Err(e)) => {
                        trace!("maybe_notify; read_from_io error: {}", e);
                        self.state.close();
                        self.state.error = Some(crate::Error::new_io(e));
                    }
                }
            }
            // 设置标志，通知调度器下一轮需要读取
            self.state.notify_read = true;
        }
    }

    /// 尝试将连接转入 keep-alive 状态。
    ///
    /// 在读写都完成后调用，检查是否可以复用连接处理下一个请求。
    fn try_keep_alive(&mut self, cx: &mut Context<'_>) {
        self.state.try_keep_alive::<T>();
        self.maybe_notify(cx);
    }

    /// 检查是否可以写入消息头部。
    ///
    /// 条件：
    /// 1. 对于客户端，如果读端已关闭则不能再写
    /// 2. 写状态必须是 Init
    /// 3. 写缓冲区必须有空间（headers_buf 可用）
    pub(crate) fn can_write_head(&self) -> bool {
        if !T::should_read_first() && matches!(self.state.reading, Reading::Closed) {
            return false;
        }

        match self.state.writing {
            Writing::Init => self.io.can_headers_buf(),
            _ => false,
        }
    }

    /// 检查是否可以写入消息体。
    pub(crate) fn can_write_body(&self) -> bool {
        match self.state.writing {
            Writing::Body(..) => true,
            Writing::Init | Writing::KeepAlive | Writing::Closed => false,
        }
    }

    /// 检查 IO 缓冲区是否还能容纳更多数据。
    pub(crate) fn can_buffer_body(&self) -> bool {
        self.io.can_buffer()
    }

    /// 写入 HTTP 消息头部并设置写状态。
    ///
    /// 编码消息头部后，根据编码器的状态决定后续的写状态：
    /// - 如果编码器不是 EOF（还有消息体要写），转为 Writing::Body
    /// - 如果编码器标记为 "last"（最后一个消息），转为 Writing::Closed
    /// - 否则转为 Writing::KeepAlive
    pub(crate) fn write_head(&mut self, head: MessageHead<T::Outgoing>, body: Option<BodyLength>) {
        if let Some(encoder) = self.encode_head(head, body) {
            self.state.writing = if !encoder.is_eof() {
                Writing::Body(encoder)
            } else if encoder.is_last() {
                Writing::Closed
            } else {
                Writing::KeepAlive
            };
        }
    }

    /// 编码 HTTP 消息头部到写缓冲区。
    ///
    /// 处理版本协商、keep-alive 头部、头部大小写等，
    /// 返回消息体的编码器（如果编码成功）。
    fn encode_head(
        &mut self,
        mut head: MessageHead<T::Outgoing>,
        body: Option<BodyLength>,
    ) -> Option<Encoder> {
        debug_assert!(self.can_write_head());

        // 对于客户端，发送请求时将状态设为 busy
        if !T::should_read_first() {
            self.state.busy();
        }

        // 根据对端版本调整消息头部
        self.enforce_version(&mut head);

        let buf = self.io.headers_buf();
        match super::role::encode_headers::<T>(
            Encode {
                head: &mut head,
                body,
                #[cfg(feature = "server")]
                keep_alive: self.state.wants_keep_alive(),
                req_method: &mut self.state.method,
                title_case_headers: self.state.title_case_headers,
                #[cfg(feature = "server")]
                date_header: self.state.date_header,
            },
            buf,
        ) {
            Ok(encoder) => {
                debug_assert!(self.state.cached_headers.is_none());
                debug_assert!(head.headers.is_empty());
                // 缓存清空后的 HeaderMap 以便下次复用
                self.state.cached_headers = Some(head.headers);

                // 提取客户端的 OnInformational 回调
                #[cfg(feature = "client")]
                {
                    self.state.on_informational =
                        head.extensions.remove::<crate::ext::OnInformational>();
                }

                Some(encoder)
            }
            Err(err) => {
                self.state.error = Some(err);
                self.state.writing = Writing::Closed;
                None
            }
        }
    }

    // Fix keep-alive when Connection: keep-alive header is not present
    /// 修复 keep-alive 行为。
    ///
    /// 根据 HTTP 版本和 Connection 头部的存在情况调整 keep-alive 行为：
    /// - HTTP/1.0：没有 Connection: keep-alive 时禁用 keep-alive
    /// - HTTP/1.1：如果需要 keep-alive 但头部中没有，则添加 Connection: keep-alive
    fn fix_keep_alive(&mut self, head: &mut MessageHead<T::Outgoing>) {
        let outgoing_is_keep_alive = head
            .headers
            .get(CONNECTION)
            .map_or(false, headers::connection_keep_alive);

        if !outgoing_is_keep_alive {
            match head.version {
                // If response is version 1.0 and keep-alive is not present in the response,
                // disable keep-alive so the server closes the connection
                Version::HTTP_10 => self.state.disable_keep_alive(),
                // If response is version 1.1 and keep-alive is wanted, add
                // Connection: keep-alive header when not present
                Version::HTTP_11 => {
                    if self.state.wants_keep_alive() {
                        head.headers
                            .insert(CONNECTION, HeaderValue::from_static("keep-alive"));
                    }
                }
                _ => (),
            }
        }
    }

    // If we know the remote speaks an older version, we try to fix up any messages
    // to work with our older peer.
    /// 强制执行版本兼容性规则。
    ///
    /// 根据对端的 HTTP 版本调整出站消息：
    /// - HTTP/1.0 对端：降级消息版本号，修复 keep-alive
    /// - HTTP/1.1 对端：如果 keep-alive 被禁用，添加 Connection: close
    fn enforce_version(&mut self, head: &mut MessageHead<T::Outgoing>) {
        match self.state.version {
            Version::HTTP_10 => {
                // Fixes response or connection when keep-alive header is not present
                self.fix_keep_alive(head);
                // If the remote only knows HTTP/1.0, we should force ourselves
                // to do only speak HTTP/1.0 as well.
                head.version = Version::HTTP_10;
            }
            Version::HTTP_11 => {
                if let KA::Disabled = self.state.keep_alive.status() {
                    head.headers
                        .insert(CONNECTION, HeaderValue::from_static("close"));
                }
            }
            _ => (),
        }
        // If the remote speaks HTTP/1.1, then it *should* be fine with
        // both HTTP/1.0 and HTTP/1.1 from us. So again, we just let
        // the user's headers be.
    }

    /// 写入消息体数据块。
    ///
    /// 将数据块通过编码器编码后放入写缓冲区。
    /// 如果编码器到达 EOF，自动转换写状态。
    pub(crate) fn write_body(&mut self, chunk: B) {
        debug_assert!(self.can_write_body() && self.can_buffer_body());
        // empty chunks should be discarded at Dispatcher level
        debug_assert!(chunk.remaining() != 0);

        let state = match self.state.writing {
            Writing::Body(ref mut encoder) => {
                self.io.buffer(encoder.encode(chunk));

                if !encoder.is_eof() {
                    return;
                }

                if encoder.is_last() {
                    Writing::Closed
                } else {
                    Writing::KeepAlive
                }
            }
            _ => unreachable!("write_body invalid state: {:?}", self.state.writing),
        };

        self.state.writing = state;
    }

    /// 写入 HTTP trailer 字段。
    ///
    /// Trailer 字段在分块传输编码的最后一个块之后发送。
    /// 仅在以下条件满足时才会发送：
    /// - 服务端：请求中包含 TE: trailers 头部
    /// - 编码方式为 chunked 且声明了 Trailer 头部
    pub(crate) fn write_trailers(&mut self, trailers: HeaderMap) {
        // 服务端检查是否允许发送 trailer 字段
        if T::is_server() && !self.state.allow_trailer_fields {
            debug!("trailers not allowed to be sent");
            return;
        }
        debug_assert!(self.can_write_body() && self.can_buffer_body());

        match self.state.writing {
            Writing::Body(ref encoder) => {
                if let Some(enc_buf) =
                    encoder.encode_trailers(trailers, self.state.title_case_headers)
                {
                    self.io.buffer(enc_buf);

                    self.state.writing = if encoder.is_last() || encoder.is_close_delimited() {
                        Writing::Closed
                    } else {
                        Writing::KeepAlive
                    };
                }
            }
            _ => unreachable!("write_trailers invalid state: {:?}", self.state.writing),
        }
    }

    /// 写入最后一个消息体数据块并结束消息体。
    ///
    /// 这是一个优化方法，将最后一个数据块和结束标记合并为一次写入。
    /// 例如，chunked 编码时可以将最后一个数据块和 "0\r\n\r\n" 一起写入。
    pub(crate) fn write_body_and_end(&mut self, chunk: B) {
        debug_assert!(self.can_write_body() && self.can_buffer_body());
        // empty chunks should be discarded at Dispatcher level
        debug_assert!(chunk.remaining() != 0);

        let state = match self.state.writing {
            Writing::Body(ref encoder) => {
                let can_keep_alive = encoder.encode_and_end(chunk, self.io.write_buf());
                if can_keep_alive {
                    Writing::KeepAlive
                } else {
                    Writing::Closed
                }
            }
            _ => unreachable!("write_body invalid state: {:?}", self.state.writing),
        };

        self.state.writing = state;
    }

    /// 结束消息体写入。
    ///
    /// 对于 chunked 编码，写入终止块 "0\r\n\r\n"。
    /// 对于 Content-Length 编码，验证已写入的字节数是否匹配。
    pub(crate) fn end_body(&mut self) -> crate::Result<()> {
        debug_assert!(self.can_write_body());

        let encoder = match self.state.writing {
            Writing::Body(ref mut enc) => enc,
            _ => return Ok(()),
        };

        // end of stream, that means we should try to eof
        match encoder.end() {
            Ok(end) => {
                if let Some(end) = end {
                    self.io.buffer(end);
                }

                self.state.writing = if encoder.is_last() || encoder.is_close_delimited() {
                    Writing::Closed
                } else {
                    Writing::KeepAlive
                };

                Ok(())
            }
            Err(not_eof) => {
                // Content-Length 不匹配：声明的长度未写满
                self.state.writing = Writing::Closed;
                Err(crate::Error::new_body_write_aborted().with(not_eof))
            }
        }
    }

    // When we get a parse error, depending on what side we are, we might be able
    // to write a response before closing the connection.
    //
    // - Client: there is nothing we can do
    // - Server: if Response hasn't been written yet, we can send a 4xx response
    /// 处理解析错误。
    ///
    /// 服务端在尚未发送响应时可以发送错误响应（如 400 Bad Request），
    /// 客户端则只能返回错误。
    /// 如果检测到 HTTP/2 前言，返回专门的版本错误。
    fn on_parse_error(&mut self, err: crate::Error) -> crate::Result<()> {
        if let Writing::Init = self.state.writing {
            // 检查是否是 HTTP/2 前言
            if self.has_h2_prefix() {
                return Err(crate::Error::new_version_h2());
            }
            // 尝试生成错误响应（仅服务端有效）
            if let Some(msg) = T::on_error(&err) {
                // Drop the cached headers so as to not trigger a debug
                // assert in `write_head`...
                self.state.cached_headers.take();
                self.write_head(msg, None);
                self.state.error = Some(err);
                return Ok(());
            }
        }

        // fallback is pass the error back up
        Err(err)
    }

    /// 异步刷新写缓冲区到底层 IO。
    ///
    /// 刷新后尝试转入 keep-alive 状态。
    pub(crate) fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self.io).poll_flush(cx))?;
        self.try_keep_alive(cx);
        trace!("flushed({}): {:?}", T::LOG, self.state);
        Poll::Ready(Ok(()))
    }

    /// 异步关闭底层 IO 连接。
    pub(crate) fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match ready!(Pin::new(self.io.io_mut()).poll_shutdown(cx)) {
            Ok(()) => {
                trace!("shut down IO complete");
                Poll::Ready(Ok(()))
            }
            Err(e) => {
                debug!("error shutting down IO: {}", e);
                Poll::Ready(Err(e))
            }
        }
    }

    /// If the read side can be cheaply drained, do so. Otherwise, close.
    /// 尝试排空读端或关闭读端。
    ///
    /// 当用户不关心请求体的剩余部分时调用。
    /// 如果消息体很小可以快速排空，就排空它；否则直接关闭读端。
    pub(super) fn poll_drain_or_close_read(&mut self, cx: &mut Context<'_>) {
        if let Reading::Continue(ref decoder) = self.state.reading {
            // skip sending the 100-continue
            // just move forward to a read, in case a tiny body was included
            self.state.reading = Reading::Body(decoder.clone());
        }

        let _ = self.poll_read_body(cx);

        // If still in Reading::Body, just give up
        match self.state.reading {
            Reading::Init | Reading::KeepAlive => {
                trace!("body drained")
            }
            _ => self.close_read(),
        }
    }

    /// 关闭连接的读端。
    pub(crate) fn close_read(&mut self) {
        self.state.close_read();
    }

    /// 关闭连接的写端。
    pub(crate) fn close_write(&mut self) {
        self.state.close_write();
    }

    /// 禁用 keep-alive（仅服务端）。
    ///
    /// 如果连接当前处于空闲状态，立即关闭；
    /// 否则标记为禁用，等当前事务完成后关闭。
    #[cfg(feature = "server")]
    pub(crate) fn disable_keep_alive(&mut self) {
        if self.state.is_idle() {
            trace!("disable_keep_alive; closing idle connection");
            self.state.close();
        } else {
            trace!("disable_keep_alive; in-progress connection");
            self.state.disable_keep_alive();
        }
    }

    /// 取出并返回存储的错误。
    ///
    /// 某些错误发生在无法直接返回给用户的时机，
    /// 这些错误会被暂存在 state 中，等后续通过此方法取出。
    pub(crate) fn take_error(&mut self) -> crate::Result<()> {
        if let Some(err) = self.state.error.take() {
            Err(err)
        } else {
            Ok(())
        }
    }

    /// 准备协议升级。
    ///
    /// 创建一个升级通道，返回接收端给调用者，
    /// 发送端存储在连接状态中，等待升级完成后传递底层 IO。
    pub(super) fn on_upgrade(&mut self) -> crate::upgrade::OnUpgrade {
        trace!("{}: prepare possible HTTP upgrade", T::LOG);
        self.state.prepare_upgrade()
    }
}

/// Conn 的 Debug 实现，显示连接的状态和 IO 信息。
impl<I, B: Buf, T> fmt::Debug for Conn<I, B, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("state", &self.state)
            .field("io", &self.io)
            .finish()
    }
}

// B and T are never pinned
// B 和 T 永远不会被 pin，所以只要 I 是 Unpin，Conn 就是 Unpin
impl<I: Unpin, B, T> Unpin for Conn<I, B, T> {}

/// 连接的内部状态结构体。
///
/// 维护了 HTTP/1.1 连接在整个生命周期中需要跟踪的所有状态，
/// 包括读写阶段、keep-alive 状态、缓存、超时等。
struct State {
    /// 是否允许半关闭（读端关闭时不自动关闭写端）
    allow_half_close: bool,
    /// Re-usable HeaderMap to reduce allocating new ones.
    /// 可复用的 HeaderMap 缓存，减少内存分配
    cached_headers: Option<HeaderMap>,
    /// If an error occurs when there wasn't a direct way to return it
    /// back to the user, this is set.
    /// 暂存的错误，在无法直接返回给用户时使用
    error: Option<crate::Error>,
    /// Current keep-alive status.
    /// 当前的 keep-alive 状态
    keep_alive: KA,
    /// If mid-message, the HTTP Method that started it.
    ///
    /// This is used to know things such as if the message can include
    /// a body or not.
    /// 当前消息的 HTTP 方法，用于判断响应是否可以包含消息体
    method: Option<Method>,
    /// httparse 解析器配置
    h1_parser_config: ParserConfig,
    /// 最大允许的头部字段数量
    h1_max_headers: Option<usize>,
    /// 头部读取超时时间（仅服务端）
    #[cfg(feature = "server")]
    h1_header_read_timeout: Option<Duration>,
    /// 头部读取超时的 Future（仅服务端）
    #[cfg(feature = "server")]
    h1_header_read_timeout_fut: Option<Pin<Box<dyn Sleep>>>,
    /// 头部读取超时计时器是否正在运行（仅服务端）
    #[cfg(feature = "server")]
    h1_header_read_timeout_running: bool,
    /// 是否自动添加 Date 头部（仅服务端）
    #[cfg(feature = "server")]
    date_header: bool,
    /// 定时器实现（仅服务端）
    #[cfg(feature = "server")]
    timer: Time,
    /// 是否保留头部字段名的原始大小写
    preserve_header_case: bool,
    /// 是否保留头部字段的原始顺序（仅 FFI）
    #[cfg(feature = "ffi")]
    preserve_header_order: bool,
    /// 是否使用 Title-Case 头部字段名
    title_case_headers: bool,
    /// 是否允许 HTTP/0.9 响应
    h09_responses: bool,
    /// If set, called with each 1xx informational response received for
    /// the current request. MUST be unset after a non-1xx response is
    /// received.
    /// 1xx 信息性响应回调（仅客户端）
    #[cfg(feature = "client")]
    on_informational: Option<crate::ext::OnInformational>,
    /// Set to true when the Dispatcher should poll read operations
    /// again. See the `maybe_notify` method for more.
    /// 通知调度器需要再次轮询读操作
    notify_read: bool,
    /// State of allowed reads
    /// 当前的读取状态
    reading: Reading,
    /// State of allowed writes
    /// 当前的写入状态
    writing: Writing,
    /// An expected pending HTTP upgrade.
    /// 挂起的 HTTP 协议升级对象
    upgrade: Option<crate::upgrade::Pending>,
    /// Either HTTP/1.0 or 1.1 connection
    /// 对端的 HTTP 版本
    version: Version,
    /// Flag to track if trailer fields are allowed to be sent
    /// 是否允许发送 trailer 字段（由请求的 TE: trailers 头部决定）
    allow_trailer_fields: bool,
}

/// 读取状态枚举。
///
/// 追踪连接当前处于读取生命周期的哪个阶段。
#[derive(Debug)]
enum Reading {
    /// 初始状态：等待开始读取新消息
    Init,
    /// 等待 100-Continue 确认后再读取消息体
    Continue(Decoder),
    /// 正在读取消息体
    Body(Decoder),
    /// 消息体读取完毕，等待 keep-alive 复用
    KeepAlive,
    /// 读端已关闭
    Closed,
}

/// 写入状态枚举。
///
/// 追踪连接当前处于写入生命周期的哪个阶段。
enum Writing {
    /// 初始状态：等待开始写入新消息
    Init,
    /// 正在写入消息体，持有编码器
    Body(Encoder),
    /// 消息写入完毕，等待 keep-alive 复用
    KeepAlive,
    /// 写端已关闭
    Closed,
}

/// State 的 Debug 实现，有选择地显示字段。
impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("State");
        builder
            .field("reading", &self.reading)
            .field("writing", &self.writing)
            .field("keep_alive", &self.keep_alive);

        // Only show error field if it's interesting...
        if let Some(ref error) = self.error {
            builder.field("error", error);
        }

        if self.allow_half_close {
            builder.field("allow_half_close", &true);
        }

        // Purposefully leaving off other fields..

        builder.finish()
    }
}

/// Writing 的 Debug 实现。
impl fmt::Debug for Writing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Writing::Init => f.write_str("Init"),
            Writing::Body(ref enc) => f.debug_tuple("Body").field(enc).finish(),
            Writing::KeepAlive => f.write_str("KeepAlive"),
            Writing::Closed => f.write_str("Closed"),
        }
    }
}

/// 为 KA 实现 `BitAndAssign<bool>` 运算符。
///
/// 当 `enabled` 为 false 时（即对端不支持 keep-alive），
/// 将 KA 状态设为 Disabled。这允许使用 `self.keep_alive &= msg.keep_alive` 语法。
impl std::ops::BitAndAssign<bool> for KA {
    fn bitand_assign(&mut self, enabled: bool) {
        if !enabled {
            trace!("remote disabling keep-alive");
            *self = KA::Disabled;
        }
    }
}

/// Keep-Alive 状态枚举。
///
/// 追踪连接的 keep-alive 状态：
/// - `Idle`: 空闲，可以接受新的请求
/// - `Busy`: 正在处理请求（默认初始状态）
/// - `Disabled`: keep-alive 已禁用，当前事务完成后将关闭连接
#[derive(Clone, Copy, Debug, Default)]
enum KA {
    /// 空闲状态
    Idle,
    /// 忙碌状态（默认值）
    #[default]
    Busy,
    /// 已禁用 keep-alive
    Disabled,
}

/// KA 的方法实现。
impl KA {
    /// 设为空闲状态
    fn idle(&mut self) {
        *self = KA::Idle;
    }

    /// 设为忙碌状态
    fn busy(&mut self) {
        *self = KA::Busy;
    }

    /// 禁用 keep-alive
    fn disable(&mut self) {
        *self = KA::Disabled;
    }

    /// 获取当前状态（通过 Copy 返回）
    fn status(&self) -> KA {
        *self
    }
}

/// State 的方法实现。
impl State {
    /// 关闭连接（读端和写端都关闭）。
    fn close(&mut self) {
        trace!("State::close()");
        self.reading = Reading::Closed;
        self.writing = Writing::Closed;
        self.keep_alive.disable();
    }

    /// 关闭读端。
    fn close_read(&mut self) {
        trace!("State::close_read()");
        self.reading = Reading::Closed;
        self.keep_alive.disable();
    }

    /// 关闭写端。
    fn close_write(&mut self) {
        trace!("State::close_write()");
        self.writing = Writing::Closed;
        self.keep_alive.disable();
    }

    /// 检查是否想要保持连接（即 keep-alive 未被禁用）。
    fn wants_keep_alive(&self) -> bool {
        !matches!(self.keep_alive.status(), KA::Disabled)
    }

    /// 尝试转入 keep-alive 状态。
    ///
    /// 当读和写都到达 KeepAlive 状态时：
    /// - 如果 keep-alive 状态为 Busy，转为 Idle（连接可复用）
    /// - 否则关闭连接
    ///
    /// 如果一端关闭而另一端 KeepAlive，也关闭连接。
    fn try_keep_alive<T: Http1Transaction>(&mut self) {
        match (&self.reading, &self.writing) {
            (&Reading::KeepAlive, &Writing::KeepAlive) => {
                if let KA::Busy = self.keep_alive.status() {
                    self.idle::<T>();
                } else {
                    trace!(
                        "try_keep_alive({}): could keep-alive, but status = {:?}",
                        T::LOG,
                        self.keep_alive
                    );
                    self.close();
                }
            }
            (&Reading::Closed, &Writing::KeepAlive) | (&Reading::KeepAlive, &Writing::Closed) => {
                self.close()
            }
            _ => (),
        }
    }

    /// 禁用 keep-alive。
    fn disable_keep_alive(&mut self) {
        self.keep_alive.disable()
    }

    /// 将 keep-alive 状态设为 Busy（如果未被禁用）。
    fn busy(&mut self) {
        if let KA::Disabled = self.keep_alive.status() {
            return;
        }
        self.keep_alive.busy();
    }

    /// 将连接转入空闲状态。
    ///
    /// 重置读写状态为 Init，清除 method 等临时状态。
    /// 对于客户端，设置 notify_read 标志以触发下一轮轮询。
    fn idle<T: Http1Transaction>(&mut self) {
        debug_assert!(!self.is_idle(), "State::idle() called while idle");

        self.method = None;
        self.keep_alive.idle();

        if !self.is_idle() {
            self.close();
            return;
        }

        self.reading = Reading::Init;
        self.writing = Writing::Init;

        // !T::should_read_first() means Client.
        //
        // If Client connection has just gone idle, the Dispatcher
        // should try the poll loop one more time, so as to poll the
        // pending requests stream.
        // 客户端连接变为空闲时，通知调度器检查是否有待发送的请求
        if !T::should_read_first() {
            self.notify_read = true;
        }

        // 服务端：如果配置了头部读取超时，通知读取以启动超时计时器
        #[cfg(feature = "server")]
        if self.h1_header_read_timeout.is_some() {
            // Next read will start and poll the header read timeout,
            // so we can close the connection if another header isn't
            // received in a timely manner.
            self.notify_read = true;
        }
    }

    /// 检查连接是否处于空闲状态。
    fn is_idle(&self) -> bool {
        matches!(self.keep_alive.status(), KA::Idle)
    }

    /// 检查读端是否已关闭。
    fn is_read_closed(&self) -> bool {
        matches!(self.reading, Reading::Closed)
    }

    /// 检查写端是否已关闭。
    fn is_write_closed(&self) -> bool {
        matches!(self.writing, Writing::Closed)
    }

    /// 准备协议升级，创建升级通道。
    ///
    /// 返回 `OnUpgrade` 接收端给调用者，发送端 `Pending` 存储在 state 中。
    fn prepare_upgrade(&mut self) -> crate::upgrade::OnUpgrade {
        let (tx, rx) = crate::upgrade::pending();
        self.upgrade = Some(tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(feature = "nightly", not(miri)))]
    #[bench]
    fn bench_read_head_short(b: &mut ::test::Bencher) {
        use super::*;
        use crate::common::io::Compat;
        let s = b"GET / HTTP/1.1\r\nHost: localhost:8080\r\n\r\n";
        let len = s.len();
        b.bytes = len as u64;

        // an empty IO, we'll be skipping and using the read buffer anyways
        let io = Compat(tokio_test::io::Builder::new().build());
        let mut conn = Conn::<_, bytes::Bytes, crate::proto::h1::ServerTransaction>::new(io);
        *conn.io.read_buf_mut() = ::bytes::BytesMut::from(&s[..]);
        conn.state.cached_headers = Some(HeaderMap::with_capacity(2));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        b.iter(|| {
            rt.block_on(futures_util::future::poll_fn(|cx| {
                match conn.poll_read_head(cx) {
                    Poll::Ready(Some(Ok(x))) => {
                        ::test::black_box(&x);
                        let mut headers = x.0.headers;
                        headers.clear();
                        conn.state.cached_headers = Some(headers);
                    }
                    f => panic!("expected Ready(Some(Ok(..))): {:?}", f),
                }

                conn.io.read_buf_mut().reserve(1);
                unsafe {
                    conn.io.read_buf_mut().set_len(len);
                }
                conn.state.reading = Reading::Init;
                Poll::Ready(())
            }));
        });
    }

    /*
    //TODO: rewrite these using dispatch... someday...
    use futures::{Async, Future, Stream, Sink};
    use futures::future;

    use proto::{self, ClientTransaction, MessageHead, ServerTransaction};
    use super::super::Encoder;
    use mock::AsyncIo;

    use super::{Conn, Decoder, Reading, Writing};
    use ::uri::Uri;

    use std::str::FromStr;

    #[test]
    fn test_conn_init_read() {
        let good_message = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let len = good_message.len();
        let io = AsyncIo::new_buf(good_message, len);
        let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);

        match conn.poll().unwrap() {
            Async::Ready(Some(Frame::Message { message, body: false })) => {
                assert_eq!(message, MessageHead {
                    subject: ::proto::RequestLine(::Get, Uri::from_str("/").unwrap()),
                    .. MessageHead::default()
                })
            },
            f => panic!("frame is not Frame::Message: {:?}", f)
        }
    }

    #[test]
    fn test_conn_parse_partial() {
        let _: Result<(), ()> = future::lazy(|| {
            let good_message = b"GET / HTTP/1.1\r\nHost: foo.bar\r\n\r\n".to_vec();
            let io = AsyncIo::new_buf(good_message, 10);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            assert!(conn.poll().unwrap().is_not_ready());
            conn.io.io_mut().block_in(50);
            let async = conn.poll().unwrap();
            assert!(async.is_ready());
            match async {
                Async::Ready(Some(Frame::Message { .. })) => (),
                f => panic!("frame is not Message: {:?}", f),
            }
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_init_read_eof_idle() {
        let io = AsyncIo::new_buf(vec![], 1);
        let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
        conn.state.idle();

        match conn.poll().unwrap() {
            Async::Ready(None) => {},
            other => panic!("frame is not None: {:?}", other)
        }
    }

    #[test]
    fn test_conn_init_read_eof_idle_partial_parse() {
        let io = AsyncIo::new_buf(b"GET / HTTP/1.1".to_vec(), 100);
        let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
        conn.state.idle();

        match conn.poll() {
            Err(ref err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {},
            other => panic!("unexpected frame: {:?}", other)
        }
    }

    #[test]
    fn test_conn_init_read_eof_busy() {
        let _: Result<(), ()> = future::lazy(|| {
            // server ignores
            let io = AsyncIo::new_eof();
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.busy();

            match conn.poll().unwrap() {
                Async::Ready(None) => {},
                other => panic!("unexpected frame: {:?}", other)
            }

            // client
            let io = AsyncIo::new_eof();
            let mut conn = Conn::<_, proto::Bytes, ClientTransaction>::new(io);
            conn.state.busy();

            match conn.poll() {
                Err(ref err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {},
                other => panic!("unexpected frame: {:?}", other)
            }
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_body_finish_read_eof() {
        let _: Result<(), ()> = future::lazy(|| {
            let io = AsyncIo::new_eof();
            let mut conn = Conn::<_, proto::Bytes, ClientTransaction>::new(io);
            conn.state.busy();
            conn.state.writing = Writing::KeepAlive;
            conn.state.reading = Reading::Body(Decoder::length(0));

            match conn.poll() {
                Ok(Async::Ready(Some(Frame::Body { chunk: None }))) => (),
                other => panic!("unexpected frame: {:?}", other)
            }

            // conn eofs, but tokio-proto will call poll() again, before calling flush()
            // the conn eof in this case is perfectly fine

            match conn.poll() {
                Ok(Async::Ready(None)) => (),
                other => panic!("unexpected frame: {:?}", other)
            }
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_message_empty_body_read_eof() {
        let _: Result<(), ()> = future::lazy(|| {
            let io = AsyncIo::new_buf(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(), 1024);
            let mut conn = Conn::<_, proto::Bytes, ClientTransaction>::new(io);
            conn.state.busy();
            conn.state.writing = Writing::KeepAlive;

            match conn.poll() {
                Ok(Async::Ready(Some(Frame::Message { body: false, .. }))) => (),
                other => panic!("unexpected frame: {:?}", other)
            }

            // conn eofs, but tokio-proto will call poll() again, before calling flush()
            // the conn eof in this case is perfectly fine

            match conn.poll() {
                Ok(Async::Ready(None)) => (),
                other => panic!("unexpected frame: {:?}", other)
            }
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_read_body_end() {
        let _: Result<(), ()> = future::lazy(|| {
            let io = AsyncIo::new_buf(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n12345".to_vec(), 1024);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.busy();

            match conn.poll() {
                Ok(Async::Ready(Some(Frame::Message { body: true, .. }))) => (),
                other => panic!("unexpected frame: {:?}", other)
            }

            match conn.poll() {
                Ok(Async::Ready(Some(Frame::Body { chunk: Some(_) }))) => (),
                other => panic!("unexpected frame: {:?}", other)
            }

            // When the body is done, `poll` MUST return a `Body` frame with chunk set to `None`
            match conn.poll() {
                Ok(Async::Ready(Some(Frame::Body { chunk: None }))) => (),
                other => panic!("unexpected frame: {:?}", other)
            }

            match conn.poll() {
                Ok(Async::NotReady) => (),
                other => panic!("unexpected frame: {:?}", other)
            }
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_closed_read() {
        let io = AsyncIo::new_buf(vec![], 0);
        let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
        conn.state.close();

        match conn.poll().unwrap() {
            Async::Ready(None) => {},
            other => panic!("frame is not None: {:?}", other)
        }
    }

    #[test]
    fn test_conn_body_write_length() {
        let _ = pretty_env_logger::try_init();
        let _: Result<(), ()> = future::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 0);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            let max = super::super::io::DEFAULT_MAX_BUFFER_SIZE + 4096;
            conn.state.writing = Writing::Body(Encoder::length((max * 2) as u64));

            assert!(conn.start_send(Frame::Body { chunk: Some(vec![b'a'; max].into()) }).unwrap().is_ready());
            assert!(!conn.can_buffer_body());

            assert!(conn.start_send(Frame::Body { chunk: Some(vec![b'b'; 1024 * 8].into()) }).unwrap().is_not_ready());

            conn.io.io_mut().block_in(1024 * 3);
            assert!(conn.poll_complete().unwrap().is_not_ready());
            conn.io.io_mut().block_in(1024 * 3);
            assert!(conn.poll_complete().unwrap().is_not_ready());
            conn.io.io_mut().block_in(max * 2);
            assert!(conn.poll_complete().unwrap().is_ready());

            assert!(conn.start_send(Frame::Body { chunk: Some(vec![b'c'; 1024 * 8].into()) }).unwrap().is_ready());
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_body_write_chunked() {
        let _: Result<(), ()> = future::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 4096);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.writing = Writing::Body(Encoder::chunked());

            assert!(conn.start_send(Frame::Body { chunk: Some("headers".into()) }).unwrap().is_ready());
            assert!(conn.start_send(Frame::Body { chunk: Some(vec![b'x'; 8192].into()) }).unwrap().is_ready());
            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_body_flush() {
        let _: Result<(), ()> = future::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 1024 * 1024 * 5);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.writing = Writing::Body(Encoder::length(1024 * 1024));
            assert!(conn.start_send(Frame::Body { chunk: Some(vec![b'a'; 1024 * 1024].into()) }).unwrap().is_ready());
            assert!(!conn.can_buffer_body());
            conn.io.io_mut().block_in(1024 * 1024 * 5);
            assert!(conn.poll_complete().unwrap().is_ready());
            assert!(conn.can_buffer_body());
            assert!(conn.io.io_mut().flushed());

            Ok(())
        }).wait();
    }

    #[test]
    fn test_conn_parking() {
        use std::sync::Arc;
        use futures::executor::Notify;
        use futures::executor::NotifyHandle;

        struct Car {
            permit: bool,
        }
        impl Notify for Car {
            fn notify(&self, _id: usize) {
                assert!(self.permit, "unparked without permit");
            }
        }

        fn car(permit: bool) -> NotifyHandle {
            Arc::new(Car {
                permit: permit,
            }).into()
        }

        // test that once writing is done, unparks
        let f = future::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 4096);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.reading = Reading::KeepAlive;
            assert!(conn.poll().unwrap().is_not_ready());

            conn.state.writing = Writing::KeepAlive;
            assert!(conn.poll_complete().unwrap().is_ready());
            Ok::<(), ()>(())
        });
        ::futures::executor::spawn(f).poll_future_notify(&car(true), 0).unwrap();


        // test that flushing when not waiting on read doesn't unpark
        let f = future::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 4096);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.writing = Writing::KeepAlive;
            assert!(conn.poll_complete().unwrap().is_ready());
            Ok::<(), ()>(())
        });
        ::futures::executor::spawn(f).poll_future_notify(&car(false), 0).unwrap();


        // test that flushing and writing isn't done doesn't unpark
        let f = future::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 4096);
            let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
            conn.state.reading = Reading::KeepAlive;
            assert!(conn.poll().unwrap().is_not_ready());
            conn.state.writing = Writing::Body(Encoder::length(5_000));
            assert!(conn.poll_complete().unwrap().is_ready());
            Ok::<(), ()>(())
        });
        ::futures::executor::spawn(f).poll_future_notify(&car(false), 0).unwrap();
    }

    #[test]
    fn test_conn_closed_write() {
        let io = AsyncIo::new_buf(vec![], 0);
        let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
        conn.state.close();

        match conn.start_send(Frame::Body { chunk: Some(b"foobar".to_vec().into()) }) {
            Err(_e) => {},
            other => panic!("did not return Err: {:?}", other)
        }

        assert!(conn.state.is_write_closed());
    }

    #[test]
    fn test_conn_write_empty_chunk() {
        let io = AsyncIo::new_buf(vec![], 0);
        let mut conn = Conn::<_, proto::Bytes, ServerTransaction>::new(io);
        conn.state.writing = Writing::KeepAlive;

        assert!(conn.start_send(Frame::Body { chunk: None }).unwrap().is_ready());
        assert!(conn.start_send(Frame::Body { chunk: Some(Vec::new().into()) }).unwrap().is_ready());
        conn.start_send(Frame::Body { chunk: Some(vec![b'a'].into()) }).unwrap_err();
    }
    */
}
