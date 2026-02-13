//! 单生产者单消费者（SPSC）广播通道。
//!
//! 本模块实现了一个轻量级的异步 watch 通道，具有以下特性：
//! - 值类型固定为 `usize`，足以表示简单的状态码
//! - 仅当新值与旧值不同时才通知消费者（去重通知）
//! - 值 `0` 被保留为"已关闭"（`CLOSED`）状态
//!
//! 在 hyper 的 HTTP/1 实现中，该通道用于在连接的读写两端之间传递状态变更通知，
//! 例如通知对端连接即将关闭或协议状态发生变化。
//!
//! 与 tokio 的 `watch` 通道不同，这个实现更加精简，专门针对 hyper 内部的
//! `usize` 状态值传递场景优化，使用原子操作和 `AtomicWaker` 实现无锁通信。

// `AtomicWaker` 提供线程安全的 Waker 注册和唤醒功能，
// 是实现异步通知的关键组件——允许一个线程注册 Waker，另一个线程触发唤醒
use atomic_waker::AtomicWaker;
// `AtomicUsize` 用于原子地存储和交换通道值，
// `Ordering` 定义内存序（此处使用 SeqCst 确保最强一致性），
// `Arc` 用于在 Sender 和 Receiver 之间共享内部状态
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
// `task` 模块提供 `Context` 类型，用于在 poll 时注册 Waker
use std::task;

/// 通道值的类型别名，使用 `usize` 作为轻量级状态值
type Value = usize;

/// 通道关闭标志。
///
/// 值 `0` 被保留为通道已关闭的特殊标记。
/// 当 `Sender` 被丢弃时，会自动发送此值通知 `Receiver`。
pub(crate) const CLOSED: usize = 0;

/// 创建一个新的 watch 通道，返回 `(Sender, Receiver)` 对。
///
/// # 参数
/// - `initial`: 通道的初始值，不能为 `CLOSED`（0）
///
/// # Panics
/// 在 debug 模式下，如果 `initial` 为 `CLOSED` 则触发断言失败。
pub(crate) fn channel(initial: Value) -> (Sender, Receiver) {
    debug_assert!(
        initial != CLOSED,
        "watch::channel initial state of 0 is reserved"
    );

    // 创建共享状态，通过 Arc 在 Sender 和 Receiver 之间共享
    let shared = Arc::new(Shared {
        value: AtomicUsize::new(initial),
        waker: AtomicWaker::new(),
    });

    (
        Sender {
            shared: shared.clone(), // Sender 持有 Arc 的克隆
        },
        Receiver { shared }, // Receiver 持有 Arc 的原始所有权
    )
}

/// 通道的发送端。
///
/// 通过 `send` 方法原子地更新通道值。
/// 当 `Sender` 被丢弃时，自动发送 `CLOSED` 值通知接收端。
pub(crate) struct Sender {
    /// 与 Receiver 共享的内部状态
    shared: Arc<Shared>,
}

/// 通道的接收端。
///
/// 通过 `load` 方法异步等待值变更，或通过 `peek` 方法同步读取当前值。
pub(crate) struct Receiver {
    /// 与 Sender 共享的内部状态
    shared: Arc<Shared>,
}

/// Sender 和 Receiver 共享的内部状态。
///
/// 使用原子类型和 AtomicWaker 实现无锁的跨线程通信。
struct Shared {
    /// 原子存储的通道值
    value: AtomicUsize,
    /// 原子 Waker，用于在值变更时唤醒等待的 Receiver
    waker: AtomicWaker,
}

/// `Sender` 的方法实现。
impl Sender {
    /// 发送一个新值到通道。
    ///
    /// 使用 `swap`（原子交换）更新值，如果新值与旧值不同则唤醒接收端。
    /// `SeqCst` 内存序确保值的更新和 Waker 的唤醒之间的顺序性，
    /// 防止 Receiver 看到过时的值。
    pub(crate) fn send(&mut self, value: Value) {
        // 原子交换：将新值写入并返回旧值
        if self.shared.value.swap(value, Ordering::SeqCst) != value {
            // 仅在值发生变化时唤醒接收端，避免不必要的唤醒（去重）
            self.shared.waker.wake();
        }
    }
}

/// `Sender` 的 Drop 实现。
///
/// 当 Sender 被丢弃时，自动发送 `CLOSED` 值，
/// 通知 Receiver 通道已关闭。这是 RAII 模式的典型应用。
impl Drop for Sender {
    fn drop(&mut self) {
        self.send(CLOSED);
    }
}

/// `Receiver` 的方法实现。
impl Receiver {
    /// 注册 Waker 并加载当前通道值（异步轮询模式）。
    ///
    /// 该方法先注册调用者的 Waker（通过 `cx`），然后读取当前值。
    /// 注册顺序很重要：先注册 Waker 再读取值，确保不会错过在读取后、
    /// 注册 Waker 前发生的更新（避免 lost wakeup 问题）。
    ///
    /// 使用 `SeqCst` 内存序确保与 Sender 的 `swap` 操作保持全序关系。
    pub(crate) fn load(&mut self, cx: &mut task::Context<'_>) -> Value {
        // 先注册 Waker，确保后续值变更能唤醒我们
        self.shared.waker.register(cx.waker());
        // 再读取当前值
        self.shared.value.load(Ordering::SeqCst)
    }

    /// 同步窥探当前通道值，不注册 Waker。
    ///
    /// 使用 `Relaxed` 内存序，因为这只是一个尽力而为的快照读取，
    /// 不需要与其他操作建立严格的顺序关系。适用于快速检查通道状态。
    pub(crate) fn peek(&self) -> Value {
        self.shared.value.load(Ordering::Relaxed)
    }
}
