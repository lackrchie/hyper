// ============================================================================
// HTTP/2 服务器协议实现模块
// ============================================================================
// 本模块实现了 HTTP/2 协议的服务器端核心逻辑，包括：
//   - Server：HTTP/2 服务器连接的主状态机
//   - Serving：处于服务状态时的连接管理
//   - H2Stream：单个 HTTP/2 流（请求/响应对）的处理
//   - Config：HTTP/2 连接的配置参数
//
// HTTP/2 的核心特性：
//   - 多路复用：一个 TCP 连接上可以并行处理多个请求
//   - 流量控制：通过窗口大小管理数据传输速率
//   - PING/PONG：用于连接保活和带宽探测
//   - CONNECT 方法：支持 HTTP 隧道/升级
// ============================================================================

use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_core::ready;
use h2::server::{Connection, Handshake, SendResponse};
use h2::{Reason, RecvStream};
use http::{Method, Request};
use pin_project_lite::pin_project;

use super::{ping, PipeToSendStream, SendBuf};
use crate::body::{Body, Incoming as IncomingBody};
use crate::common::date;
use crate::common::io::Compat;
use crate::common::time::Time;
use crate::ext::Protocol;
use crate::headers;
use crate::proto::h2::ping::Recorder;
use crate::proto::Dispatched;
use crate::rt::bounds::{Http2ServerConnExec, Http2UpgradedExec};
use crate::rt::{Read, Write};
use crate::service::HttpService;

use crate::upgrade::{OnUpgrade, Pending, Upgraded};
use crate::Response;

// ============================================================================
// 默认配置常量
// ============================================================================
// 我们的默认值是为"多数"场景选择的，这些场景通常不受资源约束，
// 因此 HTTP/2 规范默认的 64KB 可能对性能来说太有限了。
//
// 同时，服务器通常连接了多个客户端，因此比客户端更可能使用更多资源。

/// 默认连接级窗口大小：1MB
/// 连接级窗口控制整个连接上所有流的总流量
const DEFAULT_CONN_WINDOW: u32 = 1024 * 1024;

/// 默认流级窗口大小：1MB
/// 流级窗口控制单个 HTTP/2 流的流量
const DEFAULT_STREAM_WINDOW: u32 = 1024 * 1024;

/// 默认最大帧大小：16KB
/// 单个 HTTP/2 DATA 帧的最大载荷大小
const DEFAULT_MAX_FRAME_SIZE: u32 = 1024 * 16;

/// 默认最大发送缓冲区大小：400KB
/// 每个流的写入缓冲区最大容量
const DEFAULT_MAX_SEND_BUF_SIZE: usize = 1024 * 400;

/// 默认 SETTINGS_MAX_HEADER_LIST_SIZE：16KB
/// 接收的头部帧的最大大小
const DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: u32 = 1024 * 16;

/// 默认本地错误重置流数量上限：1024
/// 防止因过多的流重置导致的潜在 DOS 攻击
const DEFAULT_MAX_LOCAL_ERROR_RESET_STREAMS: usize = 1024;

/// HTTP/2 服务器连接配置。
///
/// 包含所有可调节的 HTTP/2 连接参数，用于创建新的服务器连接时传入。
#[derive(Clone, Debug)]
pub(crate) struct Config {
    /// 是否启用自适应窗口大小。
    /// 启用后会根据带宽延迟积（BDP）自动调整窗口大小。
    pub(crate) adaptive_window: bool,

    /// 初始连接级窗口大小（字节）。
    /// 控制对端在整个连接上可以发送的未确认数据总量。
    pub(crate) initial_conn_window_size: u32,

    /// 初始流级窗口大小（字节）。
    /// 控制对端在单个流上可以发送的未确认数据量。
    pub(crate) initial_stream_window_size: u32,

    /// 最大帧大小（字节）。
    /// 限制单个 HTTP/2 帧的最大载荷。
    pub(crate) max_frame_size: u32,

    /// 是否启用扩展 CONNECT 协议（RFC 8441）。
    /// 允许在 HTTP/2 上使用 CONNECT 方法进行协议升级。
    pub(crate) enable_connect_protocol: bool,

    /// 最大并发流数量。
    /// None 表示无限制；默认 200。
    /// 限制客户端可以同时打开的流数量。
    pub(crate) max_concurrent_streams: Option<u32>,

    /// 最大待接受的重置流数量。
    /// 超过此限制后会发送 GOAWAY 帧。
    pub(crate) max_pending_accept_reset_streams: Option<usize>,

    /// 最大本地错误重置流数量。
    /// 防止潜在的 DOS 攻击（参见 RUSTSEC-2024-0003）。
    pub(crate) max_local_error_reset_streams: Option<usize>,

    /// 保活 PING 发送间隔。
    /// None 表示禁用保活机制。
    pub(crate) keep_alive_interval: Option<Duration>,

    /// 保活 PING 超时时间。
    /// 如果在此时间内未收到 PONG 响应，连接将被关闭。
    pub(crate) keep_alive_timeout: Duration,

    /// 每个流的最大发送缓冲区大小（字节）。
    pub(crate) max_send_buffer_size: usize,

    /// 接收的头部列表最大大小（字节）。
    pub(crate) max_header_list_size: u32,

    /// 是否自动添加 Date 头部。
    /// RFC 7231 建议在响应中包含 Date 头部。
    pub(crate) date_header: bool,
}

/// Config 的默认值实现。
impl Default for Config {
    fn default() -> Config {
        Config {
            adaptive_window: false,
            initial_conn_window_size: DEFAULT_CONN_WINDOW,
            initial_stream_window_size: DEFAULT_STREAM_WINDOW,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            enable_connect_protocol: false,
            // 默认限制 200 个并发流
            max_concurrent_streams: Some(200),
            max_pending_accept_reset_streams: None,
            max_local_error_reset_streams: Some(DEFAULT_MAX_LOCAL_ERROR_RESET_STREAMS),
            // 默认禁用保活
            keep_alive_interval: None,
            // 保活超时默认 20 秒
            keep_alive_timeout: Duration::from_secs(20),
            max_send_buffer_size: DEFAULT_MAX_SEND_BUF_SIZE,
            max_header_list_size: DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE,
            // 默认自动添加 Date 头部
            date_header: true,
        }
    }
}

// ============================================================================
// Server 主结构体 - HTTP/2 服务器连接状态机
// ============================================================================

pin_project! {
    /// HTTP/2 服务器连接的主结构体。
    ///
    /// 实现了 Future trait，通过轮询推进 HTTP/2 连接的处理。
    /// 状态机有两个阶段：Handshaking（握手中）和 Serving（服务中）。
    ///
    /// 类型参数：
    /// - T: 底层 IO 传输层（如 TcpStream）
    /// - S: HTTP 服务，处理请求并生成响应
    /// - B: 响应体类型
    /// - E: 执行器类型，用于 spawn 子任务
    pub(crate) struct Server<T, S, B, E>
    where
        S: HttpService<IncomingBody>,
        B: Body,
    {
        // 用于 spawn H2Stream 子任务的执行器
        exec: E,
        // 定时器，用于保活超时等
        timer: Time,
        // HTTP 服务实例，处理每个请求
        service: S,
        // 当前连接状态（握手中/服务中）
        state: State<T, B>,
        // 是否自动添加 Date 头部
        date_header: bool,
        // 标记是否有待处理的关闭请求（在握手完成前收到了关闭信号）
        close_pending: bool
    }
}

/// 服务器连接状态枚举。
///
/// 代表 HTTP/2 连接生命周期的两个阶段：
/// 1. Handshaking：正在与客户端进行 HTTP/2 握手（交换 SETTINGS 帧等）
/// 2. Serving：握手完成，正在处理 HTTP 请求
enum State<T, B>
where
    B: Body,
{
    /// 握手阶段。
    /// 正在与客户端交换 HTTP/2 初始设置帧。
    Handshaking {
        /// PING/PONG 配置，包括保活和 BDP 探测设置
        ping_config: ping::Config,
        /// h2 库的握手 Future。
        /// Compat<T> 适配 hyper 的 IO trait 到 tokio 的 IO trait。
        /// SendBuf<B::Data> 是发送缓冲区类型。
        hs: Handshake<Compat<T>, SendBuf<B::Data>>,
    },
    /// 服务阶段。
    /// 握手完成，正在接受和处理请求。
    Serving(Serving<T, B>),
}

/// 处于服务状态的连接管理结构。
///
/// 持有活跃的 h2 连接以及相关的保活/带宽探测组件。
struct Serving<T, B>
where
    B: Body,
{
    /// PING/PONG 组件：(Recorder, Ponger)
    /// - Recorder: 记录数据帧和非数据帧的接收，用于 BDP 估算
    /// - Ponger: 处理 PING/PONG 帧，管理保活和窗口调整
    /// None 表示未启用保活/BDP 功能。
    ping: Option<(ping::Recorder, ping::Ponger)>,

    /// h2 库的连接对象，管理底层的 HTTP/2 帧收发。
    /// Compat<T> 适配 IO trait。
    conn: Connection<Compat<T>, SendBuf<B::Data>>,

    /// 如果有待关闭的错误，存储在此处。
    /// 连接会等待所有挂起的流完成后再关闭。
    closing: Option<crate::Error>,

    /// 是否为响应自动添加 Date 头部
    date_header: bool,
}

// ============================================================================
// Server 实现
// ============================================================================

impl<T, S, B, E> Server<T, S, B, E>
where
    T: Read + Write + Unpin,
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    B: Body + 'static,
    E: Http2ServerConnExec<S::Future, B>,
{
    /// 创建新的 HTTP/2 服务器连接。
    ///
    /// 参数：
    /// - io: 底层 IO 传输层（如 TcpStream）
    /// - service: HTTP 服务实例
    /// - config: HTTP/2 配置
    /// - exec: 任务执行器
    /// - timer: 定时器
    ///
    /// 初始化流程：
    /// 1. 根据配置创建 h2 Builder 并设置各项参数
    /// 2. 启动 HTTP/2 握手
    /// 3. 配置 BDP（带宽延迟积）探测
    /// 4. 创建 Server 实例，初始状态为 Handshaking
    pub(crate) fn new(
        io: T,
        service: S,
        config: &Config,
        exec: E,
        timer: Time,
    ) -> Server<T, S, B, E> {
        // 使用 h2 库的 Builder 配置 HTTP/2 连接参数
        let mut builder = h2::server::Builder::default();
        builder
            // 设置流级初始窗口大小
            .initial_window_size(config.initial_stream_window_size)
            // 设置连接级初始窗口大小
            .initial_connection_window_size(config.initial_conn_window_size)
            // 设置最大帧大小
            .max_frame_size(config.max_frame_size)
            // 设置最大头部列表大小
            .max_header_list_size(config.max_header_list_size)
            // 设置本地错误重置流数量上限
            .max_local_error_reset_streams(config.max_local_error_reset_streams)
            // 设置最大发送缓冲区大小
            .max_send_buffer_size(config.max_send_buffer_size);

        // 如果配置了最大并发流数量，则设置
        if let Some(max) = config.max_concurrent_streams {
            builder.max_concurrent_streams(max);
        }
        // 如果配置了最大待接受重置流数量，则设置
        if let Some(max) = config.max_pending_accept_reset_streams {
            builder.max_pending_accept_reset_streams(max);
        }
        // 如果启用了扩展 CONNECT 协议，则设置
        if config.enable_connect_protocol {
            builder.enable_connect_protocol();
        }

        // 开始 HTTP/2 握手。Compat 包装器将 hyper IO 适配为 tokio IO。
        let handshake = builder.handshake(Compat::new(io));

        // 配置带宽延迟积（BDP）探测
        // 如果启用了自适应窗口，使用当前流窗口大小作为 BDP 的初始值
        let bdp = if config.adaptive_window {
            Some(config.initial_stream_window_size)
        } else {
            None
        };

        // 配置 PING/PONG 机制
        let ping_config = ping::Config {
            bdp_initial_window: bdp,
            keep_alive_interval: config.keep_alive_interval,
            keep_alive_timeout: config.keep_alive_timeout,
            // 对于服务器，空闲时也启用保活。
            // 这样可以更积极地关闭死连接。
            keep_alive_while_idle: true,
        };

        Server {
            exec,
            timer,
            // 初始状态为握手中
            state: State::Handshaking {
                ping_config,
                hs: handshake,
            },
            service,
            date_header: config.date_header,
            close_pending: false,
        }
    }

    /// 发起优雅关闭（graceful shutdown）。
    ///
    /// 优雅关闭会：
    /// - 在握手阶段：标记 close_pending，握手完成后再关闭
    /// - 在服务阶段：调用 h2 连接的 graceful_shutdown()，
    ///   发送 GOAWAY 帧，不再接受新请求，但等待现有请求完成
    pub(crate) fn graceful_shutdown(&mut self) {
        trace!("graceful_shutdown");
        match self.state {
            State::Handshaking { .. } => {
                // 握手还没完成，先标记待关闭
                self.close_pending = true;
            }
            State::Serving(ref mut srv) => {
                // 只有在没有关闭中的错误时才发起优雅关闭
                if srv.closing.is_none() {
                    srv.conn.graceful_shutdown();
                }
            }
        }
    }
}

// ============================================================================
// Server 的 Future 实现 - 驱动整个连接的生命周期
// ============================================================================

impl<T, S, B, E> Future for Server<T, S, B, E>
where
    T: Read + Write + Unpin,
    S: HttpService<IncomingBody, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    B: Body + 'static,
    E: Http2ServerConnExec<S::Future, B>,
{
    type Output = crate::Result<Dispatched>;

    /// 轮询服务器连接，推进状态机。
    ///
    /// 状态转换：Handshaking -> Serving -> 完成
    ///
    /// 在 Handshaking 阶段：
    ///   - 等待 HTTP/2 握手完成
    ///   - 如果启用了保活/BDP，设置 PING/PONG 通道
    ///   - 转换到 Serving 状态
    ///
    /// 在 Serving 阶段：
    ///   - 如果有待处理的关闭请求，先执行优雅关闭
    ///   - 轮询服务器处理请求
    ///   - 所有请求处理完毕后返回 Dispatched::Shutdown
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        loop {
            let next = match me.state {
                State::Handshaking {
                    ref mut hs,
                    ref ping_config,
                } => {
                    // 等待 HTTP/2 握手完成
                    // ready! 宏在 Pending 时直接返回 Poll::Pending
                    let mut conn = ready!(Pin::new(hs).poll(cx).map_err(crate::Error::new_h2))?;

                    // 握手成功，如果启用了保活或 BDP，设置 PING/PONG 通道
                    let ping = if ping_config.is_enabled() {
                        let pp = conn.ping_pong().expect("conn.ping_pong");
                        Some(ping::channel(pp, ping_config.clone(), me.timer.clone()))
                    } else {
                        None
                    };

                    // 转换到 Serving 状态
                    State::Serving(Serving {
                        ping,
                        conn,
                        closing: None,
                        date_header: me.date_header,
                    })
                }
                State::Serving(ref mut srv) => {
                    // 检查是否有在握手阶段收到的关闭请求
                    if me.close_pending && srv.closing.is_none() {
                        srv.conn.graceful_shutdown();
                    }
                    // 轮询服务器处理所有请求
                    ready!(srv.poll_server(cx, &mut me.service, &mut me.exec))?;
                    // 所有请求处理完毕，连接关闭
                    return Poll::Ready(Ok(Dispatched::Shutdown));
                }
            };
            // 更新状态（从 Handshaking 到 Serving）
            me.state = next;
        }
    }
}

// ============================================================================
// Serving 实现 - 处于服务状态时的核心逻辑
// ============================================================================

impl<T, B> Serving<T, B>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
{
    /// 轮询服务器，接受并处理传入的 HTTP/2 请求。
    ///
    /// 主要流程：
    /// 1. 如果没有正在关闭，循环接受新请求
    /// 2. 对每个请求，创建 H2Stream 并 spawn 到执行器上并发处理
    /// 3. 如果正在关闭，等待连接完全关闭后返回错误
    ///
    /// 参数：
    /// - cx: 异步上下文
    /// - service: HTTP 服务实例
    /// - exec: 任务执行器
    fn poll_server<S, E>(
        &mut self,
        cx: &mut Context<'_>,
        service: &mut S,
        exec: &mut E,
    ) -> Poll<crate::Result<()>>
    where
        S: HttpService<IncomingBody, ResBody = B>,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        E: Http2ServerConnExec<S::Future, B>,
    {
        // 如果没有正在关闭（正常运行状态），接受新请求
        if self.closing.is_none() {
            loop {
                // 先处理 PING/PONG（保活和窗口调整）
                self.poll_ping(cx);

                // 尝试接受下一个传入的请求
                match ready!(self.conn.poll_accept(cx)) {
                    Some(Ok((req, mut respond))) => {
                        // 收到一个新请求
                        trace!("incoming request");

                        // 从请求头部解析 Content-Length
                        let content_length = headers::content_length_parse_all(req.headers());

                        // 获取 PING Recorder 用于记录帧接收（BDP 估算）
                        let ping = self
                            .ping
                            .as_ref()
                            .map(|ping| ping.0.clone())
                            .unwrap_or_else(ping::disabled);

                        // 记录接收到非数据帧（头部帧），用于 BDP 估算
                        ping.record_non_data();

                        // 检查是否为 CONNECT 请求（HTTP 隧道/协议升级）
                        let is_connect = req.method() == Method::CONNECT;
                        let (mut parts, stream) = req.into_parts();

                        // 根据是否为 CONNECT 请求，采用不同的处理方式
                        let (mut req, connect_parts) = if !is_connect {
                            // 普通请求：用接收流创建 IncomingBody
                            (
                                Request::from_parts(
                                    parts,
                                    IncomingBody::h2(stream, content_length.into(), ping),
                                ),
                                None,
                            )
                        } else {
                            // CONNECT 请求：特殊处理
                            // CONNECT 请求不应有非零的请求体
                            if content_length.map_or(false, |len| len != 0) {
                                warn!("h2 connect request with non-zero body not supported");
                                respond.send_reset(h2::Reason::INTERNAL_ERROR);
                                return Poll::Ready(Ok(()));
                            }
                            // 创建升级（upgrade）挂起对象
                            let (pending, upgrade) = crate::upgrade::pending();
                            debug_assert!(parts.extensions.get::<OnUpgrade>().is_none());
                            // 将升级 Future 放入请求扩展中
                            parts.extensions.insert(upgrade);
                            (
                                // CONNECT 请求使用空的请求体
                                Request::from_parts(parts, IncomingBody::empty()),
                                Some(ConnectParts {
                                    pending,
                                    ping,
                                    recv_stream: stream,
                                }),
                            )
                        };

                        // 如果请求中有 h2 扩展协议信息，转换为 hyper 的 Protocol 类型
                        if let Some(protocol) = req.extensions_mut().remove::<h2::ext::Protocol>() {
                            req.extensions_mut().insert(Protocol::from_inner(protocol));
                        }

                        // 创建 H2Stream 来处理此请求
                        // H2Stream 是一个 Future，负责：
                        // 1. 调用 service 获取响应
                        // 2. 发送响应头
                        // 3. 发送响应体
                        let fut = H2Stream::new(
                            service.call(req),
                            connect_parts,
                            respond,
                            self.date_header,
                            exec.clone(),
                        );

                        // 将 H2Stream 任务 spawn 到执行器上并发执行
                        // 这样可以同时处理多个请求（HTTP/2 多路复用）
                        exec.execute_h2stream(fut);
                    }
                    Some(Err(e)) => {
                        // 接受请求时出错
                        return Poll::Ready(Err(crate::Error::new_h2(e)));
                    }
                    None => {
                        // 没有更多传入的流了（连接即将关闭）

                        // 在关闭前检查保活是否超时
                        if let Some((ref ping, _)) = self.ping {
                            ping.ensure_not_timed_out()?;
                        }

                        trace!("incoming connection complete");
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }

        // 执行到这里说明 self.closing 不为 None（正在关闭中）
        debug_assert!(
            self.closing.is_some(),
            "poll_server broke loop without closing"
        );

        // 等待 h2 连接完全关闭
        ready!(self.conn.poll_closed(cx).map_err(crate::Error::new_h2))?;

        // 返回导致关闭的错误
        Poll::Ready(Err(self.closing.take().expect("polled after error")))
    }

    /// 轮询 PING/PONG 机制。
    ///
    /// 处理保活和带宽延迟积（BDP）探测：
    /// - SizeUpdate: BDP 估算完成，调整窗口大小
    /// - KeepAliveTimedOut: 保活超时，强制关闭连接
    fn poll_ping(&mut self, cx: &mut Context<'_>) {
        if let Some((_, ref mut estimator)) = self.ping {
            match estimator.poll(cx) {
                Poll::Ready(ping::Ponged::SizeUpdate(wnd)) => {
                    // BDP 探测得到新的窗口大小估计值
                    // 更新连接的目标窗口大小和初始窗口大小
                    self.conn.set_target_window_size(wnd);
                    let _ = self.conn.set_initial_window_size(wnd);
                }
                Poll::Ready(ping::Ponged::KeepAliveTimedOut) => {
                    // 保活超时，对端可能已经断开
                    debug!("keep-alive timed out, closing connection");
                    // 使用 NO_ERROR 原因码突然关闭连接
                    self.conn.abrupt_shutdown(h2::Reason::NO_ERROR);
                }
                Poll::Pending => {
                    // PING 尚未收到回复，继续等待
                }
            }
        }
    }
}

// ============================================================================
// H2Stream - 单个 HTTP/2 流的处理
// ============================================================================

pin_project! {
    /// HTTP/2 单个流（Stream）的处理器。
    ///
    /// 代表一个 HTTP/2 请求/响应对的完整生命周期。
    /// 它是一个 Future，被 spawn 到执行器上异步运行。
    ///
    /// 类型参数：
    /// - F: service.call() 返回的 Future 类型
    /// - B: 响应体类型
    /// - E: 执行器类型（用于 CONNECT 升级场景）
    #[allow(missing_debug_implementations)]
    pub struct H2Stream<F, B, E>
    where
        B: Body,
    {
        // h2 的响应发送器，用于发送 HTTP 响应头和数据帧
        reply: SendResponse<SendBuf<B::Data>>,
        // 当前流的状态（等待服务响应 / 发送响应体）
        #[pin]
        state: H2StreamState<F, B>,
        // 是否自动添加 Date 头部
        date_header: bool,
        // 执行器，用于 CONNECT 升级时 spawn 升级任务
        exec: E,
    }
}

pin_project! {
    /// H2Stream 的内部状态枚举。
    ///
    /// 代表流处理的两个阶段：
    /// 1. Service: 等待用户服务生成响应
    /// 2. Body: 服务已响应，正在发送响应体数据
    #[project = H2StreamStateProj]
    enum H2StreamState<F, B>
    where
        B: Body,
    {
        // 等待服务（Service）处理请求并生成响应。
        Service {
            // 服务的响应 Future
            #[pin]
            fut: F,
            // CONNECT 请求的相关部件（如果是 CONNECT 请求的话）
            connect_parts: Option<ConnectParts>,
        },
        // 已发送响应头，正在通过管道发送响应体数据。
        Body {
            // 将 Body 数据通过管道发送到 h2 SendStream 的 Future
            #[pin]
            pipe: PipeToSendStream<B>,
        },
    }
}

/// CONNECT 请求的相关部件。
///
/// 当处理 HTTP/2 CONNECT 请求时，需要保存这些信息以完成协议升级。
struct ConnectParts {
    /// 升级的 Pending 句柄，用于在响应成功后完成升级
    pending: Pending,
    /// PING Recorder，传递给升级后的连接以继续 BDP 跟踪
    ping: Recorder,
    /// h2 的接收流，升级后将直接用于双向数据传输
    recv_stream: RecvStream,
}

// ============================================================================
// H2Stream 实现
// ============================================================================

impl<F, B, E> H2Stream<F, B, E>
where
    B: Body,
{
    /// 创建新的 H2Stream。
    ///
    /// 参数：
    /// - fut: 服务的响应 Future
    /// - connect_parts: CONNECT 请求的部件（普通请求为 None）
    /// - respond: h2 的响应发送器
    /// - date_header: 是否自动添加 Date 头部
    /// - exec: 执行器
    fn new(
        fut: F,
        connect_parts: Option<ConnectParts>,
        respond: SendResponse<SendBuf<B::Data>>,
        date_header: bool,
        exec: E,
    ) -> H2Stream<F, B, E> {
        H2Stream {
            reply: respond,
            // 初始状态为等待服务响应
            state: H2StreamState::Service { fut, connect_parts },
            date_header,
            exec,
        }
    }
}

/// 发送 HTTP/2 响应的辅助宏。
///
/// 尝试通过 reply 发送响应头：
/// - 成功：返回 SendStream（用于后续发送响应体）
/// - 失败：发送 RST_STREAM 重置帧并返回错误
///
/// 参数：
/// - $me: H2Stream 的投影引用
/// - $res: 要发送的 HTTP 响应
/// - $eos: 是否是流的结束（end of stream），true 表示没有响应体
macro_rules! reply {
    ($me:expr, $res:expr, $eos:expr) => {{
        match $me.reply.send_response($res, $eos) {
            Ok(tx) => tx,
            Err(e) => {
                debug!("send response error: {}", e);
                // 发送失败，重置流
                $me.reply.send_reset(Reason::INTERNAL_ERROR);
                return Poll::Ready(Err(crate::Error::new_h2(e)));
            }
        }
    }};
}

impl<F, B, Ex, E> H2Stream<F, B, Ex>
where
    F: Future<Output = Result<Response<B>, E>>,
    B: Body,
    B::Data: 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    Ex: Http2UpgradedExec<B::Data>,
    E: Into<Box<dyn StdError + Send + Sync>>,
{
    /// H2Stream 的核心轮询逻辑。
    ///
    /// 状态机转换：Service -> Body -> 完成
    ///
    /// Service 阶段：
    ///   1. 轮询服务的 Future 获取响应
    ///   2. 同时检查客户端是否发送了 RST_STREAM
    ///   3. 处理 CONNECT 升级场景
    ///   4. 发送响应头
    ///   5. 如果有响应体，转换到 Body 阶段
    ///
    /// Body 阶段：
    ///   通过 PipeToSendStream 将响应体数据发送到 h2 流
    fn poll2(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        let mut me = self.as_mut().project();
        loop {
            let next = match me.state.as_mut().project() {
                H2StreamStateProj::Service {
                    fut: h,
                    connect_parts,
                } => {
                    // 轮询服务 Future 获取响应
                    let res = match h.poll(cx) {
                        Poll::Ready(Ok(r)) => r,
                        Poll::Pending => {
                            // 响应尚未就绪，检查客户端是否已取消请求。
                            // 通过 poll_reset 检测客户端发送的 RST_STREAM 帧。
                            if let Poll::Ready(reason) =
                                me.reply.poll_reset(cx).map_err(crate::Error::new_h2)?
                            {
                                debug!("stream received RST_STREAM: {:?}", reason);
                                return Poll::Ready(Err(crate::Error::new_h2(reason.into())));
                            }
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => {
                            // 服务处理出错
                            let err = crate::Error::new_user_service(e);
                            warn!("http2 service errored: {}", err);
                            // 根据错误类型确定 h2 重置原因码并发送 RST_STREAM
                            me.reply.send_reset(err.h2_reason());
                            return Poll::Ready(Err(err));
                        }
                    };

                    // 服务成功返回响应，分离响应头和响应体
                    let (head, body) = res.into_parts();
                    let mut res = ::http::Response::from_parts(head, ());

                    // 移除 HTTP/2 中不允许的连接级头部（如 Connection, Transfer-Encoding 等）
                    super::strip_connection_headers(res.headers_mut(), false);

                    // 如果配置了自动 Date 头部且响应中没有，则自动添加
                    if *me.date_header {
                        res.headers_mut()
                            .entry(::http::header::DATE)
                            .or_insert_with(date::update_and_header_value);
                    }

                    // 处理 CONNECT 请求的特殊响应逻辑
                    if let Some(connect_parts) = connect_parts.take() {
                        if res.status().is_success() {
                            // CONNECT 成功响应不应有 Content-Length
                            if headers::content_length_parse_all(res.headers())
                                .map_or(false, |len| len != 0)
                            {
                                warn!("h2 successful response to CONNECT request with body not supported");
                                me.reply.send_reset(h2::Reason::INTERNAL_ERROR);
                                return Poll::Ready(Err(crate::Error::new_user_header()));
                            }
                            // 移除 Content-Length 头部（如果有的话）
                            if res
                                .headers_mut()
                                .remove(::http::header::CONTENT_LENGTH)
                                .is_some()
                            {
                                warn!("successful response to CONNECT request disallows content-length header");
                            }

                            // 发送响应头（不结束流，因为升级后还需要双向通信）
                            let send_stream = reply!(me, res, false);

                            // 创建 HTTP/2 升级对（发送流 + 接收流）
                            let (h2_up, up_task) = super::upgrade::pair(
                                send_stream,
                                connect_parts.recv_stream,
                                connect_parts.ping,
                            );

                            // 完成升级，将双向通道提供给用户
                            connect_parts
                                .pending
                                .fulfill(Upgraded::new(h2_up, Bytes::new()));

                            // 将升级后的数据转发任务 spawn 到执行器
                            self.exec.execute_upgrade(up_task);
                            return Poll::Ready(Ok(()));
                        }
                        // CONNECT 请求返回非成功状态码，按普通响应处理
                    }

                    // 处理普通响应（非 CONNECT 或 CONNECT 失败）
                    if !body.is_end_stream() {
                        // 有响应体需要发送

                        // 如果 Body 能提供精确的大小信息，自动设置 Content-Length
                        if let Some(len) = body.size_hint().exact() {
                            headers::set_content_length_if_missing(res.headers_mut(), len);
                        }

                        // 发送响应头（不结束流，因为后续还有响应体）
                        let body_tx = reply!(me, res, false);

                        // 转换到 Body 状态，通过管道发送响应体
                        H2StreamState::Body {
                            pipe: PipeToSendStream::new(body, body_tx),
                        }
                    } else {
                        // 没有响应体（如 204 No Content），发送响应头并结束流
                        reply!(me, res, true);
                        return Poll::Ready(Ok(()));
                    }
                }
                H2StreamStateProj::Body { pipe } => {
                    // Body 阶段：轮询管道将响应体数据发送到 h2 流
                    return pipe.poll(cx);
                }
            };
            // 更新状态（从 Service 到 Body）
            me.state.set(next);
        }
    }
}

/// 为 H2Stream 实现 Future trait。
///
/// 这是被执行器 spawn 和轮询的入口。
/// 将 poll2 的结果（Result）映射为 ()，仅在调试日志中记录错误。
/// 错误不会向上传播，因为每个流是独立处理的，
/// 单个流的错误不应影响整个连接。
impl<F, B, Ex, E> Future for H2Stream<F, B, Ex>
where
    F: Future<Output = Result<Response<B>, E>>,
    B: Body,
    B::Data: 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    Ex: Http2UpgradedExec<B::Data>,
    E: Into<Box<dyn StdError + Send + Sync>>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll2(cx).map(|res| {
            if let Err(_e) = res {
                debug!("stream error: {}", _e);
            }
        })
    }
}
