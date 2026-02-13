//! 异步任务辅助工具模块。
//!
//! 本模块提供与异步任务调度相关的辅助函数，主要包括：
//! - `yield_now`: 主动让出当前 Future 的执行权，使运行时有机会调度其他任务
//! - `noop_waker`: 创建一个什么都不做的 Waker（空操作唤醒器）
//! - `now_or_never`: 尝试立即轮询一个 Future，如果就绪则返回结果
//!
//! 这些工具在 hyper 的 HTTP/1 连接管理和客户端实现中被使用，
//! 用于控制异步任务的执行时序和进行同步式的 Future 探测。

// `Context` 是异步轮询的上下文，携带 Waker；`Poll` 表示就绪或挂起状态
use std::task::{Context, Poll};
// 以下类型仅在 client feature 下使用，用于构建空操作 Waker
#[cfg(feature = "client")]
use std::task::{RawWaker, RawWakerVTable, Waker};

/// 主动让出当前 Future 的执行权，使其被立即重新调度。
///
/// 该函数通过调用 `cx.waker().wake_by_ref()` 通知运行时"我还没完成，但请尽快再次轮询我"，
/// 然后返回 `Poll::Pending`。这种模式在需要避免单个 Future 长时间占用执行线程时非常有用，
/// 例如在循环处理中加入 yield 点，防止饥饿（starvation）。
///
/// 返回类型使用 `std::convert::Infallible` 表示永远不会返回 `Poll::Ready`。
pub(crate) fn yield_now(cx: &mut Context<'_>) -> Poll<std::convert::Infallible> {
    // 唤醒自身，确保运行时会再次轮询此 Future
    cx.waker().wake_by_ref();
    // 返回 Pending，让出执行权
    Poll::Pending
}

// TODO: replace with `std::task::Waker::noop()` once MSRV >= 1.85
/// 创建一个空操作（no-op）Waker。
///
/// 该 Waker 的所有操作（clone、wake、wake_by_ref、drop）都不执行任何动作。
/// 主要用于 `now_or_never` 函数中，当我们只想探测 Future 是否立即就绪，
/// 而不关心后续唤醒时使用。
///
/// # 实现细节
/// 通过 `RawWaker` 和 `RawWakerVTable` 手动构建 Waker 的虚函数表，
/// 所有函数指针都指向空操作闭包。使用 `const` 确保在编译期构造，零运行时开销。
#[cfg(feature = "client")]
fn noop_waker() -> Waker {
    // 使用空指针作为数据，因为不需要任何状态
    const NOOP_RAW_WAKER: RawWaker = RawWaker::new(std::ptr::null(), &NOOP_VTABLE);
    const NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(
        // `clone` 返回相同的空操作 RawWaker
        |_: *const ()| NOOP_RAW_WAKER,
        // `wake` 不执行任何操作
        |_: *const ()| {},
        // `wake_by_ref` 不执行任何操作
        |_: *const ()| {},
        // `drop` 不执行任何操作
        |_: *const ()| {},
    );

    // SAFETY: all functions in the vtable are safe to call, and Waker's safety does not require
    // them to actually do anything.
    // SAFETY：虚函数表中的所有函数都可以安全调用，Waker 的安全性不要求它们实际执行任何操作。
    unsafe { Waker::from_raw(NOOP_RAW_WAKER) }
}

/// 尝试立即轮询一个 Future，如果就绪则返回 `Some(result)`，否则返回 `None`。
///
/// 该函数使用空操作 Waker 创建上下文进行一次性轮询。如果 Future 没有立即就绪，
/// 由于使用的是空操作 Waker，Future 将无法被正确唤醒，因此返回 `None` 后
/// 该 Future 通常不应再被继续驱动。
///
/// # 使用场景
/// 在 hyper 客户端中，用于检查某些操作是否可以同步完成（如连接池中的连接是否已就绪），
/// 避免不必要的异步调度开销。
///
/// # 注意
/// 如果 Future 返回 `Pending`，它将无法被正确唤醒，因为使用的是空操作 Waker。
/// 调用者应意识到这一点，不要在返回 `None` 后继续依赖该 Future。
#[cfg(feature = "client")]
pub(crate) fn now_or_never<F: std::future::Future>(fut: F) -> Option<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // TODO: replace with std::pin::pin! and drop pin-utils once MSRV >= 1.68
    // 使用 pin_utils::pin_mut! 宏将 Future 固定在栈上（Pin<&mut F>）
    pin_utils::pin_mut!(fut);
    match fut.poll(&mut cx) {
        Poll::Ready(res) => Some(res),
        Poll::Pending => None,
    }
}
