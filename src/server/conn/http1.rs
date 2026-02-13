// ============================================================================
// HTTP/1 服务器连接模块
// ============================================================================
// 本模块提供 HTTP/1.1 服务器端连接的完整实现，包括：
//   - Connection：代表一个 HTTP/1 连接的 Future
//   - Builder：HTTP/1 连接的配置构建器
//   - UpgradeableConnection：支持 HTTP 升级的连接
//   - Parts：连接的分解组件
//
// 使用流程：
//   1. 创建 Builder 并配置选项
//   2. 调用 serve_connection 绑定 IO 和 Service
//   3. 轮询（.await）返回的 Connection Future
// ============================================================================

//! HTTP/1 服务器连接

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::rt::{Read, Write};
use crate::upgrade::Upgraded;
use bytes::Bytes;
use futures_core::ready;

use crate::body::{Body, Incoming as IncomingBody};
use crate::proto;
use crate::service::HttpService;
use crate::{
    common::time::{Dur, Time},
    rt::Timer,
};

/// HTTP/1 调度器的类型别名。
///
/// 组合了协议层（h1::Dispatcher）和调度层（dispatch::Server），
/// 完整地处理 HTTP/1.1 请求的解析、分发和响应发送。
///
/// 类型参数：
/// - T: 底层 IO 类型
/// - B: 响应体类型
/// - S: HTTP 服务类型
type Http1Dispatcher<T, B, S> = proto::h1::Dispatcher<
    proto::h1::dispatch::Server<S, IncomingBody>,
    B,
    T,
    proto::ServerTransaction,
>;

pin_project_lite::pin_project! {
    /// 代表 HTTP/1 连接的 [`Future`](core::future::Future)。
    ///
    /// 由 [`Builder::serve_connection`](struct.Builder.html#method.serve_connection) 返回。
    /// 该连接绑定了一个 [`Service`](crate::service::Service) 来处理请求。
    ///
    /// 要驱动此连接上的 HTTP 通信，此 Future **必须被轮询**，
    /// 通常使用 `.await`。如果不轮询，此连接上不会有任何进展。
    #[must_use = "futures do nothing unless polled"]
    pub struct Connection<T, S>
    where
        S: HttpService<IncomingBody>,
    {
        // 内部的 HTTP/1 调度器，负责请求的解析、分发和响应发送
        conn: Http1Dispatcher<T, S::ResBody, S>,
    }
}

/// HTTP/1 服务器连接的配置构建器。
///
/// **注意**：选项的默认值*不被视为稳定的*，可能随时更改。
///
/// # 示例
///
/// ```
/// # use std::time::Duration;
/// # use hyper::server::conn::http1::Builder;
/// # fn main() {
/// let mut http = Builder::new();
/// // 逐个设置选项
/// http.half_close(false);
///
/// // 或者链式设置多个选项
/// http.keep_alive(false).title_case_headers(true).max_buf_size(8192);
///
/// # }
/// ```
///
/// 使用 [`Builder::serve_connection`](struct.Builder.html#method.serve_connection)
/// 将构建的连接绑定到一个服务。
#[derive(Clone, Debug)]
pub struct Builder {
    /// HTTP 解析器配置（来自 httparse crate）
    h1_parser_config: httparse::ParserConfig,
    /// 定时器，用于头部读取超时等
    timer: Time,
    /// 是否支持半关闭（half-close）。
    /// 允许客户端关闭写端而保持读端。
    h1_half_close: bool,
    /// 是否启用 HTTP 长连接（keep-alive）
    h1_keep_alive: bool,
    /// 是否将响应头部名称转换为首字母大写格式
    /// 例如 "content-type" -> "Content-Type"
    h1_title_case_headers: bool,
    /// 是否保留原始头部名称的大小写
    h1_preserve_header_case: bool,
    /// 最大头部数量限制
    h1_max_headers: Option<usize>,
    /// 头部读取超时时间
    h1_header_read_timeout: Dur,
    /// 是否使用向量化写入（writev）
    /// None 表示自动检测
    h1_writev: Option<bool>,
    /// 最大缓冲区大小
    max_buf_size: Option<usize>,
    /// 是否启用管道化（pipeline）刷新优化
    pipeline_flush: bool,
    /// 是否自动添加 Date 头部
    date_header: bool,
}

/// `Connection` 的分解组件。
///
/// 允许在稍后的时间点拆解 `Connection`，以便回收 IO 对象和其他相关部件。
/// 主要用于 HTTP 升级场景。
#[derive(Debug)]
#[non_exhaustive]
pub struct Parts<T, S> {
    /// 握手中使用的原始 IO 对象。
    pub io: T,
    /// 已读取但尚未作为 HTTP 处理的字节缓冲区。
    ///
    /// 如果客户端在最后一个请求之后发送了额外的字节，
    /// 并且此连接以升级"结束"，读取缓冲区将包含这些字节。
    ///
    /// 如果你计划继续在 IO 对象上通信，应检查是否有现有字节。
    pub read_buf: Bytes,
    /// 用于服务此连接的 `Service` 实例。
    pub service: S,
}

// ===== impl Connection =====
// ===== Connection 连接实现 =====

/// Connection 的 Debug 实现。
impl<I, S> fmt::Debug for Connection<I, S>
where
    S: HttpService<IncomingBody>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

impl<I, B, S> Connection<I, S>
where
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    /// 为此连接启动优雅关闭过程。
    ///
    /// 调用后此 `Connection` 应继续被轮询直到关闭完成。
    /// 在 HTTP/1 中，优雅关闭意味着禁用 keep-alive，
    /// 即当前请求处理完成后不再接受新请求。
    ///
    /// # 注意
    ///
    /// 仅在 `Connection` Future 仍在 pending 状态时调用此方法。
    /// 如果在 `Connection::poll` 已经解析后调用，此方法不做任何事。
    pub fn graceful_shutdown(mut self: Pin<&mut Self>) {
        // 禁用 keep-alive，当前请求完成后连接将关闭
        self.conn.disable_keep_alive();
    }

    /// 返回内部 IO 对象和附加信息。
    ///
    /// 如果 IO 对象已被"倒回"（rewind），io 将不包含那些倒回的字节。
    /// 仅在 `poll_without_shutdown` 信号表示连接"完成"后调用此方法。
    /// 否则，它可能尚未完成刷新所有必要的 HTTP 字节。
    ///
    /// # Panics
    /// 如果此连接使用 h2 协议，此方法将 panic。
    pub fn into_parts(self) -> Parts<I, S> {
        let (io, read_buf, dispatch) = self.conn.into_inner();
        Parts {
            io,
            read_buf,
            service: dispatch.into_service(),
        }
    }

    /// 轮询连接直到完成，但不在底层 IO 上调用 `shutdown`。
    ///
    /// 这在进行 HTTP 升级时非常有用。一旦升级完成，连接会"完成"，
    /// 但并不希望实际关闭 IO 对象。相反，你可以使用 `into_parts` 取回它。
    pub fn poll_without_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>>
    where
        S: Unpin,
        S::Future: Unpin,
    {
        self.conn.poll_without_shutdown(cx)
    }

    /// 阻止在请求服务结束时关闭底层 IO 对象，改为执行 `into_parts`。
    /// 这是 `poll_without_shutdown` 的便捷包装。
    ///
    /// # 错误
    ///
    /// 如果底层连接协议不是 HTTP/1，则返回错误。
    pub fn without_shutdown(self) -> impl Future<Output = crate::Result<Parts<I, S>>> {
        let mut zelf = Some(self);
        // 使用 poll_fn 创建一个 Future，等待连接完成后拆解它
        crate::common::future::poll_fn(move |cx| {
            ready!(zelf.as_mut().unwrap().conn.poll_without_shutdown(cx))?;
            Poll::Ready(Ok(zelf.take().unwrap().into_parts()))
        })
    }

    /// 使此连接支持更高层的 HTTP 升级。
    ///
    /// 参见 [upgrade 模块](crate::upgrade) 了解更多。
    ///
    /// 返回 UpgradeableConnection，它可以在协议升级时
    /// 自动将底层 IO 对象传递给升级处理器。
    pub fn with_upgrades(self) -> UpgradeableConnection<I, S>
    where
        I: Send,
    {
        UpgradeableConnection { inner: Some(self) }
    }
}

/// 为 Connection 实现 Future trait。
///
/// 轮询此 Future 以驱动 HTTP/1 连接。
/// 当连接完成（所有请求处理完毕或连接关闭）时返回 Ok(())。
impl<I, B, S> Future for Connection<I, S>
where
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 轮询内部调度器
        match ready!(Pin::new(&mut self.conn).poll(cx)) {
            Ok(done) => {
                match done {
                    // 正常关闭
                    proto::Dispatched::Shutdown => {}
                    proto::Dispatched::Upgrade(pending) => {
                        // 因为 I 没有 `Send` 约束，无法在这里进行升级。
                        // 如果用户尝试在此 API 上使用 `Body::on_upgrade`，
                        // 发送特殊错误告知他们需要使用 UpgradeableConnection。
                        pending.manual();
                    }
                };
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

// ===== impl Builder =====
// ===== Builder 配置构建器实现 =====

impl Builder {
    /// 创建新的连接构建器。
    ///
    /// 使用默认配置初始化所有选项。
    pub fn new() -> Self {
        Self {
            h1_parser_config: Default::default(),
            timer: Time::Empty,
            h1_half_close: false,
            // 默认启用 keep-alive
            h1_keep_alive: true,
            h1_title_case_headers: false,
            h1_preserve_header_case: false,
            h1_max_headers: None,
            // 默认头部读取超时 30 秒
            h1_header_read_timeout: Dur::Default(Some(Duration::from_secs(30))),
            h1_writev: None,
            max_buf_size: None,
            pipeline_flush: false,
            // 默认自动添加 Date 头部
            date_header: true,
        }
    }

    /// 设置 HTTP/1 连接是否支持半关闭。
    ///
    /// 客户端可以选择在等待服务器响应时关闭写端。
    /// 将此设置为 `true` 可以防止在请求中间
    /// `read` 检测到 EOF 时立即关闭连接。
    ///
    /// 默认为 `false`。
    pub fn half_close(&mut self, val: bool) -> &mut Self {
        self.h1_half_close = val;
        self
    }

    /// 启用或禁用 HTTP/1 长连接（keep-alive）。
    ///
    /// 启用后，同一个 TCP 连接可以处理多个请求/响应。
    /// 禁用后，每个请求处理完成后连接就会关闭。
    ///
    /// 默认为 `true`。
    pub fn keep_alive(&mut self, val: bool) -> &mut Self {
        self.h1_keep_alive = val;
        self
    }

    /// 设置 HTTP/1 连接是否在 socket 层将头部名称写为首字母大写格式。
    ///
    /// 例如 "content-type" -> "Content-Type"。
    /// 某些旧的 HTTP 客户端可能依赖此格式。
    ///
    /// 默认为 `false`。
    pub fn title_case_headers(&mut self, enabled: bool) -> &mut Self {
        self.h1_title_case_headers = enabled;
        self
    }

    /// 设置是否允许请求行分隔符中有多个空格。
    ///
    /// 某些非标准的客户端可能发送带有多个空格的请求行。
    ///
    /// 默认为 `false`。
    pub fn allow_multiple_spaces_in_request_line_delimiters(&mut self, enabled: bool) -> &mut Self {
        self.h1_parser_config
            .allow_multiple_spaces_in_request_line_delimiters(enabled);
        self
    }

    /// 设置 HTTP/1 连接是否静默忽略格式错误的头部行。
    ///
    /// 如果启用此选项，当头部行不以有效头部名称开头或不包含冒号时，
    /// 该行将被静默忽略，不会报告错误。
    ///
    /// 默认为 `false`。
    pub fn ignore_invalid_headers(&mut self, enabled: bool) -> &mut Builder {
        self.h1_parser_config
            .ignore_invalid_headers_in_requests(enabled);
        self
    }

    /// 设置是否支持保留原始头部大小写。
    ///
    /// 目前，这会记录接收到的原始大小写，并将其存储在 `Request` 的
    /// 私有扩展中。它还会在提供的 `Response` 中查找和使用这样的扩展。
    ///
    /// 由于相关扩展仍是私有的，目前无法与原始大小写交互。
    /// 它目前唯一的效果是以代理方式转发大小写。
    ///
    /// 默认为 `false`。
    pub fn preserve_header_case(&mut self, enabled: bool) -> &mut Self {
        self.h1_preserve_header_case = enabled;
        self
    }

    /// 设置最大头部数量。
    ///
    /// 收到请求时，解析器会预分配一个缓冲区来存储头部以获得最佳性能。
    ///
    /// 如果服务器收到的头部数超过缓冲区大小，
    /// 会响应客户端 "431 Request Header Fields Too Large"。
    ///
    /// 注意：默认情况下头部在栈上分配，具有更高的性能。设置此值后，
    /// 头部将在堆内存中分配，即每个请求都会发生堆内存分配，
    /// 性能会下降约 5%。
    ///
    /// 默认为 100。
    pub fn max_headers(&mut self, val: usize) -> &mut Self {
        self.h1_max_headers = Some(val);
        self
    }

    /// 设置读取客户端请求头的超时时间。
    /// 如果客户端在此时间内未传输完整头部，连接将被关闭。
    ///
    /// 需要通过 [`Builder::timer`] 设置 [`Timer`] 才能生效。
    /// 如果配置了 `header_read_timeout` 但没有设置 [`Timer`]，将 panic。
    ///
    /// 传入 `None` 以禁用。
    ///
    /// 默认为 30 秒。
    pub fn header_read_timeout(&mut self, read_timeout: impl Into<Option<Duration>>) -> &mut Self {
        self.h1_header_read_timeout = Dur::Configured(read_timeout.into());
        self
    }

    /// 设置 HTTP/1 连接是否尝试使用向量化写入（writev），
    /// 还是始终将数据展平到单个缓冲区中。
    ///
    /// 注意：将此设置为 false 可能意味着更多的 body 数据复制，
    /// 但在 IO 传输不支持向量化写入的情况下（如大多数 TLS 实现），
    /// 也可能提高性能。
    ///
    /// 将此设置为 true 将强制 hyper 使用队列策略，
    /// 这可能消除某些 TLS 后端的不必要克隆。
    ///
    /// 默认为 `auto`。在此模式下 hyper 将尝试猜测使用哪种模式。
    pub fn writev(&mut self, val: bool) -> &mut Self {
        self.h1_writev = Some(val);
        self
    }

    /// 设置连接的最大缓冲区大小。
    ///
    /// 默认约 400KB。
    ///
    /// # Panics
    ///
    /// 允许的最小值为 8192。如果传入的 `max` 小于此最小值，此方法将 panic。
    pub fn max_buf_size(&mut self, max: usize) -> &mut Self {
        assert!(
            max >= proto::h1::MINIMUM_MAX_BUFFER_SIZE,
            "the max_buf_size cannot be smaller than the minimum that h1 specifies."
        );
        self.max_buf_size = Some(max);
        self
    }

    /// 设置是否在 HTTP 响应中包含 `date` 头部。
    ///
    /// 注意：根据 RFC 7231 的建议，应在响应中包含 `date` 头部。
    ///
    /// 默认为 `true`。
    pub fn auto_date_header(&mut self, enabled: bool) -> &mut Self {
        self.date_header = enabled;
        self
    }

    /// 聚合刷新以更好地支持管道化（pipelining）响应。
    ///
    /// 实验性功能，可能有 bug。
    ///
    /// 默认为 `false`。
    pub fn pipeline_flush(&mut self, enabled: bool) -> &mut Self {
        self.pipeline_flush = enabled;
        self
    }

    /// 设置后台任务使用的定时器。
    ///
    /// 定时器用于实现头部读取超时等功能。
    pub fn timer<M>(&mut self, timer: M) -> &mut Self
    where
        M: Timer + Send + Sync + 'static,
    {
        self.timer = Time::Timer(Arc::new(timer));
        self
    }

    /// 将连接与 [`Service`](crate::service::Service) 绑定在一起。
    ///
    /// 返回一个必须被轮询的 Future，以便在连接上驱动 HTTP 通信。
    ///
    /// # Panics
    ///
    /// 如果配置了超时选项但未提供 `timer`，调用 `serve_connection` 将 panic。
    ///
    /// # 示例
    ///
    /// ```
    /// # use hyper::{body::Incoming, Request, Response};
    /// # use hyper::service::Service;
    /// # use hyper::server::conn::http1::Builder;
    /// # use hyper::rt::{Read, Write};
    /// # async fn run<I, S>(some_io: I, some_service: S)
    /// # where
    /// #     I: Read + Write + Unpin + Send + 'static,
    /// #     S: Service<hyper::Request<Incoming>, Response=hyper::Response<Incoming>> + Send + 'static,
    /// #     S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    /// #     S::Future: Send,
    /// # {
    /// let http = Builder::new();
    /// let conn = http.serve_connection(some_io, some_service);
    ///
    /// if let Err(e) = conn.await {
    ///     eprintln!("server connection error: {}", e);
    /// }
    /// # }
    /// # fn main() {}
    /// ```
    pub fn serve_connection<I, S>(&self, io: I, service: S) -> Connection<I, S>
    where
        S: HttpService<IncomingBody>,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        S::ResBody: 'static,
        <S::ResBody as Body>::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin,
    {
        // 创建底层的 HTTP/1 协议连接
        let mut conn = proto::Conn::new(io);

        // 应用各项配置
        conn.set_h1_parser_config(self.h1_parser_config.clone());
        conn.set_timer(self.timer.clone());

        // 如果禁用了 keep-alive
        if !self.h1_keep_alive {
            conn.disable_keep_alive();
        }
        // 如果启用了半关闭
        if self.h1_half_close {
            conn.set_allow_half_close();
        }
        // 如果启用了首字母大写头部
        if self.h1_title_case_headers {
            conn.set_title_case_headers();
        }
        // 如果启用了保留头部大小写
        if self.h1_preserve_header_case {
            conn.set_preserve_header_case();
        }
        // 设置最大头部数量
        if let Some(max_headers) = self.h1_max_headers {
            conn.set_http1_max_headers(max_headers);
        }
        // 设置头部读取超时（需要定时器支持）
        if let Some(dur) = self
            .timer
            .check(self.h1_header_read_timeout, "header_read_timeout")
        {
            conn.set_http1_header_read_timeout(dur);
        };
        // 设置写入策略
        if let Some(writev) = self.h1_writev {
            if writev {
                // 使用队列策略（向量化写入）
                conn.set_write_strategy_queue();
            } else {
                // 使用展平策略（单缓冲区）
                conn.set_write_strategy_flatten();
            }
        }
        // 设置管道化刷新
        conn.set_flush_pipeline(self.pipeline_flush);
        // 设置最大缓冲区大小
        if let Some(max) = self.max_buf_size {
            conn.set_max_buf_size(max);
        }
        // 如果禁用了 Date 头部
        if !self.date_header {
            conn.disable_date_header();
        }

        // 创建服务端调度器
        let sd = proto::h1::dispatch::Server::new(service);
        // 创建 HTTP/1 调度器，组合协议层和调度层
        let proto = proto::h1::Dispatcher::new(sd, conn);
        Connection { conn: proto }
    }
}

// ============================================================================
// UpgradeableConnection - 支持 HTTP 升级的连接
// ============================================================================

/// 将连接与服务绑定并支持 HTTP 升级的 Future。
///
/// 与普通 Connection 的区别在于，当收到升级请求时，
/// 此连接会将底层 IO 对象传递给升级处理器，
/// 而不是简单地通知用户。
///
/// 需要 IO 类型满足 Send 约束，以便安全地传递给另一个任务。
#[must_use = "futures do nothing unless polled"]
#[allow(missing_debug_implementations)]
pub struct UpgradeableConnection<T, S>
where
    S: HttpService<IncomingBody>,
{
    /// 内部的普通连接。
    /// 使用 Option 以便在升级时 take 出来获取 IO 对象。
    /// None 表示连接已经完成了升级。
    pub(super) inner: Option<Connection<T, S>>,
}

impl<I, B, S> UpgradeableConnection<I, S>
where
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    /// 为此连接启动优雅关闭过程。
    ///
    /// 调用后此 `Connection` 应继续被轮询直到关闭完成。
    pub fn graceful_shutdown(mut self: Pin<&mut Self>) {
        // 如果连接已升级（inner 为 None），则不需要关闭。
        // 只有当连接仍在正常的 HTTP 处理中时才需要关闭。
        if let Some(conn) = self.inner.as_mut() {
            Pin::new(conn).graceful_shutdown()
        }
    }
}

/// 为 UpgradeableConnection 实现 Future trait。
///
/// 处理三种情况：
/// 1. Shutdown：正常关闭
/// 2. Upgrade：需要进行 HTTP 升级，将 IO 传递给升级处理器
/// 3. Error：连接出错
impl<I, B, S> Future for UpgradeableConnection<I, S>
where
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin + Send + 'static,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(conn) = self.inner.as_mut() {
            // 连接尚未升级，轮询内部调度器
            match ready!(Pin::new(&mut conn.conn).poll(cx)) {
                Ok(proto::Dispatched::Shutdown) => {
                    // 正常关闭
                    Poll::Ready(Ok(()))
                }
                Ok(proto::Dispatched::Upgrade(pending)) => {
                    // 需要进行 HTTP 升级
                    // 从连接中取出 IO 对象和读取缓冲区
                    let (io, buf, _) = self.inner.take().unwrap().conn.into_inner();
                    // 将 IO 和缓冲区传递给升级处理器
                    pending.fulfill(Upgraded::new(io, buf));
                    Poll::Ready(Ok(()))
                }
                Err(e) => Poll::Ready(Err(e)),
            }
        } else {
            // inner 为 None，说明连接已经升级完成，返回 Ok
            Poll::Ready(Ok(()))
        }
    }
}
