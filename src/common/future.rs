//! Future 辅助工具模块。
//!
//! 本模块提供 `poll_fn` 函数和 `PollFn` 类型，允许将一个闭包转换为 `Future`。
//! 这是对标准库 `std::future::poll_fn`（自 Rust 1.64 起稳定）的内部替代实现，
//! 用于在 hyper 支持较低 MSRV（最低支持 Rust 版本）时保持兼容性。
//!
//! `poll_fn` 模式在异步编程中非常常见，它将手写的 poll 逻辑包装为一个合法的 Future，
//! 适用于不需要完整 async/await 语法糖，但需要精细控制轮询行为的场景。

// 标准库的异步编程核心类型
use std::{
    // `Future` trait 定义了异步计算的接口
    future::Future,
    // `Pin` 确保 Future 不被移动（用于自引用 Future 的安全性）
    pin::Pin,
    // `Context` 包含 Waker（用于通知运行时 Future 就绪），
    // `Poll` 表示 Future 的就绪（Ready）或挂起（Pending）状态
    task::{Context, Poll},
};

// TODO: replace with `std::future::poll_fn` once MSRV >= 1.64
/// 将一个闭包转换为 `Future`。
///
/// 传入的闭包 `f` 接受 `&mut Context<'_>` 参数，返回 `Poll<T>`。
/// 每次 Future 被轮询时，都会调用该闭包，使得调用者可以用闭包形式
/// 编写 poll 逻辑，而无需定义完整的 Future 类型。
///
/// # 参数
/// - `f`: 实现了 `FnMut(&mut Context<'_>) -> Poll<T>` 的闭包
///
/// # 返回
/// 返回一个 `PollFn<F>` 实例，实现了 `Future<Output = T>`
pub(crate) fn poll_fn<T, F>(f: F) -> PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    PollFn { f }
}

/// 基于闭包的 Future 包装器。
///
/// 该结构体持有一个闭包 `f`，每次被轮询时调用该闭包。
/// 通过 `poll_fn` 函数创建。
pub(crate) struct PollFn<F> {
    /// 内部闭包，接受异步上下文并返回 Poll 结果
    f: F,
}

/// 为 `PollFn` 实现 `Unpin`。
///
/// 由于 `PollFn` 只持有一个闭包（没有自引用结构），可以安全地实现 `Unpin`，
/// 这意味着它不需要被 Pin 住就能安全使用，简化了调用方的代码。
impl<F> Unpin for PollFn<F> {}

/// 为 `PollFn<F>` 实现 `Future` trait。
///
/// 每次轮询时直接调用内部闭包 `f`，将异步上下文 `cx` 传递给它。
impl<T, F> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;

    /// 轮询该 Future，委托给内部闭包。
    ///
    /// 由于 `PollFn` 实现了 `Unpin`，这里可以安全地通过 `self.as_mut()` 获取
    /// 内部闭包的可变引用并调用它。
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 通过 Pin::as_mut() 获取 &mut Self，然后访问闭包字段 f 并调用
        (self.as_mut().f)(cx)
    }
}
