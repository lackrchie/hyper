//! 服务工具函数模块。
//!
//! 本模块提供了便捷的工具函数和辅助类型，用于简化 `Service` 的创建。
//! 其中最重要的是 [`service_fn`] 函数，它允许用户从一个闭包或函数
//! 快速创建一个 `Service` 实例，而无需手动定义结构体并实现 trait。
//!
//! # 在 hyper 架构中的角色
//!
//! 本模块是 hyper 服务层的"用户友好"入口。对于大多数简单的 HTTP 服务，
//! 用户只需使用 `service_fn` 包装一个异步闭包即可开始处理请求，
//! 无需深入理解 `Service` trait 的细节。

/// 引入标准库的 `Error` trait，用于约束 `ServiceFn` 的错误类型。
/// 要求错误类型可以转换为 `Box<dyn StdError + Send + Sync>`，
/// 这是 hyper 对服务错误类型的标准要求。
use std::error::Error as StdError;

/// 引入 `fmt` 模块，用于为 `ServiceFn` 实现 `Debug` trait。
use std::fmt;

/// 引入 `Future` trait，`ServiceFn` 的 `Service::Future` 关联类型需要此约束。
use std::future::Future;

/// 引入 `PhantomData`，用于在 `ServiceFn` 中标记未使用的泛型参数 `R`（请求体类型）。
/// `PhantomData<fn(R)>` 特别使用函数指针类型，使得 `ServiceFn` 不会对 `R`
/// 产生所有权或生命周期约束（逆变关系），这是 Rust 类型系统中的常见技巧。
use std::marker::PhantomData;

/// 引入 hyper 的 `Body` trait，用于约束 `ServiceFn` 处理的请求体和响应体类型。
use crate::body::Body;

/// 引入 hyper 的 `Service` trait 定义。
use crate::service::service::Service;

/// 引入 hyper 的 `Request` 和 `Response` 类型别名。
use crate::{Request, Response};

/// 从函数或闭包创建一个 [`Service`]。
///
/// 这是创建 HTTP 服务最简便的方式。传入一个接受 `Request<R>` 并返回
/// `Future<Output = Result<Response<ResBody>, E>>` 的函数或闭包，
/// 即可得到一个实现了 `Service` trait 的对象。
///
/// # 类型参数
///
/// - `F`：闭包或函数类型
/// - `R`：请求体类型
/// - `S`：闭包返回的 Future 类型
///
/// # 示例
///
/// ```
/// use bytes::Bytes;
/// use hyper::{body, Request, Response, Version};
/// use http_body_util::Full;
/// use hyper::service::service_fn;
///
/// let service = service_fn(|req: Request<body::Incoming>| async move {
///     if req.version() == Version::HTTP_11 {
///         Ok(Response::new(Full::<Bytes>::from("Hello World")))
///     } else {
///         // 注意：通常返回一个带有适当状态码的 Response 比返回 Err 更好
///         Err("not HTTP/1.1, abort connection")
///     }
/// });
/// ```
pub fn service_fn<F, R, S>(f: F) -> ServiceFn<F, R>
where
    F: Fn(Request<R>) -> S,
    S: Future,
{
    ServiceFn {
        f,
        _req: PhantomData, // 使用 PhantomData 标记请求体类型 R，不实际持有 R 类型的值
    }
}

/// [`service_fn`] 返回的 Service 类型。
///
/// 这是一个结构体包装器，将闭包 `F` 适配为 `Service` trait 的实现。
///
/// # 泛型参数
///
/// - `F`：存储的闭包或函数
/// - `R`：请求体类型，通过 `PhantomData` 标记但不实际持有
///
/// `PhantomData<fn(R)>` 的使用是一个 Rust 惯用技巧：
/// - `fn(R)` 表示一个接受 `R` 参数的函数指针类型
/// - 这使 `ServiceFn` 对 `R` 具有逆变（contravariant）关系，
///   与函数参数的逆变性保持一致
/// - 不会对 `R` 产生额外的 `Send`、`Sync` 或生命周期约束
pub struct ServiceFn<F, R> {
    /// 存储的闭包或函数，在 `Service::call` 中被调用
    f: F,
    /// PhantomData 标记请求体类型，不占用实际内存空间
    _req: PhantomData<fn(R)>,
}

/// 为 `ServiceFn` 实现 `Service` trait。
///
/// 泛型约束确保：
/// - `F` 是一个接受 `Request<ReqBody>` 并返回 `Ret` 的闭包（`Fn` trait）
/// - `ReqBody` 实现了 `Body`（有效的 HTTP 请求体）
/// - `Ret` 是一个输出 `Result<Response<ResBody>, E>` 的 Future
/// - `E` 可以转换为 `Box<dyn StdError + Send + Sync>`
/// - `ResBody` 实现了 `Body`（有效的 HTTP 响应体）
///
/// 使用 `Fn` 而非 `FnMut` 约束与 `Service::call(&self, ...)` 的 `&self` 签名一致，
/// 因为 `&self` 不允许获取闭包的可变访问。
impl<F, ReqBody, Ret, ResBody, E> Service<Request<ReqBody>> for ServiceFn<F, ReqBody>
where
    F: Fn(Request<ReqBody>) -> Ret,
    ReqBody: Body,
    Ret: Future<Output = Result<Response<ResBody>, E>>,
    E: Into<Box<dyn StdError + Send + Sync>>,
    ResBody: Body,
{
    type Response = crate::Response<ResBody>;
    type Error = E;
    type Future = Ret;

    fn call(&self, req: Request<ReqBody>) -> Self::Future {
        (self.f)(req) // 直接调用存储的闭包，将请求转发给用户代码
    }
}

/// 为 `ServiceFn` 实现 `Debug` trait。
///
/// 由于闭包类型 `F` 通常不实现 `Debug`，这里使用自定义格式化，
/// 输出 `impl Service` 字符串而非尝试打印闭包内容。
impl<F, R> fmt::Debug for ServiceFn<F, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("impl Service").finish() // 仅显示类型描述，不暴露内部状态
    }
}

/// 为 `ServiceFn` 实现 `Clone` trait。
///
/// 仅当闭包 `F` 实现了 `Clone` 时才可克隆。
/// `PhantomData` 总是可以被克隆的，因此只需要约束 `F: Clone`。
///
/// 注意：这里手动实现 `Clone` 而非使用 `#[derive(Clone)]`，
/// 是因为 derive 会为所有泛型参数添加 `Clone` 约束，
/// 但 `R`（请求体类型）不需要实现 `Clone`。
impl<F, R> Clone for ServiceFn<F, R>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        ServiceFn {
            f: self.f.clone(),
            _req: PhantomData, // PhantomData 是零大小类型，无需克隆内部状态
        }
    }
}

/// 为 `ServiceFn` 实现 `Copy` trait。
///
/// 仅当闭包 `F` 实现了 `Copy` 时才可复制。
/// `Copy` 是 `Clone` 的子 trait，表示按位复制的语义。
/// 对于简单的函数指针或不捕获环境的闭包，通常会自动实现 `Copy`。
impl<F, R> Copy for ServiceFn<F, R> where F: Copy {}
