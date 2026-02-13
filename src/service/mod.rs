//! 异步服务（Asynchronous Services）模块。
//!
//! 本模块是 hyper 服务抽象层的核心，定义了将 HTTP 请求异步转换为响应的统一接口。
//! [`Service`] trait 是整个模块的基石，它类似于异步函数签名
//! `async fn(Request) -> Result<Response, Error>`。
//!
//! # 在 hyper 架构中的角色
//!
//! `Service` trait 是 hyper 的核心抽象之一，它解耦了协议处理（HTTP/1、HTTP/2）
//! 与业务逻辑。无论是客户端中间件还是服务端请求处理器，都通过实现 `Service`
//! trait 来参与 hyper 的请求-响应处理流程。
//!
//! 虽然 `Service` 的请求和响应类型不严格要求必须是 HTTP 类型，
//! 但 hyper 提供了以下 "trait 别名" 来简化常见的 HTTP 场景中的类型约束：
//!
//! - [`HttpService`]：对所有实现了
//!   `Service<http::Request<B1>, Response = http::Response<B2>>` 的类型自动实现。
//!   这是一个密封 trait，不可直接实现，只需实现 `Service` 即可自动获得。
//!
//! # HttpService
//!
//! 在 hyper 中，尤其是在服务端场景下，一个 `Service` 通常绑定到单个连接。
//! 它定义了如何响应该连接上接收到的**所有**请求。
//!
//! 对于大多数场景，辅助函数 [`service_fn`] 已经足够使用。
//! 如果需要手动为自定义类型实现 `Service`，可以参考 `service_struct_impl.rs` 示例。

/// 声明 `http` 子模块，包含 `HttpService` 密封 trait。
/// 注意：这里的 `http` 是本地子模块，不是外部 `http` crate。
mod http;

/// 声明 `service` 子模块，包含核心 `Service` trait 定义。
mod service;

/// 声明 `util` 子模块，包含 `service_fn` 等实用工具函数。
mod util;

/// 公开导出 `HttpService` trait——HTTP 场景下 `Service` 的特化版本（密封 trait）。
pub use self::http::HttpService;

/// 公开导出核心 `Service` trait——异步请求-响应函数的抽象。
pub use self::service::Service;

/// 公开导出 `service_fn` 函数——从闭包快速创建 `Service` 实例的工厂函数。
pub use self::util::service_fn;
