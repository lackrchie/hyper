// ============================================================================
// HTTP/2 服务器连接模块
// ============================================================================
// 本模块提供 HTTP/2 服务器端连接的公开 API，包括：
//   - Connection：代表一个 HTTP/2 连接的 Future
//   - Builder：HTTP/2 连接的配置构建器
//
// HTTP/2 相比 HTTP/1 的主要优势：
//   - 多路复用：一个连接上并行处理多个请求
//   - 头部压缩（HPACK）
//   - 服务器推送
//   - 流量控制
//   - 二进制帧格式（更高效的解析）
//
// 使用流程：
//   1. 创建 Builder 并传入执行器
//   2. 配置 HTTP/2 相关选项
//   3. 调用 serve_connection 绑定 IO 和 Service
//   4. 轮询（.await）返回的 Connection Future
// ============================================================================

//! HTTP/2 服务器连接

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::rt::{Read, Write};
use futures_core::ready;
use pin_project_lite::pin_project;

use crate::body::{Body, Incoming as IncomingBody};
use crate::proto;
use crate::rt::bounds::Http2ServerConnExec;
use crate::service::HttpService;
use crate::{common::time::Time, rt::Timer};

pin_project! {
    /// 代表 HTTP/2 连接的 [`Future`](core::future::Future)。
    ///
    /// 由 [`Builder::serve_connection`](struct.Builder.html#method.serve_connection) 返回。
    /// 该连接绑定了一个 [`Service`](crate::service::Service) 来处理请求。
    ///
    /// 要驱动此连接上的 HTTP 通信，此 Future **必须被轮询**，
    /// 通常使用 `.await`。如果不轮询，此连接上不会有任何进展。
    ///
    /// 与 HTTP/1 不同，HTTP/2 连接需要一个额外的执行器类型参数 E，
    /// 用于 spawn 并发处理多个流（请求）的子任务。
    #[must_use = "futures do nothing unless polled"]
    pub struct Connection<T, S, E>
    where
        S: HttpService<IncomingBody>,
    {
        // 内部的 HTTP/2 服务器协议处理器
        conn: proto::h2::Server<T, S, S::ResBody, E>,
    }
}

/// HTTP/2 服务器连接的配置构建器。
///
/// **注意**：选项的默认值*不被视为稳定的*，可能随时更改。
///
/// 与 HTTP/1 Builder 的主要区别：
/// - 需要一个执行器（exec）来 spawn 子任务
/// - 提供 HTTP/2 特有的配置选项（窗口大小、并发流数等）
#[derive(Clone, Debug)]
pub struct Builder<E> {
    /// 执行器，用于 spawn HTTP/2 流的处理任务。
    /// 必须实现 Http2ServerConnExec trait。
    exec: E,
    /// 定时器，用于保活超时等
    timer: Time,
    /// HTTP/2 内部配置
    h2_builder: proto::h2::server::Config,
}

// ===== impl Connection =====
// ===== Connection 连接实现 =====

/// Connection 的 Debug 实现。
impl<I, S, E> fmt::Debug for Connection<I, S, E>
where
    S: HttpService<IncomingBody>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

impl<I, B, S, E> Connection<I, S, E>
where
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    E: Http2ServerConnExec<S::Future, B>,
{
    /// 为此连接启动优雅关闭过程。
    ///
    /// 调用后此 `Connection` 应继续被轮询直到关闭完成。
    /// 在 HTTP/2 中，优雅关闭会发送 GOAWAY 帧，
    /// 告知客户端不再接受新的请求，但现有请求会被处理完毕。
    ///
    /// # 注意
    ///
    /// 仅在 `Connection` Future 仍在 pending 状态时调用此方法。
    /// 如果在 `Connection::poll` 已经解析后调用，此方法不做任何事。
    pub fn graceful_shutdown(mut self: Pin<&mut Self>) {
        self.conn.graceful_shutdown();
    }
}

/// 为 Connection 实现 Future trait。
///
/// 轮询此 Future 以驱动 HTTP/2 连接。
/// 内部委托给 proto::h2::Server 处理所有 HTTP/2 帧的收发。
impl<I, B, S, E> Future for Connection<I, S, E>
where
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    E: Http2ServerConnExec<S::Future, B>,
{
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 轮询内部的 h2 服务器
        match ready!(Pin::new(&mut self.conn).poll(cx)) {
            Ok(_done) => {
                //TODO: proto::h2::Server 不再需要返回 Dispatched 枚举
                // 未来可以简化返回类型
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

// ===== impl Builder =====
// ===== Builder 配置构建器实现 =====

impl<E> Builder<E> {
    /// 创建新的连接构建器。
    ///
    /// 使用默认选项和一个执行器来创建。执行器的类型必须实现
    /// [`Http2ServerConnExec`] trait。
    ///
    /// 执行器用于 spawn 每个 HTTP/2 流的处理任务。
    /// 常用的执行器包括 tokio::spawn 的包装器。
    ///
    /// [`Http2ServerConnExec`]: crate::rt::bounds::Http2ServerConnExec
    pub fn new(exec: E) -> Self {
        Self {
            exec,
            timer: Time::Empty,
            h2_builder: Default::default(),
        }
    }

    /// 配置发送 GOAWAY 之前允许的最大待接受重置流数量。
    ///
    /// 默认值由 [`h2` crate](https://crates.io/crates/h2) 的默认值决定。
    /// 截至 v0.4.0，默认值为 20。
    ///
    /// 参见 <https://github.com/hyperium/hyper/issues/2877> 了解更多信息。
    pub fn max_pending_accept_reset_streams(&mut self, max: impl Into<Option<usize>>) -> &mut Self {
        self.h2_builder.max_pending_accept_reset_streams = max.into();
        self
    }

    /// 配置发送 GOAWAY 之前允许的最大本地错误重置流数量。
    ///
    /// 如果未设置，hyper 将使用默认值，当前为 1024。
    ///
    /// 如果提供 `None`，hyper 将不应用任何限制。
    /// 不建议这样做，因为这可能使服务器暴露于 DOS 攻击。
    ///
    /// 参见 <https://rustsec.org/advisories/RUSTSEC-2024-0003.html> 了解更多信息。
    #[cfg(feature = "http2")]
    #[cfg_attr(docsrs, doc(cfg(feature = "http2")))]
    pub fn max_local_error_reset_streams(&mut self, max: impl Into<Option<usize>>) -> &mut Self {
        self.h2_builder.max_local_error_reset_streams = max.into();
        self
    }

    /// 设置 HTTP/2 流级流量控制的 [`SETTINGS_INITIAL_WINDOW_SIZE`][spec] 选项。
    ///
    /// 窗口大小决定了对端在收到 WINDOW_UPDATE 之前可以发送的数据量。
    /// 较大的窗口可以提高高延迟连接的吞吐量，但会消耗更多内存。
    ///
    /// 传入 `None` 不做任何操作。
    ///
    /// 如果未设置，hyper 将使用默认值（1MB）。
    ///
    /// [spec]: https://httpwg.org/specs/rfc9113.html#SETTINGS_INITIAL_WINDOW_SIZE
    pub fn initial_stream_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            // 显式设置窗口大小会禁用自适应窗口
            self.h2_builder.adaptive_window = false;
            self.h2_builder.initial_stream_window_size = sz;
        }
        self
    }

    /// 设置 HTTP/2 的最大连接级流量控制窗口。
    ///
    /// 连接级窗口控制所有流上的总数据流量。
    ///
    /// 传入 `None` 不做任何操作。
    ///
    /// 如果未设置，hyper 将使用默认值（1MB）。
    pub fn initial_connection_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            // 显式设置窗口大小会禁用自适应窗口
            self.h2_builder.adaptive_window = false;
            self.h2_builder.initial_conn_window_size = sz;
        }
        self
    }

    /// 设置是否使用自适应流量控制。
    ///
    /// 启用此选项将覆盖在 `initial_stream_window_size` 和
    /// `initial_connection_window_size` 中设置的限制。
    ///
    /// 自适应流量控制通过测量带宽延迟积（BDP）来动态调整窗口大小，
    /// 以最大化吞吐量。它通过发送 PING 帧来估算 RTT。
    pub fn adaptive_window(&mut self, enabled: bool) -> &mut Self {
        use proto::h2::SPEC_WINDOW_SIZE;

        self.h2_builder.adaptive_window = enabled;
        if enabled {
            // 启用自适应窗口时，重置为 HTTP/2 规范的默认窗口大小（64KB）
            // BDP 探测会根据网络状况自动调整
            self.h2_builder.initial_conn_window_size = SPEC_WINDOW_SIZE;
            self.h2_builder.initial_stream_window_size = SPEC_WINDOW_SIZE;
        }
        self
    }

    /// 设置 HTTP/2 的最大帧大小。
    ///
    /// 限制单个 HTTP/2 帧的最大载荷。较大的帧可以减少帧头开销，
    /// 但也会增加单次读取的延迟。
    ///
    /// 传入 `None` 不做任何操作。
    ///
    /// 如果未设置，hyper 将使用默认值（16KB）。
    pub fn max_frame_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            self.h2_builder.max_frame_size = sz;
        }
        self
    }

    /// 设置 HTTP/2 连接的 [`SETTINGS_MAX_CONCURRENT_STREAMS`][spec] 选项。
    ///
    /// 限制客户端可以同时打开的最大流（请求）数量。
    /// 这是防止资源耗尽的重要参数。
    ///
    /// 默认为 200，但不是 hyper 稳定性保证的一部分，
    /// 可能在未来版本中更改。建议您设置自己的限制。
    ///
    /// 传入 `None` 将移除所有限制。
    ///
    /// [spec]: https://httpwg.org/specs/rfc9113.html#SETTINGS_MAX_CONCURRENT_STREAMS
    pub fn max_concurrent_streams(&mut self, max: impl Into<Option<u32>>) -> &mut Self {
        self.h2_builder.max_concurrent_streams = max.into();
        self
    }

    /// 设置 HTTP/2 保活 Ping 帧的发送间隔。
    ///
    /// 定期发送 PING 帧以检测连接是否仍然活跃。
    /// 如果对端在超时时间内未响应 PONG，连接将被关闭。
    ///
    /// 传入 `None` 以禁用 HTTP/2 保活。
    ///
    /// 默认为禁用。
    pub fn keep_alive_interval(&mut self, interval: impl Into<Option<Duration>>) -> &mut Self {
        self.h2_builder.keep_alive_interval = interval.into();
        self
    }

    /// 设置保活 ping 的确认超时时间。
    ///
    /// 如果 ping 在超时时间内未被确认（收到 PONG），连接将被关闭。
    /// 如果 `keep_alive_interval` 被禁用，此设置不起作用。
    ///
    /// 默认为 20 秒。
    pub fn keep_alive_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.h2_builder.keep_alive_timeout = timeout;
        self
    }

    /// 设置每个 HTTP/2 流的最大写入缓冲区大小。
    ///
    /// 默认约为 400KB，但可能会更改。
    ///
    /// # Panics
    ///
    /// 值不得大于 `u32::MAX`。
    pub fn max_send_buf_size(&mut self, max: usize) -> &mut Self {
        assert!(max <= u32::MAX as usize);
        self.h2_builder.max_send_buffer_size = max;
        self
    }

    /// 启用[扩展 CONNECT 协议][spec]。
    ///
    /// 允许客户端在 HTTP/2 上使用 CONNECT 方法进行协议升级，
    /// 例如 WebSocket over HTTP/2。
    ///
    /// [spec]: https://datatracker.ietf.org/doc/html/rfc8441#section-4
    pub fn enable_connect_protocol(&mut self) -> &mut Self {
        self.h2_builder.enable_connect_protocol = true;
        self
    }

    /// 设置接收的头部帧的最大大小。
    ///
    /// 限制客户端发送的头部帧大小，防止恶意的大头部攻击。
    ///
    /// 默认为 16KB，但可能会更改。
    pub fn max_header_list_size(&mut self, max: u32) -> &mut Self {
        self.h2_builder.max_header_list_size = max;
        self
    }

    /// 设置后台任务使用的定时器。
    ///
    /// 定时器用于实现保活超时等功能。
    pub fn timer<M>(&mut self, timer: M) -> &mut Self
    where
        M: Timer + Send + Sync + 'static,
    {
        self.timer = Time::Timer(Arc::new(timer));
        self
    }

    /// 设置是否在 HTTP 响应中包含 `date` 头部。
    ///
    /// 注意：根据 RFC 7231 的建议，应在响应中包含 `date` 头部。
    ///
    /// 默认为 true。
    pub fn auto_date_header(&mut self, enabled: bool) -> &mut Self {
        self.h2_builder.date_header = enabled;
        self
    }

    /// 将连接与 [`Service`](crate::service::Service) 绑定在一起。
    ///
    /// 返回一个必须被轮询的 Future，以便在连接上驱动 HTTP 通信。
    ///
    /// 内部会创建 proto::h2::Server 实例，该实例管理 HTTP/2 的：
    /// - 连接握手
    /// - 帧的收发
    /// - 流的多路复用
    /// - 保活和 BDP 探测
    pub fn serve_connection<S, I, Bd>(&self, io: I, service: S) -> Connection<I, S, E>
    where
        S: HttpService<IncomingBody, ResBody = Bd>,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        Bd: Body + 'static,
        Bd::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin,
        E: Http2ServerConnExec<S::Future, Bd>,
    {
        // 创建底层的 HTTP/2 服务器协议处理器
        let proto = proto::h2::Server::new(
            io,
            service,
            &self.h2_builder,
            self.exec.clone(),
            self.timer.clone(),
        );
        Connection { conn: proto }
    }
}
