//! 定时器抽象模块。
//!
//! 本模块提供 `Time` 和 `Dur` 两个类型，用于抽象和管理 hyper 中的超时操作。
//!
//! hyper 本身不绑定特定的异步运行时（如 tokio），因此通过 `crate::rt::Timer` trait
//! 让用户自行提供定时器实现。`Time` 枚举封装了用户提供的 `Timer` trait 对象，
//! 提供统一的 `sleep`、`sleep_until`、`reset`、`now` 等方法。
//!
//! `Dur` 枚举用于区分超时时长的来源——是使用默认值还是用户显式配置的值，
//! 并在检查时根据是否有可用的 Timer 实例做出相应处理（警告或 panic）。
//!
//! 这种设计使得 hyper 可以在没有 tokio 的环境中运行，只要用户提供了
//! 兼容的 Timer 实现即可。

// `Duration` 用于表示超时时长（仅在需要 sleep 的 feature 组合下引入）
#[cfg(any(
    all(any(feature = "client", feature = "server"), feature = "http2"),
    all(feature = "server", feature = "http1"),
))]
use std::time::Duration;
// `fmt` 用于 Debug 实现，`Arc` 用于共享 Timer trait 对象的所有权
use std::{fmt, sync::Arc};
// `Pin` 用于包装 Sleep Future（Sleep 通常是 !Unpin 的），`Instant` 用于表示时间点
use std::{pin::Pin, time::Instant};

// hyper 的异步运行时抽象 trait：Sleep 是异步等待 Future，Timer 是创建 Sleep 的工厂
use crate::rt::Sleep;
use crate::rt::Timer;

/// 用户提供的定时器封装。
///
/// `Time` 是 hyper 内部统一使用的定时器接口，封装了一个可选的 `Timer` trait 对象。
/// - `Timer(Arc<dyn Timer + Send + Sync>)`: 持有用户提供的定时器实现
/// - `Empty`: 未配置定时器的状态
///
/// 使用 `Arc<dyn Timer + Send + Sync>` 使其可在多个连接间共享，
/// 并且满足跨线程发送的要求。
#[derive(Clone)]
pub(crate) enum Time {
    /// 持有用户提供的 Timer trait 对象（通过 Arc 共享所有权）
    Timer(Arc<dyn Timer + Send + Sync>),
    /// 未配置定时器的空状态
    Empty,
}

/// 超时时长配置枚举。
///
/// 用于区分超时时长的来源，决定在缺少 Timer 时的行为：
/// - `Default`: 使用默认超时值。如果没有配置 Timer，仅发出警告并忽略超时
/// - `Configured`: 用户显式配置的超时值。如果没有配置 Timer，则 panic
///
/// 内部的 `Option<Duration>` 为 `None` 表示该超时被禁用。
#[cfg(all(feature = "server", feature = "http1"))]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dur {
    /// 默认超时值（可选）。缺少 Timer 时仅警告
    Default(Option<Duration>),
    /// 用户显式配置的超时值（可选）。缺少 Timer 时 panic
    Configured(Option<Duration>),
}

/// 为 `Time` 实现 `Debug` trait。
///
/// 仅输出类型名称，不暴露内部 Timer 的细节（因为 trait 对象不一定实现 Debug）。
impl fmt::Debug for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Time").finish()
    }
}

/// `Time` 的方法实现，提供各种定时器操作。
impl Time {
    /// 创建一个在指定 `duration` 后就绪的异步 Sleep Future。
    ///
    /// # Panics
    /// 如果 `Time` 为 `Empty`（未配置定时器），则 panic。
    /// 调用者应在使用前确保已配置 Timer。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(crate) fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        match *self {
            Time::Empty => {
                panic!("You must supply a timer.")
            }
            Time::Timer(ref t) => t.sleep(duration),
        }
    }

    /// 创建一个在指定 `deadline` 时间点就绪的异步 Sleep Future。
    ///
    /// # Panics
    /// 如果 `Time` 为 `Empty`（未配置定时器），则 panic。
    #[cfg(all(feature = "server", feature = "http1"))]
    pub(crate) fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
        match *self {
            Time::Empty => {
                panic!("You must supply a timer.")
            }
            Time::Timer(ref t) => t.sleep_until(deadline),
        }
    }

    /// 获取当前时间。
    ///
    /// 如果配置了 Timer，使用 Timer 提供的时间源（可能是模拟时间）；
    /// 否则回退到 `Instant::now()`。这使得在测试中可以注入模拟时间。
    pub(crate) fn now(&self) -> Instant {
        match *self {
            Time::Empty => Instant::now(),
            Time::Timer(ref t) => t.now(),
        }
    }

    /// 重置一个已有的 Sleep Future 到新的截止时间。
    ///
    /// 这比创建新的 Sleep 更高效，因为可以复用已有的 Future 分配。
    ///
    /// # Panics
    /// 如果 `Time` 为 `Empty`（未配置定时器），则 panic。
    pub(crate) fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: Instant) {
        match *self {
            Time::Empty => {
                panic!("You must supply a timer.")
            }
            Time::Timer(ref t) => t.reset(sleep, new_deadline),
        }
    }

    /// 检查超时配置并返回有效的 Duration。
    ///
    /// 根据 `Dur` 的类型（默认值或用户配置）和当前 Timer 的可用性，
    /// 决定返回 `Some(duration)` 还是 `None`：
    /// - `Dur::Default(Some(dur))` + 无 Timer -> 发出警告日志，返回 `None`
    /// - `Dur::Configured(Some(dur))` + 无 Timer -> panic（用户显式要求了超时但没配置 Timer）
    /// - 有 Timer -> 返回 `Some(dur)`
    /// - `None` -> 超时被禁用，返回 `None`
    #[cfg(all(feature = "server", feature = "http1"))]
    pub(crate) fn check(&self, dur: Dur, name: &'static str) -> Option<Duration> {
        match dur {
            Dur::Default(Some(dur)) => match self {
                Time::Empty => {
                    // 默认超时存在但没有 Timer，仅警告不 panic
                    warn!("timeout `{}` has default, but no timer set", name,);
                    None
                }
                Time::Timer(..) => Some(dur),
            },
            Dur::Configured(Some(dur)) => match self {
                // 用户显式配置了超时但没有 Timer，这是编程错误，直接 panic
                Time::Empty => panic!("timeout `{}` set, but no timer set", name,),
                Time::Timer(..) => Some(dur),
            },
            // 超时被禁用（None），无论有无 Timer 都返回 None
            Dur::Default(None) | Dur::Configured(None) => None,
        }
    }
}
