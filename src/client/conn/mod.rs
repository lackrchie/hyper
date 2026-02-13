//! 低层客户端连接 API 模块
//!
//! 本模块提供了基于单个连接（single connection）的低层 HTTP 客户端 API。
//! 它不处理主机连接、连接池管理等高层逻辑，而是提供构建这些功能所需的基础组件，
//! 允许用户在外部自定义这些行为。
//!
//! ## 模块结构
//!
//! - [`http1`] — HTTP/1.1 客户端连接实现，提供 `SendRequest`、`Connection` 和 `Builder`
//! - [`http2`] — HTTP/2 客户端连接实现，提供支持多路复用的 `SendRequest`、`Connection` 和 `Builder`
//!
//! ## 设计理念
//!
//! hyper 的客户端连接 API 采用"握手"（handshake）模式：
//! 1. 用户提供一个已建立的 IO 对象（如 TCP 连接）
//! 2. 调用 `handshake()` 执行 HTTP 协议握手
//! 3. 返回 `(SendRequest, Connection)` 对：
//!    - `SendRequest` 用于发送 HTTP 请求
//!    - `Connection` 是一个 Future，需要被 spawn 到执行器中驱动协议状态机
//!
//! ## 如何选择
//!
//! 如果你需要一个便捷的 HTTP 客户端，推荐使用：
//! - [reqwest](https://github.com/seanmonstar/reqwest) — 高层 HTTP 客户端
//! - [`hyper-util` 的客户端](https://docs.rs/hyper-util/latest/hyper_util/client/index.html) — 较低层但仍提供连接池等功能
//!
//! ## 示例
//!
//! 请参阅 [客户端指南](https://hyper.rs/guides/1/client/basic/)。

// HTTP/1 客户端连接子模块，仅在启用 "http1" feature 时编译
#[cfg(feature = "http1")]
pub mod http1;
// HTTP/2 客户端连接子模块，仅在启用 "http2" feature 时编译
#[cfg(feature = "http2")]
pub mod http2;

// 重新导出 TrySendError 类型，使用户可以通过 `client::conn::TrySendError` 访问。
// `super::dispatch` 指向 `client::dispatch` 模块。
pub use super::dispatch::TrySendError;
