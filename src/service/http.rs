//! HTTP 服务 trait（`HttpService`）模块。
//!
//! 本模块定义了 `HttpService` 密封 trait（sealed trait），它是 `Service` trait
//! 在 HTTP 场景下的特化版本。`HttpService` 将 `Service` 的泛型请求/响应类型
//! 固定为 `http::Request` 和 `http::Response`，简化了在 hyper 内部处理
//! HTTP 服务时的类型约束。
//!
//! # 在 hyper 架构中的角色
//!
//! `HttpService` 是 hyper 服务端和客户端内部使用的核心 trait。
//! 当 hyper 需要约束一个类型"能够处理 HTTP 请求并返回 HTTP 响应"时，
//! 会使用 `HttpService` 作为 trait bound。由于它是密封的，
//! 外部用户只需实现 `Service` trait，即可自动获得 `HttpService` 的实现。
//!
//! # 密封 trait 模式
//!
//! `HttpService` 继承自 `sealed::Sealed<ReqBody>`，这是一个私有 trait，
//! 外部 crate 无法实现。这种设计使得 hyper 可以在未来修改 `HttpService`
//! 的方法签名而不造成破坏性变更。

/// 引入标准库的 `Error` trait，用于约束 `HttpService::Error` 关联类型。
use std::error::Error as StdError;

/// 引入标准库的 `Future` trait，用于约束 `HttpService::Future` 关联类型。
use std::future::Future;

/// 引入 hyper 的 `Body` trait，用于约束请求体和响应体类型。
use crate::body::Body;

/// 引入 hyper 内部的 `Service` trait 定义。
/// 路径 `crate::service::service::Service` 是因为子模块名与 trait 同名。
use crate::service::service::Service;

/// 引入 hyper 的 `Request` 和 `Response` 类型别名。
use crate::{Request, Response};

/// HTTP 请求到响应的异步转换 trait。
///
/// 这是一个**密封**（sealed）trait，不可被外部直接实现。
/// 它实际上是 [`Service`] 在 HTTP 场景下的别名：任何实现了
/// `Service<Request<B1>, Response = Response<B2>>` 的类型都会自动获得
/// `HttpService<B1>` 的实现（通过 blanket implementation）。
///
/// 与 `Service` 不同的是，`HttpService` 的泛型参数是请求 [`Body`] 类型
/// 和响应 [`Body`] 类型，而不是完整的请求/响应类型，更加直观。
///
/// 更多信息请参阅 crate 级别的 [`service`][crate::service] 文档和 [`Service`]。
pub trait HttpService<ReqBody>: sealed::Sealed<ReqBody> {
    /// 响应的 [`Body`] 类型。
    type ResBody: Body;

    /// 此服务可能产生的错误类型。
    ///
    /// 注意：向 hyper 服务器返回 `Error` 时，具体行为取决于协议。
    /// 在大多数情况下，hyper 会突然中断连接。通常更好的做法是
    /// 返回一个带有 4xx 或 5xx 状态码的 `Response`。
    ///
    /// 参见 [`Service::Error`] 了解更多。
    type Error: Into<Box<dyn StdError + Send + Sync>>;

    /// 此服务返回的 [`Future`] 类型。
    type Future: Future<Output = Result<Response<Self::ResBody>, Self::Error>>;

    /// 处理请求并返回响应的 Future。
    ///
    /// 此方法被标记为 `#[doc(hidden)]`，因为外部用户不应直接调用此方法，
    /// 而应通过 `Service::call` 来调用。
    #[doc(hidden)]
    fn call(&mut self, req: Request<ReqBody>) -> Self::Future;
}

/// 为所有满足条件的 `Service` 实现自动提供 `HttpService` 实现（blanket implementation）。
///
/// 条件：
/// - `T` 实现了 `Service<Request<B1>, Response = Response<B2>>`
/// - `B2` 实现了 `Body`
/// - `T::Error` 可以转换为 `Box<dyn StdError + Send + Sync>`
///
/// 这意味着用户只需实现 `Service` trait，就能自动获得 `HttpService` 的能力，
/// 无需手动实现 `HttpService`。
impl<T, B1, B2> HttpService<B1> for T
where
    T: Service<Request<B1>, Response = Response<B2>>,
    B2: Body,
    T::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type ResBody = B2;

    type Error = T::Error;
    type Future = T::Future;

    fn call(&mut self, req: Request<B1>) -> Self::Future {
        // 委托给 Service::call 方法，注意这里使用完全限定语法
        // 以避免与 HttpService::call 产生歧义（二者同名）
        Service::call(self, req)
    }
}

/// 为所有满足条件的 `Service` 自动实现密封 trait `Sealed`。
///
/// 这是密封 trait 模式的关键部分：
/// `Sealed` 只能在本模块中被实现（因为它定义在私有的 `sealed` 模块中），
/// 而 `HttpService` 要求实现者同时实现 `Sealed`，
/// 因此外部 crate 无法为自己的类型实现 `HttpService`。
impl<T, B1, B2> sealed::Sealed<B1> for T
where
    T: Service<Request<B1>, Response = Response<B2>>,
    B2: Body,
{
}

/// 私有的密封模块，包含 `Sealed` trait。
///
/// 由于此模块是私有的，外部 crate 无法访问 `Sealed` trait，
/// 因此无法为自己的类型实现它。这确保了 `HttpService` 只能通过
/// blanket implementation 自动获得，不能被外部直接实现。
mod sealed {
    /// 密封 trait，用于限制 `HttpService` 的实现范围。
    /// 泛型参数 `T` 对应请求体类型。
    pub trait Sealed<T> {}
}
