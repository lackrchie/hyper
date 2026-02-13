//! HTTP/1 客户端连接模块
//!
//! 本模块实现了 HTTP/1.1 协议的客户端连接 API，是 hyper 客户端最核心的模块之一。
//! 它提供了三个主要类型：
//!
//! - [`SendRequest`] — 请求发送端，用于向已建立的连接发送 HTTP 请求
//! - [`Connection`] — 连接状态机 Future，负责驱动 HTTP/1 协议的读写
//! - [`Builder`] — 连接配置构建器，用于自定义连接参数后执行握手
//!
//! ## 使用流程
//!
//! 1. 准备一个实现了 `Read + Write` 的 IO 对象（如 TCP 连接）
//! 2. 调用 `handshake(io)` 或 `Builder::new().handshake(io)` 执行握手
//! 3. 获得 `(SendRequest, Connection)` 对
//! 4. 将 `Connection` spawn 到异步运行时中驱动协议状态机
//! 5. 通过 `SendRequest` 发送请求并等待响应
//!
//! ## 与 dispatch 模块的关系
//!
//! `SendRequest` 内部持有 `dispatch::Sender`，`Connection` 内部的 `Dispatcher` 持有
//! `dispatch::Receiver`。用户发送的请求通过 dispatch 通道传递给连接状态机处理。

// 标准库错误 trait，用于泛型约束中的错误类型边界
use std::error::Error as StdError;
// 格式化输出 trait，用于 Debug 实现
use std::fmt;
// Future trait，用于 Connection 的异步实现
use std::future::Future;
// Pin 类型，用于固定 Future 在内存中的位置（自引用安全）
use std::pin::Pin;
// 异步任务的上下文和轮询结果类型
use std::task::{Context, Poll};

// hyper 自定义的异步 Read/Write trait（非 tokio 的版本），提供更灵活的 IO 抽象
use crate::rt::{Read, Write};
// bytes crate 的 Bytes 类型，用于零拷贝的字节缓冲区
use bytes::Bytes;
// futures_core 的 ready! 宏，简化 Poll::Ready 的模式匹配
use futures_core::ready;
// http crate 的 HTTP 请求和响应类型
use http::{Request, Response};
// httparse crate 的解析器配置，用于自定义 HTTP/1 解析行为
use httparse::ParserConfig;

// dispatch 模块：客户端内部的请求调度通道
// `super::super::dispatch` 即 `client::dispatch`
use super::super::dispatch::{self, TrySendError};
// hyper 的 Body trait 和 Incoming body 类型
use crate::body::{Body, Incoming as IncomingBody};
// hyper 的协议实现模块
use crate::proto;

/// Dispatcher 类型别名，简化复杂泛型类型的书写。
///
/// 这是 HTTP/1 客户端 Dispatcher 的完整类型：
/// - `proto::dispatch::Client<B>` — 客户端分发器（持有 dispatch::Receiver）
/// - `B` — 请求体类型
/// - `T` — IO 传输层类型
/// - `proto::h1::ClientTransaction` — HTTP/1 客户端事务处理器
type Dispatcher<T, B> =
    proto::dispatch::Dispatcher<proto::dispatch::Client<B>, B, T, proto::h1::ClientTransaction>;

/// HTTP/1 连接的请求发送端。
///
/// 通过 `handshake()` 建立连接后获得此类型。用户通过它向服务器发送 HTTP 请求。
/// 内部持有一个 `dispatch::Sender`，请求通过它传递给连接状态机。
///
/// 对于 HTTP/1，同一时间只能有一个请求在途（非管线化模式），
/// 因此在发送下一个请求前需要等待前一个请求完成。
pub struct SendRequest<B> {
    dispatch: dispatch::Sender<Request<B>, Response<IncomingBody>>,
}

/// `Connection` 被解构后的组成部分。
///
/// 允许在稍后的时间点拆解 `Connection`，以回收 IO 对象及其他相关部件。
/// 典型用途是在 HTTP 升级（upgrade）场景中获取底层 IO 对象。
///
/// `#[non_exhaustive]` 属性表示将来可能会添加更多字段，
/// 防止用户代码使用结构体字面量语法构造此类型。
#[derive(Debug)]
#[non_exhaustive]
pub struct Parts<T> {
    /// 握手时使用的原始 IO 对象。
    pub io: T,
    /// 已读取但未作为 HTTP 处理的字节缓冲区。
    ///
    /// 例如，如果 `Connection` 用于 HTTP 升级请求，服务器可能在升级响应中
    /// 同时发送了新协议的首批数据。如果你计划继续在 IO 对象上通信，
    /// 应该检查此缓冲区中是否有数据。
    pub read_buf: Bytes,
}

/// 处理 IO 对象上所有 HTTP 状态的 Future。
///
/// 在大多数情况下，这应该被 spawn 到执行器中运行，以便它能够
/// 处理传入和传出的消息、检测连接断开等。
///
/// 此类型的实例通常通过 [`handshake`] 函数创建。
///
/// `#[must_use]` 属性确保用户不会忘记 poll 这个 Future——
/// 如果 Connection 没有被驱动，SendRequest 将无法工作。
#[must_use = "futures do nothing unless polled"]
pub struct Connection<T, B>
where
    T: Read + Write,
    B: Body + 'static,
{
    /// 内部的 HTTP/1 Dispatcher，驱动整个协议状态机
    inner: Dispatcher<T, B>,
}

/// `Connection` 的核心方法实现
impl<T, B> Connection<T, B>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    /// 返回内部 IO 对象及额外信息。
    ///
    /// 仅适用于 HTTP/1 连接。调用后 Connection 被消费。
    /// 返回的 `Parts` 包含原始 IO 对象和未处理的读缓冲区。
    pub fn into_parts(self) -> Parts<T> {
        // 从 Dispatcher 中提取 IO 对象和读缓冲区，第三个值（_）被丢弃
        let (io, read_buf, _) = self.inner.into_inner();
        Parts { io, read_buf }
    }

    /// 轮询连接是否完成，但不调用底层 IO 的 `shutdown`。
    ///
    /// 这在 HTTP 升级场景中很有用：升级完成后，连接状态为"完成"，
    /// 但我们不希望关闭 IO 对象，因为它将被新协议继续使用。
    /// 之后可以调用 `into_parts` 取回 IO 对象。
    ///
    /// 可配合 [`poll_fn`](https://docs.rs/futures/0.1.25/futures/future/fn.poll_fn.html)
    /// 和 [`try_ready!`](https://docs.rs/futures/0.1.25/futures/macro.try_ready.html) 使用；
    /// 或使用 `without_shutdown` 封装。
    pub fn poll_without_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        self.inner.poll_without_shutdown(cx)
    }

    /// 阻止在请求处理结束时关闭底层 IO 对象，转而调用 `into_parts`。
    ///
    /// 这是 `poll_without_shutdown` 的便捷异步封装。
    /// 等待连接完成处理后，自动返回解构后的 Parts。
    pub async fn without_shutdown(self) -> crate::Result<Parts<T>> {
        // 将 self 移入 Option 中，以便在闭包内可变借用和最终消费
        let mut conn = Some(self);
        crate::common::future::poll_fn(move |cx| -> Poll<crate::Result<Parts<T>>> {
            // 使用 ready! 宏等待 poll_without_shutdown 完成
            ready!(conn.as_mut().unwrap().poll_without_shutdown(cx))?;
            // 取出 Connection 并解构为 Parts
            Poll::Ready(Ok(conn.take().unwrap().into_parts()))
        })
        .await
    }
}

/// HTTP/1 连接配置构建器。
///
/// 通过设置各种选项后，调用 `handshake` 方法创建连接。
/// 采用 Builder 模式，每个配置方法返回 `&mut Self` 以支持链式调用。
///
/// **注意**：选项的默认值不被视为稳定 API，可能在任何时候发生变化。
#[derive(Clone, Debug)]
pub struct Builder {
    /// 是否容忍 HTTP/0.9 响应（无状态行的原始响应）
    h09_responses: bool,
    /// HTTP/1 解析器配置（来自 httparse crate），控制解析行为的各种容错选项
    h1_parser_config: ParserConfig,
    /// 是否强制使用向量化写入（writev）。None 表示自动检测。
    h1_writev: Option<bool>,
    /// 是否以 Title-Case 形式写入 header 名称（如 "Content-Type" 而非 "content-type"）
    h1_title_case_headers: bool,
    /// 是否保留 header 名称的原始大小写
    h1_preserve_header_case: bool,
    /// 最大 header 数量限制
    h1_max_headers: Option<usize>,
    /// 是否保留 header 的原始顺序（仅 FFI feature 启用时可用）
    #[cfg(feature = "ffi")]
    h1_preserve_header_order: bool,
    /// 精确的读缓冲区大小。设置后禁用自适应缓冲区。
    h1_read_buf_exact_size: Option<usize>,
    /// 最大缓冲区大小限制
    h1_max_buf_size: Option<usize>,
}

/// 在 IO 对象上执行 HTTP/1 握手的快捷函数。
///
/// 等同于 `Builder::new().handshake(io)`。
/// 有关更多信息，请参阅 [`client::conn`](crate::client::conn)。
///
/// # 类型参数
/// - `T`: IO 传输层类型，需实现 `Read + Write + Unpin`
/// - `B`: 请求体类型，需实现 `Body + 'static`
///
/// # 返回值
/// 成功时返回 `(SendRequest<B>, Connection<T, B>)` 对。
pub async fn handshake<T, B>(io: T) -> crate::Result<(SendRequest<B>, Connection<T, B>)>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    Builder::new().handshake(io).await
}

// ===== impl SendRequest

/// `SendRequest` 的基础方法实现
impl<B> SendRequest<B> {
    /// 异步轮询发送端是否准备好发送请求。
    ///
    /// 如果关联的连接已关闭，返回 Error。
    /// 内部委托给 dispatch::Sender 的 poll_ready 方法。
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        self.dispatch.poll_ready(cx)
    }

    /// 等待 dispatcher 就绪。
    ///
    /// 如果关联的连接已关闭，返回 Error。
    /// 这是 `poll_ready` 的 async 封装，使用 `poll_fn` 将 poll 风格转为 async。
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
    /// # Uri 格式
    ///
    /// 请求的 `Uri` 将按原样序列化：
    /// - 通常你需要使用 origin-form（`/path?query`）
    /// - 如果是发送到 HTTP 代理，则需要 absolute-form（`https://hyper.rs/guides`）
    ///
    /// hyper 不会强制或验证 Uri 格式，由用户自行确保正确性。
    pub fn send_request(
        &mut self,
        req: Request<B>,
    ) -> impl Future<Output = crate::Result<Response<IncomingBody>>> {
        // 通过 dispatch 通道发送请求（不可重试模式）
        let sent = self.dispatch.send(req);

        async move {
            match sent {
                Ok(rx) => match rx.await {
                    // 成功收到响应
                    Ok(Ok(resp)) => Ok(resp),
                    // 连接处理过程中出错
                    Ok(Err(err)) => Err(err),
                    // oneshot 通道被丢弃而未发送响应——这是一个内部 bug
                    // this is definite bug if it happens, but it shouldn't happen!
                    Err(_canceled) => panic!("dispatch dropped without returning error"),
                },
                Err(_req) => {
                    // dispatch 通道不允许发送（连接未就绪）
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
    /// 原始请求消息会作为 `TrySendError` 的一部分返回，允许调用者重试。
    pub fn try_send_request(
        &mut self,
        req: Request<B>,
    ) -> impl Future<Output = Result<Response<IncomingBody>, TrySendError<Request<B>>>> {
        // 通过 dispatch 通道发送请求（可重试模式）
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
                    // 将原始请求一起返回，允许调用者重试
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
///
/// 出于安全和简洁考虑，不显示内部状态细节。
impl<B> fmt::Debug for SendRequest<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendRequest").finish()
    }
}

// ===== impl Connection

/// `Connection` 的扩展功能实现（需要 Send 约束）
impl<T, B> Connection<T, B>
where
    T: Read + Write + Unpin + Send,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    /// 启用此连接的 HTTP 升级支持。
    ///
    /// 返回一个 `UpgradeableConnection`，它在检测到升级响应时
    /// 会自动完成升级过程（将 IO 对象传递给升级处理器）。
    ///
    /// 更多信息请参阅 [upgrade 模块](crate::upgrade)。
    pub fn with_upgrades(self) -> upgrades::UpgradeableConnection<T, B> {
        upgrades::UpgradeableConnection { inner: Some(self) }
    }
}

/// 为 `Connection` 实现 `Debug` trait。
impl<T, B> fmt::Debug for Connection<T, B>
where
    T: Read + Write + fmt::Debug,
    B: Body + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

/// 为 `Connection` 实现 `Future` trait。
///
/// 当 Connection 被轮询时，它驱动内部的 HTTP/1 Dispatcher 处理协议状态。
/// 完成时表示连接已正常关闭或需要升级。
///
/// 如果返回 `Dispatched::Upgrade`，说明收到了升级响应，但由于此 Connection
/// 不支持升级（没有 Send 约束），只能调用 `pending.manual()` 让用户手动处理。
/// 要支持自动升级，应使用 `with_upgrades()` 方法。
impl<T, B> Future for Connection<T, B>
where
    T: Read + Write + Unpin,
    B: Body + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 轮询内部 Dispatcher，使用 ready! 宏等待完成，? 传播错误
        match ready!(Pin::new(&mut self.inner).poll(cx))? {
            // 正常关闭
            proto::Dispatched::Shutdown => Poll::Ready(Ok(())),
            // 收到升级响应，但此 Connection 类型不支持自动升级
            proto::Dispatched::Upgrade(pending) => {
                // With no `Send` bound on `I`, we can't try to do
                // upgrades here. In case a user was trying to use
                // `upgrade` with this API, send a special
                // error letting them know about that.
                // 通知用户需要手动处理升级
                pending.manual();
                Poll::Ready(Ok(()))
            }
        }
    }
}

// ===== impl Builder

/// `Builder` 的方法实现
impl Builder {
    /// 创建新的连接配置构建器，所有选项使用默认值。
    #[inline]
    pub fn new() -> Builder {
        Builder {
            h09_responses: false,
            h1_writev: None,
            h1_read_buf_exact_size: None,
            h1_parser_config: Default::default(),
            h1_title_case_headers: false,
            h1_preserve_header_case: false,
            h1_max_headers: None,
            #[cfg(feature = "ffi")]
            h1_preserve_header_order: false,
            h1_max_buf_size: None,
        }
    }

    /// 设置是否容忍 HTTP/0.9 响应。
    ///
    /// HTTP/0.9 响应没有状态行和 header，直接返回响应体。
    /// 默认为 false。
    pub fn http09_responses(&mut self, enabled: bool) -> &mut Builder {
        self.h09_responses = enabled;
        self
    }

    /// 设置 HTTP/1 连接是否接受响应中 header 名称和冒号之间的空格。
    ///
    /// 一般不需要启用此选项。根据 [RFC 7230 Section 3.2.4.] 的规定：
    ///
    /// > header 字段名和冒号之间不允许有空格。过去对此类空格的不同处理
    /// > 导致了请求路由和响应处理中的安全漏洞。服务器必须拒绝包含此类空格的请求，
    /// > 代理必须在转发前移除响应中的此类空格。
    ///
    /// 默认为 false。
    ///
    /// [RFC 7230 Section 3.2.4.]: https://tools.ietf.org/html/rfc7230#section-3.2.4
    pub fn allow_spaces_after_header_name_in_responses(&mut self, enabled: bool) -> &mut Builder {
        self.h1_parser_config
            .allow_spaces_after_header_name_in_responses(enabled);
        self
    }

    /// 设置 HTTP/1 连接是否接受响应中过时的多行 header 折叠。
    ///
    /// 启用后，解析时会将换行符（`\r` 和 `\n`）转换为空格。
    ///
    /// 一般不需要启用此选项。根据 [RFC 7230 Section 3.2.4.] 的规定，
    /// 用户代理接收到此类折叠应将其替换为空格后再解释字段值。
    ///
    /// 默认为 false。
    ///
    /// [RFC 7230 Section 3.2.4.]: https://tools.ietf.org/html/rfc7230#section-3.2.4
    pub fn allow_obsolete_multiline_headers_in_responses(&mut self, enabled: bool) -> &mut Builder {
        self.h1_parser_config
            .allow_obsolete_multiline_headers_in_responses(enabled);
        self
    }

    /// 设置 HTTP/1 连接是否静默忽略格式错误的 header 行。
    ///
    /// 启用后，如果某行不以有效的 header 名称开头，或完全不包含冒号，
    /// 该行将被静默忽略，不会报告错误。
    ///
    /// 默认为 false。
    pub fn ignore_invalid_headers_in_responses(&mut self, enabled: bool) -> &mut Builder {
        self.h1_parser_config
            .ignore_invalid_headers_in_responses(enabled);
        self
    }

    /// 设置 HTTP/1 连接是否尝试使用向量化写入（writev），
    /// 或始终将数据合并到单个缓冲区。
    ///
    /// 设置为 false 可能意味着更多的 body 数据拷贝，但在 IO 传输层
    /// 不支持向量化写入时（如大多数 TLS 实现）可能反而提高性能。
    ///
    /// 设置为 true 将强制 hyper 使用队列策略（queued strategy），
    /// 可能消除某些 TLS 后端上的不必要克隆。
    ///
    /// 默认为 `auto`（自动检测最佳模式）。
    pub fn writev(&mut self, enabled: bool) -> &mut Builder {
        self.h1_writev = Some(enabled);
        self
    }

    /// 设置 HTTP/1 连接是否在 socket 层面以 Title Case 格式写入 header 名称。
    ///
    /// 例如 "content-type" 会被写为 "Content-Type"。
    /// 默认为 false。
    pub fn title_case_headers(&mut self, enabled: bool) -> &mut Builder {
        self.h1_title_case_headers = enabled;
        self
    }

    /// 设置是否保留 header 名称的原始大小写。
    ///
    /// 目前会记录接收到的原始大小写，并存储在 `Response` 的私有扩展中。
    /// 也会在发送 `Request` 时查找并使用此扩展。
    ///
    /// 由于相关扩展仍然是私有的，目前无法直接与之交互。
    /// 此功能主要的效果是在代理场景中转发原始大小写。
    ///
    /// 默认为 false。
    pub fn preserve_header_case(&mut self, enabled: bool) -> &mut Builder {
        self.h1_preserve_header_case = enabled;
        self
    }

    /// 设置最大 header 数量。
    ///
    /// 收到响应时，解析器会预分配缓冲区以存储 header，以获得最佳性能。
    ///
    /// 如果客户端收到的 header 数量超过缓冲区大小，将返回 "message header too large" 错误。
    ///
    /// 注意，默认情况下 header 分配在栈上，性能更好。设置此值后，
    /// header 将在堆上分配，每个响应都会触发堆分配，性能下降约 5%。
    ///
    /// 默认为 100。
    pub fn max_headers(&mut self, val: usize) -> &mut Self {
        self.h1_max_headers = Some(val);
        self
    }

    /// 设置是否保留 header 的原始接收顺序。
    ///
    /// 目前会记录 header 的接收顺序，并存储在 `Response` 的私有扩展中。
    /// 也会在发送 `Request` 时查找并使用此扩展。
    ///
    /// 默认为 false。
    #[cfg(feature = "ffi")]
    pub fn preserve_header_order(&mut self, enabled: bool) -> &mut Builder {
        self.h1_preserve_header_order = enabled;
        self
    }

    /// 设置读缓冲区的精确大小。
    ///
    /// 注意，设置此选项会取消 `max_buf_size` 选项。
    /// 默认使用自适应读缓冲区。
    pub fn read_buf_exact_size(&mut self, sz: Option<usize>) -> &mut Builder {
        self.h1_read_buf_exact_size = sz;
        // 互斥：精确大小和最大大小不能同时设置
        self.h1_max_buf_size = None;
        self
    }

    /// 设置连接的最大缓冲区大小。
    ///
    /// 默认约 400KB。
    ///
    /// 注意，设置此选项会取消 `read_exact_buf_size` 选项。
    ///
    /// # Panics
    ///
    /// 允许的最小值为 8192。如果传入的 `max` 小于此最小值，方法会 panic。
    pub fn max_buf_size(&mut self, max: usize) -> &mut Self {
        assert!(
            max >= proto::h1::MINIMUM_MAX_BUFFER_SIZE,
            "the max_buf_size cannot be smaller than the minimum that h1 specifies."
        );

        self.h1_max_buf_size = Some(max);
        // 互斥：最大大小和精确大小不能同时设置
        self.h1_read_buf_exact_size = None;
        self
    }

    /// 使用已配置的选项在 IO 对象上构建连接。
    ///
    /// 有关更多信息，请参阅 [`client::conn`](crate::client::conn)。
    ///
    /// 注意，如果 [`Connection`] 未被 `await`，[`SendRequest`] 将不会工作。
    ///
    /// # 返回值
    /// 返回一个 Future，成功时产生 `(SendRequest<B>, Connection<T, B>)` 对。
    pub fn handshake<T, B>(
        &self,
        io: T,
    ) -> impl Future<Output = crate::Result<(SendRequest<B>, Connection<T, B>)>>
    where
        T: Read + Write + Unpin,
        B: Body + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        // 克隆配置，以便在 async 块中使用（避免借用 self 的生命周期问题）
        let opts = self.clone();

        async move {
            trace!("client handshake HTTP/1");

            // 创建 dispatch 通道：tx 给 SendRequest，rx 给 Connection 的 Dispatcher
            let (tx, rx) = dispatch::channel();
            // 创建底层协议连接对象
            let mut conn = proto::Conn::new(io);
            // 应用解析器配置
            conn.set_h1_parser_config(opts.h1_parser_config);
            // 根据配置设置写入策略
            if let Some(writev) = opts.h1_writev {
                if writev {
                    conn.set_write_strategy_queue(); // 队列策略（向量化写入）
                } else {
                    conn.set_write_strategy_flatten(); // 扁平化策略（合并到单缓冲区）
                }
            }
            // 应用 Title Case header 设置
            if opts.h1_title_case_headers {
                conn.set_title_case_headers();
            }
            // 应用保留 header 大小写设置
            if opts.h1_preserve_header_case {
                conn.set_preserve_header_case();
            }
            // 应用最大 header 数量限制
            if let Some(max_headers) = opts.h1_max_headers {
                conn.set_http1_max_headers(max_headers);
            }
            // 应用保留 header 顺序设置（仅 FFI）
            #[cfg(feature = "ffi")]
            if opts.h1_preserve_header_order {
                conn.set_preserve_header_order();
            }

            // 应用 HTTP/0.9 响应容忍设置
            if opts.h09_responses {
                conn.set_h09_responses();
            }

            // 应用精确读缓冲区大小
            if let Some(sz) = opts.h1_read_buf_exact_size {
                conn.set_read_buf_exact_size(sz);
            }
            // 应用最大缓冲区大小
            if let Some(max) = opts.h1_max_buf_size {
                conn.set_max_buf_size(max);
            }
            // 创建 HTTP/1 客户端 dispatch 处理器，绑定接收端
            let cd = proto::h1::dispatch::Client::new(rx);
            // 创建 HTTP/1 Dispatcher，组合客户端处理器和协议连接
            let proto = proto::h1::Dispatcher::new(cd, conn);

            // 返回 SendRequest（持有发送端）和 Connection（持有 Dispatcher）
            Ok((SendRequest { dispatch: tx }, Connection { inner: proto }))
        }
    }
}

/// HTTP 升级支持子模块
///
/// 包含 `UpgradeableConnection`，它是 `Connection` 的包装，
/// 添加了自动处理 HTTP 升级的能力。
mod upgrades {
    // Upgraded 类型：代表一个已完成 HTTP 升级的 IO 对象
    use crate::upgrade::Upgraded;

    // 导入父模块中的所有公共项
    use super::*;

    /// 支持 HTTP 升级的连接 Future。
    ///
    /// 与普通的 `Connection` 不同，当检测到升级响应时，
    /// 此类型会自动将底层 IO 对象包装为 `Upgraded` 并传递给升级处理器。
    ///
    /// 此类型在 crate 外部不可命名（unnameable），只能通过
    /// `Connection::with_upgrades()` 方法获得。
    // A future binding a connection with a Service with Upgrade support.
    //
    // This type is unnameable outside the crate.
    #[must_use = "futures do nothing unless polled"]
    #[allow(missing_debug_implementations)]
    pub struct UpgradeableConnection<T, B>
    where
        T: Read + Write + Unpin + Send + 'static,
        B: Body + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        /// 使用 Option 包装 Connection，以便在升级时能够 take() 消费它
        pub(super) inner: Option<Connection<T, B>>,
    }

    /// 为 `UpgradeableConnection` 实现 `Future` trait。
    ///
    /// 工作方式：
    /// 1. 轮询内部 Connection 的 Dispatcher
    /// 2. 如果正常关闭（Shutdown），直接完成
    /// 3. 如果收到升级信号（Upgrade），取出 IO 对象和读缓冲区，
    ///    包装为 `Upgraded` 并完成升级流程
    /// 4. 如果出错，传播错误
    impl<I, B> Future for UpgradeableConnection<I, B>
    where
        I: Read + Write + Unpin + Send + 'static,
        B: Body + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        type Output = crate::Result<()>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            // 轮询内部 Connection 的 Dispatcher
            match ready!(Pin::new(&mut self.inner.as_mut().unwrap().inner).poll(cx)) {
                // 正常关闭
                Ok(proto::Dispatched::Shutdown) => Poll::Ready(Ok(())),
                // 收到升级信号
                Ok(proto::Dispatched::Upgrade(pending)) => {
                    // 取出 Connection 并解构为 Parts，获取 IO 对象和读缓冲区
                    let Parts { io, read_buf } = self.inner.take().unwrap().into_parts();
                    // 使用 IO 对象和未消费的读缓冲区创建 Upgraded，完成升级
                    pending.fulfill(Upgraded::new(io, read_buf));
                    Poll::Ready(Ok(()))
                }
                // 传播错误
                Err(e) => Poll::Ready(Err(e)),
            }
        }
    }
}
