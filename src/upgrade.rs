//! HTTP 升级（Upgrade）模块
//!
//! 本模块处理 hyper 中的 [HTTP 升级][mdn] 机制。由于 HTTP 中有多种概念
//! 允许先进行 HTTP 通信，然后转换到不同的协议，本模块将它们统一到一个 API 中。
//! 这些概念包括：
//!
//! - HTTP/1.1 Upgrade（如 WebSocket 升级）
//! - HTTP `CONNECT`（如 HTTP 隧道代理）
//!
//! 你需要自行处理建立升级的其他前置条件，例如发送适当的头部、方法和状态码。
//! 然后可以使用 [`on`] 函数获取一个 `Future`，该 Future 将解析为升级后的
//! 连接对象，或在升级失败时返回错误。
//!
//! [mdn]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Protocol_upgrade_mechanism
//!
//! # Client
//!
//! 从[客户端](super::client)发起 HTTP 升级需要在 `http::Request` 上设置
//! 适当的方法（如 `CONNECT`）或头部（如 `Upgrade` 和 `Connection`）。
//! 收到 `http::Response` 后，你必须检查服务器是否同意升级
//! （例如检查 `101` 状态码），然后从 `Response` 中获取 `Future`。
//!
//! # Server
//!
//! 在服务端接收升级请求需要检查 `Request` 中的相关头部，
//! 如果需要升级，则在响应中发送对应的头部。然后调用 `on()`
//! 传入 `Request`，等待 hyper 完成升级过程。
//!
//! # Example
//!
//! See [this example][example] showing how upgrades work with both
//! Clients and Servers.
//!
//! [example]: https://github.com/hyperium/hyper/blob/master/examples/upgrades.rs

// 用于运行时类型识别（RTTI），支持 Upgraded 类型的向下转型（downcast）
use std::any::TypeId;
// 标准库错误 trait，重命名以避免与 hyper::Error 冲突
use std::error::Error as StdError;
// 格式化输出 trait
use std::fmt;
// Future trait，OnUpgrade 实现了此 trait
use std::future::Future;
// 标准 I/O 类型
use std::io;
// Pin 用于固定 Future/Stream 在内存中的位置，这是异步编程所必需的
use std::pin::Pin;
// Arc + Mutex 用于在多个 OnUpgrade 克隆之间共享 oneshot::Receiver
use std::sync::{Arc, Mutex};
// 异步任务的 Context 和 Poll 类型
use std::task::{Context, Poll};

// hyper 自定义的 Read/Write trait，基于 tokio 但做了抽象
use crate::rt::{Read, ReadBufCursor, Write};
// 不可变字节缓冲区类型
use bytes::Bytes;
// tokio 的一次性通道，用于在 HTTP 状态机和用户代码之间传递升级结果
use tokio::sync::oneshot;

// 内部的 Rewind I/O 包装器，支持将已读取但未处理的字节"回退"到流中
use crate::common::io::Rewind;

/// 升级后的 HTTP 连接。
///
/// 此类型内部持有一个 trait 对象，代表升级前用于 HTTP 通信的原始 I/O 对象。
/// 它可以直接作为 [`Read`] 或 [`Write`] 使用，非常方便。
///
/// 或者，如果已知确切的类型，可以通过 [`downcast`](Upgraded::downcast) 方法
/// 将其解构为各个组成部分。
///
/// # 设计说明
///
/// 使用 `Rewind<Box<dyn Io + Send>>` 作为内部类型：
/// - `Box<dyn Io + Send>` 是类型擦除的 I/O trait 对象，允许存储任意 I/O 类型
/// - `Rewind` 包装器允许在 I/O 流前面"预填充"缓冲区数据（升级过程中可能已读取的额外字节）
pub struct Upgraded {
    io: Rewind<Box<dyn Io + Send>>,
}

/// HTTP 升级的 Future。
///
/// 如果没有可用的升级，或升级不成功，将产生一个 `Error`。
///
/// 内部使用 `Arc<Mutex<oneshot::Receiver>>` 使得 `OnUpgrade` 可以被 `Clone`，
/// 这在需要在多处等待同一个升级结果时非常有用。
/// `Mutex` 是必需的，因为 `oneshot::Receiver` 的 `poll` 方法需要 `&mut self`。
#[derive(Clone)]
pub struct OnUpgrade {
    // Option 允许表示"没有升级可用"的状态（rx 为 None）
    // Arc<Mutex<>> 使得此类型可以安全地 Clone 和跨线程共享
    rx: Option<Arc<Mutex<oneshot::Receiver<crate::Result<Upgraded>>>>>,
}

/// [`Upgraded`] 类型解构后的各部分。
///
/// 包含原始的 I/O 类型和一个字节缓冲区，该缓冲区包含 HTTP 状态机
/// 可能在完成升级之前已经读取的字节。
///
/// `#[non_exhaustive]` 属性允许 hyper 在未来的小版本中添加新字段而不破坏兼容性。
#[derive(Debug)]
#[non_exhaustive]
pub struct Parts<T> {
    /// 升级前使用的原始 I/O 对象。
    pub io: T,
    /// 已读取但未作为 HTTP 处理的字节缓冲区。
    ///
    /// 例如，如果 `Connection` 用于 HTTP 升级请求，
    /// 服务器可能在升级响应中一起发送了新协议的第一批字节。
    ///
    /// 如果你计划继续在 I/O 对象上通信，需要检查此缓冲区中是否有已存在的字节。
    pub read_buf: Bytes,
}

/// 从 HTTP 消息中获取待处理的 HTTP 升级。
///
/// 此函数可以在以下类型上调用：
///
/// - `http::Request<B>`
/// - `http::Response<B>`
/// - `&mut http::Request<B>`
/// - `&mut http::Response<B>`
///
/// 升级信息存储在 HTTP 消息的 extensions 中。此函数通过 `sealed::CanUpgrade`
/// trait 从 extensions 中提取 `OnUpgrade`。如果消息中没有升级信息，
/// 返回的 `OnUpgrade` 在被 await 时将立即产生一个错误。
pub fn on<T: sealed::CanUpgrade>(msg: T) -> OnUpgrade {
    msg.on_upgrade()
}

/// 表示一个待完成的升级操作。
///
/// 内部持有 `oneshot::Sender`，当 HTTP 连接完成升级时，
/// hyper 的 HTTP 状态机会通过此 sender 发送升级后的连接对象。
///
/// 仅在启用了协议版本和角色时编译。
#[cfg(all(
    any(feature = "client", feature = "server"),
    any(feature = "http1", feature = "http2"),
))]
pub(super) struct Pending {
    tx: oneshot::Sender<crate::Result<Upgraded>>,
}

/// 创建一对 `Pending` 和 `OnUpgrade`。
///
/// 这是 oneshot 通道模式的应用：
/// - `Pending`（持有 Sender）由 hyper 的 HTTP 状态机持有，负责在升级完成时发送结果
/// - `OnUpgrade`（持有 Receiver）由用户代码持有，负责等待升级完成
///
/// 仅在启用了协议版本和角色时编译。
#[cfg(all(
    any(feature = "client", feature = "server"),
    any(feature = "http1", feature = "http2"),
))]
pub(super) fn pending() -> (Pending, OnUpgrade) {
    let (tx, rx) = oneshot::channel();
    (
        Pending { tx },
        OnUpgrade {
            // 将 Receiver 包装在 Arc<Mutex<>> 中，使 OnUpgrade 可以 Clone
            rx: Some(Arc::new(Mutex::new(rx))),
        },
    )
}

// ===== impl Upgraded =====

/// `Upgraded` 的方法实现。
impl Upgraded {
    /// 创建一个新的 `Upgraded` 实例。
    ///
    /// # 参数
    /// - `io`：原始的 I/O 对象，需要实现 Read + Write + Unpin + Send + 'static
    /// - `read_buf`：在升级过程中已预读的字节缓冲区
    ///
    /// 将 I/O 对象装箱为 trait 对象并用 Rewind 包装，
    /// 使得预读的字节可以在后续读取时首先被返回。
    #[cfg(all(
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(super) fn new<T>(io: T, read_buf: Bytes) -> Self
    where
        T: Read + Write + Unpin + Send + 'static,
    {
        Upgraded {
            // Box::new(io) 将具体类型转换为 trait 对象
            // Rewind::new_buffered 创建带预填充缓冲区的 I/O 包装器
            io: Rewind::new_buffered(Box::new(io), read_buf),
        }
    }

    /// 尝试将内部的 trait 对象向下转型（downcast）为指定的具体类型。
    ///
    /// 成功时返回包含向下转型后的各部分的 `Ok(Parts<T>)`；
    /// 失败时返回 `Err(Upgraded)`，将 Upgraded 归还给调用者。
    ///
    /// # 类型参数
    /// - `T`：期望的原始 I/O 类型，必须实现 Read + Write + Unpin + 'static
    ///
    /// # 工作原理
    /// 利用自定义的 `Io` trait 和 `__hyper_downcast` 方法实现类型安全的向下转型。
    /// 这类似于 `std::any::Any::downcast`，但适用于 hyper 自定义的 Io trait 对象。
    pub fn downcast<T: Read + Write + Unpin + 'static>(self) -> Result<Parts<T>, Self> {
        // 先解构 Rewind，获取内部的 I/O 对象和缓冲区
        let (io, buf) = self.io.into_inner();
        // 尝试向下转型
        match io.__hyper_downcast() {
            Ok(t) => Ok(Parts {
                io: *t, // 解引用 Box<T> 获取 T
                read_buf: buf,
            }),
            Err(io) => Err(Upgraded {
                // 转型失败，重新构建 Upgraded
                io: Rewind::new_buffered(io, buf),
            }),
        }
    }
}

/// 为 `Upgraded` 实现异步读取 trait。
///
/// 将读取操作委托给内部的 `Rewind<Box<dyn Io + Send>>`。
/// Rewind 会首先返回预填充缓冲区中的数据，然后才从底层 I/O 读取。
impl Read for Upgraded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // Pin::new 是安全的，因为 Rewind<Box<dyn Io>> 实现了 Unpin
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

/// 为 `Upgraded` 实现异步写入 trait。
///
/// 将所有写入操作委托给内部的 I/O 对象。
impl Write for Upgraded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    /// 向量化写入，可以一次写入多个不连续的缓冲区。
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    /// 刷新写入缓冲区。
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    /// 关闭写入端。
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    /// 返回底层 I/O 是否支持向量化写入。
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

/// 为 `Upgraded` 实现 Debug trait。
///
/// 只输出结构体名称，不暴露内部 I/O 对象的细节
///（因为 trait 对象可能不实现 Debug）。
impl fmt::Debug for Upgraded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upgraded").finish()
    }
}

// ===== impl OnUpgrade =====

/// `OnUpgrade` 的方法实现。
impl OnUpgrade {
    /// 创建一个表示"没有升级"的 `OnUpgrade` 实例。
    ///
    /// 当 await 这个实例时，将立即返回一个 `NoUpgrade` 错误。
    pub(super) fn none() -> Self {
        OnUpgrade { rx: None }
    }

    /// 检查此 `OnUpgrade` 是否为"没有升级"状态。
    ///
    /// 仅在 HTTP/1 + (client 或 server) 时编译。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(super) fn is_none(&self) -> bool {
        self.rx.is_none()
    }
}

/// 为 `OnUpgrade` 实现 `Future` trait。
///
/// 这使得用户可以 `.await` 一个 `OnUpgrade` 来等待升级完成。
///
/// Future 的输出结果有三种情况：
/// 1. `Ok(Upgraded)`：升级成功，返回升级后的连接
/// 2. `Err(Error)`：升级过程中发生错误
/// 3. `Err(Error::Canceled)`：oneshot 通道被关闭（Sender 被 drop），
///    表示升级被取消（通常是因为连接 Future 没有被正确轮询）
impl Future for OnUpgrade {
    type Output = Result<Upgraded, crate::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.rx {
            Some(ref rx) => {
                // 锁定 Mutex，获取 Receiver 的可变引用，然后 poll
                // unwrap() 是安全的，因为不会在持有锁的情况下 panic
                Pin::new(&mut *rx.lock().unwrap())
                    .poll(cx)
                    .map(|res| match res {
                        // oneshot 成功接收到升级结果
                        Ok(Ok(upgraded)) => Ok(upgraded),
                        // oneshot 接收到错误
                        Ok(Err(err)) => Err(err),
                        // oneshot Sender 被 drop，升级被取消
                        Err(_oneshot_canceled) => {
                            Err(crate::Error::new_canceled().with(UpgradeExpected))
                        }
                    })
            }
            // rx 为 None，表示没有可用的升级
            None => Poll::Ready(Err(crate::Error::new_user_no_upgrade())),
        }
    }
}

/// 为 `OnUpgrade` 实现 Debug trait。
///
/// 只输出结构体名称，不暴露内部状态。
impl fmt::Debug for OnUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnUpgrade").finish()
    }
}

// ===== impl Pending =====

/// `Pending` 的方法实现。
///
/// 仅在启用了协议版本和角色时编译。
#[cfg(all(
    any(feature = "client", feature = "server"),
    any(feature = "http1", feature = "http2")
))]
impl Pending {
    /// 完成升级，将升级后的连接发送给等待的 `OnUpgrade`。
    ///
    /// 通过 oneshot 通道发送 `Ok(upgraded)`。`let _ =` 忽略发送结果，
    /// 因为如果 Receiver 已被 drop，发送失败是可以接受的。
    pub(super) fn fulfill(self, upgraded: Upgraded) {
        trace!("pending upgrade fulfill");
        let _ = self.tx.send(Ok(upgraded));
    }

    /// 不完成待处理的升级，而是通知对方升级将由用户手动处理。
    ///
    /// 这用于底层 API 场景，用户选择自己管理升级过程，
    /// 而不是通过 hyper 的高级 API。
    #[cfg(feature = "http1")]
    pub(super) fn manual(self) {
        #[cfg(any(feature = "http1", feature = "http2"))]
        trace!("pending upgrade handled manually");
        let _ = self.tx.send(Err(crate::Error::new_user_manual_upgrade()));
    }
}

// ===== impl UpgradeExpected =====

/// 升级被期望但被取消时返回的错误原因。
///
/// 这通常意味着实际的 `Conn` future 没有被轮询（poll）并完成升级。
/// 例如，如果用户获取了 `OnUpgrade` future 但没有驱动底层连接，
/// 当连接被 drop 时，oneshot Sender 也会被 drop，导致此错误。
#[derive(Debug)]
struct UpgradeExpected;

/// 为 `UpgradeExpected` 实现 Display trait。
impl fmt::Display for UpgradeExpected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upgrade expected but not completed")
    }
}

/// 为 `UpgradeExpected` 实现标准错误 trait（空实现，无 source）。
impl StdError for UpgradeExpected {}

// ===== impl Io =====

/// hyper 内部使用的 I/O trait，统一了 Read + Write + Unpin + 'static 约束。
///
/// 此 trait 的关键功能是提供运行时类型识别能力（`__hyper_type_id`），
/// 使得 trait 对象可以被安全地向下转型为具体类型。这类似于 `std::any::Any`
/// 的功能，但适用于 hyper 自定义的 trait 组合。
///
/// 前缀 `__hyper_` 用于避免与用户代码的方法名冲突。
pub(super) trait Io: Read + Write + Unpin + 'static {
    /// 返回实现此 trait 的具体类型的 TypeId。
    ///
    /// 默认实现使用 `TypeId::of::<Self>()` 获取具体类型的 TypeId。
    /// 这使得向下转型成为可能。
    fn __hyper_type_id(&self) -> TypeId {
        TypeId::of::<Self>()
    }
}

/// 为所有满足 Read + Write + Unpin + 'static 的类型自动实现 Io trait。
///
/// 这是一个 blanket implementation，使得任何满足约束的类型都自动获得
/// Io trait 及其类型识别能力。
impl<T: Read + Write + Unpin + 'static> Io for T {}

/// 为 `dyn Io + Send` trait 对象实现向下转型方法。
///
/// 这些方法直接在 trait 对象上定义（而非在 trait 中），
/// 因为它们需要操作 `Box<Self>` 等 trait 对象特有的类型。
impl dyn Io + Send {
    /// 检查此 trait 对象的底层类型是否为 `T`。
    ///
    /// 通过比较 TypeId 实现，类似于 `Any::is::<T>()`。
    fn __hyper_is<T: Io>(&self) -> bool {
        let t = TypeId::of::<T>();
        self.__hyper_type_id() == t
    }

    /// 尝试将装箱的 trait 对象向下转型为具体类型 `T`。
    ///
    /// 成功时返回 `Ok(Box<T>)`，失败时返回原始的 `Err(Box<dyn Io + Send>)`。
    ///
    /// # Safety
    /// 内部使用 `unsafe` 进行指针转换，但在转换前通过 `__hyper_is` 检查
    /// 确保类型匹配，因此是安全的。这种模式取自 `std::error::Error::downcast()`。
    fn __hyper_downcast<T: Io>(self: Box<Self>) -> Result<Box<T>, Box<Self>> {
        if self.__hyper_is::<T>() {
            // Taken from `std::error::Error::downcast()`.
            unsafe {
                // 将 Box<dyn Io> 转换为裸指针
                let raw: *mut dyn Io = Box::into_raw(self);
                // 将裸指针重新解释为具体类型的指针，并重新装箱
                Ok(Box::from_raw(raw as *mut T))
            }
        } else {
            Err(self)
        }
    }
}

/// 密封模块（sealed module），防止外部代码实现 `CanUpgrade` trait。
///
/// 密封 trait 模式（sealed trait pattern）是 Rust 中常见的 API 设计模式：
/// 将 trait 定义在私有模块中，使得只有当前 crate 可以为类型实现该 trait，
/// 同时通过公共函数（如 `on()`）暴露功能。这允许 hyper 在不破坏兼容性的
/// 情况下修改 trait 的定义。
mod sealed {
    use super::OnUpgrade;

    /// 密封 trait：标记可以从中提取 HTTP 升级的消息类型。
    ///
    /// 为 `http::Request<B>`、`http::Response<B>` 及其可变引用实现。
    pub trait CanUpgrade {
        /// 从消息中提取 `OnUpgrade` future。
        fn on_upgrade(self) -> OnUpgrade;
    }

    /// 为拥有所有权的 `http::Request<B>` 实现 `CanUpgrade`。
    ///
    /// 从请求的 extensions 中移除（remove）OnUpgrade。
    /// 如果 extensions 中没有 OnUpgrade，返回一个空的 OnUpgrade（将产生 NoUpgrade 错误）。
    impl<B> CanUpgrade for http::Request<B> {
        fn on_upgrade(mut self) -> OnUpgrade {
            self.extensions_mut()
                .remove::<OnUpgrade>()
                .unwrap_or_else(OnUpgrade::none)
        }
    }

    /// 为 `&mut http::Request<B>` 实现 `CanUpgrade`。
    ///
    /// 与拥有所有权版本相同，但通过可变引用操作。
    /// 这允许用户在不消耗 Request 的情况下获取升级 future。
    impl<B> CanUpgrade for &'_ mut http::Request<B> {
        fn on_upgrade(self) -> OnUpgrade {
            self.extensions_mut()
                .remove::<OnUpgrade>()
                .unwrap_or_else(OnUpgrade::none)
        }
    }

    /// 为拥有所有权的 `http::Response<B>` 实现 `CanUpgrade`。
    impl<B> CanUpgrade for http::Response<B> {
        fn on_upgrade(mut self) -> OnUpgrade {
            self.extensions_mut()
                .remove::<OnUpgrade>()
                .unwrap_or_else(OnUpgrade::none)
        }
    }

    /// 为 `&mut http::Response<B>` 实现 `CanUpgrade`。
    impl<B> CanUpgrade for &'_ mut http::Response<B> {
        fn on_upgrade(self) -> OnUpgrade {
            self.extensions_mut()
                .remove::<OnUpgrade>()
                .unwrap_or_else(OnUpgrade::none)
        }
    }
}

/// 升级模块的单元测试。
///
/// 仅在启用了协议版本和角色时编译。
#[cfg(all(
    any(feature = "client", feature = "server"),
    any(feature = "http1", feature = "http2"),
))]
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 `Upgraded::downcast` 方法。
    ///
    /// 验证向下转型到错误类型时返回 Err，
    /// 向下转型到正确类型时返回 Ok。
    #[test]
    fn upgraded_downcast() {
        let upgraded = Upgraded::new(Mock, Bytes::new());

        // 尝试转型为错误的类型，应该失败并返回 Upgraded
        let upgraded = upgraded
            .downcast::<crate::common::io::Compat<std::io::Cursor<Vec<u8>>>>()
            .unwrap_err();

        // 尝试转型为正确的类型 Mock，应该成功
        upgraded.downcast::<Mock>().unwrap();
    }

    // TODO: replace with tokio_test::io when it can test write_buf
    /// 测试用的 Mock I/O 类型。
    ///
    /// 实现了 Read 和 Write trait 的最小化 mock，
    /// 用于测试 Upgraded 的向下转型功能。
    struct Mock;

    /// 为 Mock 实现异步读取。所有方法都标记为 unreachable，
    /// 因为测试只关注向下转型，不需要实际的 I/O 操作。
    impl Read for Mock {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: ReadBufCursor<'_>,
        ) -> Poll<io::Result<()>> {
            unreachable!("Mock::poll_read")
        }
    }

    /// 为 Mock 实现异步写入。
    /// `poll_write` 返回成功（写入了所有字节），
    /// 其他方法标记为 unreachable。
    impl Write for Mock {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            // panic!("poll_write shouldn't be called");
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            unreachable!("Mock::poll_flush")
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            unreachable!("Mock::poll_shutdown")
        }
    }
}
