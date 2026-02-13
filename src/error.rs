//! hyper 错误与结果类型模块
//!
//! 本模块定义了 hyper 的核心错误类型 [`Error`] 及其相关的辅助类型。
//! hyper 中所有可能发生的错误——包括 HTTP 解析错误、用户代码错误、I/O 错误、
//! 超时、协议错误等——都被统一封装在 `Error` 类型中。
//!
//! ## 在 hyper 中的角色
//!
//! `Error` 是 hyper 对外暴露的唯一错误类型（通过 `lib.rs` 中的 `pub use` 导出），
//! 用户代码通过 `Error` 上的各种 `is_*` 方法来判断错误类别。内部的错误分类
//! （`Kind`、`Parse`、`User` 等枚举）是 `pub(super)` 可见性，仅供 hyper 内部使用。
//!
//! ## 设计理念
//!
//! - **不透明错误**：`Error` 使用 `Box<ErrorImpl>` 包装内部实现，既减小了栈上的大小
//!   （仅一个指针宽度），又隐藏了内部结构细节。
//! - **错误链**：通过 `std::error::Error::source()` 支持错误链，方便调试。
//! - **条件编译**：大量使用 `#[cfg(...)]` 来确保只有在相关 feature 启用时才编译
//!   对应的错误变体，减少不必要的代码。

// 引入标准库的 Error trait，在本模块中重命名为 StdError 以避免与 hyper 自身的 Error 类型冲突
use std::error::Error as StdError;
// 引入格式化 trait，用于实现 Debug 和 Display
use std::fmt;

/// hyper 方法常用的 Result 类型别名。
///
/// 这是对 `std::result::Result<T, hyper::Error>` 的简写，
/// 使得 hyper 内部和用户代码中可以直接写 `Result<T>` 而无需每次指定错误类型。
pub type Result<T> = std::result::Result<T, Error>;

/// 错误原因的类型别名。
///
/// 使用 `Box<dyn StdError + Send + Sync>` 作为类型擦除的错误原因，
/// 允许存储任意实现了 `Error + Send + Sync` 的错误类型。
/// `Send + Sync` 约束确保错误可以安全地跨线程传递，这在异步编程中至关重要。
type Cause = Box<dyn StdError + Send + Sync>;

/// 表示处理 HTTP 流时可能发生的错误。
///
/// # 格式化
///
/// 此类型的 `Display` 实现只会打印当前层级的错误详情，
/// 即使它可能是由另一个错误引起的，且包含该错误作为 source。
/// 要打印所有相关信息（包括 source 链），请使用
/// `std::error::Report` 或等效的第三方类型。
///
/// 此 `Error` 类型的格式化错误消息的具体内容是未指定的。
/// **你不能依赖它。** 措辞和细节可能在任何版本中更改，
/// 目的是改善错误消息。
///
/// # Source
///
/// `hyper::Error` 可能由另一个错误引起。为了辅助调试，
/// 这些错误通过 `Error::source()` 以类型擦除的方式暴露。
/// 虽然可以检查 source 的确切类型，但**不能依赖它们**。
/// 它们可能来自私有的内部依赖，且随时可能更改。
pub struct Error {
    // 使用 Box 进行堆分配，使 Error 的栈大小仅为一个指针宽度（通常 8 字节）。
    // 这对于 Result<T, Error> 的大小优化非常重要，因为 Result 的大小取决于
    // 较大的那个变体。
    inner: Box<ErrorImpl>,
}

/// 错误的内部实现结构体。
///
/// 包含错误的分类（kind）和可选的原因链（cause）。
/// 这是一个私有类型，不对外暴露。
struct ErrorImpl {
    /// 错误的分类，决定了此错误属于哪种类型
    kind: Kind,
    /// 可选的底层错误原因，用于构建错误链
    cause: Option<Cause>,
}

/// 错误分类枚举。
///
/// 这是 hyper 内部使用的错误分类体系，通过 `pub(super)` 可见性限制在 crate 内使用。
/// 每个变体代表一类特定的错误场景，许多变体通过 `#[cfg]` 进行条件编译，
/// 只在相关 feature 启用时才存在。
#[derive(Debug)]
pub(super) enum Kind {
    /// HTTP 解析错误，包含具体的解析错误子类型
    Parse(Parse),
    /// 用户代码引起的错误，包含具体的用户错误子类型
    User(User),
    /// 消息在完成之前遇到了 EOF（连接关闭）。
    /// 仅在 HTTP/1 + (client 或 server) 时编译。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    IncompleteMessage,
    /// 连接在非预期时刻收到了消息（或字节）。
    /// 仅在 HTTP/1 + (client 或 server) 时编译。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    UnexpectedMessage,
    /// 一个待处理的项在被处理之前就被丢弃了。
    /// 例如，一个请求在发送之前，其对应的 future 被 drop。
    Canceled,
    /// 表示一个通道（客户端或 body 发送端）已关闭。
    /// 在 HTTP/1 的 client/server 模式以及 HTTP/2 的 client 模式下可用。
    #[cfg(any(
        all(feature = "http1", any(feature = "client", feature = "server")),
        all(feature = "http2", feature = "client")
    ))]
    ChannelClosed,
    /// 在尝试读写网络流时发生的 `io::Error`。
    /// 仅在启用了至少一种协议版本和至少一种角色时编译。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    Io,
    /// 用户发送请求头超时。
    /// 仅在 HTTP/1 server 模式下可用。
    #[cfg(all(feature = "http1", feature = "server"))]
    HeaderTimeout,
    /// 从连接读取 body 时发生的错误。
    /// 在启用了协议版本和角色时可用。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    Body,
    /// 向连接写入 body 时发生的错误。
    /// 在启用了协议版本和角色时可用。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    BodyWrite,
    /// 调用 `AsyncWrite::shutdown()` 时发生的错误。
    /// 仅在 HTTP/1 + (client 或 server) 时编译。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    Shutdown,

    /// 来自 h2 库的通用错误。
    /// 仅在 HTTP/2 + (client 或 server) 时编译。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    Http2,
}

/// HTTP 解析错误的子分类枚举。
///
/// 涵盖了 HTTP 消息解析过程中可能遇到的各种错误，
/// 如无效的方法、版本号、URI、头部等。
#[derive(Debug)]
pub(super) enum Parse {
    /// 解析到无效的 HTTP 方法
    Method,
    /// 解析到无效的 HTTP 版本号（仅 HTTP/1）
    #[cfg(feature = "http1")]
    Version,
    /// 在 HTTP/1 解析中检测到 HTTP/2 前导帧（preface），表示协议版本不匹配
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    VersionH2,
    /// 解析到无效的 URI
    Uri,
    /// URI 过长（仅 HTTP/1 server 模式）
    #[cfg(all(feature = "http1", feature = "server"))]
    UriTooLong,
    /// HTTP 头部解析错误，包含具体的头部错误子类型（仅 HTTP/1）
    #[cfg(feature = "http1")]
    Header(Header),
    /// 消息头部过大。
    /// 在 HTTP/1 和 HTTP/2 中都可能出现，但在 HTTP/2 中允许未使用（因为 h2 库自行处理）。
    #[cfg(any(feature = "http1", feature = "http2"))]
    #[cfg_attr(feature = "http2", allow(unused))]
    TooLarge,
    /// 解析到无效的 HTTP 状态码
    Status,
    /// hyper 内部或其依赖的内部错误
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    Internal,
}

/// HTTP 头部解析错误的子分类枚举（仅 HTTP/1）。
///
/// 当 HTTP/1 的头部解析失败时，此枚举提供更具体的错误信息。
#[derive(Debug)]
#[cfg(feature = "http1")]
pub(super) enum Header {
    /// 头部包含无效的 token 字符
    Token,
    /// Content-Length 头部值无效（仅 client/server 模式）
    #[cfg(any(feature = "client", feature = "server"))]
    ContentLengthInvalid,
    /// Transfer-Encoding 头部值无效（仅 server 模式）
    #[cfg(feature = "server")]
    TransferEncodingInvalid,
    /// 出现了不期望的 Transfer-Encoding 头部（仅 client/server 模式）
    #[cfg(any(feature = "client", feature = "server"))]
    TransferEncodingUnexpected,
}

/// 用户代码引起的错误的子分类枚举。
///
/// 这些错误是由 hyper 的用户（调用者）代码的行为引起的，
/// 而非 HTTP 协议解析或网络 I/O 问题。
#[derive(Debug)]
pub(super) enum User {
    /// 调用用户的 `Body::poll_data()` 时出错。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    Body,
    /// 用户中止了出站 body 的写入。
    #[cfg(any(
        all(feature = "http1", any(feature = "client", feature = "server")),
        feature = "ffi"
    ))]
    BodyWriteAborted,
    /// 用户尝试发送带有非零 body 的 CONNECT 请求（仅 HTTP/2 client）
    #[cfg(all(feature = "client", feature = "http2"))]
    InvalidConnectWithBody,
    /// 用户 Service 的 future 返回了错误。
    #[cfg(any(
        all(any(feature = "client", feature = "server"), feature = "http1"),
        all(feature = "server", feature = "http2")
    ))]
    Service,
    /// 用户尝试在不期望的上下文中发送某个头部。
    ///
    /// 例如，同时发送 `content-length` 和 `transfer-encoding`。
    #[cfg(any(feature = "http1", feature = "http2"))]
    #[cfg(feature = "server")]
    UnexpectedHeader,
    /// 用户尝试响应 1xx（非 101）状态码。
    /// hyper 的 HTTP/1 server 不支持发送信息性状态码（100-199），
    /// 101 Switching Protocols 除外。
    #[cfg(feature = "http1")]
    #[cfg(feature = "server")]
    UnsupportedStatusCode,

    /// 用户轮询了一个不存在的 upgrade。
    NoUpgrade,

    /// 用户轮询了 upgrade，但底层 API 没有使用 upgrade 机制。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    ManualUpgrade,

    /// dispatch 任务已消失（被 drop）。
    /// 这通常意味着连接池中的连接已断开。
    #[cfg(all(feature = "client", any(feature = "http1", feature = "http2")))]
    DispatchGone,

    /// 用户在 FFI 回调中中止了操作。
    #[cfg(feature = "ffi")]
    AbortedByCallback,
}

/// 哨兵类型，用于标识错误是由超时引起的。
///
/// 这是一个标记类型（marker type），通过 `Error::find_source::<TimedOut>()`
/// 在错误链中查找来判断错误是否由超时导致。
// Sentinel type to indicate the error was caused by a timeout.
#[derive(Debug)]
pub(super) struct TimedOut;

/// `Error` 类型的主要方法实现。
///
/// 包括公共的 `is_*` 查询方法（用于判断错误类别）和
/// 内部的构造方法（`new_*` 系列，用于创建特定类型的错误）。
impl Error {
    /// 如果这是一个 HTTP 解析错误，返回 `true`。
    pub fn is_parse(&self) -> bool {
        matches!(self.inner.kind, Kind::Parse(_))
    }

    /// 如果这是一个由消息过大引起的 HTTP 解析错误，返回 `true`。
    /// 仅在 HTTP/1 server 模式下可用。
    #[cfg(all(feature = "http1", feature = "server"))]
    pub fn is_parse_too_large(&self) -> bool {
        matches!(
            self.inner.kind,
            Kind::Parse(Parse::TooLarge) | Kind::Parse(Parse::UriTooLong)
        )
    }

    /// 如果这是一个由无效响应状态码或原因短语引起的 HTTP 解析错误，返回 `true`。
    pub fn is_parse_status(&self) -> bool {
        matches!(self.inner.kind, Kind::Parse(Parse::Status))
    }

    /// 如果此错误由用户代码引起，返回 `true`。
    pub fn is_user(&self) -> bool {
        matches!(self.inner.kind, Kind::User(_))
    }

    /// 如果这是关于一个被取消的 `Request`，返回 `true`。
    pub fn is_canceled(&self) -> bool {
        matches!(self.inner.kind, Kind::Canceled)
    }

    /// 如果发送端的通道已关闭，返回 `true`。
    pub fn is_closed(&self) -> bool {
        // 当 ChannelClosed 变体不存在时（相关 feature 未启用），直接返回 false
        #[cfg(not(any(
            all(feature = "http1", any(feature = "client", feature = "server")),
            all(feature = "http2", feature = "client")
        )))]
        return false;

        // 当 ChannelClosed 变体存在时，检查是否匹配
        #[cfg(any(
            all(feature = "http1", any(feature = "client", feature = "server")),
            all(feature = "http2", feature = "client")
        ))]
        matches!(self.inner.kind, Kind::ChannelClosed)
    }

    /// 如果连接在消息完成之前就关闭了，返回 `true`。
    ///
    /// 这意味着提供的 IO 连接报告了 EOF（关闭），而 hyper 的 HTTP 状态
    /// 表明消息（请求或响应）还需要传输更多数据。
    ///
    /// 可能发生的一些场景（非穷举）：
    ///
    /// - 在连接上写入请求后，下一次 `read` 报告 EOF
    ///   （可能服务器关闭了一个"空闲"连接）。
    /// - 消息 body 只接收到一部分就遇到 EOF。
    /// - 客户端写入请求后关闭了写入端，同时等待响应。
    ///   如果需要支持这种场景，请考虑启用 [`half_close`]。
    ///
    /// [`half_close`]: crate::server::conn::http1::Builder::half_close()
    pub fn is_incomplete_message(&self) -> bool {
        // 与 is_closed 类似的条件编译模式：feature 不匹配时返回 false
        #[cfg(not(all(any(feature = "client", feature = "server"), feature = "http1")))]
        return false;

        #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
        matches!(self.inner.kind, Kind::IncompleteMessage)
    }

    /// 如果 body 写入被中止，返回 `true`。
    pub fn is_body_write_aborted(&self) -> bool {
        #[cfg(not(any(
            all(feature = "http1", any(feature = "client", feature = "server")),
            feature = "ffi"
        )))]
        return false;

        #[cfg(any(
            all(feature = "http1", any(feature = "client", feature = "server")),
            feature = "ffi"
        ))]
        matches!(self.inner.kind, Kind::User(User::BodyWriteAborted))
    }

    /// 如果错误发生在调用 `AsyncWrite::shutdown()` 时，返回 `true`。
    pub fn is_shutdown(&self) -> bool {
        #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
        if matches!(self.inner.kind, Kind::Shutdown) {
            return true;
        }
        false
    }

    /// 如果错误由超时引起，返回 `true`。
    ///
    /// 检查两种超时情况：
    /// 1. HeaderTimeout（HTTP/1 server 模式下的头部读取超时）
    /// 2. 错误链中包含 `TimedOut` 哨兵类型
    pub fn is_timeout(&self) -> bool {
        #[cfg(all(feature = "http1", feature = "server"))]
        if matches!(self.inner.kind, Kind::HeaderTimeout) {
            return true;
        }
        // 在错误链中查找 TimedOut 哨兵类型
        self.find_source::<TimedOut>().is_some()
    }

    /// 创建一个新的 `Error`，仅指定错误类别，不带原因。
    ///
    /// 这是所有 `new_*` 构造方法的基础。使用 `Box::new` 将 `ErrorImpl`
    /// 分配到堆上，使得 `Error` 本身只占一个指针的大小。
    pub(super) fn new(kind: Kind) -> Error {
        Error {
            inner: Box::new(ErrorImpl { kind, cause: None }),
        }
    }

    /// 为已有的 `Error` 附加一个错误原因。
    ///
    /// 采用 builder 模式，返回 `self` 以支持链式调用：
    /// `Error::new(kind).with(cause)`
    ///
    /// 泛型约束 `C: Into<Cause>` 允许传入任何可以转换为 `Box<dyn Error + Send + Sync>` 的类型。
    pub(super) fn with<C: Into<Cause>>(mut self, cause: C) -> Error {
        self.inner.cause = Some(cause.into());
        self
    }

    /// 获取错误类别的引用。
    /// 仅在 HTTP/1 server 模式或 FFI 模式下可用。
    #[cfg(any(all(feature = "http1", feature = "server"), feature = "ffi"))]
    pub(super) fn kind(&self) -> &Kind {
        &self.inner.kind
    }

    /// 在错误的 source 链中查找特定类型的错误。
    ///
    /// 遍历整个错误链（通过 `source()` 方法），尝试将每个错误
    /// 向下转型（downcast）为目标类型 `E`。这利用了 `std::error::Error`
    /// trait 的 `downcast_ref()` 方法实现类型安全的向下转型。
    ///
    /// 这种模式在 Rust 错误处理中很常见，用于在类型擦除的错误链中
    /// 恢复具体的错误类型信息。
    pub(crate) fn find_source<E: StdError + 'static>(&self) -> Option<&E> {
        let mut cause = self.source();
        while let Some(err) = cause {
            if let Some(typed) = err.downcast_ref() {
                return Some(typed);
            }
            cause = err.source();
        }

        // else
        None
    }

    /// 从错误链中提取 h2 的 Reason 码。
    ///
    /// 在错误链中查找 `h2::Error`，如果找到则返回其 Reason 码；
    /// 否则默认返回 `INTERNAL_ERROR`。这用于在 HTTP/2 连接中
    /// 将 hyper 的错误转换为合适的 h2 错误码。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(super) fn h2_reason(&self) -> h2::Reason {
        // Find an h2::Reason somewhere in the cause stack, if it exists,
        // otherwise assume an INTERNAL_ERROR.
        self.find_source::<h2::Error>()
            .and_then(|h2_err| h2_err.reason())
            .unwrap_or(h2::Reason::INTERNAL_ERROR)
    }

    /// 创建一个"已取消"错误。
    pub(super) fn new_canceled() -> Error {
        Error::new(Kind::Canceled)
    }

    /// 创建一个"消息不完整"错误。
    /// 仅在 HTTP/1 + (client 或 server) 时可用。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn new_incomplete() -> Error {
        Error::new(Kind::IncompleteMessage)
    }

    /// 创建一个"消息头过大"错误。
    /// 仅在 HTTP/1 + (client 或 server) 时可用。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn new_too_large() -> Error {
        Error::new(Kind::Parse(Parse::TooLarge))
    }

    /// 创建一个"检测到 HTTP/2 前导帧"的版本解析错误。
    /// 当在 HTTP/1 解析器中意外检测到 HTTP/2 连接前导时触发。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn new_version_h2() -> Error {
        Error::new(Kind::Parse(Parse::VersionH2))
    }

    /// 创建一个"意外消息"错误。
    /// 当连接收到不在预期中的消息时触发。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn new_unexpected_message() -> Error {
        Error::new(Kind::UnexpectedMessage)
    }

    /// 创建一个 I/O 错误，将 `std::io::Error` 作为原因附加。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(super) fn new_io(cause: std::io::Error) -> Error {
        Error::new(Kind::Io).with(cause)
    }

    /// 创建一个"通道已关闭"错误。
    #[cfg(any(
        all(feature = "http1", any(feature = "client", feature = "server")),
        all(feature = "http2", feature = "client")
    ))]
    pub(super) fn new_closed() -> Error {
        Error::new(Kind::ChannelClosed)
    }

    /// 创建一个"读取 body 错误"，将底层错误作为原因附加。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(super) fn new_body<E: Into<Cause>>(cause: E) -> Error {
        Error::new(Kind::Body).with(cause)
    }

    /// 创建一个"写入 body 错误"，将底层错误作为原因附加。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(super) fn new_body_write<E: Into<Cause>>(cause: E) -> Error {
        Error::new(Kind::BodyWrite).with(cause)
    }

    /// 创建一个"body 写入中止"错误。
    #[cfg(any(
        all(feature = "http1", any(feature = "client", feature = "server")),
        feature = "ffi"
    ))]
    pub(super) fn new_body_write_aborted() -> Error {
        Error::new(Kind::User(User::BodyWriteAborted))
    }

    /// 创建一个用户错误（内部辅助方法）。
    fn new_user(user: User) -> Error {
        Error::new(Kind::User(user))
    }

    /// 创建一个"意外头部"用户错误。
    /// 例如 server 模式下同时收到 content-length 和 transfer-encoding。
    #[cfg(any(feature = "http1", feature = "http2"))]
    #[cfg(feature = "server")]
    pub(super) fn new_user_header() -> Error {
        Error::new_user(User::UnexpectedHeader)
    }

    /// 创建一个"头部读取超时"错误。
    /// 仅在 HTTP/1 server 模式下可用。
    #[cfg(all(feature = "http1", feature = "server"))]
    pub(super) fn new_header_timeout() -> Error {
        Error::new(Kind::HeaderTimeout)
    }

    /// 创建一个"不支持的状态码"用户错误。
    /// 当 server 尝试发送 1xx 信息性响应（101 除外）时触发。
    #[cfg(feature = "http1")]
    #[cfg(feature = "server")]
    pub(super) fn new_user_unsupported_status_code() -> Error {
        Error::new_user(User::UnsupportedStatusCode)
    }

    /// 创建一个"没有可用的 upgrade"错误。
    /// 当用户轮询一个不存在的 upgrade 时触发。
    pub(super) fn new_user_no_upgrade() -> Error {
        Error::new_user(User::NoUpgrade)
    }

    /// 创建一个"手动升级"错误。
    /// 当用户期望 upgrade 但使用的是底层 API 时触发。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn new_user_manual_upgrade() -> Error {
        Error::new_user(User::ManualUpgrade)
    }

    /// 创建一个"Service 错误"，将用户 Service 的错误作为原因附加。
    #[cfg(any(
        all(any(feature = "client", feature = "server"), feature = "http1"),
        all(feature = "server", feature = "http2")
    ))]
    pub(super) fn new_user_service<E: Into<Cause>>(cause: E) -> Error {
        Error::new_user(User::Service).with(cause)
    }

    /// 创建一个"用户 Body 流错误"，将底层错误作为原因附加。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(super) fn new_user_body<E: Into<Cause>>(cause: E) -> Error {
        Error::new_user(User::Body).with(cause)
    }

    /// 创建一个"无效的 CONNECT 请求"错误。
    /// 当 HTTP/2 client 发送带有非零 body 的 CONNECT 请求时触发。
    #[cfg(all(feature = "client", feature = "http2"))]
    pub(super) fn new_user_invalid_connect() -> Error {
        Error::new_user(User::InvalidConnectWithBody)
    }

    /// 创建一个"shutdown 错误"，将 I/O 错误作为原因附加。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn new_shutdown(cause: std::io::Error) -> Error {
        Error::new(Kind::Shutdown).with(cause)
    }

    /// 创建一个"FFI 回调中止"错误。
    /// 仅在 FFI feature 启用时可用。
    #[cfg(feature = "ffi")]
    pub(super) fn new_user_aborted_by_callback() -> Error {
        Error::new_user(User::AbortedByCallback)
    }

    /// 创建一个"dispatch 任务已消失"错误。
    /// 通常在客户端的连接池连接断开时触发。
    #[cfg(all(feature = "client", any(feature = "http1", feature = "http2")))]
    pub(super) fn new_user_dispatch_gone() -> Error {
        Error::new(Kind::User(User::DispatchGone))
    }

    /// 创建一个来自 h2 库的错误。
    ///
    /// 特殊处理：如果 h2 错误本质上是 I/O 错误，则转换为 `Kind::Io`；
    /// 否则使用 `Kind::Http2` 并将 h2 错误作为原因附加。
    /// 这种转换确保了 `is_*` 方法能正确识别底层的 I/O 错误。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(super) fn new_h2(cause: ::h2::Error) -> Error {
        if cause.is_io() {
            // h2 错误本质上是 I/O 错误，提取出来作为 Io 类型处理
            Error::new_io(cause.into_io().expect("h2::Error::is_io"))
        } else {
            Error::new(Kind::Http2).with(cause)
        }
    }

    /// 获取当前错误的人类可读描述字符串。
    ///
    /// 这是 `Display` 实现的核心，根据错误的 Kind 返回对应的静态描述。
    /// 每个 match 分支都有对应的 `#[cfg]` 属性，确保只在相关 feature 启用时编译。
    fn description(&self) -> &str {
        match self.inner.kind {
            Kind::Parse(Parse::Method) => "invalid HTTP method parsed",
            #[cfg(feature = "http1")]
            Kind::Parse(Parse::Version) => "invalid HTTP version parsed",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::Parse(Parse::VersionH2) => "invalid HTTP version parsed (found HTTP2 preface)",
            Kind::Parse(Parse::Uri) => "invalid URI",
            #[cfg(all(feature = "http1", feature = "server"))]
            Kind::Parse(Parse::UriTooLong) => "URI too long",
            #[cfg(feature = "http1")]
            Kind::Parse(Parse::Header(Header::Token)) => "invalid HTTP header parsed",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::Parse(Parse::Header(Header::ContentLengthInvalid)) => {
                "invalid content-length parsed"
            }
            #[cfg(all(feature = "http1", feature = "server"))]
            Kind::Parse(Parse::Header(Header::TransferEncodingInvalid)) => {
                "invalid transfer-encoding parsed"
            }
            #[cfg(all(feature = "http1", any(feature = "client", feature = "server")))]
            Kind::Parse(Parse::Header(Header::TransferEncodingUnexpected)) => {
                "unexpected transfer-encoding parsed"
            }
            #[cfg(any(feature = "http1", feature = "http2"))]
            Kind::Parse(Parse::TooLarge) => "message head is too large",
            Kind::Parse(Parse::Status) => "invalid HTTP status-code parsed",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::Parse(Parse::Internal) => {
                "internal error inside Hyper and/or its dependencies, please report"
            }
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::IncompleteMessage => "connection closed before message completed",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::UnexpectedMessage => "received unexpected message from connection",
            #[cfg(any(
                all(feature = "http1", any(feature = "client", feature = "server")),
                all(feature = "http2", feature = "client")
            ))]
            Kind::ChannelClosed => "channel closed",
            Kind::Canceled => "operation was canceled",
            #[cfg(all(feature = "http1", feature = "server"))]
            Kind::HeaderTimeout => "read header from client timeout",
            #[cfg(all(
                any(feature = "client", feature = "server"),
                any(feature = "http1", feature = "http2")
            ))]
            Kind::Body => "error reading a body from connection",
            #[cfg(all(
                any(feature = "client", feature = "server"),
                any(feature = "http1", feature = "http2")
            ))]
            Kind::BodyWrite => "error writing a body to connection",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::Shutdown => "error shutting down connection",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
            Kind::Http2 => "http2 error",
            #[cfg(all(
                any(feature = "client", feature = "server"),
                any(feature = "http1", feature = "http2")
            ))]
            Kind::Io => "connection error",

            #[cfg(all(
                any(feature = "client", feature = "server"),
                any(feature = "http1", feature = "http2")
            ))]
            Kind::User(User::Body) => "error from user's Body stream",
            #[cfg(any(
                all(feature = "http1", any(feature = "client", feature = "server")),
                feature = "ffi"
            ))]
            Kind::User(User::BodyWriteAborted) => "user body write aborted",
            #[cfg(all(feature = "client", feature = "http2"))]
            Kind::User(User::InvalidConnectWithBody) => {
                "user sent CONNECT request with non-zero body"
            }
            #[cfg(any(
                all(any(feature = "client", feature = "server"), feature = "http1"),
                all(feature = "server", feature = "http2")
            ))]
            Kind::User(User::Service) => "error from user's Service",
            #[cfg(any(feature = "http1", feature = "http2"))]
            #[cfg(feature = "server")]
            Kind::User(User::UnexpectedHeader) => "user sent unexpected header",
            #[cfg(feature = "http1")]
            #[cfg(feature = "server")]
            Kind::User(User::UnsupportedStatusCode) => {
                "response has 1xx status code, not supported by server"
            }
            Kind::User(User::NoUpgrade) => "no upgrade available",
            #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
            Kind::User(User::ManualUpgrade) => "upgrade expected but low level API in use",
            #[cfg(all(feature = "client", any(feature = "http1", feature = "http2")))]
            Kind::User(User::DispatchGone) => "dispatch task is gone",
            #[cfg(feature = "ffi")]
            Kind::User(User::AbortedByCallback) => "operation aborted by an application callback",
        }
    }
}

/// 为 `Error` 实现 `Debug` trait。
///
/// 输出格式为 `hyper::Error(Kind, Cause)`，使用 `debug_tuple` 来格式化，
/// 既显示了错误类别，也在存在原因时显示原因。
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_tuple("hyper::Error");
        f.field(&self.inner.kind);
        if let Some(ref cause) = self.inner.cause {
            f.field(cause);
        }
        f.finish()
    }
}

/// 为 `Error` 实现 `Display` trait。
///
/// 仅显示当前层级的错误描述，不包含 source 链。
/// 这是有意为之的设计：让调用者决定是否以及如何展示完整的错误链。
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

/// 为 `Error` 实现标准库的 `Error` trait。
///
/// `source()` 方法返回底层原因（如果有的话），允许调用者遍历错误链。
/// 此处的 `&**cause` 解引用链：`&Box<dyn Error>` -> `&dyn Error` -> `&(dyn Error + 'static)`。
impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .cause
            .as_ref()
            // 将 &Box<dyn StdError + Send + Sync> 转换为 &(dyn StdError + 'static)
            // 这里需要两次解引用：第一次从 &Box 到 &dyn，第二次取引用适配生命周期
            .map(|cause| &**cause as &(dyn StdError + 'static))
    }
}

/// 从 `Parse` 到 `Error` 的转换实现。
///
/// 允许使用 `?` 操作符直接将 `Parse` 错误转换为 `Error`。
/// `#[doc(hidden)]` 隐藏此实现，因为 `Parse` 本身不是公共类型。
#[doc(hidden)]
impl From<Parse> for Error {
    fn from(err: Parse) -> Error {
        Error::new(Kind::Parse(err))
    }
}

/// `Parse` 枚举的便利构造方法（仅 HTTP/1）。
///
/// 这些方法提供了创建特定头部解析错误的快捷方式，
/// 避免在调用点暴露 `Header` 枚举的细节。
#[cfg(feature = "http1")]
impl Parse {
    /// 创建一个"Content-Length 无效"解析错误。
    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn content_length_invalid() -> Self {
        Parse::Header(Header::ContentLengthInvalid)
    }

    /// 创建一个"Transfer-Encoding 无效"解析错误。
    #[cfg(feature = "server")]
    pub(crate) fn transfer_encoding_invalid() -> Self {
        Parse::Header(Header::TransferEncodingInvalid)
    }

    /// 创建一个"意外的 Transfer-Encoding"解析错误。
    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn transfer_encoding_unexpected() -> Self {
        Parse::Header(Header::TransferEncodingUnexpected)
    }
}

/// 从 `httparse::Error` 到 `Parse` 的转换（仅 HTTP/1）。
///
/// `httparse` 是 hyper 用于底层 HTTP/1 解析的 crate。
/// 此实现将 httparse 的错误类型映射到 hyper 内部的 `Parse` 枚举，
/// 使得解析错误可以无缝地融入 hyper 的错误体系。
#[cfg(feature = "http1")]
impl From<httparse::Error> for Parse {
    fn from(err: httparse::Error) -> Parse {
        match err {
            // 这些 httparse 错误都属于"无效 token"类别的头部错误
            httparse::Error::HeaderName
            | httparse::Error::HeaderValue
            | httparse::Error::NewLine
            | httparse::Error::Token => Parse::Header(Header::Token),
            httparse::Error::Status => Parse::Status,
            httparse::Error::TooManyHeaders => Parse::TooLarge,
            httparse::Error::Version => Parse::Version,
        }
    }
}

/// 从 `http::method::InvalidMethod` 到 `Parse` 的转换。
///
/// 当 `http` crate 报告方法无效时，转换为 `Parse::Method`。
/// 参数使用 `_` 忽略，因为无需保留原始错误的细节。
impl From<http::method::InvalidMethod> for Parse {
    fn from(_: http::method::InvalidMethod) -> Parse {
        Parse::Method
    }
}

/// 从 `http::status::InvalidStatusCode` 到 `Parse` 的转换。
///
/// 当 `http` crate 报告状态码无效时，转换为 `Parse::Status`。
impl From<http::status::InvalidStatusCode> for Parse {
    fn from(_: http::status::InvalidStatusCode) -> Parse {
        Parse::Status
    }
}

/// 从 `http::uri::InvalidUri` 到 `Parse` 的转换。
///
/// 当 `http` crate 报告 URI 无效时，转换为 `Parse::Uri`。
impl From<http::uri::InvalidUri> for Parse {
    fn from(_: http::uri::InvalidUri) -> Parse {
        Parse::Uri
    }
}

/// 从 `http::uri::InvalidUriParts` 到 `Parse` 的转换。
///
/// 当通过各部分组装 URI 失败时，转换为 `Parse::Uri`。
impl From<http::uri::InvalidUriParts> for Parse {
    fn from(_: http::uri::InvalidUriParts) -> Parse {
        Parse::Uri
    }
}

// ===== impl TimedOut ====

/// 为 `TimedOut` 哨兵类型实现 `Display`。
///
/// 显示一个简单的超时消息，用于在错误链中标识超时原因。
impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation timed out")
    }
}

/// 为 `TimedOut` 实现 `Error` trait。
///
/// 这是一个空实现——`TimedOut` 没有 source，仅作为错误链中的标记使用。
impl StdError for TimedOut {}

/// 错误模块的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    /// 辅助函数：断言类型 T 实现了 Send + Sync + 'static。
    /// 利用 Rust 的 trait bound 在编译时检查，如果不满足则编译失败。
    fn assert_send_sync<T: Send + Sync + 'static>() {}

    /// 测试 `Error` 类型满足 `Send + Sync` 约束。
    /// 这在异步编程中至关重要，因为错误经常需要跨线程传递。
    #[test]
    fn error_satisfies_send_sync() {
        assert_send_sync::<Error>()
    }

    /// 测试 `Error` 的内存大小。
    /// 由于使用了 `Box<ErrorImpl>`，Error 在栈上应该只占一个指针的大小（通常 8 字节）。
    /// 这对 `Result<T, Error>` 的大小优化非常重要。
    #[test]
    fn error_size_of() {
        assert_eq!(mem::size_of::<Error>(), mem::size_of::<usize>());
    }

    /// 测试：当错误链中没有 h2::Error 时，h2_reason 返回 INTERNAL_ERROR。
    #[cfg(feature = "http2")]
    #[test]
    fn h2_reason_unknown() {
        let closed = Error::new_closed();
        assert_eq!(closed.h2_reason(), h2::Reason::INTERNAL_ERROR);
    }

    /// 测试：当 h2::Error 直接作为原因时，能正确提取 Reason。
    #[cfg(feature = "http2")]
    #[test]
    fn h2_reason_one_level() {
        let body_err = Error::new_user_body(h2::Error::from(h2::Reason::ENHANCE_YOUR_CALM));
        assert_eq!(body_err.h2_reason(), h2::Reason::ENHANCE_YOUR_CALM);
    }

    /// 测试：当 h2::Error 嵌套在多层错误链中时，仍能正确提取 Reason。
    /// 模拟代理场景：收到 h2 错误后包装为 Service 错误。
    #[cfg(feature = "http2")]
    #[test]
    fn h2_reason_nested() {
        let recvd = Error::new_h2(h2::Error::from(h2::Reason::HTTP_1_1_REQUIRED));
        // Suppose a user were proxying the received error
        let svc_err = Error::new_user_service(recvd);
        assert_eq!(svc_err.h2_reason(), h2::Reason::HTTP_1_1_REQUIRED);
    }
}
