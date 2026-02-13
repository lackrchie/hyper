//! 客户端请求调度通道（Dispatch Channel）模块
//!
//! 本模块实现了 hyper 客户端内部的请求/响应调度机制。它的核心思想是：
//! 通过一对 `Sender` / `Receiver` 在"用户发送请求的一侧"和"连接状态机处理请求的一侧"
//! 之间建立异步通道（channel），从而实现请求的排队与响应的回调。
//!
//! ## 设计要点
//!
//! - 底层使用 `tokio::sync::mpsc::unbounded_channel` 作为消息传输通道，
//!   但通过 `want` crate 提供的 `Giver` / `Taker` 机制实现了逻辑上的"有界"控制，
//!   确保接收端（连接状态机）准备好后才允许发送端投递新请求。
//! - 对于 HTTP/1，每次只允许一个请求在途（pipeline 不启用时），
//!   通过 `buffered_once` 字段允许缓冲一个请求。
//! - 对于 HTTP/2，使用 `UnboundedSender` 变体，因为 HTTP/2 支持多路复用，
//!   不需要逐个等待接收端就绪。
//! - 每个请求附带一个 `Callback`（基于 `oneshot` 通道），
//!   连接状态机处理完请求后通过该回调返回响应或错误。
//! - `Callback` 分为 `Retry`（可重试，会返回原始请求）和 `NoRetry`（不可重试）两种模式。

// 标准库导入：异步任务上下文和轮询结果类型
use std::task::{Context, Poll};
// 仅在启用 http2 feature 时导入 Future 和 Pin，用于 SendWhen 的 Future 实现
#[cfg(feature = "http2")]
use std::{future::Future, pin::Pin};

// http crate 的 Request/Response 类型，仅 http2 使用（SendWhen 中需要）
#[cfg(feature = "http2")]
use http::{Request, Response};
// http_body crate 的 Body trait，仅 http2 使用（SendWhen 的泛型约束）
#[cfg(feature = "http2")]
use http_body::Body;
// pin_project_lite 宏，用于安全地对结构体字段进行 Pin 投影（projection），仅 http2 使用
#[cfg(feature = "http2")]
use pin_project_lite::pin_project;
// tokio 异步通道原语：mpsc 用于多生产者单消费者通道，oneshot 用于一次性响应回调
use tokio::sync::{mpsc, oneshot};

// hyper 内部的 Incoming body 类型和 HTTP/2 客户端响应 Future 映射类型
#[cfg(feature = "http2")]
use crate::{body::Incoming, proto::h2::client::ResponseFutMap};

/// 可重试的 Promise 类型别名。
///
/// 当请求通过 `try_send` 发送时返回此类型。如果发送过程中连接出错，
/// 错误中会包含原始请求（`TrySendError<T>`），允许调用者重试。
pub(crate) type RetryPromise<T, U> = oneshot::Receiver<Result<U, TrySendError<T>>>;

/// 不可重试的 Promise 类型别名。
///
/// 当请求通过 `send` 发送时返回此类型。出错时仅返回 `crate::Error`，
/// 不包含原始请求。
pub(crate) type Promise<T> = oneshot::Receiver<Result<T, crate::Error>>;

/// 调用 `try_send_request` 时可能产生的错误。
///
/// 在请求被排队到实际写入 IO 传输层之间，连接可能发生错误。
/// 如果发生这种情况，可以安全地将请求返回给调用者，因为请求从未被完整发送出去。
///
/// 该结构体包含错误信息和可选的原始请求消息，允许调用者在适当时候重试。
#[derive(Debug)]
pub struct TrySendError<T> {
    /// 导致发送失败的具体错误
    pub(crate) error: crate::Error,
    /// 可选的原始请求消息。如果请求尚未被序列化到连接上，则会在此返回。
    /// 如果请求已经部分发送，则为 None。
    pub(crate) message: Option<T>,
}

/// 创建一对调度通道（Sender, Receiver）。
///
/// 内部使用 tokio 的无界 mpsc 通道和 `want` crate 的 Giver/Taker 配对。
/// Giver/Taker 用于实现背压（backpressure）：Receiver 通过 Taker 表明自己已准备好接收，
/// Sender 通过 Giver 检查是否可以发送。
///
/// # 类型参数
/// - `T`: 发送的请求类型（通常是 `Request<B>`）
/// - `U`: 期望的响应类型（通常是 `Response<Incoming>`）
pub(crate) fn channel<T, U>() -> (Sender<T, U>, Receiver<T, U>) {
    // 创建无界 mpsc 通道，用于实际的消息传输
    let (tx, rx) = mpsc::unbounded_channel();
    // 创建 want 的 Giver/Taker 对，用于流量控制
    let (giver, taker) = want::new();
    let tx = Sender {
        // 仅 HTTP/1 使用的缓冲标志，初始为 false 表示还未缓冲过消息
        #[cfg(feature = "http1")]
        buffered_once: false,
        giver,
        inner: tx,
    };
    let rx = Receiver { inner: rx, taker };
    (tx, rx)
}

/// 请求的有界发送端。
///
/// 虽然内部的 mpsc 发送端是无界的，但通过 `Giver` 机制实现了逻辑上的有界控制。
/// `Giver` 用于判断 `Receiver` 是否已准备好接收下一个请求。
///
/// 对于 HTTP/1 连接，这确保了请求按顺序处理（一问一答模式）。
///
/// # 类型参数
/// - `T`: 请求类型
/// - `U`: 响应类型
pub(crate) struct Sender<T, U> {
    /// 标记是否已经缓冲了一个消息（即使 Receiver 还未请求）。
    /// HTTP/1 中，即使 Receiver 尚未 poll，也允许缓冲一条消息，
    /// 这样用户可以立即发送第一个请求而不必等待连接完全就绪。
    /// 但第二个请求必须等 Receiver 消费后才能发送。
    #[cfg(feature = "http1")]
    buffered_once: bool,
    /// Giver 用于监控 Receiver 端是否已经 poll 过（即是否"想要"更多消息）。
    /// 当队列为空时，这帮助我们知道一个请求/响应是否已经被完全处理，
    /// 连接已准备好处理更多请求。
    giver: want::Giver,
    /// 实际的无界 mpsc 发送端。逻辑上受 Giver 和 `buffered_once` 的约束。
    inner: mpsc::UnboundedSender<Envelope<T, U>>,
}

/// 无界版本的发送端，用于 HTTP/2 连接。
///
/// 不能 poll Giver（因为需要 Clone 能力），但仍然可以用它来判断 Receiver 是否已被丢弃。
/// 与 `Sender` 不同，此版本可以被克隆，适合 HTTP/2 的多路复用场景，
/// 多个请求可以同时发送到同一个连接上。
#[cfg(feature = "http2")]
pub(crate) struct UnboundedSender<T, U> {
    /// SharedGiver：Giver 的可共享版本，仅用于检查 Receiver 是否已关闭。
    /// 使用 `is_canceled()` 来判断连接是否仍然存活。
    giver: want::SharedGiver,
    /// 实际的无界 mpsc 发送端
    inner: mpsc::UnboundedSender<Envelope<T, U>>,
}

/// `Sender` 的方法实现
impl<T, U> Sender<T, U> {
    /// 异步轮询发送端是否准备好发送下一个请求。
    ///
    /// 内部通过 `giver.poll_want()` 检查 Receiver 端是否已经表示"想要"接收消息。
    /// 如果 Receiver 已被丢弃（连接关闭），返回错误。
    #[cfg(feature = "http1")]
    pub(crate) fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        self.giver
            .poll_want(cx)
            .map_err(|_| crate::Error::new_closed())
    }

    /// 同步检查发送端是否已就绪（即 Receiver 正在等待消息）。
    ///
    /// 这是一个非阻塞的即时检查，不会注册 waker。
    #[cfg(feature = "http1")]
    pub(crate) fn is_ready(&self) -> bool {
        self.giver.is_wanting()
    }

    /// 检查连接（Receiver 端）是否已关闭。
    ///
    /// 如果 Receiver 被丢弃，`is_canceled()` 返回 true。
    #[cfg(feature = "http1")]
    pub(crate) fn is_closed(&self) -> bool {
        self.giver.is_canceled()
    }

    /// 判断当前是否可以发送消息。
    ///
    /// 实现了"允许缓冲一条"的策略：
    /// - 如果 `giver.give()` 返回 true，说明 Receiver 已准备好，可以发送。
    /// - 如果 Receiver 还没准备好，但 `buffered_once` 为 false（还没缓冲过），
    ///   仍然允许发送一条消息，避免死锁（首次请求时 Receiver 可能还未开始 poll）。
    /// - 一旦缓冲过一次，后续发送必须等 Receiver 明确表示就绪。
    #[cfg(feature = "http1")]
    fn can_send(&mut self) -> bool {
        if self.giver.give() || !self.buffered_once {
            // If the receiver is ready *now*, then of course we can send.
            //
            // If the receiver isn't ready yet, but we don't have anything
            // in the channel yet, then allow one message.
            self.buffered_once = true;
            true
        } else {
            false
        }
    }

    /// 尝试发送请求（可重试模式）。
    ///
    /// 如果当前不允许发送（`can_send()` 返回 false），则返回 `Err(val)` 将请求原样返回。
    /// 成功时返回一个 `RetryPromise`，调用者可以 await 该 Promise 获取响应。
    /// 如果连接在处理过程中出错，Promise 会返回 `TrySendError`，其中包含原始请求。
    #[cfg(feature = "http1")]
    pub(crate) fn try_send(&mut self, val: T) -> Result<RetryPromise<T, U>, T> {
        if !self.can_send() {
            return Err(val);
        }
        // 创建一次性通道，tx 端给连接状态机用来发送响应，rx 端作为 Promise 返回给调用者
        let (tx, rx) = oneshot::channel();
        self.inner
            // 将请求和回调封装在 Envelope 中发送
            .send(Envelope(Some((val, Callback::Retry(Some(tx))))))
            .map(move |_| rx)
            // 如果 mpsc 发送失败（Receiver 已关闭），从 Envelope 中取回原始请求
            .map_err(|mut e| (e.0).0.take().expect("envelope not dropped").0)
    }

    /// 发送请求（不可重试模式）。
    ///
    /// 与 `try_send` 类似，但使用 `Callback::NoRetry`，
    /// 出错时 Promise 只返回错误信息，不返回原始请求。
    #[cfg(feature = "http1")]
    pub(crate) fn send(&mut self, val: T) -> Result<Promise<U>, T> {
        if !self.can_send() {
            return Err(val);
        }
        let (tx, rx) = oneshot::channel();
        self.inner
            .send(Envelope(Some((val, Callback::NoRetry(Some(tx))))))
            .map(move |_| rx)
            .map_err(|mut e| (e.0).0.take().expect("envelope not dropped").0)
    }

    /// 将有界 `Sender` 转换为无界 `UnboundedSender`。
    ///
    /// 在 HTTP/2 握手完成后调用，因为 HTTP/2 支持多路复用，
    /// 不需要逐个等待 Receiver 就绪的背压机制。
    /// `self.giver.shared()` 将 Giver 转换为可共享版本（SharedGiver），
    /// 使得 UnboundedSender 可以被克隆。
    #[cfg(feature = "http2")]
    pub(crate) fn unbound(self) -> UnboundedSender<T, U> {
        UnboundedSender {
            giver: self.giver.shared(),
            inner: self.inner,
        }
    }
}

/// `UnboundedSender` 的方法实现（仅 HTTP/2）
#[cfg(feature = "http2")]
impl<T, U> UnboundedSender<T, U> {
    /// 检查发送端是否就绪。
    ///
    /// 对于无界发送端，只要 Receiver 没有被关闭（取消），就认为是就绪的。
    pub(crate) fn is_ready(&self) -> bool {
        !self.giver.is_canceled()
    }

    /// 检查连接（Receiver 端）是否已关闭。
    pub(crate) fn is_closed(&self) -> bool {
        self.giver.is_canceled()
    }

    /// 尝试发送请求（可重试模式），无背压限制。
    ///
    /// 与 `Sender::try_send` 不同，不检查 `can_send()`，直接发送。
    /// 因为 HTTP/2 多路复用不需要等待上一个请求完成。
    pub(crate) fn try_send(&mut self, val: T) -> Result<RetryPromise<T, U>, T> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .send(Envelope(Some((val, Callback::Retry(Some(tx))))))
            .map(move |_| rx)
            .map_err(|mut e| (e.0).0.take().expect("envelope not dropped").0)
    }

    /// 发送请求（不可重试模式），无背压限制。
    pub(crate) fn send(&mut self, val: T) -> Result<Promise<U>, T> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .send(Envelope(Some((val, Callback::NoRetry(Some(tx))))))
            .map(move |_| rx)
            .map_err(|mut e| (e.0).0.take().expect("envelope not dropped").0)
    }
}

/// 为 `UnboundedSender` 实现 `Clone`（仅 HTTP/2）。
///
/// HTTP/2 支持多路复用，多个任务可能需要同时通过同一连接发送请求，
/// 因此 UnboundedSender 必须可克隆。内部的 SharedGiver 和 mpsc::UnboundedSender
/// 都支持 Clone。
#[cfg(feature = "http2")]
impl<T, U> Clone for UnboundedSender<T, U> {
    fn clone(&self) -> Self {
        UnboundedSender {
            giver: self.giver.clone(),
            inner: self.inner.clone(),
        }
    }
}

/// 请求的接收端，由连接状态机持有。
///
/// 连接状态机通过 `poll_recv` 从通道中获取请求和对应的回调，
/// 处理请求后通过回调返回响应。
pub(crate) struct Receiver<T, U> {
    /// 内部的 mpsc 无界接收端
    inner: mpsc::UnboundedReceiver<Envelope<T, U>>,
    /// Taker：与 Sender 端的 Giver 配对，用于通知 Sender "我已准备好接收更多消息"
    taker: want::Taker,
}

/// `Receiver` 的方法实现
impl<T, U> Receiver<T, U> {
    /// 异步轮询接收下一个请求。
    ///
    /// 返回 `Poll::Ready(Some((T, Callback)))` 表示收到一个请求；
    /// 返回 `Poll::Ready(None)` 表示所有 Sender 已丢弃（通道关闭）；
    /// 返回 `Poll::Pending` 表示当前没有请求，并通过 `taker.want()` 通知 Sender
    /// "Receiver 已准备好接收"。
    pub(crate) fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<(T, Callback<T, U>)>> {
        match self.inner.poll_recv(cx) {
            Poll::Ready(item) => {
                // 从 Envelope 中取出请求和回调。Envelope 使用 Option 包装以支持 take() 语义。
                Poll::Ready(item.map(|mut env| env.0.take().expect("envelope not dropped")))
            }
            Poll::Pending => {
                // 没有请求可接收，通知 Giver 端 "Receiver 正在等待"
                self.taker.want();
                Poll::Pending
            }
        }
    }

    /// 关闭接收端。
    ///
    /// 首先通过 `taker.cancel()` 通知 Sender 端连接已关闭，
    /// 然后关闭 mpsc 接收端，不再接受新消息。
    /// 已在通道中的消息在被 drop 时会自动通过回调发送取消错误。
    #[cfg(feature = "http1")]
    pub(crate) fn close(&mut self) {
        self.taker.cancel();
        self.inner.close();
    }

    /// 非阻塞地尝试接收一个请求。
    ///
    /// 使用 `now_or_never` 工具函数将异步 recv 转为同步尝试。
    /// 如果有消息立即可用则返回 `Some`，否则返回 `None`。
    /// 用于连接状态机在特定时机检查是否有待处理的请求。
    #[cfg(feature = "http1")]
    pub(crate) fn try_recv(&mut self) -> Option<(T, Callback<T, U>)> {
        match crate::common::task::now_or_never(self.inner.recv()) {
            Some(Some(mut env)) => env.0.take(),
            _ => None,
        }
    }
}

/// `Receiver` 的 Drop 实现。
///
/// 当 Receiver 被丢弃时，首先通过 `taker.cancel()` 通知 Giver 端，
/// 然后再让 mpsc::Receiver 自然 drop。顺序很重要：
/// 先通知 Giver 可以确保 Sender 端能立即检测到连接已关闭，
/// 而不是等到下次尝试发送时才发现。
impl<T, U> Drop for Receiver<T, U> {
    fn drop(&mut self) {
        // Notify the giver about the closure first, before dropping
        // the mpsc::Receiver.
        self.taker.cancel();
    }
}

/// 信封（Envelope）：包装请求和回调的传输容器。
///
/// 使用 `Option` 包装内部元组，以支持 `take()` 语义——
/// 在消息被消费后将内容取出，同时在 Drop 时检测消息是否被正确处理。
struct Envelope<T, U>(Option<(T, Callback<T, U>)>);

/// `Envelope` 的 Drop 实现。
///
/// 如果 Envelope 在消息未被消费的情况下被丢弃（例如通道被关闭），
/// 则自动通过回调向调用者发送"连接已关闭"的取消错误，
/// 并将原始请求一并返回（如果是可重试回调的话）。
/// 这确保了调用者的 Promise 不会永远挂起（hang）。
impl<T, U> Drop for Envelope<T, U> {
    fn drop(&mut self) {
        if let Some((val, cb)) = self.0.take() {
            // 消息未被消费就被丢弃，通知调用者连接已关闭
            cb.send(Err(TrySendError {
                error: crate::Error::new_canceled().with("connection closed"),
                message: Some(val),
            }));
        }
    }
}

/// 回调枚举：用于将响应或错误返回给请求的发起者。
///
/// 每个通过 dispatch 通道发送的请求都附带一个 Callback，
/// 连接状态机处理完请求后通过它发送结果。
///
/// - `Retry`: 可重试模式，出错时会将原始请求一起返回（`TrySendError<T>`）
/// - `NoRetry`: 不可重试模式，出错时只返回错误信息（`crate::Error`）
///
/// 内部使用 `Option<oneshot::Sender<...>>` 是为了在 Drop 时能够 `take()` 出来发送错误，
/// 避免调用者的 Promise 永远 pending。
pub(crate) enum Callback<T, U> {
    #[allow(unused)]
    Retry(Option<oneshot::Sender<Result<U, TrySendError<T>>>>),
    NoRetry(Option<oneshot::Sender<Result<U, crate::Error>>>),
}

/// `Callback` 的 Drop 实现。
///
/// 当 Callback 在未被显式使用（send）的情况下被丢弃时，
/// 自动向调用者发送 "dispatch gone" 错误，确保 Promise 不会永远挂起。
/// 这是一种防御性编程，处理连接状态机意外退出的情况。
impl<T, U> Drop for Callback<T, U> {
    fn drop(&mut self) {
        match self {
            Callback::Retry(tx) => {
                if let Some(tx) = tx.take() {
                    // 尝试发送错误，忽略发送失败（调用者可能已放弃等待）
                    let _ = tx.send(Err(TrySendError {
                        error: dispatch_gone(),
                        message: None,
                    }));
                }
            }
            Callback::NoRetry(tx) => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(Err(dispatch_gone()));
                }
            }
        }
    }
}

/// 生成 "dispatch gone" 错误。
///
/// `#[cold]` 属性提示编译器这个函数很少被调用（冷路径），
/// 有助于优化热路径的指令缓存布局。
///
/// 根据当前线程是否正在 panic 来生成不同的错误消息：
/// - 如果正在 panic，说明用户代码导致了 panic
/// - 否则，说明运行时丢弃了 dispatch 任务
#[cold]
fn dispatch_gone() -> crate::Error {
    // FIXME(nox): What errors do we want here?
    crate::Error::new_user_dispatch_gone().with(if std::thread::panicking() {
        "user code panicked"
    } else {
        "runtime dropped the dispatch task"
    })
}

/// `Callback` 的核心方法实现
impl<T, U> Callback<T, U> {
    /// 检查回调的接收端是否已取消（调用者已放弃等待响应）。
    ///
    /// 通过检查 oneshot 通道的 `is_closed()` 来判断。
    /// 这在 HTTP/2 中用于在发送响应前检查是否还有人在等待。
    #[cfg(feature = "http2")]
    pub(crate) fn is_canceled(&self) -> bool {
        match *self {
            // 使用 ref 模式匹配来借用而非移动内部值
            Callback::Retry(Some(ref tx)) => tx.is_closed(),
            Callback::NoRetry(Some(ref tx)) => tx.is_closed(),
            // Option 为 None 的情况不应发生（已被 take），故 unreachable
            _ => unreachable!(),
        }
    }

    /// 异步轮询回调的接收端是否已取消。
    ///
    /// 与 `is_canceled()` 不同，这个方法会注册 waker，
    /// 当调用者取消等待时会得到通知。用于连接状态机在处理请求时
    /// 同时监听调用者是否已放弃。
    pub(crate) fn poll_canceled(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match *self {
            Callback::Retry(Some(ref mut tx)) => tx.poll_closed(cx),
            Callback::NoRetry(Some(ref mut tx)) => tx.poll_closed(cx),
            _ => unreachable!(),
        }
    }

    /// 通过回调发送响应结果。
    ///
    /// 使用 `mut self` 消费 Callback，从 Option 中 take 出 oneshot::Sender 并发送结果。
    /// 对于 `NoRetry` 变体，会将 `TrySendError` 映射为仅包含错误信息的 `crate::Error`，
    /// 丢弃可能附带的原始请求。
    pub(crate) fn send(mut self, val: Result<U, TrySendError<T>>) {
        match self {
            Callback::Retry(ref mut tx) => {
                // unwrap 安全：send 只会被调用一次，Option 此时一定是 Some
                let _ = tx.take().unwrap().send(val);
            }
            Callback::NoRetry(ref mut tx) => {
                // 将 TrySendError<T> 映射为 crate::Error，丢弃原始消息
                let _ = tx.take().unwrap().send(val.map_err(|e| e.error));
            }
        }
    }
}

/// `TrySendError` 的公共 API 方法
impl<T> TrySendError<T> {
    /// 从错误中取出原始请求消息。
    ///
    /// 消息不一定总是可以恢复的。如果错误发生在请求已经被序列化到连接之后，
    /// 则消息将不可用（返回 None）。
    pub fn take_message(&mut self) -> Option<T> {
        self.message.take()
    }

    /// 返回对已恢复消息的引用。
    ///
    /// 消息不一定总是可以恢复的。如果错误发生在请求已经被序列化到连接之后，
    /// 则消息将不可用（返回 None）。
    pub fn message(&self) -> Option<&T> {
        self.message.as_ref()
    }

    /// 消费此错误并返回内部的 `crate::Error`。
    pub fn into_error(self) -> crate::Error {
        self.error
    }

    /// 返回对内部错误的引用。
    pub fn error(&self) -> &crate::Error {
        &self.error
    }
}

// HTTP/2 场景下的条件发送 Future。
//
// SendWhen 用于将一个 HTTP/2 响应 Future 与对应的回调绑定在一起。
// 当响应 Future 完成时，通过回调将结果发送给调用者。
// 如果在响应完成之前调用者取消了等待，则提前结束。
//
// 使用 pin_project! 宏生成安全的 Pin 投影代码，
// 使得可以分别 pin 投影到 when 和 call_back 字段。
#[cfg(feature = "http2")]
pin_project! {
    pub struct SendWhen<B, E>
    where
        B: Body,
        B: 'static,
    {
        // HTTP/2 响应的 Future，被 pin 投影以支持异步轮询
        #[pin]
        pub(crate) when: ResponseFutMap<B, E>,
        // 响应就绪后用于发送结果的回调。
        // 使用 Option 包装以支持在 poll 过程中临时取出和放回。
        #[pin]
        pub(crate) call_back: Option<Callback<Request<B>, Response<Incoming>>>,
    }
}

/// 为 `SendWhen` 实现 `Future` trait（仅 HTTP/2）。
///
/// 该 Future 的职责是：
/// 1. 轮询 HTTP/2 响应 Future（`when`）
/// 2. 如果响应就绪，通过回调发送给调用者
/// 3. 如果调用者已取消等待，提前结束
/// 4. 如果响应出错，通过回调发送错误
#[cfg(feature = "http2")]
impl<B, E> Future for SendWhen<B, E>
where
    B: Body + 'static,
    E: crate::rt::bounds::Http2UpgradedExec<B::Data>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 使用 pin_project 生成的 project() 方法安全地获取各字段的 Pin 投影
        let mut this = self.project();

        // 从 Option 中取出 call_back，如果已被取出则 panic（不应重复 poll 已完成的 Future）
        let mut call_back = this.call_back.take().expect("polled after complete");

        match Pin::new(&mut this.when).poll(cx) {
            Poll::Ready(Ok(res)) => {
                // 响应成功，通过回调发送
                call_back.send(Ok(res));
                Poll::Ready(())
            }
            Poll::Pending => {
                // 响应尚未就绪，检查调用者是否已取消等待
                // check if the callback is canceled
                match call_back.poll_canceled(cx) {
                    Poll::Ready(v) => v,
                    Poll::Pending => {
                        // 调用者仍在等待，将 call_back 放回原处，继续等待
                        // Move call_back back to struct before return
                        this.call_back.set(Some(call_back));
                        return Poll::Pending;
                    }
                };
                // 调用者已取消，记录日志并结束
                trace!("send_when canceled");
                Poll::Ready(())
            }
            Poll::Ready(Err((error, message))) => {
                // 响应出错，通过回调将错误和可能的原始请求发送给调用者
                call_back.send(Err(TrySendError { error, message }));
                Poll::Ready(())
            }
        }
    }
}

// ==================== 以下是 dispatch 模块的单元测试 ====================

#[cfg(test)]
mod tests {
    // 引入 nightly 专用的 benchmark 框架（仅在 nightly 编译器上可用）
    #[cfg(feature = "nightly")]
    extern crate test;

    // 标准库的 Future 相关类型，用于测试中的异步操作
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // 从父模块导入测试所需的类型
    use super::{channel, Callback, Receiver};

    /// 测试用的自定义类型，用于验证泛型通道的行为。
    /// `#[allow(dead_code)]` 抑制字段未使用的警告（字段仅用于调试输出）。
    #[derive(Debug)]
    struct Custom(#[allow(dead_code)] i32);

    /// 为 `Receiver` 实现 `Future` trait，方便在测试中直接 await。
    ///
    /// 这个实现仅用于测试目的，将 `poll_recv` 包装为标准的 `Future` 接口。
    impl<T, U> Future for Receiver<T, U> {
        type Output = Option<(T, Callback<T, U>)>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.poll_recv(cx)
        }
    }

    /// 辅助结构体：用于将一个 Future 轮询恰好一次。
    ///
    /// 如果 Future 立即就绪则返回 `Some(())`，如果 Pending 则返回 `None`。
    /// 这在测试中非常有用，可以精确控制异步操作的执行步骤。
    /// Helper to check if the future is ready after polling once.
    struct PollOnce<'a, F>(&'a mut F);

    /// `PollOnce` 的 `Future` 实现。
    ///
    /// 无论内部 Future 是 Ready 还是 Pending，`PollOnce` 自身总是立即 Ready，
    /// 只是通过 `Option<()>` 来表示内部 Future 的状态。
    impl<F, T> Future for PollOnce<'_, F>
    where
        F: Future<Output = T> + Unpin,
    {
        type Output = Option<()>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match Pin::new(&mut self.0).poll(cx) {
                Poll::Ready(_) => Poll::Ready(Some(())),
                Poll::Pending => Poll::Ready(None),
            }
        }
    }

    /// 测试：丢弃 Receiver 时，已发送的请求会收到取消错误。
    ///
    /// 验证流程：
    /// 1. 创建通道并轮询 Receiver 一次（使 try_send 能够成功）
    /// 2. 发送一个请求
    /// 3. 丢弃 Receiver（模拟连接关闭）
    /// 4. 验证 Promise 返回带有原始消息的取消错误
    #[cfg(not(miri))]
    #[tokio::test]
    async fn drop_receiver_sends_cancel_errors() {
        let _ = pretty_env_logger::try_init();

        let (mut tx, mut rx) = channel::<Custom, ()>();

        // must poll once for try_send to succeed
        assert!(PollOnce(&mut rx).await.is_none(), "rx empty");

        let promise = tx.try_send(Custom(43)).unwrap();
        drop(rx);

        let fulfilled = promise.await;
        let err = fulfilled
            .expect("fulfilled")
            .expect_err("promise should error");
        match (err.error.is_canceled(), err.message) {
            (true, Some(_)) => (),
            e => panic!("expected Error::Cancel(_), found {:?}", e),
        }
    }

    /// 测试：Sender 的 want 机制（背压控制）。
    ///
    /// 验证流程：
    /// 1. 第一个请求可以缓冲（buffered_once 机制）
    /// 2. 第二个请求被拒绝（Receiver 尚未表示就绪）
    /// 3. Receiver 消费一个请求后
    /// 4. 第二个请求仍然被拒绝（Receiver 需要再次 poll 才能标记就绪）
    /// 5. Receiver 再次 poll（返回 Pending），此时第二个请求可以发送
    #[cfg(not(miri))]
    #[tokio::test]
    async fn sender_checks_for_want_on_send() {
        let (mut tx, mut rx) = channel::<Custom, ()>();

        // one is allowed to buffer, second is rejected
        let _ = tx.try_send(Custom(1)).expect("1 buffered");
        tx.try_send(Custom(2)).expect_err("2 not ready");

        assert!(PollOnce(&mut rx).await.is_some(), "rx once");

        // Even though 1 has been popped, only 1 could be buffered for the
        // lifetime of the channel.
        tx.try_send(Custom(2)).expect_err("2 still not ready");

        assert!(PollOnce(&mut rx).await.is_none(), "rx empty");

        let _ = tx.try_send(Custom(2)).expect("2 ready");
    }

    /// 测试：UnboundedSender 不受 want 机制限制。
    ///
    /// 验证 HTTP/2 的无界发送端可以连续发送多个请求而不被拒绝，
    /// 只有在 Receiver 被丢弃后才会发送失败。
    #[cfg(feature = "http2")]
    #[test]
    fn unbounded_sender_doesnt_bound_on_want() {
        let (tx, rx) = channel::<Custom, ()>();
        // 将有界 Sender 转换为无界 UnboundedSender
        let mut tx = tx.unbound();

        // 连续发送3个请求，均应成功
        let _ = tx.try_send(Custom(1)).unwrap();
        let _ = tx.try_send(Custom(2)).unwrap();
        let _ = tx.try_send(Custom(3)).unwrap();

        // 丢弃 Receiver 后，发送应失败
        drop(rx);

        let _ = tx.try_send(Custom(4)).unwrap_err();
    }

    /// 基准测试：Giver 队列的吞吐量（仅 nightly）。
    ///
    /// 测量通过 dispatch 通道发送一个请求并消费响应的完整往返时间。
    #[cfg(feature = "nightly")]
    #[bench]
    fn giver_queue_throughput(b: &mut test::Bencher) {
        use crate::{body::Incoming, Request, Response};

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut tx, mut rx) = channel::<Request<Incoming>, Response<Incoming>>();

        b.iter(move || {
            let _ = tx.send(Request::new(Incoming::empty())).unwrap();
            rt.block_on(async {
                loop {
                    let poll_once = PollOnce(&mut rx);
                    let opt = poll_once.await;
                    if opt.is_none() {
                        break;
                    }
                }
            });
        })
    }

    /// 基准测试：Receiver 未就绪时的 poll 性能（仅 nightly）。
    ///
    /// 测量在没有消息时 poll Receiver 的开销。
    #[cfg(feature = "nightly")]
    #[bench]
    fn giver_queue_not_ready(b: &mut test::Bencher) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (_tx, mut rx) = channel::<i32, ()>();
        b.iter(move || {
            rt.block_on(async {
                let poll_once = PollOnce(&mut rx);
                assert!(poll_once.await.is_none());
            });
        })
    }

    /// 基准测试：Taker 取消操作的性能（仅 nightly）。
    ///
    /// 测量调用 `taker.cancel()` 的开销。
    #[cfg(feature = "nightly")]
    #[bench]
    fn giver_queue_cancel(b: &mut test::Bencher) {
        let (_tx, mut rx) = channel::<i32, ()>();

        b.iter(move || {
            rx.taker.cancel();
        })
    }
}
