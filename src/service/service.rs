//! 核心 `Service` trait 定义模块。
//!
//! 本模块定义了 hyper 中最基础的异步服务抽象——`Service` trait。
//! 它是整个 hyper 服务层的基石，定义了从请求到响应的异步转换接口。
//!
//! # 在 hyper 架构中的角色
//!
//! `Service` trait 的设计灵感来源于 Tower 生态系统中的 `Service` trait，
//! 但进行了简化。它将异步请求处理抽象为一个通用接口，使得 hyper 的
//! HTTP 处理逻辑可以与具体的业务逻辑解耦。
//!
//! # 与 Tower 的区别
//!
//! hyper 的 `Service::call` 接受 `&self` 而非 `&mut self`，
//! 这意味着服务可以并发处理多个请求而无需独占访问。
//! 如果需要可变状态，用户应使用 `Arc<Mutex<_>>` 等同步原语。
//!
//! # 智能指针的 blanket implementation
//!
//! 本模块为 `&S`、`&mut S`、`Box<S>`、`Rc<S>` 和 `Arc<S>` 提供了
//! blanket implementation，使得用户可以透明地通过各种智能指针使用服务。

/// 引入 `Future` trait，`Service::Future` 关联类型的约束。
use std::future::Future;

/// 异步请求到响应的转换 trait。
///
/// `Service` trait 是一个简化的接口，使得编写网络应用变得模块化和可复用，
/// 与底层协议解耦。
///
/// # 函数式特性
///
/// `Service` 本质上是一个请求的函数。它立即返回一个 [`Future`]，
/// 代表请求处理的最终完成。实际的请求处理可以在未来的任何时间、
/// 任何线程或执行器上进行。处理过程可能依赖于调用其他服务。
/// 在未来的某个时刻，处理将完成，[`Future`] 将解析为响应或错误。
///
/// 从高层来看，`Service::call` 代表一次 RPC（远程过程调用）请求。
/// `Service` 的具体实现可以是服务端处理器，也可以是客户端连接器。
///
/// # 工具与生态
///
/// [`hyper-util`][util] crate 提供了将此 trait 桥接到其他库的工具，
/// 例如 [`tower`][tower]，后者可能提供自己的 `Service` 变体。
///
/// 参见 [`hyper_util::service`][util-service] 了解更多。
///
/// [tower]: https://docs.rs/tower
/// [util]: https://docs.rs/hyper-util
/// [util-service]: https://docs.rs/hyper-util/latest/hyper_util/service/index.html
pub trait Service<Request> {
    /// 服务返回的响应类型。
    type Response;

    /// 服务可能产生的错误类型。
    ///
    /// 注意：向 hyper 服务器返回 `Error` 时，行为取决于协议。
    /// 在大多数情况下，hyper 会突然中断连接——它会通过协议允许的方式
    /// 中止请求，例如发送 RST_STREAM（HTTP/2），或者如果协议不支持
    /// 则直接关闭连接。
    type Error;

    /// 异步响应的 Future 类型。
    ///
    /// `Future::Output` 必须是 `Result<Self::Response, Self::Error>`，
    /// 表示异步操作成功产生响应或返回错误。
    type Future: Future<Output = Result<Self::Response, Self::Error>>;

    /// 处理请求并返回响应 Future。
    ///
    /// `call` 接受 `&self` 而非 `&mut self`，这一设计选择有以下考量：
    /// - 为未来的 `async fn` 做准备——async fn 生成的 Future 只借用 `&self`，
    ///   使得 Service 可以同时处理多个未完成的请求。
    /// - 更清晰地表明 Service 通常可以被 Clone。
    /// - 要在 Clone 之间共享状态，通常需要 `Arc<Mutex<_>>`，
    ///   此时实际上并不需要 `&mut self`，`&self` 就足够了。
    /// - 相关讨论：<https://github.com/hyperium/hyper/issues/3040>
    fn call(&self, req: Request) -> Self::Future;
}

/// 为不可变引用 `&S` 实现 `Service`。
///
/// 这允许通过共享引用调用服务，是最基本的委托实现。
/// `?Sized` 约束允许 `S` 是 unsized 类型（如 trait 对象 `dyn Service`）。
impl<Request, S: Service<Request> + ?Sized> Service<Request> for &'_ S {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn call(&self, req: Request) -> Self::Future {
        (**self).call(req) // 解引用后调用被引用类型的 call 方法
    }
}

/// 为可变引用 `&mut S` 实现 `Service`。
///
/// 即使持有可变引用，也只需 `&self` 访问，因此直接委托给 `&S` 的实现。
/// 这使得在需要 `&mut` 语义的上下文中也能使用 Service。
impl<Request, S: Service<Request> + ?Sized> Service<Request> for &'_ mut S {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn call(&self, req: Request) -> Self::Future {
        (**self).call(req) // 解引用 &mut S 到 S，再调用 S 的 call
    }
}

/// 为 `Box<S>` 实现 `Service`。
///
/// 允许将 Service 装箱（通常用于类型擦除，如 `Box<dyn Service<...>>`）。
/// `?Sized` 约束是关键——它使得 `Box<dyn Service<...>>` 也能匹配此实现。
impl<Request, S: Service<Request> + ?Sized> Service<Request> for Box<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn call(&self, req: Request) -> Self::Future {
        (**self).call(req) // 解引用 Box<S> 到 S
    }
}

/// 为 `Rc<S>` 实现 `Service`。
///
/// 允许通过引用计数智能指针共享 Service（单线程场景）。
/// 由于 `Service::call` 只需 `&self`，`Rc` 的不可变共享语义完全适用。
impl<Request, S: Service<Request> + ?Sized> Service<Request> for std::rc::Rc<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn call(&self, req: Request) -> Self::Future {
        (**self).call(req) // 解引用 Rc<S> 到 S
    }
}

/// 为 `Arc<S>` 实现 `Service`。
///
/// 允许通过原子引用计数智能指针在多线程间安全共享 Service。
/// 这是最常用的 Service 共享方式，因为 hyper 通常运行在多线程异步运行时上。
/// 配合 `Service::call` 的 `&self` 语义，多个任务可以同时通过同一个
/// `Arc<S>` 并发处理请求。
impl<Request, S: Service<Request> + ?Sized> Service<Request> for std::sync::Arc<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn call(&self, req: Request) -> Self::Future {
        (**self).call(req) // 解引用 Arc<S> 到 S
    }
}
