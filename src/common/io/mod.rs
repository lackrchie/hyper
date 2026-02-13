//! IO 工具模块。
//!
//! 本模块提供 hyper 内部使用的 IO 工具类型，主要包括：
//! - `Compat`: IO trait 兼容层，在 hyper 自定义的 `rt::Read`/`rt::Write` trait
//!   与 tokio 的 `AsyncRead`/`AsyncWrite` trait 之间进行适配转换
//! - `Rewind`: 可回绕的 IO 包装器，支持将已读取的数据"放回"流中重新读取
//!
//! 这些类型在 hyper 的协议解析过程中扮演重要角色。例如，在 HTTP 升级或
//! 协议探测时，可能需要先读取一部分数据来判断协议类型，然后将这些数据
//! "回绕"后交给正确的协议处理器重新读取。

/// IO trait 兼容适配器模块。
/// 仅在同时启用 client/server 和 http2 feature 时编译，
/// 因为 h2 crate 使用 tokio 的 AsyncRead/AsyncWrite trait。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
mod compat;
/// 可回绕读取的 IO 包装器模块。
mod rewind;

/// 重导出 `Compat` 类型，供 crate 内部使用。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
pub(crate) use self::compat::Compat;
/// 重导出 `Rewind` 类型，供 crate 内部使用。
pub(crate) use self::rewind::Rewind;
