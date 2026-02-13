//! 执行器 trait 约束别名模块
//!
//! 本模块通过 trait 别名（trait alias）的方式，简化了 hyper 中对 HTTP/2 执行器的
//! 复杂泛型约束。这些 trait 通常会被自动满足——只要你实现了底层的 `Executor` trait，
//! 这里的约束 trait 就会自动实现。
//!
//! 本模块主要包含三个层次的 trait：
//! - `Http2UpgradedExec`：HTTP/2 升级连接的执行器约束（内部使用）
//! - `Http2ClientConnExec`：HTTP/2 客户端连接的执行器约束
//! - `Http2ServerConnExec`：HTTP/2 服务端连接的执行器约束
//!
//! 这些 trait 都使用了"密封 trait（sealed trait）"模式，
//! 防止下游 crate 直接实现它们，确保 hyper 对其行为的完全控制。

// --- 条件导出 ---

/// 导出 HTTP/2 客户端连接执行器 trait（需要同时启用 `client` 和 `http2` 特性）
#[cfg(all(feature = "client", feature = "http2"))]
pub use self::h2_client::Http2ClientConnExec;
/// 导出 HTTP/2 服务端连接执行器 trait（需要同时启用 `server` 和 `http2` 特性）
#[cfg(all(feature = "server", feature = "http2"))]
pub use self::h2_server::Http2ServerConnExec;

/// 导出内部使用的 HTTP/2 升级连接执行器 trait
#[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
pub(crate) use self::h2_common::Http2UpgradedExec;

/// HTTP/2 升级连接执行器的公共模块。
///
/// 当 HTTP/2 连接被升级（如 WebSocket over HTTP/2）时，需要一个执行器来运行
/// 升级后的发送流任务。此模块定义了对应的 trait 和自动实现。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
mod h2_common {
    /// 导入 HTTP/2 升级发送流任务类型
    use crate::proto::h2::upgrade::UpgradedSendStreamTask;
    /// 导入通用的执行器 trait
    use crate::rt::Executor;

    /// HTTP/2 升级连接的执行器 trait。
    ///
    /// 此 trait 用于在 HTTP/2 连接升级时执行发送流任务。
    /// 泛型参数 `B` 代表 body 数据的类型。
    pub trait Http2UpgradedExec<B> {
        #[doc(hidden)]
        /// 执行一个 HTTP/2 升级后的发送流任务
        fn execute_upgrade(&self, fut: UpgradedSendStreamTask<B>);
    }

    /// 为所有实现了 `Executor<UpgradedSendStreamTask<B>>` 的类型
    /// 自动实现 `Http2UpgradedExec<B>`。
    ///
    /// 这是 Rust 中常见的 blanket implementation 模式——通过泛型 impl
    /// 为满足特定约束的所有类型提供默认实现。
    #[doc(hidden)]
    impl<E, B> Http2UpgradedExec<B> for E
    where
        E: Executor<UpgradedSendStreamTask<B>>,
    {
        fn execute_upgrade(&self, fut: UpgradedSendStreamTask<B>) {
            // 直接委托给底层的 Executor::execute 方法
            self.execute(fut)
        }
    }
}

/// HTTP/2 客户端连接执行器模块。
///
/// 定义了 `Http2ClientConnExec` trait，它组合了多个约束，
/// 确保执行器能够运行 HTTP/2 客户端连接所需的所有异步任务。
#[cfg(all(feature = "client", feature = "http2"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "client", feature = "http2"))))]
mod h2_client {
    /// 导入标准库的错误 trait 和 Future trait
    use std::{error::Error, future::Future};

    /// 导入 hyper 的异步 IO trait
    use crate::rt::{Read, Write};
    /// 导入 HTTP/2 客户端 Future 类型和通用执行器 trait
    use crate::{proto::h2::client::H2ClientFuture, rt::Executor};

    /// HTTP/2 客户端连接的执行器 trait。
    ///
    /// 此 trait 为任何实现了 [`Executor`] 的类型自动实现。
    /// 它组合了以下约束：
    /// - `Http2UpgradedExec<B::Data>`：能够执行升级连接任务
    /// - `sealed_client::Sealed<(B, T)>`：密封 trait，防止外部实现
    /// - `Clone`：执行器需要可克隆
    ///
    /// 此 trait 是密封的（sealed），不能由 hyper crate 之外的类型实现。
    ///
    /// [`Executor`]: crate::rt::Executor
    pub trait Http2ClientConnExec<B, T>:
        super::Http2UpgradedExec<B::Data> + sealed_client::Sealed<(B, T)> + Clone
    where
        B: http_body::Body,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        T: Read + Write + Unpin,
    {
        #[doc(hidden)]
        /// 执行一个 HTTP/2 客户端 Future
        fn execute_h2_future(&mut self, future: H2ClientFuture<B, T, Self>);
    }

    /// 为所有满足约束的类型自动实现 `Http2ClientConnExec`。
    ///
    /// 约束条件：
    /// - `E: Clone`：执行器可克隆
    /// - `E: Executor<H2ClientFuture<B, T, E>>`：能够执行 H2 客户端 Future
    /// - `E: Http2UpgradedExec<B::Data>`：能够执行升级连接任务
    /// - `B: http_body::Body + 'static`：body 类型满足 Body trait
    /// - `T: Read + Write + Unpin`：IO 传输类型满足读写要求
    impl<E, B, T> Http2ClientConnExec<B, T> for E
    where
        E: Clone,
        E: Executor<H2ClientFuture<B, T, E>>,
        E: super::Http2UpgradedExec<B::Data>,
        B: http_body::Body + 'static,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        H2ClientFuture<B, T, E>: Future<Output = ()>,
        T: Read + Write + Unpin,
    {
        fn execute_h2_future(&mut self, future: H2ClientFuture<B, T, E>) {
            // 委托给底层的 Executor::execute 方法
            self.execute(future)
        }
    }

    /// 为满足约束的类型自动实现密封 trait。
    ///
    /// 密封 trait（sealed trait）是 Rust 中的一种设计模式：
    /// 通过在私有模块中定义一个 trait，并要求公开 trait 继承它，
    /// 可以防止外部 crate 实现该公开 trait。
    impl<E, B, T> sealed_client::Sealed<(B, T)> for E
    where
        E: Clone,
        E: Executor<H2ClientFuture<B, T, E>>,
        E: super::Http2UpgradedExec<B::Data>,
        B: http_body::Body + 'static,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        H2ClientFuture<B, T, E>: Future<Output = ()>,
        T: Read + Write + Unpin,
    {
    }

    /// 密封 trait 所在的私有模块。
    ///
    /// `Sealed` trait 定义在私有模块中，外部 crate 无法访问，
    /// 从而确保 `Http2ClientConnExec` 不能被外部实现。
    mod sealed_client {
        /// 密封标记 trait，泛型参数 `X` 允许不同的 body/transport 组合
        pub trait Sealed<X> {}
    }
}

/// HTTP/2 服务端连接执行器模块。
///
/// 定义了 `Http2ServerConnExec` trait，结构与客户端模块类似，
/// 但针对服务端场景——执行器需要能够运行 HTTP/2 服务端流处理任务。
#[cfg(all(feature = "server", feature = "http2"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "server", feature = "http2"))))]
mod h2_server {
    /// 导入 HTTP/2 服务端流处理类型
    use crate::{proto::h2::server::H2Stream, rt::Executor};
    /// 导入 `http_body::Body` trait
    use http_body::Body;
    /// 导入 `Future` trait
    use std::future::Future;

    /// HTTP/2 服务端连接的执行器 trait。
    ///
    /// 此 trait 为任何实现了 [`Executor`] 的类型自动实现。
    /// 它组合了以下约束：
    /// - `Http2UpgradedExec<B::Data>`：能够执行升级连接任务
    /// - `sealed::Sealed<(F, B)>`：密封 trait，防止外部实现
    /// - `Clone`：执行器需要可克隆
    ///
    /// 泛型参数：
    /// - `F`：用户提供的服务（service）函数类型
    /// - `B`：HTTP body 类型
    ///
    /// 此 trait 是密封的，不能由 hyper crate 之外的类型实现。
    ///
    /// [`Executor`]: crate::rt::Executor
    pub trait Http2ServerConnExec<F, B: Body>:
        super::Http2UpgradedExec<B::Data> + sealed::Sealed<(F, B)> + Clone
    {
        #[doc(hidden)]
        /// 执行一个 HTTP/2 服务端流处理任务
        fn execute_h2stream(&mut self, fut: H2Stream<F, B, Self>);
    }

    /// 为所有满足约束的类型自动实现 `Http2ServerConnExec`。
    ///
    /// 注意这里的 `Self` 引用模式：`H2Stream<F, B, E>` 中的 `E` 就是实现类型本身，
    /// 这意味着执行器类型会出现在它要执行的 Future 类型参数中——
    /// 这是一种递归泛型（recursive generics）模式。
    #[doc(hidden)]
    impl<E, F, B> Http2ServerConnExec<F, B> for E
    where
        E: Clone,
        E: Executor<H2Stream<F, B, E>>,
        E: super::Http2UpgradedExec<B::Data>,
        H2Stream<F, B, E>: Future<Output = ()>,
        B: Body,
    {
        fn execute_h2stream(&mut self, fut: H2Stream<F, B, E>) {
            // 委托给底层的 Executor::execute 方法
            self.execute(fut)
        }
    }

    /// 为满足约束的类型自动实现密封 trait
    impl<E, F, B> sealed::Sealed<(F, B)> for E
    where
        E: Clone,
        E: Executor<H2Stream<F, B, E>>,
        E: super::Http2UpgradedExec<B::Data>,
        H2Stream<F, B, E>: Future<Output = ()>,
        B: Body,
    {
    }

    /// 密封 trait 所在的私有模块
    mod sealed {
        /// 密封标记 trait，防止外部 crate 实现 `Http2ServerConnExec`
        pub trait Sealed<T> {}
    }
}
