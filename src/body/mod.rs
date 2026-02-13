//! HTTP 请求和响应的流式 Body 模块
//!
//! 本模块是 hyper 中处理 HTTP 消息体（body）的核心模块。对于
//! [客户端](crate::client) 和 [服务端](crate::server) 而言，请求和响应都使用
//! 流式的 body，而非一次性缓冲整个消息体。这种设计允许应用程序按需使用内存，
//! 并通过"仅在被请求时才读取"的方式对连接施加背压（back-pressure）。
//!
//! hyper 中有两个关键组件：
//!
//! - **[`Body`] trait**：描述所有可能的 body 类型。
//!   hyper 允许任何实现了 `Body` 的类型作为消息体，从而让应用程序对流式传输
//!   拥有细粒度的控制权。
//! - **[`Incoming`] 具体类型**：这是 `Body` trait 的一个具体实现，
//!   由 hyper 作为"接收流"返回（即用于服务端接收的请求体和客户端接收的响应体）。
//!
//! 在 [`http-body-util`][] 中还提供了额外的实现，例如 `Full`（完整 body）
//! 或 `Empty`（空 body）。
//!
//! [`http-body-util`]: https://docs.rs/http-body-util

// --- 公开的 re-export ---

/// 从 `bytes` crate 重新导出 `Buf` trait，用于操作字节缓冲区
/// `Bytes` 是 hyper 中传递 body 数据块的基本类型
pub use bytes::{Buf, Bytes};
/// 从 `http_body` crate 重新导出 `Body` trait——所有 HTTP 消息体的核心抽象
pub use http_body::Body;
/// 从 `http_body` crate 重新导出 `Frame` 类型，表示 body 流中的一帧（可以是数据帧或 trailers 帧）
pub use http_body::Frame;
/// 从 `http_body` crate 重新导出 `SizeHint`，用于提示 body 的大小信息
pub use http_body::SizeHint;

/// 将本模块内部的 `Incoming` 类型公开导出，供外部使用
pub use self::incoming::Incoming;

// --- crate 内部使用的 re-export ---

/// 在 HTTP/1 客户端或服务端场景下，导出内部使用的 `Sender` 类型
/// `Sender` 是 `Incoming` body 通道的发送端，用于向流中推送数据
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
pub(crate) use self::incoming::Sender;
/// 在 HTTP/1 或 HTTP/2 的客户端或服务端场景下，导出 `DecodedLength`
/// `DecodedLength` 用于表示解码后的消息体长度（精确值、分块传输或连接关闭分隔）
#[cfg(all(
    any(feature = "http1", feature = "http2"),
    any(feature = "client", feature = "server")
))]
pub(crate) use self::length::DecodedLength;

// --- 子模块声明 ---

/// `incoming` 子模块：包含 `Incoming` body 类型及其发送端 `Sender` 的实现
mod incoming;
/// `length` 子模块：包含 `DecodedLength` 类型，仅在启用 HTTP 协议特性时编译
#[cfg(all(
    any(feature = "http1", feature = "http2"),
    any(feature = "client", feature = "server")
))]
mod length;

/// 编译期静态断言函数，确保 `Incoming` 类型实现了 `Send` 和 `Sync`。
///
/// 这是 hyper 中常见的模式：通过在泛型函数中要求 `T: Send` / `T: Sync` 约束，
/// 利用 Rust 编译器在编译期验证类型的线程安全性。
/// 该函数永远不会被调用，仅用于触发编译期检查。
fn _assert_send_sync() {
    /// 辅助函数：要求泛型 `T` 实现 `Send`
    fn _assert_send<T: Send>() {}
    /// 辅助函数：要求泛型 `T` 实现 `Sync`
    fn _assert_sync<T: Sync>() {}

    // 编译期验证 Incoming 实现了 Send + Sync
    _assert_send::<Incoming>();
    _assert_sync::<Incoming>();
}
