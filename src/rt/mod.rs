//! 运行时抽象组件模块
//!
//! 本模块为 hyper 提供了运行时无关（runtime-agnostic）的 trait 和类型抽象。
//! 通过对异步运行时进行抽象，hyper 可以与不同的执行器（executor）、
//! 定时器（timer）和 IO 传输层协同工作。
//!
//! 本模块的主要组件包括：
//!
//! - **执行器（Executor）**：用于生成（spawn）和运行 Future 的 trait，
//!   允许与任何异步运行时集成（如 tokio、async-std 等）。
//! - **定时器（Timer）**：定时休眠和任务调度的抽象，
//!   使基于时间的操作能够独立于具体运行时实现。
//! - **IO 传输层（IO Transport）**：异步读写 trait，
//!   使 hyper 能够与各种 IO 后端（TCP、TLS、Unix Socket 等）协同工作。
//!
//! 通过实现这些 trait，你可以自定义 hyper 与你所选运行时环境的交互方式。
//!
//! 要了解更多信息，请查阅[运行时指南](https://hyper.rs/guides/1/init/runtime/)。

// --- 子模块声明 ---

/// `bounds` 子模块：包含 trait 别名和约束组合，
/// 简化了对 HTTP/2 执行器的复杂泛型约束
pub mod bounds;
/// `io` 子模块（内部模块）：定义异步 `Read` 和 `Write` trait 及相关缓冲区类型
mod io;
/// `timer` 子模块（内部模块）：定义 `Timer` 和 `Sleep` trait
mod timer;

// --- 公开的 re-export ---

/// 从 `io` 子模块重新导出异步读取 trait 和缓冲区类型：
/// - `Read`：异步读取 trait
/// - `ReadBuf`：读取缓冲区，跟踪已填充和已初始化的区域
/// - `ReadBufCursor`：`ReadBuf` 未填充部分的游标，用于安全地写入数据
/// - `Write`：异步写入 trait
pub use self::io::{Read, ReadBuf, ReadBufCursor, Write};
/// 从 `timer` 子模块重新导出定时器相关 trait：
/// - `Sleep`：由定时器返回的 Future，在指定时间后完成
/// - `Timer`：提供定时功能的 trait
pub use self::timer::{Sleep, Timer};

/// Future 执行器 trait。
///
/// 此 trait 允许 hyper 对异步运行时进行抽象。用户需要为自己的类型实现此 trait，
/// 以便 hyper 能够在该运行时上生成和执行异步任务。
///
/// 泛型参数 `Fut` 代表要执行的 Future 类型。hyper 使用泛型而非 trait object
/// 来避免动态分派的开销，同时允许不同类型的 Future。
///
/// # 示例
///
/// ```
/// # use hyper::rt::Executor;
/// # use std::future::Future;
/// #[derive(Clone)]
/// struct TokioExecutor;
///
/// impl<F> Executor<F> for TokioExecutor
/// where
///     F: Future + Send + 'static,
///     F::Output: Send + 'static,
/// {
///     fn execute(&self, future: F) {
///         tokio::spawn(future);
///     }
/// }
/// ```
pub trait Executor<Fut> {
    /// 将 future 放入执行器中运行。
    ///
    /// 实现者应将给定的 future 提交到底层异步运行时进行调度和执行。
    fn execute(&self, fut: Fut);
}
