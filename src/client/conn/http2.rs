//! HTTP/2 客户端连接模块
//!
//! 本模块实现了 HTTP/2 协议的客户端连接 API。与 HTTP/1 模块类似，
//! 它提供了三个主要类型，但在设计上有关键差异以适应 HTTP/2 的多路复用特性：
//!
//! - [`SendRequest`] — 请求发送端，支持 `Clone`，允许多个任务同时发送请求
//! - [`Connection`] — 连接状态机 Future，驱动 HTTP/2 协议处理
//! - [`Builder`] — 连接配置构建器，支持 HTTP/2 特有的配置（如流控、并发流数等）
//!
//! ## 与 HTTP/1 的关键差异
//!
//! - **多路复用**：HTTP/2 支持在单个连接上同时发送多个请求，
//!   因此 `SendRequest` 可以被克隆，底层使用 `UnboundedSender`（无背压限制）
//! - **需要 Executor**：HTTP/2 需要一个执行器来 spawn 内部任务（如处理流），
//!   因此 `Builder` 和 `handshake` 函数都需要一个 `Executor` 参数
//! - **流控制**：HTTP/2 有流级别和连接级别的流量控制，Builder 提供了相应的配置
//! - **无升级**：HTTP/2 不支持 HTTP/1 风格的连接升级（但支持扩展 CONNECT 协议）
//!
//! ## 使用流程
//!
//! 1. 准备一个实现了 `Read + Write` 的 IO 对象
//! 2. 调用 `handshake(exec, io)` 或 `Builder::new(exec).handshake(io)` 执行握手
//! 3. 获得 `(SendRequest, Connection)` 对
//! 4. 将 `Connection` spawn 到异步运行时中
//! 5. 可以克隆 `SendRequest`，从多个任务同时发送请求

// 标准库错误 trait，用于泛型约束中的错误类型边界
use std::error::Error;
// 格式化输出 trait，用于 Debug 实现
use std::fmt;
// Future trait，用于 Connection 的异步实现
use std::future::Future;
// PhantomData：零大小类型标记，用于在类型中"持有"泛型参数 T 而不实际拥有它
use std::marker::PhantomData;
// Pin 类型，用于固定 Future 在内存中的位置
use std::pin::Pin;
// Arc：原子引用计数智能指针，用于跨任务共享 Timer
use std::sync::Arc;
// 异步任务的上下文和轮询结果类型
use std::task::{Context, Poll};
// Duration 类型，用于 keep-alive 超时配置
use std::time::Duration;

// hyper 自定义的异步 Read/Write trait
use crate::rt::{Read, Write};
// futures_core 的 ready! 宏
use futures_core::ready;
// http crate 的 HTTP 请求和响应类型
use http::{Request, Response};

// dispatch 模块和 TrySendError 类型
use super::super::dispatch::{self, TrySendError};
// hyper 的 Body trait 和 Incoming body 类型
use crate::body::{Body, Incoming as IncomingBody};
// hyper 内部的 Time 封装，用于延迟操作（如 keep-alive）
use crate::common::time::Time;
// hyper 的协议实现模块
use crate::proto;
// HTTP/2 客户端连接所需的 Executor trait 约束
use crate::rt::bounds::Http2ClientConnExec;
// hyper 的 Timer trait，用于提供定时器功能
use crate::rt::Timer;

/// HTTP/2 连接的请求发送端。
///
/// 与 HTTP/1 的 `SendRequest` 不同，此类型可以被克隆（`Clone`），
/// 因为 HTTP/2 支持多路复用——多个请求可以同时在同一连接上发送。
///
/// 内部使用 `dispatch::UnboundedSender`，不受背压限制。
pub struct SendRequest<B> {
    dispatch: dispatch::UnboundedSender<Request<B>, Response<IncomingBody>>,
}

/// 为 `SendRequest` 手动实现 `Clone`。
///
/// 不使用 `#[derive(Clone)]` 是因为 derive 会错误地要求 `B: Clone`，
/// 而实际上我们只需要克隆内部的 dispatch sender，不需要 B 可克隆。
impl<B> Clone for SendRequest<B> {
    fn clone(&self) -> SendRequest<B> {
        SendRequest {
            dispatch: self.dispatch.clone(),
        }
    }
}

/// 处理 IO 对象上所有 HTTP/2 状态的 Future。
///
/// 在大多数情况下，这应该被 spawn 到执行器中运行。
/// 此类型的实例通常通过 [`handshake`] 函数创建。
///
/// `#[must_use]` 确保用户不会忘记 poll 这个 Future。
///
/// # 类型参数
/// - `T`: IO 传输层类型
/// - `B`: 请求体类型
/// - `E`: 执行器类型，用于 spawn HTTP/2 内部任务
#[must_use = "futures do nothing unless polled"]
pub struct Connection<T, B, E>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
    E: Http2ClientConnExec<B, T> + Unpin,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    /// 内部使用元组 `(PhantomData<T>, ClientTask)`:
    /// - `PhantomData<T>` 确保 Connection 在类型层面"持有" T，维持正确的泛型关系
    /// - `ClientTask` 是 HTTP/2 客户端的实际工作任务
    inner: (PhantomData<T>, proto::h2::ClientTask<B, E, T>),
}

/// HTTP/2 连接配置构建器。
///
/// 通过设置各种 HTTP/2 选项后，调用 `handshake` 方法创建连接。
/// 与 HTTP/1 的 Builder 不同，此构建器需要一个 Executor 泛型参数，
/// 因为 HTTP/2 需要 spawn 内部任务来处理流。
///
/// **注意**：选项的默认值不被视为稳定 API，可能在任何时候发生变化。
#[derive(Clone, Debug)]
pub struct Builder<Ex> {
    /// 执行器，用于 spawn HTTP/2 内部任务（如处理各个流的响应）。
    /// `pub(super)` 表示对父模块可见。
    pub(super) exec: Ex,
    /// 定时器封装，用于 keep-alive、超时等需要计时的操作。
    /// `Time::Empty` 表示未设置定时器。
    pub(super) timer: Time,
    /// HTTP/2 协议层的配置，包含流控参数、并发流数、keep-alive 设置等
    h2_builder: proto::h2::client::Config,
}

/// 在 IO 对象上执行 HTTP/2 握手的快捷函数。
///
/// 等同于 `Builder::new(exec).handshake(io)`。
/// 有关更多信息，请参阅 [`client::conn`](crate::client::conn)。
///
/// # 类型参数
/// - `E`: 执行器类型
/// - `T`: IO 传输层类型
/// - `B`: 请求体类型
pub async fn handshake<E, T, B>(
    exec: E,
    io: T,
) -> crate::Result<(SendRequest<B>, Connection<T, B, E>)>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    E: Http2ClientConnExec<B, T> + Unpin + Clone,
{
    Builder::new(exec).handshake(io).await
}

// ===== impl SendRequest

/// `SendRequest` 的基础方法实现
impl<B> SendRequest<B> {
    /// 异步轮询发送端是否准备好发送请求。
    ///
    /// 对于 HTTP/2，如果连接已关闭则返回 Error，否则立即返回 Ready(Ok(()))。
    /// 与 HTTP/1 不同，HTTP/2 不需要等待上一个请求完成，
    /// 因为多路复用允许同时发送多个请求。
    ///
    /// `_cx` 参数未使用（以下划线前缀），因为 HTTP/2 的就绪状态不需要异步等待。
    pub fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        if self.is_closed() {
            Poll::Ready(Err(crate::Error::new_closed()))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    /// 等待 dispatcher 就绪。
    ///
    /// 如果关联的连接已关闭，返回 Error。
    pub async fn ready(&mut self) -> crate::Result<()> {
        crate::common::future::poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// 检查连接当前是否准备好发送请求。
    ///
    /// # 注意
    ///
    /// 这主要是一个提示。由于网络固有的延迟，即使此方法返回 true，
    /// 发送请求仍可能失败，因为连接可能在此期间被关闭。
    pub fn is_ready(&self) -> bool {
        self.dispatch.is_ready()
    }

    /// 检查连接端是否已关闭。
    pub fn is_closed(&self) -> bool {
        self.dispatch.is_closed()
    }
}

/// `SendRequest` 的请求发送方法实现（需要 Body 约束）
impl<B> SendRequest<B>
where
    B: Body + 'static,
{
    /// 在关联的连接上发送 `Request`。
    ///
    /// 返回一个 Future，成功时产生 `Response`。
    ///
    /// `req` 必须包含 `Host` header。
    ///
    /// 不要求使用 absolute-form 的 `Uri`。如果收到 absolute-form 的 Uri，
    /// 将按原样序列化。
    pub fn send_request(
        &mut self,
        req: Request<B>,
    ) -> impl Future<Output = crate::Result<Response<IncomingBody>>> {
        // 通过无界 dispatch 通道发送请求（不可重试模式）
        let sent = self.dispatch.send(req);

        async move {
            match sent {
                Ok(rx) => match rx.await {
                    Ok(Ok(resp)) => Ok(resp),
                    Ok(Err(err)) => Err(err),
                    // this is definite bug if it happens, but it shouldn't happen!
                    Err(_canceled) => panic!("dispatch dropped without returning error"),
                },
                Err(_req) => {
                    debug!("connection was not ready");

                    Err(crate::Error::new_canceled().with("connection was not ready"))
                }
            }
        }
    }

    /// 在关联的连接上发送 `Request`（可重试模式）。
    ///
    /// 返回一个 Future，成功时产生 `Response`。
    ///
    /// # 错误
    ///
    /// 如果在尝试将请求序列化到连接之前就发生错误，
    /// 原始请求消息会作为 `TrySendError` 的一部分返回。
    pub fn try_send_request(
        &mut self,
        req: Request<B>,
    ) -> impl Future<Output = Result<Response<IncomingBody>, TrySendError<Request<B>>>> {
        let sent = self.dispatch.try_send(req);
        async move {
            match sent {
                Ok(rx) => match rx.await {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(err)) => Err(err),
                    // this is definite bug if it happens, but it shouldn't happen!
                    Err(_) => panic!("dispatch dropped without returning error"),
                },
                Err(req) => {
                    debug!("connection was not ready");
                    let error = crate::Error::new_canceled().with("connection was not ready");
                    Err(TrySendError {
                        error,
                        message: Some(req),
                    })
                }
            }
        }
    }
}

/// 为 `SendRequest` 实现 `Debug` trait。
impl<B> fmt::Debug for SendRequest<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendRequest").finish()
    }
}

// ===== impl Connection

/// `Connection` 的扩展功能实现
impl<T, B, E> Connection<T, B, E>
where
    T: Read + Write + Unpin + 'static,
    B: Body + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    E: Http2ClientConnExec<B, T> + Unpin,
{
    /// 返回[扩展 CONNECT 协议][1]是否已启用。
    ///
    /// 此设置由服务器端通过在 `SETTINGS` 帧中发送
    /// [`SETTINGS_ENABLE_CONNECT_PROTOCOL` 参数][2]来配置。
    /// 此方法返回当前已确认的从远端接收到的值。
    ///
    /// [1]: https://datatracker.ietf.org/doc/html/rfc8441#section-4
    /// [2]: https://datatracker.ietf.org/doc/html/rfc8441#section-3
    pub fn is_extended_connect_protocol_enabled(&self) -> bool {
        // 通过元组的第二个元素（ClientTask）访问 HTTP/2 连接状态
        self.inner.1.is_extended_connect_protocol_enabled()
    }
}

/// 为 `Connection` 实现 `Debug` trait。
impl<T, B, E> fmt::Debug for Connection<T, B, E>
where
    T: Read + Write + fmt::Debug + 'static + Unpin,
    B: Body + 'static,
    E: Http2ClientConnExec<B, T> + Unpin,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

/// 为 `Connection` 实现 `Future` trait。
///
/// 当 Connection 被轮询时，驱动内部的 HTTP/2 ClientTask 处理协议状态。
/// HTTP/2 连接不支持 HTTP/1 风格的升级，因此 Upgrade 分支是不可达的。
impl<T, B, E> Future for Connection<T, B, E>
where
    T: Read + Write + Unpin + 'static,
    B: Body + 'static + Unpin,
    B::Data: Send,
    E: Unpin,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    E: Http2ClientConnExec<B, T> + Unpin,
{
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 轮询元组的第二个元素（ClientTask）
        match ready!(Pin::new(&mut self.inner.1).poll(cx))? {
            // 正常关闭
            proto::Dispatched::Shutdown => Poll::Ready(Ok(())),
            // HTTP/2 不支持升级，此分支不应被到达
            // 使用条件编译：仅在同时启用 http1 feature 时才有 Upgrade 变体
            #[cfg(feature = "http1")]
            proto::Dispatched::Upgrade(_pending) => unreachable!("http2 cannot upgrade"),
        }
    }
}

// ===== impl Builder

/// `Builder` 的方法实现
impl<Ex> Builder<Ex>
where
    Ex: Clone,
{
    /// 创建新的 HTTP/2 连接配置构建器。
    ///
    /// 需要提供一个执行器 `exec`，用于 spawn HTTP/2 内部任务。
    /// 执行器需要实现 `Clone`，因为可能在多个地方使用。
    #[inline]
    pub fn new(exec: Ex) -> Builder<Ex> {
        Builder {
            exec,
            timer: Time::Empty, // 默认不设置定时器
            h2_builder: Default::default(), // 使用 h2 crate 的默认配置
        }
    }

    /// 提供一个定时器来执行后台 HTTP/2 任务。
    ///
    /// 定时器用于 keep-alive ping、超时等操作。
    /// 使用 `Arc` 包装以支持跨任务共享。
    pub fn timer<M>(&mut self, timer: M) -> &mut Builder<Ex>
    where
        M: Timer + Send + Sync + 'static,
    {
        self.timer = Time::Timer(Arc::new(timer));
        self
    }

    /// 设置 [`SETTINGS_INITIAL_WINDOW_SIZE`][spec] 选项，用于 HTTP/2 流级别的流量控制。
    ///
    /// 传入 `None` 不做任何改变。
    /// 如果未设置，hyper 使用默认值。
    /// 设置此值会禁用自适应窗口（adaptive_window）。
    ///
    /// [spec]: https://httpwg.org/specs/rfc9113.html#SETTINGS_INITIAL_WINDOW_SIZE
    pub fn initial_stream_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            // 设置具体值时禁用自适应窗口
            self.h2_builder.adaptive_window = false;
            self.h2_builder.initial_stream_window_size = sz;
        }
        self
    }

    /// 设置 HTTP/2 连接级别的最大流量控制窗口。
    ///
    /// 传入 `None` 不做任何改变。
    /// 如果未设置，hyper 使用默认值。
    /// 设置此值会禁用自适应窗口。
    pub fn initial_connection_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            self.h2_builder.adaptive_window = false;
            self.h2_builder.initial_conn_window_size = sz;
        }
        self
    }

    /// 设置本地发起（发送）流的初始最大数量。
    ///
    /// 此值会被对端在[连接前奏][connection preface]中发送的初始 SETTINGS 帧中的值覆盖。
    ///
    /// 传入 `None` 不做任何改变。
    /// 如果未设置，hyper 使用默认值。
    ///
    /// [connection preface]: https://httpwg.org/specs/rfc9113.html#preface
    pub fn initial_max_send_streams(&mut self, initial: impl Into<Option<usize>>) -> &mut Self {
        if let Some(initial) = initial.into() {
            self.h2_builder.initial_max_send_streams = initial;
        }
        self
    }

    /// 设置是否使用自适应流量控制。
    ///
    /// 启用此选项会覆盖 `initial_stream_window_size` 和
    /// `initial_connection_window_size` 中设置的值，
    /// 将它们重置为 HTTP/2 规范默认值（SPEC_WINDOW_SIZE）。
    pub fn adaptive_window(&mut self, enabled: bool) -> &mut Self {
        use proto::h2::SPEC_WINDOW_SIZE;

        self.h2_builder.adaptive_window = enabled;
        if enabled {
            // 启用自适应窗口时，重置为规范默认值
            self.h2_builder.initial_conn_window_size = SPEC_WINDOW_SIZE;
            self.h2_builder.initial_stream_window_size = SPEC_WINDOW_SIZE;
        }
        self
    }

    /// 设置 HTTP/2 使用的最大帧大小。
    ///
    /// 默认当前为 16KB，但可能会变化。
    pub fn max_frame_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        self.h2_builder.max_frame_size = sz.into();
        self
    }

    /// 设置接收的 header 帧的最大大小。
    ///
    /// 默认当前为 16KB，但可能会变化。
    pub fn max_header_list_size(&mut self, max: u32) -> &mut Self {
        self.h2_builder.max_header_list_size = max;
        self
    }

    /// 设置 HPACK header 压缩表大小。
    ///
    /// 此设置通知对端用于编码 header 块的压缩表的最大大小（以字节为单位）。
    /// 编码器可以选择使用等于或小于发送方指定值的任何值。
    ///
    /// `h2` crate 的默认值为 4,096。
    pub fn header_table_size(&mut self, size: impl Into<Option<u32>>) -> &mut Self {
        self.h2_builder.header_table_size = size.into();
        self
    }

    /// 设置最大并发流数。
    ///
    /// 此设置仅控制远端对端可以发起的最大流数量。换言之，
    /// 当设置为 100 时，并不限制调用者可以创建的并发流数量。
    ///
    /// 建议此值不小于 100，以免不必要地限制并行度。
    /// 不过任何值都是合法的，包括 0（禁止远端发起流）。
    ///
    /// 注意处于保留状态（即已保留但未启动的推送承诺）的流不计入此限制。
    ///
    /// 如果远端超过此限制，不会产生协议级错误，h2 库会立即重置该流。
    ///
    /// 参见 HTTP/2 规范 [Section 5.1.2]。
    ///
    /// [Section 5.1.2]: https://http2.github.io/http2-spec/#rfc.section.5.1.2
    pub fn max_concurrent_streams(&mut self, max: impl Into<Option<u32>>) -> &mut Self {
        self.h2_builder.max_concurrent_streams = max.into();
        self
    }

    /// 设置 HTTP/2 Keep-Alive Ping 帧的发送间隔。
    ///
    /// 传入 `None` 禁用 keep-alive。
    /// 默认当前为禁用。
    pub fn keep_alive_interval(&mut self, interval: impl Into<Option<Duration>>) -> &mut Self {
        self.h2_builder.keep_alive_interval = interval.into();
        self
    }

    /// 设置等待 keep-alive ping 确认的超时时间。
    ///
    /// 如果在超时时间内未收到确认，连接将被关闭。
    /// 在 `keep_alive_interval` 被禁用时此设置无效。
    ///
    /// 默认为 20 秒。
    pub fn keep_alive_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.h2_builder.keep_alive_timeout = timeout;
        self
    }

    /// 设置 HTTP/2 keep-alive 是否在连接空闲时也生效。
    ///
    /// 禁用时，keep-alive ping 仅在有活跃的请求/响应流时发送。
    /// 启用时，即使没有活跃流也会发送 ping。
    /// 在 `keep_alive_interval` 被禁用时此设置无效。
    ///
    /// 默认为 `false`。
    pub fn keep_alive_while_idle(&mut self, enabled: bool) -> &mut Self {
        self.h2_builder.keep_alive_while_idle = enabled;
        self
    }

    /// 设置 HTTP/2 本地重置流的最大并发数。
    ///
    /// 参见 [`h2::client::Builder::max_concurrent_reset_streams`] 文档获取更多信息。
    ///
    /// 默认值由 `h2` crate 决定。
    ///
    /// [`h2::client::Builder::max_concurrent_reset_streams`]: https://docs.rs/h2/client/struct.Builder.html#method.max_concurrent_reset_streams
    pub fn max_concurrent_reset_streams(&mut self, max: usize) -> &mut Self {
        self.h2_builder.max_concurrent_reset_streams = Some(max);
        self
    }

    /// 设置每个 HTTP/2 流的最大写缓冲区大小。
    ///
    /// 默认当前为 1MB，但可能会变化。
    ///
    /// # Panics
    ///
    /// 值不能大于 `u32::MAX`。
    pub fn max_send_buf_size(&mut self, max: usize) -> &mut Self {
        assert!(max <= u32::MAX as usize);
        self.h2_builder.max_send_buffer_size = max;
        self
    }

    /// 配置在发送 GOAWAY 之前允许的最大待接受重置流数量。
    ///
    /// 默认使用 [`h2` crate](https://crates.io/crates/h2) 的默认值。
    /// 截至 v0.4.0，该值为 20。
    ///
    /// 参见 <https://github.com/hyperium/hyper/issues/2877> 获取更多信息。
    pub fn max_pending_accept_reset_streams(&mut self, max: impl Into<Option<usize>>) -> &mut Self {
        self.h2_builder.max_pending_accept_reset_streams = max.into();
        self
    }

    /// 使用已配置的选项在 IO 对象上构建 HTTP/2 连接。
    ///
    /// 有关更多信息，请参阅 [`client::conn`](crate::client::conn)。
    ///
    /// 注意，如果 [`Connection`] 未被 `await`，[`SendRequest`] 将不会工作。
    ///
    /// # 返回值
    /// 返回一个 Future，成功时产生 `(SendRequest<B>, Connection<T, B, Ex>)` 对。
    pub fn handshake<T, B>(
        &self,
        io: T,
    ) -> impl Future<Output = crate::Result<(SendRequest<B>, Connection<T, B, Ex>)>>
    where
        T: Read + Write + Unpin,
        B: Body + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        Ex: Http2ClientConnExec<B, T> + Unpin,
    {
        // 克隆配置，以便在 async 块中使用
        let opts = self.clone();

        async move {
            trace!("client handshake HTTP/2");

            // 创建 dispatch 通道
            let (tx, rx) = dispatch::channel();
            // 执行 HTTP/2 协议握手，建立 h2 连接
            // 传入 IO 对象、接收端、h2 配置、执行器和定时器
            let h2 = proto::h2::client::handshake(io, rx, &opts.h2_builder, opts.exec, opts.timer)
                .await?;
            Ok((
                SendRequest {
                    // 将有界 Sender 转为无界 UnboundedSender（HTTP/2 多路复用）
                    dispatch: tx.unbound(),
                },
                Connection {
                    // PhantomData 标记 T 类型，ClientTask 包含实际的 h2 协议处理逻辑
                    inner: (PhantomData, h2),
                },
            ))
        }
    }
}

// ==================== 以下是 HTTP/2 模块的编译测试 ====================

#[cfg(test)]
mod tests {

    /// 编译测试：Send + Sync 执行器搭配非 Send 的 Future。
    ///
    /// 验证 `LocalTokioExecutor`（不要求 Future 实现 Send）可以与
    /// HTTP/2 握手一起使用，并通过 `spawn_local` 在本地任务集中运行。
    /// `#[ignore]` 表示此测试仅检查编译，不实际运行。
    #[tokio::test]
    #[ignore] // only compilation is checked
    async fn send_sync_executor_of_non_send_futures() {
        #[derive(Clone)]
        struct LocalTokioExecutor;

        impl<F> crate::rt::Executor<F> for LocalTokioExecutor
        where
            F: std::future::Future + 'static, // not requiring `Send`
        {
            fn execute(&self, fut: F) {
                // This will spawn into the currently running `LocalSet`.
                tokio::task::spawn_local(fut);
            }
        }

        #[allow(unused)]
        async fn run(io: impl crate::rt::Read + crate::rt::Write + Unpin + 'static) {
            let (_sender, conn) = crate::client::conn::http2::handshake::<
                _,
                _,
                http_body_util::Empty<bytes::Bytes>,
            >(LocalTokioExecutor, io)
            .await
            .unwrap();

            tokio::task::spawn_local(async move {
                conn.await.unwrap();
            });
        }
    }

    /// 编译测试：!Send + !Sync 执行器搭配非 Send 的 Future。
    ///
    /// 使用 `PhantomData<Rc<()>>` 使执行器既不是 Send 也不是 Sync（Rc 不是 Send/Sync）。
    /// 验证这种情况下 HTTP/2 握手仍能编译通过。
    #[tokio::test]
    #[ignore] // only compilation is checked
    async fn not_send_not_sync_executor_of_not_send_futures() {
        #[derive(Clone)]
        struct LocalTokioExecutor {
            // PhantomData<Rc<()>> 使得此类型既不是 Send 也不是 Sync
            _x: std::marker::PhantomData<std::rc::Rc<()>>,
        }

        impl<F> crate::rt::Executor<F> for LocalTokioExecutor
        where
            F: std::future::Future + 'static, // not requiring `Send`
        {
            fn execute(&self, fut: F) {
                // This will spawn into the currently running `LocalSet`.
                tokio::task::spawn_local(fut);
            }
        }

        #[allow(unused)]
        async fn run(io: impl crate::rt::Read + crate::rt::Write + Unpin + 'static) {
            let (_sender, conn) =
                crate::client::conn::http2::handshake::<_, _, http_body_util::Empty<bytes::Bytes>>(
                    LocalTokioExecutor {
                        _x: Default::default(),
                    },
                    io,
                )
                .await
                .unwrap();

            tokio::task::spawn_local(async move {
                conn.await.unwrap();
            });
        }
    }

    /// 编译测试：Send + !Sync 执行器搭配非 Send 的 Future。
    ///
    /// 使用 `PhantomData<Cell<()>>` 使执行器是 Send 但不是 Sync（Cell 不是 Sync）。
    /// 验证这种情况下 HTTP/2 握手仍能编译通过。
    #[tokio::test]
    #[ignore] // only compilation is checked
    async fn send_not_sync_executor_of_not_send_futures() {
        #[derive(Clone)]
        struct LocalTokioExecutor {
            // PhantomData<Cell<()>> 使得此类型是 Send 但不是 Sync
            _x: std::marker::PhantomData<std::cell::Cell<()>>,
        }

        impl<F> crate::rt::Executor<F> for LocalTokioExecutor
        where
            F: std::future::Future + 'static, // not requiring `Send`
        {
            fn execute(&self, fut: F) {
                // This will spawn into the currently running `LocalSet`.
                tokio::task::spawn_local(fut);
            }
        }

        #[allow(unused)]
        async fn run(io: impl crate::rt::Read + crate::rt::Write + Unpin + 'static) {
            let (_sender, conn) =
                crate::client::conn::http2::handshake::<_, _, http_body_util::Empty<bytes::Bytes>>(
                    LocalTokioExecutor {
                        _x: Default::default(),
                    },
                    io,
                )
                .await
                .unwrap();

            tokio::task::spawn_local(async move {
                conn.await.unwrap();
            });
        }
    }

    /// 编译测试：Send + Sync 执行器搭配 Send 的 Future。
    ///
    /// 标准场景：使用 `tokio::task::spawn` 来执行 Send 的 Future。
    /// 验证这种最常见的配置能编译通过。
    #[tokio::test]
    #[ignore] // only compilation is checked
    async fn send_sync_executor_of_send_futures() {
        #[derive(Clone)]
        struct TokioExecutor;

        impl<F> crate::rt::Executor<F> for TokioExecutor
        where
            F: std::future::Future + 'static + Send,
            F::Output: Send + 'static,
        {
            fn execute(&self, fut: F) {
                tokio::task::spawn(fut);
            }
        }

        #[allow(unused)]
        async fn run(io: impl crate::rt::Read + crate::rt::Write + Send + Unpin + 'static) {
            let (_sender, conn) = crate::client::conn::http2::handshake::<
                _,
                _,
                http_body_util::Empty<bytes::Bytes>,
            >(TokioExecutor, io)
            .await
            .unwrap();

            tokio::task::spawn(async move {
                conn.await.unwrap();
            });
        }
    }

    /// 编译测试：Send + !Sync 执行器搭配 Send 的 Future。
    ///
    /// 使用 `PhantomData<Cell<()>>` 使执行器是 Send 但不是 Sync。
    /// 注意最后使用 `spawn_local` 而非 `spawn`，因为当执行器不是 Sync 时，
    /// Connection 本身可能也不满足 Send 约束（取决于具体实现）。
    #[tokio::test]
    #[ignore] // only compilation is checked
    async fn send_not_sync_executor_of_send_futures() {
        #[derive(Clone)]
        struct TokioExecutor {
            // !Sync 标记
            _x: std::marker::PhantomData<std::cell::Cell<()>>,
        }

        impl<F> crate::rt::Executor<F> for TokioExecutor
        where
            F: std::future::Future + 'static + Send,
            F::Output: Send + 'static,
        {
            fn execute(&self, fut: F) {
                tokio::task::spawn(fut);
            }
        }

        #[allow(unused)]
        async fn run(io: impl crate::rt::Read + crate::rt::Write + Send + Unpin + 'static) {
            let (_sender, conn) =
                crate::client::conn::http2::handshake::<_, _, http_body_util::Empty<bytes::Bytes>>(
                    TokioExecutor {
                        _x: Default::default(),
                    },
                    io,
                )
                .await
                .unwrap();

            tokio::task::spawn_local(async move {
                // can't use spawn here because when executor is !Send
                conn.await.unwrap();
            });
        }
    }
}
