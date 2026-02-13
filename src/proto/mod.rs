//! HTTP 消息协议相关的核心组件模块。
//!
//! 本模块是 hyper 中 HTTP 协议实现的顶层组织模块，负责将 HTTP/1.1 和 HTTP/2
//! 两种协议的实现统一管理。它定义了 HTTP 消息的通用数据结构，如消息头
//! （`MessageHead`）、请求行（`RequestLine`）、响应体长度（`BodyLength`）等，
//! 这些结构被 HTTP/1.1 (`h1`) 和 HTTP/2 (`h2`) 子模块共同使用。
//!
//! 在 hyper 的架构中，`proto` 模块位于传输层（`rt`）和上层 API（`client`/`server`）
//! 之间，负责 HTTP 协议的编解码逻辑。

// 使用 cfg_feature! 宏，仅在启用 "http1" feature 时编译 HTTP/1.1 相关模块
cfg_feature! {
    #![feature = "http1"]

    // HTTP/1.1 协议实现子模块，包含连接管理、编解码、调度等核心组件
    pub(crate) mod h1;

    // 重新导出 h1 模块中的 Conn 类型，供 crate 内部使用
    // Conn 是 HTTP/1.1 连接的核心结构体，管理单个 TCP 连接上的多次 HTTP 事务
    pub(crate) use self::h1::Conn;

    // 客户端模式下导出 dispatch 模块，用于驱动客户端请求/响应的调度循环
    #[cfg(feature = "client")]
    pub(crate) use self::h1::dispatch;
    // 服务端模式下导出 ServerTransaction 类型，表示服务端的 HTTP/1.1 事务处理角色
    #[cfg(feature = "server")]
    pub(crate) use self::h1::ServerTransaction;
}

// HTTP/2 协议实现子模块，仅在启用 "http2" feature 时编译
#[cfg(feature = "http2")]
pub(crate) mod h2;

/// 传入的 HTTP 消息头部结构体。
///
/// 包含请求行/状态行（`subject`）、HTTP 版本号、头部字段集合以及扩展信息。
/// 泛型参数 `S` 表示消息的"主题"类型：
/// - 对于请求消息，`S = RequestLine`（包含 HTTP 方法和 URI）
/// - 对于响应消息，`S = StatusCode`（HTTP 状态码）
///
/// 使用 `#[derive(Default)]` 允许创建带有默认值的消息头部，
/// 这在构造错误响应等场景中很有用。
#[cfg(feature = "http1")]
#[derive(Debug, Default)]
pub(crate) struct MessageHead<S> {
    /// HTTP 协议版本号（如 HTTP/1.0 或 HTTP/1.1）
    pub(crate) version: http::Version,
    /// 消息主题：请求行（方法 + URI）或状态行（状态码）
    pub(crate) subject: S,
    /// HTTP 头部字段集合，使用 http crate 的 HeaderMap 实现高效存储
    pub(crate) headers: http::HeaderMap,
    /// HTTP 扩展字段，用于存储非标准的元数据（如原始头部大小写映射、升级信息等）
    /// 使用私有可见性，防止外部直接访问
    extensions: http::Extensions,
}

/// 传入的 HTTP 请求消息类型别名。
///
/// 将 `MessageHead` 的泛型参数特化为 `RequestLine`，
/// 表示一个完整的 HTTP 请求头部（包含方法、URI、版本、头部字段）。
#[cfg(feature = "http1")]
pub(crate) type RequestHead = MessageHead<RequestLine>;

/// HTTP 请求行结构体，封装了 HTTP 方法和请求 URI。
///
/// 使用元组结构体的形式，第一个字段是 HTTP 方法（GET/POST 等），
/// 第二个字段是请求 URI（如 "/api/users"）。
/// 实现了 `PartialEq` 以支持测试中的断言比较。
#[derive(Debug, Default, PartialEq)]
#[cfg(feature = "http1")]
pub(crate) struct RequestLine(pub(crate) http::Method, pub(crate) http::Uri);

/// 传入的 HTTP 响应消息类型别名。
///
/// 将 `MessageHead` 的泛型参数特化为 `StatusCode`，
/// 表示一个完整的 HTTP 响应头部（包含状态码、版本、头部字段）。
/// 仅在同时启用 "http1" 和 "client" feature 时可用（客户端接收响应）。
#[cfg(all(feature = "http1", feature = "client"))]
pub(crate) type ResponseHead = MessageHead<http::StatusCode>;

/// HTTP 消息体长度的枚举表示。
///
/// 在 HTTP/1.1 中，消息体的长度通过以下两种方式确定：
/// - `Known(u64)`: 通过 Content-Length 头部明确指定的确切长度
/// - `Unknown`: 使用 Transfer-Encoding: chunked 分块传输编码，长度未知
#[derive(Debug)]
#[cfg(feature = "http1")]
pub(crate) enum BodyLength {
    /// 通过 Content-Length 头部指定的确切字节长度
    Known(u64),
    /// 使用 Transfer-Encoding: chunked 传输编码（HTTP/1.1），长度未预先确定
    Unknown,
}

/// Dispatcher Future 完成时的状态枚举。
///
/// 表示 HTTP 连接调度器完成后的两种可能结果：
/// - `Shutdown`: 连接正常关闭
/// - `Upgrade`: 连接需要进行协议升级（如 WebSocket），不关闭底层连接
pub(crate) enum Dispatched {
    /// 调度器已完全关闭连接
    Shutdown,
    /// 调度器检测到挂起的协议升级请求，因此未关闭底层连接。
    /// 包含一个 `Pending` 对象，用于将升级后的 IO 传递给用户代码。
    #[cfg(feature = "http1")]
    Upgrade(crate::upgrade::Pending),
}

/// 为响应消息头部实现转换方法（仅客户端 + HTTP/1.1）。
///
/// 将解析后的 `MessageHead<StatusCode>` 转换为标准的 `http::Response<B>` 对象，
/// 方便上层 API 使用。
#[cfg(all(feature = "client", feature = "http1"))]
impl MessageHead<http::StatusCode> {
    /// 将消息头部与消息体组合为完整的 HTTP 响应对象。
    ///
    /// 通过解构 `self` 的各个字段，分别设置到 `http::Response` 的对应位置。
    fn into_response<B>(self, body: B) -> http::Response<B> {
        let mut res = http::Response::new(body);
        *res.status_mut() = self.subject;
        *res.headers_mut() = self.headers;
        *res.version_mut() = self.version;
        *res.extensions_mut() = self.extensions;
        res
    }
}
