//! hyper 公共工具模块（common）。
//!
//! 本模块汇集了 hyper 内部各处共用的基础工具类型和子模块，包括：
//! - 缓冲区管理（`buf`）
//! - HTTP 日期格式化与缓存（`date`）
//! - 二选一 Future 组合器（`either`）
//! - Future 辅助工具（`future`）
//! - IO 兼容层与回绕读取（`io`）
//! - 异步任务辅助（`task`）
//! - 定时器抽象（`time`）
//! - 单生产者单消费者广播通道（`watch`）
//!
//! 所有子模块均为 `pub(crate)` 可见性，仅供 hyper 内部使用，不对外暴露。
//! 各子模块通过条件编译（`#[cfg(...)]`）按需启用，以减少未使用 feature 时的编译开销。

/// 缓冲区列表模块，提供 `BufList` 类型，用于高效管理多段连续缓冲区。
/// 仅在同时启用 client/server 和 http1 feature 时编译。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
pub(crate) mod buf;
/// HTTP 日期格式化与缓存模块，用于生成符合 RFC 7231 的 Date 响应头。
/// 仅在启用 server 端且 http1 或 http2 任一协议时编译。
#[cfg(all(feature = "server", any(feature = "http1", feature = "http2")))]
pub(crate) mod date;
/// 二选一 Future 枚举模块，提供 `Either<F1, F2>` 类型。
/// 仅在启用 client 和 http2 feature 时编译。
#[cfg(all(feature = "client", feature = "http2"))]
pub(crate) mod either;
/// Future 辅助工具模块，提供 `poll_fn` 等异步编程辅助函数。
/// 在 client+http1/http2 或 server+http1 场景下编译。
#[cfg(any(
    all(feature = "client", any(feature = "http1", feature = "http2")),
    all(feature = "server", feature = "http1"),
))]
pub(crate) mod future;
/// IO 兼容层模块，提供 hyper IO trait 与 tokio IO trait 之间的适配，以及回绕读取功能。
pub(crate) mod io;
/// 异步任务辅助模块，提供 `yield_now` 和 `now_or_never` 等工具函数。
/// 仅在同时启用 client/server 和 http1 feature 时编译。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
pub(crate) mod task;
/// 定时器抽象模块，封装用户提供的 `Timer` trait 对象，统一管理超时操作。
/// 在 server+http1 或 client/server+http2 场景下编译。
#[cfg(any(
    all(feature = "server", feature = "http1"),
    all(any(feature = "client", feature = "server"), feature = "http2"),
))]
pub(crate) mod time;
/// 单生产者单消费者（SPSC）广播通道模块，用于在异步任务间传递状态变更通知。
/// 仅在同时启用 client/server 和 http1 feature 时编译。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
pub(crate) mod watch;
