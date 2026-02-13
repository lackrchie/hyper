//! 二选一 Future 组合器模块。
//!
//! 本模块提供 `Either<F1, F2>` 枚举类型，用于在编译时将两个不同类型但具有
//! 相同输出类型的 Future 统一为一个类型。这在 hyper 的 HTTP/2 客户端实现中
//! 被用于处理条件分支中返回不同 Future 类型的场景。
//!
//! 例如，当一个函数可能根据条件返回 Future A 或 Future B 时，
//! 可以用 `Either::left(future_a)` 或 `Either::right(future_b)` 包装，
//! 使返回类型统一为 `Either<A, B>`。
//!
//! 该类型使用 `pin_project_lite` 生成安全的 Pin 投影代码，
//! 确保内部 Future 的 Pin 约束被正确传递。

// `pin_project_lite` 提供轻量级的 Pin 投影宏，用于安全地访问 Pin 包裹结构体的内部字段。
// 相比 `pin-project`，它是纯声明宏实现，编译速度更快。
use pin_project_lite::pin_project;
// 标准库的 Future 相关类型
use std::{
    // `Future` trait 是异步编程的核心抽象
    future::Future,
    // `Pin` 用于确保 Future 在内存中不被移动（self-referential 安全性）
    pin::Pin,
    // `Context` 携带 Waker，`Poll` 表示异步操作的就绪状态
    task::{Context, Poll},
};

// 使用 `pin_project!` 宏为 `Either` 枚举生成 Pin 投影类型 `EitherProj`。
// `#[pin]` 属性标记需要进行 Pin 投影的字段（即内部 Future），
// 这样在 `poll` 方法中可以安全地获取被 Pin 住的内部 Future 的可变引用。
pin_project! {
    /// 二选一 Future 枚举，可包含两个不同类型但输出相同的 Future 之一。
    ///
    /// # 投影
    /// `#[project = EitherProj]` 指定投影类型的名称，
    /// 在 `poll` 方法中通过 `self.project()` 获取投影后的枚举变体。
    #[project = EitherProj]
    pub(crate) enum Either<F1, F2> {
        // 左变体，包含类型为 F1 的 Future
        Left {
            // #[pin] 标记该字段需要 Pin 投影
            #[pin]
            fut: F1
        },
        // 右变体，包含类型为 F2 的 Future
        Right {
            // #[pin] 标记该字段需要 Pin 投影
            #[pin]
            fut: F2,
        },
    }
}

/// `Either` 的便捷构造方法。
impl<F1, F2> Either<F1, F2> {
    /// 将一个 Future 包装为 `Either::Left` 变体。
    pub(crate) fn left(fut: F1) -> Self {
        Either::Left { fut }
    }

    /// 将一个 Future 包装为 `Either::Right` 变体。
    pub(crate) fn right(fut: F2) -> Self {
        Either::Right { fut }
    }
}

/// 为 `Either<F1, F2>` 实现 `Future` trait。
///
/// # 约束
/// - `F1` 必须实现 `Future`
/// - `F2` 必须实现 `Future`，且其输出类型与 `F1` 相同（`Output = F1::Output`）
///
/// 这确保了无论 `Either` 持有哪个变体，轮询结果的类型始终一致。
impl<F1, F2> Future for Either<F1, F2>
where
    F1: Future,
    F2: Future<Output = F1::Output>,
{
    /// 输出类型与 `F1` 的输出类型相同
    type Output = F1::Output;

    /// 轮询内部的 Future。
    ///
    /// 通过 `self.project()` 进行 Pin 投影，安全地获取内部 Future 的 `Pin<&mut>` 引用，
    /// 然后委托给具体变体的 `poll` 方法。
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Pin 投影：将 Pin<&mut Either> 转为 EitherProj，安全地解构枚举
        match self.project() {
            EitherProj::Left { fut } => fut.poll(cx),
            EitherProj::Right { fut } => fut.poll(cx),
        }
    }
}
