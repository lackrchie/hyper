//! HTTP/2 客户端协议实现模块
//!
//! 本模块是 hyper HTTP/2 客户端连接的核心实现层，负责管理 HTTP/2 客户端连接的
//! 完整生命周期，包括：
//!
//! - **连接握手**：通过 `handshake` 函数完成 HTTP/2 连接的初始化，包括设置流量控制
//!   窗口、帧大小等参数，并建立 PING/PONG 机制。
//! - **请求调度**：`ClientTask` 作为主要的 Future，不断从请求通道接收新请求，
//!   通过 h2 发送请求，并将响应通过回调函数返回给调用者。
//! - **连接管理**：`ConnTask` 负责监控底层 h2 连接的状态，在所有客户端句柄被
//!   释放后优雅关闭连接。
//! - **请求体发送**：通过 `PipeToSendStream` 将请求体异步写入 h2 流。
//!
//! 架构概览：
//! ```text
//! Client (用户API)
//!   └─> ClientTask (请求调度 Future)
//!         ├─> h2::SendRequest (发送HTTP请求)
//!         ├─> PipeMap (请求体发送任务)
//!         ├─> ResponseFutMap (响应接收任务)
//!         └─> ConnTask (连接管理任务)
//!               └─> h2::Connection (底层h2连接)
//! ```

// 标准库导入
use std::{
    convert::Infallible, // 不可能的错误类型，用于表示不会产生值的通道
    future::Future,      // Future trait，异步编程基础
    marker::PhantomData,  // 幽灵数据标记，用于携带未使用的泛型参数
    pin::Pin,            // Pin 类型，确保自引用类型的内存安全
    task::{Context, Poll}, // 异步任务上下文和轮询结果
    time::Duration,       // 时间间隔类型
};

// 内部 crate 导入
use crate::rt::{Read, Write}; // hyper 的异步 I/O trait，抽象底层传输层
// 第三方 crate 导入
use bytes::Bytes; // 高效的不可变字节缓冲区
use futures_channel::mpsc::{Receiver, Sender}; // 多生产者单消费者通道的收发端
use futures_channel::{mpsc, oneshot}; // mpsc 和 oneshot 通道
use futures_core::{ready, FusedFuture, FusedStream, Stream}; // 异步核心 trait 和 ready! 宏
use h2::client::{Builder, Connection, SendRequest}; // h2 客户端类型：构建器、连接和请求发送器
use h2::SendStream; // h2 发送流
use http::{Method, StatusCode}; // HTTP 方法和状态码
use pin_project_lite::pin_project; // 轻量级 pin projection 宏

// 内部模块导入
use super::ping::{Ponger, Recorder}; // Ping 响应器和记录器
use super::{ping, PipeToSendStream, SendBuf}; // Ping 模块、Body 到 SendStream 的适配器和发送缓冲区
use crate::body::{Body, Incoming as IncomingBody}; // Body trait 和传入请求体类型
use crate::client::dispatch::{Callback, SendWhen, TrySendError}; // 客户端调度相关类型
use crate::common::either::Either; // 二选一枚举，类似 Result 但两个变体都是成功值
use crate::common::io::Compat; // I/O 兼容层，将 hyper 的 Read/Write 适配为 tokio 的
use crate::common::time::Time; // 时间抽象
use crate::ext::Protocol; // HTTP/2 扩展 CONNECT 协议
use crate::headers; // HTTP 头部处理工具函数
use crate::proto::Dispatched; // 调度完成状态枚举
use crate::rt::bounds::{Http2ClientConnExec, Http2UpgradedExec}; // 执行器 trait 约束
use crate::upgrade::Upgraded; // 升级后的连接类型
use crate::{Request, Response}; // HTTP 请求和响应类型
use h2::client::ResponseFuture; // h2 的响应 Future 类型

/// 客户端请求接收通道类型别名
///
/// 从 `client::dispatch::Sender` 接收请求和回调，其中：
/// - `Request<B>` 是要发送的 HTTP 请求
/// - `Response<IncomingBody>` 是期望的响应类型（通过 Callback 返回）
type ClientRx<B> = crate::client::dispatch::Receiver<Request<B>, Response<IncomingBody>>;

///// An mpsc channel is used to help notify the `Connection` task when *all*
///// other handles to it have been dropped, so that it can shutdown.
/// 连接丢弃引用类型
///
/// mpsc 通道的发送端，当所有 `ConnDropRef` 被 drop 时，`ConnTask` 会收到通知
/// 并开始关闭连接。这是一种利用通道关闭语义来检测引用释放的模式。
type ConnDropRef = mpsc::Sender<Infallible>;

///// A oneshot channel watches the `Connection` task, and when it completes,
///// the "dispatch" task will be notified and can shutdown sooner.
/// 连接结束信号类型
///
/// oneshot 通道的接收端，当 `ConnTask`（连接任务）完成时，
/// 发送端会被 drop，导致接收端收到错误，从而通知 `ClientTask` 连接已关闭。
type ConnEof = oneshot::Receiver<Infallible>;

// 默认连接窗口大小 5MB（HTTP/2 规范默认 64KB 对性能限制较大）
// Our defaults are chosen for the "majority" case, which usually are not
// resource constrained, and so the spec default of 64kb can be too limiting
// for performance.
const DEFAULT_CONN_WINDOW: u32 = 1024 * 1024 * 5; // 5mb
/// 默认流窗口大小 2MB
const DEFAULT_STREAM_WINDOW: u32 = 1024 * 1024 * 2; // 2mb
/// 默认最大帧大小 16KB
const DEFAULT_MAX_FRAME_SIZE: u32 = 1024 * 16; // 16kb
/// 默认最大发送缓冲区大小 1MB
const DEFAULT_MAX_SEND_BUF_SIZE: usize = 1024 * 1024; // 1mb
/// 默认最大头部列表大小 16KB
const DEFAULT_MAX_HEADER_LIST_SIZE: u32 = 1024 * 16; // 16kb

// The maximum number of concurrent streams that the client is allowed to open
// before it receives the initial SETTINGS frame from the server.
// This default value is derived from what the HTTP/2 spec recommends as the
// minimum value that endpoints advertise to their peers. It means that using
// this value will minimize the chance of the failure where the local endpoint
// attempts to open too many streams and gets rejected by the remote peer with
// the `REFUSED_STREAM` error.
/// 默认初始最大并发发送流数量（100）
///
/// 这是在收到服务端 SETTINGS 帧之前允许打开的最大并发流数量。
/// 使用 HTTP/2 规范推荐的最小值，以减少被远端 REFUSED_STREAM 拒绝的风险。
const DEFAULT_INITIAL_MAX_SEND_STREAMS: usize = 100;

/// HTTP/2 客户端连接配置
///
/// 包含所有可调整的 HTTP/2 连接参数，用于构建 h2 客户端连接。
#[derive(Clone, Debug)]
pub(crate) struct Config {
    /// 是否启用自适应窗口（BDP 算法）
    pub(crate) adaptive_window: bool,
    /// 连接级别的初始窗口大小
    pub(crate) initial_conn_window_size: u32,
    /// 流级别的初始窗口大小
    pub(crate) initial_stream_window_size: u32,
    /// 初始最大并发发送流数量
    pub(crate) initial_max_send_streams: usize,
    /// 最大帧大小（None 使用 h2 默认值）
    pub(crate) max_frame_size: Option<u32>,
    /// 最大头部列表大小
    pub(crate) max_header_list_size: u32,
    /// Keep-alive ping 间隔（None 表示禁用）
    pub(crate) keep_alive_interval: Option<Duration>,
    /// Keep-alive 超时时间
    pub(crate) keep_alive_timeout: Duration,
    /// 是否在空闲时也发送 keep-alive ping
    pub(crate) keep_alive_while_idle: bool,
    /// 最大并发重置流数量（None 使用 h2 默认值）
    pub(crate) max_concurrent_reset_streams: Option<usize>,
    /// 最大发送缓冲区大小
    pub(crate) max_send_buffer_size: usize,
    /// 最大挂起接受重置流数量
    pub(crate) max_pending_accept_reset_streams: Option<usize>,
    /// HPACK 头部表大小
    pub(crate) header_table_size: Option<u32>,
    /// 最大并发流数量
    pub(crate) max_concurrent_streams: Option<u32>,
}

/// Config 的默认值实现
impl Default for Config {
    fn default() -> Config {
        Config {
            adaptive_window: false,
            initial_conn_window_size: DEFAULT_CONN_WINDOW,
            initial_stream_window_size: DEFAULT_STREAM_WINDOW,
            initial_max_send_streams: DEFAULT_INITIAL_MAX_SEND_STREAMS,
            max_frame_size: Some(DEFAULT_MAX_FRAME_SIZE),
            max_header_list_size: DEFAULT_MAX_HEADER_LIST_SIZE,
            keep_alive_interval: None,
            keep_alive_timeout: Duration::from_secs(20),
            keep_alive_while_idle: false,
            max_concurrent_reset_streams: None,
            max_send_buffer_size: DEFAULT_MAX_SEND_BUF_SIZE,
            max_pending_accept_reset_streams: None,
            header_table_size: None,
            max_concurrent_streams: None,
        }
    }
}

/// 根据配置创建 h2 客户端 Builder
///
/// 将 hyper 的 Config 转换为 h2 crate 的 Builder 配置。
/// 注意：`enable_push(false)` 禁用了服务器推送功能。
fn new_builder(config: &Config) -> Builder {
    let mut builder = Builder::default();
    builder
        .initial_max_send_streams(config.initial_max_send_streams)
        .initial_window_size(config.initial_stream_window_size)
        .initial_connection_window_size(config.initial_conn_window_size)
        .max_header_list_size(config.max_header_list_size)
        .max_send_buffer_size(config.max_send_buffer_size)
        .enable_push(false); // hyper 客户端不支持服务器推送
    if let Some(max) = config.max_frame_size {
        builder.max_frame_size(max);
    }
    if let Some(max) = config.max_concurrent_reset_streams {
        builder.max_concurrent_reset_streams(max);
    }
    if let Some(max) = config.max_pending_accept_reset_streams {
        builder.max_pending_accept_reset_streams(max);
    }
    if let Some(size) = config.header_table_size {
        builder.header_table_size(size);
    }
    if let Some(max) = config.max_concurrent_streams {
        builder.max_concurrent_streams(max);
    }
    builder
}

/// 根据连接配置创建 Ping 配置
///
/// 将 Config 中与 Ping 相关的字段提取为独立的 ping::Config。
fn new_ping_config(config: &Config) -> ping::Config {
    ping::Config {
        bdp_initial_window: if config.adaptive_window {
            Some(config.initial_stream_window_size)
        } else {
            None
        },
        keep_alive_interval: config.keep_alive_interval,
        keep_alive_timeout: config.keep_alive_timeout,
        keep_alive_while_idle: config.keep_alive_while_idle,
    }
}

/// 执行 HTTP/2 客户端握手
///
/// 完成 HTTP/2 连接前言（connection preface）的交换，设置 PING/PONG 机制，
/// 并启动后台连接管理任务。
///
/// # 参数
/// - `io`: 底层 I/O 传输层（通常是 TCP 或 TLS 连接）
/// - `req_rx`: 请求接收通道，从客户端 API 层接收待发送的请求
/// - `config`: HTTP/2 连接配置
/// - `exec`: 执行器，用于 spawn 后台任务
/// - `timer`: 时间抽象，用于定时器功能
///
/// # 返回值
/// 返回 `ClientTask`，它是一个 Future，需要被持续轮询以驱动 HTTP/2 连接。
pub(crate) async fn handshake<T, B, E>(
    io: T,
    req_rx: ClientRx<B>,
    config: &Config,
    mut exec: E,
    timer: Time,
) -> crate::Result<ClientTask<B, E, T>>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
    B::Data: Send + 'static,
    E: Http2ClientConnExec<B, T> + Clone + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // 使用 Compat 适配器将 hyper 的 Read/Write 转换为 h2 期望的 tokio I/O trait
    let (h2_tx, mut conn) = new_builder(config)
        .handshake::<_, SendBuf<B::Data>>(Compat::new(io))
        .await
        .map_err(crate::Error::new_h2)?;

    // An mpsc channel is used entirely to detect when the
    // 'Client' has been dropped. This is to get around a bug
    // in h2 where dropping all SendRequests won't notify a
    // parked Connection.
    // 创建 mpsc 通道用于检测 Client 句柄是否已全部被 drop。
    // 这是为了绕过 h2 中 drop 所有 SendRequest 不会通知 Connection 的 bug。
    let (conn_drop_ref, conn_drop_rx) = mpsc::channel(1);
    // 创建 oneshot 通道用于从连接任务向调度任务传递关闭信号
    let (cancel_tx, conn_eof) = oneshot::channel();

    let ping_config = new_ping_config(config);

    // 根据 ping 配置决定是否启用 PING/PONG 机制
    let (conn, ping) = if ping_config.is_enabled() {
        // 从 h2 连接获取 PingPong 句柄
        let pp = conn.ping_pong().expect("conn.ping_pong");
        let (recorder, ponger) = ping::channel(pp, ping_config, timer);

        // 将 Ponger 与 Connection 组合，使连接任务同时处理 PING 响应
        let conn: Conn<_, B> = Conn::new(ponger, conn);
        // 使用 Either::left 包装（启用 ping 的连接类型）
        (Either::left(conn), recorder)
    } else {
        // 不启用 ping，使用 Either::right 直接包装原始连接
        (Either::right(conn), ping::disabled())
    };
    // 包装连接，将 h2::Error 映射为 ()，简化错误处理
    let conn: ConnMapErr<T, B> = ConnMapErr {
        conn,
        is_terminated: false,
    };

    // 在执行器中 spawn 连接管理任务
    exec.execute_h2_future(H2ClientFuture::Task {
        task: ConnTask::new(conn, conn_drop_rx, cancel_tx),
    });

    // 返回 ClientTask，它将作为请求调度的主 Future
    Ok(ClientTask {
        ping,
        conn_drop_ref,
        conn_eof,
        executor: exec,
        h2_tx,
        req_rx,
        fut_ctx: None,
        marker: PhantomData,
    })
}

// 使用 pin_project 宏定义带有 Ponger 的连接包装器
pin_project! {
    /// 带有 Ping 功能的 h2 连接包装器
    ///
    /// 将 `Ponger`（Ping 响应处理器）与 h2 `Connection` 组合在一起，
    /// 在轮询连接时同时处理 PING/PONG 帧和 BDP 窗口调整。
    struct Conn<T, B>
    where
        B: Body,
    {
        // Ping 响应器，处理 PONG 帧和 BDP 计算
        #[pin]
        ponger: Ponger,
        // h2 客户端连接，使用 Compat 适配器包装 I/O 层
        #[pin]
        conn: Connection<Compat<T>, SendBuf<<B as Body>::Data>>,
    }
}

impl<T, B> Conn<T, B>
where
    B: Body,
    T: Read + Write + Unpin,
{
    /// 创建新的带 Ping 功能的连接
    fn new(ponger: Ponger, conn: Connection<Compat<T>, SendBuf<<B as Body>::Data>>) -> Self {
        Conn { ponger, conn }
    }
}

/// Conn 的 Future 实现
///
/// 在每次轮询时：
/// 1. 先轮询 Ponger 处理 PING/PONG 事件
/// 2. 如果 BDP 计算出新窗口大小，则更新连接和流的窗口
/// 3. 如果 keep-alive 超时，则返回完成（将导致连接关闭）
/// 4. 最后轮询底层 h2 连接
impl<T, B> Future for Conn<T, B>
where
    B: Body,
    T: Read + Write + Unpin,
{
    type Output = Result<(), h2::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        // 先处理 PING/PONG 事件
        match this.ponger.poll(cx) {
            Poll::Ready(ping::Ponged::SizeUpdate(wnd)) => {
                // BDP 计算出新的窗口大小，更新连接和流的窗口
                this.conn.set_target_window_size(wnd);
                this.conn.set_initial_window_size(wnd)?;
            }
            Poll::Ready(ping::Ponged::KeepAliveTimedOut) => {
                // keep-alive 超时，关闭连接
                debug!("connection keep-alive timed out");
                return Poll::Ready(Ok(()));
            }
            Poll::Pending => {}
        }

        // 轮询底层 h2 连接
        Pin::new(&mut this.conn).poll(cx)
    }
}

// 连接错误映射包装器，将 h2::Error 映射为 ()
pin_project! {
    /// 连接错误映射包装器
    ///
    /// 包装 `Conn` 或原始 `Connection`（通过 `Either`），
    /// 将它们的错误输出从 `h2::Error` 映射为 `()`。
    /// 同时实现 `FusedFuture` 以支持 select/join 等组合器。
    struct ConnMapErr<T, B>
    where
        B: Body,
        T: Read,
        T: Write,
        T: Unpin,
    {
        // 底层连接，Left 为带 Ping 的连接，Right 为原始 h2 连接
        #[pin]
        conn: Either<Conn<T, B>, Connection<Compat<T>, SendBuf<<B as Body>::Data>>>,
        // 标记 Future 是否已完成（用于 FusedFuture 实现）
        #[pin]
        is_terminated: bool,
    }
}

/// ConnMapErr 的 Future 实现
///
/// 将底层连接的错误通过 debug 日志输出后丢弃，简化上层的错误处理。
impl<T, B> Future for ConnMapErr<T, B>
where
    B: Body,
    T: Read + Write + Unpin,
{
    type Output = Result<(), ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // 如果已完成，返回 Pending（不再轮询）
        if *this.is_terminated {
            return Poll::Pending;
        }
        let polled = this.conn.poll(cx);
        if polled.is_ready() {
            *this.is_terminated = true;
        }
        // 将错误映射为 ()，仅通过 debug 日志记录
        polled.map_err(|_e| {
            debug!(error = %_e, "connection error");
        })
    }
}

/// 为 ConnMapErr 实现 FusedFuture
///
/// FusedFuture 允许 select! 等宏知道 Future 是否已完成，
/// 避免重复轮询已完成的 Future。
impl<T, B> FusedFuture for ConnMapErr<T, B>
where
    B: Body,
    T: Read + Write + Unpin,
{
    fn is_terminated(&self) -> bool {
        self.is_terminated
    }
}

// 连接管理任务
pin_project! {
    /// HTTP/2 连接管理任务
    ///
    /// 在后台运行，同时监控：
    /// 1. 底层 h2 连接的状态（conn）
    /// 2. 所有客户端句柄是否已被释放（drop_rx）
    ///
    /// 当所有客户端句柄被释放时，通过 drop cancel_tx 通知 ClientTask 连接已关闭。
    pub struct ConnTask<T, B>
    where
        B: Body,
        T: Read,
        T: Write,
        T: Unpin,
    {
        // mpsc 接收端，当所有发送端被 drop 时会收到通知
        #[pin]
        drop_rx: Receiver<Infallible>,
        // oneshot 发送端，drop 时会通知 ClientTask 连接已结束
        #[pin]
        cancel_tx: Option<oneshot::Sender<Infallible>>,
        // 底层连接（带错误映射）
        #[pin]
        conn: ConnMapErr<T, B>,
    }
}

impl<T, B> ConnTask<T, B>
where
    B: Body,
    T: Read + Write + Unpin,
{
    /// 创建新的连接管理任务
    fn new(
        conn: ConnMapErr<T, B>,
        drop_rx: Receiver<Infallible>,
        cancel_tx: oneshot::Sender<Infallible>,
    ) -> Self {
        Self {
            drop_rx,
            cancel_tx: Some(cancel_tx),
            conn,
        }
    }
}

/// ConnTask 的 Future 实现
///
/// 同时轮询连接和 drop 检测通道：
/// - 如果连接完成（无论成功或失败），任务结束
/// - 如果所有客户端句柄被 drop（mpsc 通道关闭），
///   drop cancel_tx 以通知 ClientTask，然后继续轮询连接以完成关闭
impl<T, B> Future for ConnTask<T, B>
where
    B: Body,
    T: Read + Write + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // 轮询底层连接
        if !this.conn.is_terminated() && Pin::new(&mut this.conn).poll(cx).is_ready() {
            // ok or err, the `conn` has finished.
            // 连接已完成（成功或出错），任务结束
            return Poll::Ready(());
        }

        // 检查是否所有客户端句柄已被 drop
        if !this.drop_rx.is_terminated() && Pin::new(&mut this.drop_rx).poll_next(cx).is_ready() {
            // mpsc has been dropped, hopefully polling
            // the connection some more should start shutdown
            // and then close.
            // 所有客户端句柄已释放，开始关闭连接流程
            trace!("send_request dropped, starting conn shutdown");
            // drop cancel_tx 以通知 ClientTask
            drop(this.cancel_tx.take().expect("ConnTask Future polled twice"));
        }

        Poll::Pending
    }
}

// H2 客户端后台任务的统一 Future 枚举
pin_project! {
    /// HTTP/2 客户端后台任务枚举
    ///
    /// 使用 `#[project]` 属性生成投影枚举 `H2ClientFutureProject`，
    /// 用于在 `poll` 方法中安全地匹配和访问 pin 投影的字段。
    ///
    /// 该枚举统一了三种后台任务类型：
    /// - `Pipe`: 请求体发送任务
    /// - `Send`: 响应等待和回调任务
    /// - `Task`: 连接管理任务
    #[project = H2ClientFutureProject]
    pub enum H2ClientFuture<B, T, E>
    where
        B: http_body::Body,
        B: 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        T: Read,
        T: Write,
        T: Unpin,
    {
        /// 请求体发送管道任务
        Pipe {
            #[pin]
            pipe: PipeMap<B>,
        },
        /// 响应等待和回调分发任务
        Send {
            #[pin]
            send_when: SendWhen<B, E>,
        },
        /// 连接管理任务
        Task {
            #[pin]
            task: ConnTask<T, B>,
        },
    }
}

/// H2ClientFuture 的 Future 实现
///
/// 根据当前变体委托给对应的内部 Future 进行轮询。
impl<B, T, E> Future for H2ClientFuture<B, T, E>
where
    B: http_body::Body + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    T: Read + Write + Unpin,
    E: Http2UpgradedExec<B::Data>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> std::task::Poll<Self::Output> {
        let this = self.project();

        match this {
            H2ClientFutureProject::Pipe { pipe } => pipe.poll(cx),
            H2ClientFutureProject::Send { send_when } => send_when.poll(cx),
            H2ClientFutureProject::Task { task } => task.poll(cx),
        }
    }
}

/// 请求上下文，保存正在处理的请求的完整状态
///
/// 当 `ClientTask` 发送请求后但尚未完成处理时，使用此结构体保存中间状态。
/// 例如当 h2 流处于 "pending open" 状态时，需要暂存请求上下文等待流就绪。
struct FutCtx<B>
where
    B: Body,
{
    /// 是否为 CONNECT 请求
    is_connect: bool,
    /// 请求体是否已结束（空 Body）
    eos: bool,
    /// h2 响应 Future
    fut: ResponseFuture,
    /// h2 发送流，用于发送请求体
    body_tx: SendStream<SendBuf<B::Data>>,
    /// 请求体
    body: B,
    /// 回调函数，用于将响应返回给调用者
    cb: Callback<Request<B>, Response<IncomingBody>>,
}

/// 为 FutCtx 实现 Unpin
///
/// FutCtx 的所有字段都不包含自引用结构，因此可以安全地 Unpin。
impl<B: Body> Unpin for FutCtx<B> {}

/// HTTP/2 客户端请求调度任务
///
/// 这是客户端 HTTP/2 连接的核心 Future。它不断从请求通道接收新请求，
/// 通过 h2 发送请求，处理响应，并在连接关闭时清理资源。
///
/// # 泛型参数
/// - `B`: 请求体类型
/// - `E`: 执行器类型，用于 spawn 后台任务
/// - `T`: I/O 传输层类型（用于 PhantomData 标记）
pub(crate) struct ClientTask<B, E, T>
where
    B: Body,
    E: Unpin,
{
    /// Ping 记录器，用于跟踪 BDP 和 keep-alive
    ping: ping::Recorder,
    /// 连接丢弃引用，clone 并分发给各后台任务以跟踪存活状态
    conn_drop_ref: ConnDropRef,
    /// 连接结束信号接收端
    conn_eof: ConnEof,
    /// 任务执行器
    executor: E,
    /// h2 请求发送器
    h2_tx: SendRequest<SendBuf<B::Data>>,
    /// 请求接收通道
    req_rx: ClientRx<B>,
    /// 暂存的请求上下文（当 h2 流 pending open 时使用）
    fut_ctx: Option<FutCtx<B>>,
    /// PhantomData 标记 T 类型，使 ClientTask 的泛型签名包含 T
    marker: PhantomData<T>,
}

/// ClientTask 的辅助方法
impl<B, E, T> ClientTask<B, E, T>
where
    B: Body + 'static,
    E: Http2ClientConnExec<B, T> + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    T: Read + Write + Unpin,
{
    /// 检查扩展 CONNECT 协议是否已启用
    ///
    /// 扩展 CONNECT 协议（RFC 8441）允许在 HTTP/2 上建立 WebSocket 等协议。
    pub(crate) fn is_extended_connect_protocol_enabled(&self) -> bool {
        self.h2_tx.is_extended_connect_protocol_enabled()
    }
}

// 请求体发送管道的映射包装器
pin_project! {
    /// 请求体发送管道的包装器
    ///
    /// 在 `PipeToSendStream` 的基础上添加了连接引用计数和 Ping 记录器，
    /// 确保在请求体发送期间连接不会被意外关闭，且 BDP 跟踪保持准确。
    pub struct PipeMap<S>
    where
        S: Body,
    {
        // 底层的 Body 到 SendStream 管道
        #[pin]
        pipe: PipeToSendStream<S>,
        // 连接丢弃引用，保持连接存活直到 Body 发送完成
        #[pin]
        conn_drop_ref: Option<Sender<Infallible>>,
        // Ping 记录器，维持"活跃流"状态以支持 BDP 计算
        #[pin]
        ping: Option<Recorder>,
    }
}

/// PipeMap 的 Future 实现
///
/// 轮询底层管道，完成时释放连接引用和 Ping 记录器。
impl<B> Future for PipeMap<B>
where
    B: http_body::Body,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> std::task::Poll<Self::Output> {
        let mut this = self.project();

        match Pin::new(&mut this.pipe).poll(cx) {
            Poll::Ready(result) => {
                if let Err(_e) = result {
                    debug!("client request body error: {}", _e);
                }
                // Body 发送完成（成功或失败），释放资源
                drop(this.conn_drop_ref.take().expect("Future polled twice"));
                drop(this.ping.take().expect("Future polled twice"));
                return Poll::Ready(());
            }
            Poll::Pending => (),
        };
        Poll::Pending
    }
}

/// ClientTask 的请求处理方法
impl<B, E, T> ClientTask<B, E, T>
where
    B: Body + 'static + Unpin,
    B::Data: Send,
    E: Http2ClientConnExec<B, T> + Clone + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    T: Read + Write + Unpin,
{
    /// 处理已发送请求的后续工作（Body 发送和响应等待）
    ///
    /// 对于非 CONNECT 请求：创建 PipeToSendStream 发送请求体
    /// 对于 CONNECT 请求：保留 SendStream 供升级后使用
    ///
    /// 然后创建 ResponseFutMap 等待服务端响应。
    fn poll_pipe(&mut self, f: FutCtx<B>, cx: &mut Context<'_>) {
        let ping = self.ping.clone();

        let send_stream = if !f.is_connect {
            if !f.eos {
                // 非 CONNECT 请求且 Body 非空，创建管道发送请求体
                let mut pipe = PipeToSendStream::new(f.body, f.body_tx);

                // eagerly see if the body pipe is ready and
                // can thus skip allocating in the executor
                // 尝试立即轮询管道，如果 Body 已就绪则避免 spawn 新任务
                match Pin::new(&mut pipe).poll(cx) {
                    Poll::Ready(_) => (),
                    Poll::Pending => {
                        // Body 未就绪，需要 spawn 后台任务
                        let conn_drop_ref = self.conn_drop_ref.clone();
                        // keep the ping recorder's knowledge of an
                        // "open stream" alive while this body is
                        // still sending...
                        // 保持 Ping 记录器的"活跃流"状态
                        let ping = ping.clone();

                        let pipe = PipeMap {
                            pipe,
                            conn_drop_ref: Some(conn_drop_ref),
                            ping: Some(ping),
                        };
                        // Clear send task
                        // 在执行器中 spawn 请求体发送任务
                        self.executor
                            .execute_h2_future(H2ClientFuture::Pipe { pipe });
                    }
                }
            }

            None
        } else {
            // CONNECT 请求：保留 SendStream 供升级使用
            Some(f.body_tx)
        };

        // 创建响应等待任务并 spawn 到执行器
        self.executor.execute_h2_future(H2ClientFuture::Send {
            send_when: SendWhen {
                when: ResponseFutMap {
                    fut: f.fut,
                    ping: Some(ping),
                    send_stream: Some(send_stream),
                    exec: self.executor.clone(),
                },
                call_back: Some(f.cb),
            },
        });
    }
}

// 响应 Future 映射器
pin_project! {
    /// HTTP/2 响应 Future 映射器
    ///
    /// 包装 h2 的 `ResponseFuture`，在收到响应后进行额外处理：
    /// - 记录 Ping 数据（用于 BDP 计算）
    /// - 处理 CONNECT 请求的升级流程
    /// - 将 h2 响应转换为 hyper 的 Response 类型
    pub(crate) struct ResponseFutMap<B, E>
    where
        B: Body,
        B: 'static,
    {
        // h2 响应 Future
        #[pin]
        fut: ResponseFuture,
        // Ping 记录器（使用 Option 以支持 take）
        ping: Option<Recorder>,
        // 可选的 SendStream，仅 CONNECT 请求时为 Some(Some(stream))
        // 外层 Option 用于 take，内层 Option 区分 CONNECT 与非 CONNECT
        #[pin]
        send_stream: Option<Option<SendStream<SendBuf<<B as Body>::Data>>>>,
        // 执行器，用于 spawn 升级任务
        exec: E,
    }
}

/// ResponseFutMap 的 Future 实现
///
/// 等待 h2 响应到达后：
/// 1. 记录非数据帧接收（Ping）
/// 2. 对 CONNECT 请求：如果状态码为 200 OK，建立升级连接
/// 3. 对普通请求：将 h2 响应流包装为 IncomingBody
impl<B, E> Future for ResponseFutMap<B, E>
where
    B: Body + 'static,
    E: Http2UpgradedExec<B::Data>,
{
    type Output = Result<Response<crate::body::Incoming>, (crate::Error, Option<Request<B>>)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        // 等待 h2 响应
        let result = ready!(this.fut.poll(cx));

        let ping = this.ping.take().expect("Future polled twice");
        let send_stream = this.send_stream.take().expect("Future polled twice");

        match result {
            Ok(res) => {
                // record that we got the response headers
                // 记录收到响应头部（用于 keep-alive 和 BDP）
                ping.record_non_data();

                let content_length = headers::content_length_parse_all(res.headers());
                // 检查是否为 CONNECT 请求的成功响应
                if let (Some(mut send_stream), StatusCode::OK) = (send_stream, res.status()) {
                    // CONNECT 请求成功，建立隧道
                    if content_length.map_or(false, |len| len != 0) {
                        // CONNECT 成功响应不应包含非零 Body
                        warn!("h2 connect response with non-zero body not supported");

                        send_stream.send_reset(h2::Reason::INTERNAL_ERROR);
                        return Poll::Ready(Err((
                            crate::Error::new_h2(h2::Reason::INTERNAL_ERROR.into()),
                            None::<Request<B>>,
                        )));
                    }
                    // 将 h2 流转换为升级连接
                    let (parts, recv_stream) = res.into_parts();
                    let mut res = Response::from_parts(parts, IncomingBody::empty());

                    let (pending, on_upgrade) = crate::upgrade::pending();

                    // 创建升级连接对（用户端 + 后台任务）
                    let (h2_up, up_task) = super::upgrade::pair(send_stream, recv_stream, ping);
                    // spawn 后台发送任务
                    self.exec.execute_upgrade(up_task);
                    let upgraded = Upgraded::new(h2_up, Bytes::new());

                    // 完成升级流程，将 Upgraded 传递给等待方
                    pending.fulfill(upgraded);
                    res.extensions_mut().insert(on_upgrade);

                    Poll::Ready(Ok(res))
                } else {
                    // 普通响应：将 h2 RecvStream 包装为 IncomingBody
                    let res = res.map(|stream| {
                        let ping = ping.for_stream(&stream);
                        IncomingBody::h2(stream, content_length.into(), ping)
                    });
                    Poll::Ready(Ok(res))
                }
            }
            Err(err) => {
                // 检查是否为 keep-alive 超时错误
                ping.ensure_not_timed_out().map_err(|e| (e, None))?;

                debug!("client response error: {}", err);
                Poll::Ready(Err((crate::Error::new_h2(err), None::<Request<B>>)))
            }
        }
    }
}

/// ClientTask 的 Future 实现
///
/// 这是客户端 HTTP/2 连接的主要驱动循环。它不断执行以下步骤：
/// 1. 等待 h2 就绪（有容量发送新请求）
/// 2. 从请求通道接收下一个请求
/// 3. 处理请求：移除连接头部、设置 Content-Length、发送请求头
/// 4. 启动请求体发送和响应接收的后台任务
/// 5. 监控连接关闭信号
impl<B, E, T> Future for ClientTask<B, E, T>
where
    B: Body + 'static + Unpin,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    E: Http2ClientConnExec<B, T> + Clone + Unpin,
    T: Read + Write + Unpin,
{
    type Output = crate::Result<Dispatched>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            // 步骤 1：等待 h2 就绪
            match ready!(self.h2_tx.poll_ready(cx)) {
                Ok(()) => (),
                Err(err) => {
                    // h2 就绪轮询失败，检查是否为 keep-alive 超时
                    self.ping.ensure_not_timed_out()?;
                    return if err.reason() == Some(::h2::Reason::NO_ERROR) {
                        // NO_ERROR 表示正常关闭
                        trace!("connection gracefully shutdown");
                        Poll::Ready(Ok(Dispatched::Shutdown))
                    } else {
                        Poll::Ready(Err(crate::Error::new_h2(err)))
                    };
                }
            };

            // If we were waiting on pending open
            // continue where we left off.
            // 步骤 2：如果有暂存的请求上下文（之前因 pending open 而暂停），继续处理
            if let Some(f) = self.fut_ctx.take() {
                self.poll_pipe(f, cx);
                continue;
            }

            // 步骤 3：从通道接收下一个请求
            match self.req_rx.poll_recv(cx) {
                Poll::Ready(Some((req, cb))) => {
                    // check that future hasn't been canceled already
                    // 检查调用者是否已取消请求
                    if cb.is_canceled() {
                        trace!("request callback is canceled");
                        continue;
                    }
                    // 分离请求头和请求体
                    let (head, body) = req.into_parts();
                    let mut req = ::http::Request::from_parts(head, ());
                    // 移除 HTTP/2 中不允许的连接级头部
                    super::strip_connection_headers(req.headers_mut(), true);
                    // 如果 Body 有确切大小，自动设置 Content-Length 头部
                    if let Some(len) = body.size_hint().exact() {
                        if len != 0 || headers::method_has_defined_payload_semantics(req.method()) {
                            headers::set_content_length_if_missing(req.headers_mut(), len);
                        }
                    }

                    let is_connect = req.method() == Method::CONNECT;
                    let eos = body.is_end_stream();

                    // CONNECT 请求不支持非零 Body
                    if is_connect
                        && headers::content_length_parse_all(req.headers())
                            .map_or(false, |len| len != 0)
                    {
                        debug!("h2 connect request with non-zero body not supported");
                        cb.send(Err(TrySendError {
                            error: crate::Error::new_user_invalid_connect(),
                            message: None,
                        }));
                        continue;
                    }

                    // 处理扩展 CONNECT 协议
                    if let Some(protocol) = req.extensions_mut().remove::<Protocol>() {
                        req.extensions_mut().insert(protocol.into_inner());
                    }

                    // 通过 h2 发送请求头
                    let (fut, body_tx) = match self.h2_tx.send_request(req, !is_connect && eos) {
                        Ok(ok) => ok,
                        Err(err) => {
                            debug!("client send request error: {}", err);
                            cb.send(Err(TrySendError {
                                error: crate::Error::new_h2(err),
                                message: None,
                            }));
                            continue;
                        }
                    };

                    // 创建请求上下文
                    let f = FutCtx {
                        is_connect,
                        eos,
                        fut,
                        body_tx,
                        body,
                        cb,
                    };

                    // Check poll_ready() again.
                    // If the call to send_request() resulted in the new stream being pending open
                    // we have to wait for the open to complete before accepting new requests.
                    // 再次检查 h2 就绪状态。如果新流处于 "pending open" 状态，
                    // 需要等待流就绪后才能接受新请求。
                    match self.h2_tx.poll_ready(cx) {
                        Poll::Pending => {
                            // Save Context 暂存请求上下文，等待下次轮询
                            self.fut_ctx = Some(f);
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(())) => (),
                        Poll::Ready(Err(err)) => {
                            f.cb.send(Err(TrySendError {
                                error: crate::Error::new_h2(err),
                                message: None,
                            }));
                            continue;
                        }
                    }
                    // 流已就绪，处理请求体发送和响应等待
                    self.poll_pipe(f, cx);
                    continue;
                }

                Poll::Ready(None) => {
                    // 请求通道已关闭（所有 Sender 被 drop），开始关闭
                    trace!("client::dispatch::Sender dropped");
                    return Poll::Ready(Ok(Dispatched::Shutdown));
                }

                Poll::Pending => match ready!(Pin::new(&mut self.conn_eof).poll(cx)) {
                    // As of Rust 1.82, this pattern is no longer needed, and emits a warning.
                    // But we cannot remove it as long as MSRV is less than that.
                    // Infallible 类型的匹配，在 Rust 1.82+ 中不再需要此 pattern
                    #[allow(unused)]
                    Ok(never) => match never {},
                    Err(_conn_is_eof) => {
                        // 连接任务已结束，关闭调度任务
                        trace!("connection task is closed, closing dispatch task");
                        return Poll::Ready(Ok(Dispatched::Shutdown));
                    }
                },
            }
        }
    }
}
