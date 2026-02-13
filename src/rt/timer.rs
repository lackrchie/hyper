//! 定时器 trait 模块
//!
//! 本模块提供了与定时器相关的 trait 抽象，使 hyper 的超时和延迟操作
//! 能够独立于具体的异步运行时实现。
//!
//! 主要包含两个 trait：
//! - [`Timer`]：提供创建和管理定时 Future 的能力
//! - [`Sleep`]：由定时器返回的 Future，在指定时间后完成（resolve）
//!
//! hyper 使用这些 trait 来实现各种超时机制，例如：
//! - HTTP 连接超时
//! - Keep-alive 超时
//! - 请求/响应读写超时
//!
//! # 使用 tokio 定时器的示例
//!
//! ```rust
//! use std::{
//!     future::Future,
//!     pin::Pin,
//!     task::{Context, Poll},
//!     time::{Duration, Instant},
//! };
//!
//! use pin_project_lite::pin_project;
//! use hyper::rt::{Timer, Sleep};
//!
//! #[derive(Clone, Debug)]
//! pub struct TokioTimer;
//!
//! impl Timer for TokioTimer {
//!     fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
//!         Box::pin(TokioSleep {
//!             inner: tokio::time::sleep(duration),
//!         })
//!     }
//!
//!     fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
//!         Box::pin(TokioSleep {
//!             inner: tokio::time::sleep_until(deadline.into()),
//!         })
//!     }
//!
//!     fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: Instant) {
//!         if let Some(sleep) = sleep.as_mut().downcast_mut_pin::<TokioSleep>() {
//!             sleep.reset(new_deadline.into())
//!         }
//!     }
//! }
//!
//! pin_project! {
//!     pub(crate) struct TokioSleep {
//!         #[pin]
//!         pub(crate) inner: tokio::time::Sleep,
//!     }
//! }
//!
//! impl Future for TokioSleep {
//!     type Output = ();
//!
//!     fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//!         self.project().inner.poll(cx)
//!     }
//! }
//!
//! impl Sleep for TokioSleep {}
//!
//! impl TokioSleep {
//!     pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
//!         self.project().inner.as_mut().reset(deadline.into());
//!     }
//! }
//! ```

// --- 标准库导入 ---

// TypeId 用于运行时类型识别（RTTI），支持 Sleep trait object 的向下转型（downcast）
// Future trait 是异步编程的基础——Sleep 继承了它
// Pin 用于固定 Future，确保 poll 方法调用的安全性
// Duration 表示时间段，Instant 表示时间点
use std::{
    any::TypeId,
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

/// 定时器 trait，提供与定时器相关的功能。
///
/// 实现此 trait 的类型能够创建休眠 Future 并管理截止时间。
/// hyper 内部使用此 trait 来实现各种超时逻辑。
///
/// 此 trait 返回的是 `Pin<Box<dyn Sleep>>` 而非具体类型，
/// 这是因为不同运行时的 Sleep 类型不同，需要使用 trait object 来擦除类型。
pub trait Timer {
    /// 返回一个在 `duration` 时间后完成的 Future。
    ///
    /// # 参数
    /// - `duration`：需要等待的时间段
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>>;

    /// 返回一个在 `deadline` 时间点完成的 Future。
    ///
    /// # 参数
    /// - `deadline`：目标时间点
    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>>;

    /// 返回代表当前时间的 `Instant`。
    ///
    /// 默认实现直接调用 [`Instant::now()`]。
    /// 在测试场景中，可以覆盖此方法以返回模拟时间。
    fn now(&self) -> Instant {
        Instant::now()
    }

    /// 重置一个已有的 sleep Future，使其改为在 `new_deadline` 时间点完成。
    ///
    /// 默认实现会创建一个全新的 sleep Future 来替换现有的。
    /// 优化的实现（如使用 tokio）可以通过向下转型（downcast）来原地重置定时器，
    /// 避免额外的内存分配——这就是 `Sleep` trait 提供 `downcast_mut_pin` 方法的原因。
    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: Instant) {
        *sleep = self.sleep_until(new_deadline);
    }
}

/// 由 `Timer` 返回的休眠 Future trait。
///
/// `Sleep` 继承了以下 trait：
/// - `Send`：可以跨线程传递
/// - `Sync`：可以跨线程共享引用
/// - `Future<Output = ()>`：是一个返回 `()` 的异步 Future
///
/// 此 trait 还提供了运行时类型识别功能（通过 `__type_id` 方法），
/// 使得 `dyn Sleep` trait object 可以被向下转型为具体类型。
/// 这对于 `Timer::reset` 的优化实现至关重要。
pub trait Sleep: Send + Sync + Future<Output = ()> {
    #[doc(hidden)]
    /// 返回实现类型的 `TypeId`，用于支持向下转型。
    ///
    /// 此方法是私有的，不能被下游 crate 实现（通过 `private::Sealed` 参数保证）。
    /// 默认实现通过 `TypeId::of::<Self>()` 返回具体类型的 ID。
    ///
    /// 使用 `Sealed` 参数防止外部覆盖此方法，确保类型 ID 的正确性——
    /// 这是 Rust 中保护 trait 默认实现不被覆盖的常见模式。
    fn __type_id(&self, _: private::Sealed) -> TypeId
    where
        Self: 'static,
    {
        TypeId::of::<Self>()
    }
}

/// 为 `dyn Sleep` trait object 提供向下转型（downcast）方法。
///
/// 这是对 `std::any::Any` 中向下转型方法的重新实现，
/// 因为 `Sleep` trait object 不能直接使用 `Any` 的方法（trait object 的限制）。
impl dyn Sleep {
    //! This is a re-implementation of downcast methods from std::any::Any

    /// 检查此 `dyn Sleep` 的实际类型是否为 `T`。
    ///
    /// 通过比较 `TypeId` 来实现运行时类型检查。
    pub fn is<T>(&self) -> bool
    where
        T: Sleep + 'static,
    {
        // 调用 __type_id 获取实际类型的 ID，与目标类型 T 的 ID 进行比较
        self.__type_id(private::Sealed {}) == TypeId::of::<T>()
    }

    /// 将 `Pin<&mut dyn Sleep>` 向下转型为 `Pin<&mut T>`。
    ///
    /// 如果实际类型是 `T`，返回 `Some(Pin<&mut T>)`；否则返回 `None`。
    ///
    /// 此方法在 `Timer::reset` 的优化实现中非常有用：
    /// 通过向下转型到具体的 Sleep 类型（如 `TokioSleep`），
    /// 可以原地重置定时器而无需重新分配内存。
    pub fn downcast_mut_pin<T>(self: Pin<&mut Self>) -> Option<Pin<&mut T>>
    where
        T: Sleep + 'static,
    {
        if self.is::<T>() {
            unsafe {
                // 从 Pin 中获取裸指针，进行类型转换
                let inner = Pin::into_inner_unchecked(self);
                // 将 &mut dyn Sleep 转为 *mut dyn Sleep 再转为 *mut T，
                // 然后重新包装为 Pin<&mut T>
                Some(Pin::new_unchecked(
                    &mut *(&mut *inner as *mut dyn Sleep as *mut T),
                ))
            }
        } else {
            None
        }
    }
}

/// 私有模块，包含密封类型，防止外部 crate 调用 `__type_id` 方法。
///
/// 由于 `Sealed` 结构体在私有模块中定义，外部 crate 无法构造它的实例，
/// 因此也无法调用或覆盖 `__type_id` 方法——这确保了类型 ID 机制的安全性。
mod private {
    #![allow(missing_debug_implementations)]
    /// 密封类型：仅能在 hyper crate 内部构造
    pub struct Sealed {}
}
