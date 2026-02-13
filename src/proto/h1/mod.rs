//! HTTP/1.1 协议实现的核心模块。
//!
//! 本模块是 hyper 中 HTTP/1.1 协议栈的入口，组织和导出所有 HTTP/1.1 相关的子模块。
//! 它定义了 HTTP/1.1 事务处理的核心 trait (`Http1Transaction`)、消息解析的
//! 上下文结构（`ParseContext`）和解析结果（`ParsedMessage`），以及编码相关的辅助类型。
//!
//! 子模块分工：
//! - `conn`: 连接状态机，管理单个 TCP 连接上的读写状态
//! - `decode`: HTTP 消息体的解码器（Content-Length / Chunked / EOF）
//! - `dispatch`: 调度器，驱动连接上的请求/响应循环
//! - `encode`: HTTP 消息体的编码器
//! - `io`: 带缓冲的 IO 层，处理底层读写和消息解析
//! - `role`: 定义 Client 和 Server 两种角色的具体实现

// bytes crate 的 BytesMut，用于高效的零拷贝字节缓冲区操作
use bytes::BytesMut;
// http crate 的 HeaderMap 和 Method 类型，分别表示 HTTP 头部集合和 HTTP 方法
use http::{HeaderMap, Method};
// httparse crate 的解析器配置，允许自定义 HTTP 解析行为（如允许宽松的语法）
use httparse::ParserConfig;

// 从 body 模块导入 DecodedLength，表示已解码的消息体长度（已知/分块/连接关闭等）
use crate::body::DecodedLength;
// 从上层 proto 模块导入共享的消息头部类型和消息体长度枚举
use crate::proto::{BodyLength, MessageHead};

// 重新导出子模块中的核心类型，供 crate 内部使用
// Conn: HTTP/1.1 连接状态机
pub(crate) use self::conn::Conn;
// Decoder: 消息体解码器
pub(crate) use self::decode::Decoder;
// Dispatcher: 请求/响应调度器
pub(crate) use self::dispatch::Dispatcher;
// EncodedBuf: 已编码的缓冲区；Encoder: 消息体编码器
pub(crate) use self::encode::{EncodedBuf, Encoder};
//TODO: move out of h1::io
// MINIMUM_MAX_BUFFER_SIZE: 最小的最大缓冲区大小限制
pub(crate) use self::io::MINIMUM_MAX_BUFFER_SIZE;

// 声明各子模块
mod conn;     // 连接状态机
mod decode;   // 消息体解码
pub(crate) mod dispatch;  // 调度器（pub(crate) 因为客户端需要访问内部类型）
mod encode;   // 消息体编码
mod io;       // 缓冲 IO 层
mod role;     // Client/Server 角色实现

// 客户端事务类型别名：将 role::Client 导出为 ClientTransaction
cfg_client! {
    pub(crate) type ClientTransaction = role::Client;
}

// 服务端事务类型别名：将 role::Server 导出为 ServerTransaction
cfg_server! {
    pub(crate) type ServerTransaction = role::Server;
}

/// HTTP/1.1 事务处理的核心 trait。
///
/// 定义了 HTTP/1.1 消息的解析和编码接口。`Client` 和 `Server` 两种角色
/// 分别实现此 trait，以处理各自方向的消息。
///
/// 关联类型：
/// - `Incoming`: 接收的消息主题类型（Server 接收 RequestLine，Client 接收 StatusCode）
/// - `Outgoing`: 发送的消息主题类型（Server 发送 StatusCode，Client 发送 RequestLine）
pub(crate) trait Http1Transaction {
    /// 接收（解析）的消息主题类型
    type Incoming;
    /// 发送（编码）的消息主题类型，要求实现 Default 以便构造错误响应
    type Outgoing: Default;
    /// 用于 tracing 日志的角色标识字符串
    #[cfg(feature = "tracing")]
    const LOG: &'static str;

    /// 从字节缓冲区解析 HTTP 消息头部。
    ///
    /// 返回 `Ok(Some(...))` 表示解析成功，
    /// 返回 `Ok(None)` 表示数据不完整需要继续读取，
    /// 返回 `Err(...)` 表示解析错误。
    fn parse(bytes: &mut BytesMut, ctx: ParseContext<'_>) -> ParseResult<Self::Incoming>;

    /// 将 HTTP 消息头部编码为字节写入目标缓冲区。
    ///
    /// 根据消息内容确定合适的消息体编码方式（Content-Length / Chunked 等），
    /// 返回对应的 Encoder。
    fn encode(enc: Encode<'_, Self::Outgoing>, dst: &mut Vec<u8>) -> crate::Result<Encoder>;

    /// 当解析发生错误时的回调。
    ///
    /// 服务端可以返回一个错误响应头（如 400 Bad Request），
    /// 客户端则返回 None（无法向服务端报告错误）。
    fn on_error(err: &crate::Error) -> Option<MessageHead<Self::Outgoing>>;

    /// 判断当前角色是否为客户端。默认通过 `is_server()` 取反实现。
    fn is_client() -> bool {
        !Self::is_server()
    }

    /// 判断当前角色是否为服务端。默认通过 `is_client()` 取反实现。
    fn is_server() -> bool {
        !Self::is_client()
    }

    /// 解析时遇到 EOF 是否应视为错误。
    ///
    /// 客户端在等待响应时遇到 EOF 是错误（服务端异常断开），
    /// 服务端在空闲时遇到 EOF 是正常的（客户端关闭连接）。
    fn should_error_on_parse_eof() -> bool {
        Self::is_client()
    }

    /// 是否应该先读取（再写入）。
    ///
    /// 服务端应先读取请求再发送响应，客户端则先发送请求再等待响应。
    /// 此方法影响连接状态机的读写顺序逻辑。
    fn should_read_first() -> bool {
        Self::is_server()
    }

    /// 更新缓存的日期头部值。
    ///
    /// 仅服务端实现（用于自动添加 Date 头部），客户端为空操作。
    fn update_date() {}
}

/// `Http1Transaction::parse` 的结果类型别名。
///
/// - `Ok(Some(ParsedMessage))`: 成功解析出一个完整的消息
/// - `Ok(None)`: 数据不完整，需要继续读取
/// - `Err(Parse)`: 解析错误
pub(crate) type ParseResult<T> = Result<Option<ParsedMessage<T>>, crate::error::Parse>;

/// 解析完成后的消息结构体。
///
/// 包含解析出的消息头部及相关的协议语义信息，如消息体长度、
/// 是否需要 100-Continue 确认、是否保持连接、是否请求协议升级。
#[derive(Debug)]
pub(crate) struct ParsedMessage<T> {
    /// 解析出的消息头部（版本、主题、头部字段、扩展）
    head: MessageHead<T>,
    /// 解码后的消息体长度信息（确切长度 / 分块 / 连接关闭分隔等）
    decode: DecodedLength,
    /// 请求是否包含 `Expect: 100-continue` 头部
    /// 服务端收到后应先发送 100 Continue 响应再读取请求体
    expect_continue: bool,
    /// 是否保持连接（keep-alive），取决于 HTTP 版本和 Connection 头部
    keep_alive: bool,
    /// 是否请求协议升级（如 WebSocket 升级或 CONNECT 隧道）
    wants_upgrade: bool,
}

/// 解析上下文结构体，在解析过程中传递所需的可变状态和配置。
///
/// 由于 HTTP 解析需要跨多次调用保持状态（如缓存的头部、请求方法等），
/// 这些信息通过 `ParseContext` 传入解析函数。
pub(crate) struct ParseContext<'a> {
    /// 缓存的 HeaderMap，用于复用已分配的内存，减少堆分配开销。
    /// 当一个请求处理完毕后，清空的 HeaderMap 可以被下一个请求复用。
    cached_headers: &'a mut Option<HeaderMap>,
    /// 当前请求的 HTTP 方法（客户端解析响应时需要知道对应请求的方法，
    /// 因为 HEAD 请求的响应不包含消息体等特殊规则）
    req_method: &'a mut Option<Method>,
    /// httparse 解析器配置，控制解析的宽严程度
    h1_parser_config: ParserConfig,
    /// 最大允许的头部字段数量限制
    h1_max_headers: Option<usize>,
    /// 是否保留头部字段名的原始大小写（默认会转为小写）
    preserve_header_case: bool,
    /// （FFI feature）是否保留头部字段的原始顺序
    #[cfg(feature = "ffi")]
    preserve_header_order: bool,
    /// 是否允许 HTTP/0.9 响应（极度简化的协议，仅响应体无头部）
    h09_responses: bool,
    /// （客户端 feature）收到 1xx 信息性响应时的回调
    #[cfg(feature = "client")]
    on_informational: &'a mut Option<crate::ext::OnInformational>,
}

/// 传递给 `Http1Transaction::encode` 的编码参数结构体。
///
/// 包含待编码的消息头部、消息体长度信息，以及影响编码行为的各种选项。
pub(crate) struct Encode<'a, T> {
    /// 待编码的消息头部（可变引用，编码过程中会修改/清空头部字段）
    head: &'a mut MessageHead<T>,
    /// 消息体长度信息。None 表示没有消息体，Some 表示有消息体及其长度类型
    body: Option<BodyLength>,
    /// （服务端 feature）是否保持连接
    #[cfg(feature = "server")]
    keep_alive: bool,
    /// 请求方法的可变引用。服务端编码响应时需要知道对应请求的方法，
    /// 客户端编码请求时会在此设置方法值
    req_method: &'a mut Option<Method>,
    /// 是否将头部字段名转换为 Title-Case（如 "content-type" -> "Content-Type"）
    title_case_headers: bool,
    /// （服务端 feature）是否自动添加 Date 头部
    #[cfg(feature = "server")]
    date_header: bool,
}

/// 请求的额外需求标志位。
///
/// 使用位标志模式来高效地表示请求是否需要 Expect 确认和/或协议升级。
/// 这比使用多个 bool 字段更紧凑。
#[derive(Clone, Copy, Debug)]
struct Wants(u8);

/// Wants 位标志的实现
impl Wants {
    /// 空标志：不需要任何额外处理
    const EMPTY: Wants = Wants(0b00);
    /// 需要 Expect: 100-continue 确认
    const EXPECT: Wants = Wants(0b01);
    /// 需要协议升级（WebSocket / CONNECT）
    const UPGRADE: Wants = Wants(0b10);

    /// 添加一个标志位，返回新的 Wants 值。
    /// 使用 `#[must_use]` 确保调用者不会忽略返回值（因为 self 不会被修改）。
    #[must_use]
    fn add(self, other: Wants) -> Wants {
        // 使用位或运算合并标志位
        Wants(self.0 | other.0)
    }

    /// 检查是否包含指定的标志位。
    /// 使用位与运算判断标志位是否被设置。
    fn contains(&self, other: Wants) -> bool {
        (self.0 & other.0) == other.0
    }
}
