// ============================================================================
// FFI 任务（Task）模块
// ============================================================================
// 本模块为 hyper 的 C 语言 FFI（外部函数接口）提供异步任务执行框架。
// 核心概念：
//   - hyper_executor：任务执行器，负责驱动所有异步任务的执行
//   - hyper_task：异步任务，代表一个最终会产生结果的工作单元
//   - hyper_context：异步上下文，包含唤醒器（waker）
//   - hyper_waker：唤醒器，用于通知执行器某个任务可以继续推进
//
// 工作流程：
//   1. 创建执行器 (hyper_executor_new)
//   2. 创建任务（例如通过 hyper_clientconn_handshake）
//   3. 将任务推入执行器 (hyper_executor_push)
//   4. 轮询执行器 (hyper_executor_poll) 以推进任务
//   5. 当任务完成时，从 poll 中取回任务并获取结果
// ============================================================================

use std::ffi::{c_int, c_void};
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
use std::task::{Context, Poll};

use futures_util::stream::{FuturesUnordered, Stream};

use super::error::hyper_code;
use super::UserDataPointer;

/// 装箱的 Future 类型别名
/// Pin<Box<...>> 确保 Future 在内存中的位置固定（不会被移动），
/// 这是异步运行时轮询 Future 的必要条件。
/// Send 约束允许任务跨线程传递。
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 装箱的任意任务类型
/// AsTaskType trait 用于在运行时识别任务值的具体类型（错误、响应、缓冲区等）。
/// Send + Sync 约束确保线程安全。
type BoxAny = Box<dyn AsTaskType + Send + Sync>;

/// 在轮询函数中返回此值表示任务已就绪（完成）。
pub const HYPER_POLL_READY: c_int = 0;

/// 在轮询函数中返回此值表示任务仍在等待中。
///
/// 此时传入的 `hyper_waker` 应被注册，以便在稍后某个时间点唤醒该任务。
pub const HYPER_POLL_PENDING: c_int = 1;

/// 在轮询函数中返回此值表示发生了错误。
pub const HYPER_POLL_ERROR: c_int = 3;

/// `hyper_task` 的任务执行器。
///
/// 任务（task）是一个可能在 IO 上阻塞的工作单元，可以通过轮询来推进工作进度。
///
/// 一个执行器可以持有多个任务，包括来自不相关 HTTP 连接的任务。
/// 执行器是单线程的。通常你可能每个线程一个执行器，或者为了简单起见，
/// 每个连接一个执行器。
///
/// 任务的进展仅在调用 `hyper_executor_poll` 时发生，且仅对那些对应的
/// `hyper_waker` 已被调用以指示它们可以推进的任务（例如，操作系统指示
/// 有更多数据可读或更多缓冲区空间可写）。
///
/// 死锁风险：`hyper_executor_poll` 不得在任务的回调内部调用，否则会导致死锁。
///
/// 方法：
///
/// - hyper_executor_new:  创建新的任务执行器。
/// - hyper_executor_push: 将任务推入执行器。
/// - hyper_executor_poll: 轮询执行器，尝试推进已通知就绪的任务。
/// - hyper_executor_free: 释放执行器及其中所有未完成的任务。
pub struct hyper_executor {
    /// 所有任务 Future 的执行驱动器。
    ///
    /// FuturesUnordered 是一个可以并发轮询多个 Future 的集合，
    /// 当其中任何一个 Future 就绪时，就返回该 Future 的结果。
    ///
    /// 互斥锁上不应存在竞争，因为它仅在驱动 Future 时才被锁定。
    /// 但我们无法保证 `hyper_executor_poll()` 的正确使用——
    /// 在 C 代码中，它可能在某个已存储的 Future 内部被调用。
    /// 互斥锁不是可重入的，因此这样做会导致死锁，但这比数据损坏要好。
    driver: Mutex<FuturesUnordered<TaskFuture>>,

    /// 需要推入 `driver` 的 Future 队列。
    ///
    /// 这使用单独的互斥锁，因为 `spawn` 可能在某个 Future 内部被调用，
    /// 这意味着 driver 的互斥锁已经被锁定了。
    /// 使用单独的锁避免了死锁问题。
    spawn_queue: Mutex<Vec<TaskFuture>>,

    /// 用于跟踪在 `hyper_executor::poll_next` 执行期间
    /// 某个 Future 是否调用了 `wake`。
    /// 如果有 Future 在轮询过程中自我唤醒，执行器需要再次轮询。
    is_woken: Arc<ExecWaker>,
}

/// 执行器的弱引用包装器。
///
/// 使用 Weak 引用而非 Arc 强引用，避免循环引用导致内存泄漏。
/// 当执行器已被释放时，Weak::upgrade() 返回 None，任务将被静默丢弃。
#[derive(Clone)]
pub(crate) struct WeakExec(Weak<hyper_executor>);

/// 执行器唤醒器。
///
/// 内部使用 AtomicBool 标记执行器是否有任务被唤醒。
/// 这是一种轻量级的通知机制，避免了额外的同步开销。
struct ExecWaker(AtomicBool);

/// 异步任务。
///
/// 任务代表一个最终会产生恰好一个 `hyper_task_value` 的工作块。
/// 任务被推入执行器，执行器负责调用任务的内部私有函数来推进工作。
/// 在大多数情况下，这些私有函数最终会导致 `hyper_io` 对象上的
/// 读或写回调被调用。
///
/// 任务由以下函数创建：
///
/// - hyper_clientconn_handshake: 创建 HTTP 客户端握手任务。
/// - hyper_clientconn_send:      创建在客户端连接上发送请求的任务。
/// - hyper_body_data:            创建轮询响应体以获取下一个数据缓冲区的任务。
/// - hyper_body_foreach:         创建对每个接收到的消息体块执行回调的任务。
///
/// 然后使用 `hyper_task_set_userdata` 将用户数据关联到任务。
/// 这很重要，例如将请求 ID 与给定请求关联。当多个任务在同一个执行器上运行时，
/// 这允许区分不同请求的任务。
///
/// 任务被推入执行器，最终从 hyper_executor_poll 中产出：
///
/// - hyper_executor_push:        将任务推入执行器。
/// - hyper_executor_poll:        轮询执行器，尝试推进已通知就绪的任务。
///
/// 一旦任务从 poll 中产出，检索其用户数据，检查其类型，并提取其值。
/// 这需要从 void* 转换为适当的类型。
///
/// hyper_task 上的方法：
///
/// - hyper_task_type:            查询此任务的返回类型。
/// - hyper_task_value:           获取此任务的输出值。
/// - hyper_task_set_userdata:    设置与此任务关联的用户数据指针。
/// - hyper_task_userdata:        检索通过 hyper_task_set_userdata 设置的用户数据。
/// - hyper_task_free:            释放任务。
pub struct hyper_task {
    /// 任务的 Future，轮询后最终产生一个 BoxAny 类型的结果。
    future: BoxFuture<BoxAny>,
    /// 任务完成后的输出值。
    /// 初始为 None，当 Future 完成时设置为 Some(value)。
    output: Option<BoxAny>,
    /// 用户数据指针，用于在 C 侧区分不同的任务。
    /// 例如可以关联请求 ID，以便在多个任务并发时辨别它们。
    userdata: UserDataPointer,
}

/// 任务 Future 包装器。
///
/// 将 hyper_task 包装为标准的 Rust Future，以便放入 FuturesUnordered 中。
/// 当内部 Future 完成时，将结果存储到 task.output 中，然后返回整个 task。
struct TaskFuture {
    /// 持有的任务，使用 Option 以便在完成时 take 出来。
    task: Option<Box<hyper_task>>,
}

/// 任务的异步上下文，包含相关的唤醒器（waker）。
///
/// 这被提供给 `hyper_io` 的读写回调。目前它的唯一用途是提供对唤醒器的访问。
/// 参见 `hyper_waker`。
///
/// 对应的 Rust 类型：<https://doc.rust-lang.org/std/task/struct.Context.html>
///
/// 通过 #[repr(transparent)] 语义（单字段结构体），可以安全地在
/// Context 和 hyper_context 之间进行 transmute 转换。
pub struct hyper_context<'a>(Context<'a>);

/// 唤醒器，用于保存并唤醒挂起的任务。
///
/// 通过 `hyper_context` 和 `hyper_context_waker` 提供给 `hyper_io` 的读写回调。
///
/// 当回调中的非阻塞 IO 无法继续（返回 `EAGAIN` 或 `EWOULDBLOCK`）时，
/// 回调必须返回以避免阻塞执行器。但它也必须安排在将来有更多数据可用时被调用。
/// 这就是异步上下文和唤醒器的作用——唤醒器可以告诉执行器"这个任务可以继续推进了"。
///
/// 读写回调在发现无法继续时，必须：
///   1. 从上下文获取唤醒器 (`hyper_context_waker`)
///   2. 安排该唤醒器在将来被调用
///   3. 返回 `HYPER_POLL_PENDING`
///
/// 安排唤醒器被调用的具体方式由应用程序决定，但通常涉及一个大的 `select(2)` 循环，
/// 检查哪些文件描述符就绪，以及文件描述符和唤醒器对象之间的对应关系。
/// 对于每个就绪的文件描述符，必须调用对应的唤醒器。然后必须调用 `hyper_executor_poll`，
/// 这将使执行器尝试推进每个被唤醒的任务。
///
/// 对应的 Rust 类型：<https://doc.rust-lang.org/std/task/struct.Waker.html>
pub struct hyper_waker {
    /// 内部的 Rust 标准库 Waker
    waker: std::task::Waker,
}

/// `hyper_task` 值的类型描述符。
///
/// 用于在 C 侧确定任务完成后返回值的具体类型，以便进行正确的类型转换。
#[repr(C)]
pub enum hyper_task_return_type {
    /// 此任务的值为空（不意味着有错误，只是没有有意义的返回值）。
    HYPER_TASK_EMPTY,
    /// 此任务的值为 `hyper_error *`（错误指针）。
    HYPER_TASK_ERROR,
    /// 此任务的值为 `hyper_clientconn *`（客户端连接指针）。
    HYPER_TASK_CLIENTCONN,
    /// 此任务的值为 `hyper_response *`（HTTP 响应指针）。
    HYPER_TASK_RESPONSE,
    /// 此任务的值为 `hyper_buf *`（数据缓冲区指针）。
    HYPER_TASK_BUF,
}

/// 用于运行时类型识别的 trait。
///
/// unsafe 是因为实现者必须保证 as_task_type 返回正确的类型标识，
/// 否则 C 侧可能进行错误的类型转换导致未定义行为。
pub(crate) unsafe trait AsTaskType {
    /// 返回此值对应的任务返回类型标识符
    fn as_task_type(&self) -> hyper_task_return_type;
}

/// 将具体类型转换为动态分发的 trait 对象的 trait。
///
/// 这允许将各种不同的结果类型统一转换为 BoxAny，
/// 以便存储在 hyper_task 的 output 字段中。
pub(crate) trait IntoDynTaskType {
    /// 将自身转换为装箱的动态分发任务类型
    fn into_dyn_task_type(self) -> BoxAny;
}

// ===== impl hyper_executor =====
// ===== hyper_executor 执行器实现 =====

impl hyper_executor {
    /// 创建新的执行器实例。
    ///
    /// 返回 Arc<hyper_executor>，因为执行器需要被多个地方引用：
    /// - C 侧持有一个引用（通过 Arc::into_raw 转换为裸指针）
    /// - WeakExec 持有弱引用，用于在 Future 内部 spawn 新任务
    fn new() -> Arc<hyper_executor> {
        Arc::new(hyper_executor {
            // FuturesUnordered 用于高效地并发轮询多个 Future
            driver: Mutex::new(FuturesUnordered::new()),
            // spawn 队列初始为空
            spawn_queue: Mutex::new(Vec::new()),
            // 初始化唤醒标志为 false
            is_woken: Arc::new(ExecWaker(AtomicBool::new(false))),
        })
    }

    /// 创建执行器的弱引用。
    ///
    /// 降级为 Weak 引用，以便在任务内部安全地引用执行器，
    /// 而不会阻止执行器的释放。
    pub(crate) fn downgrade(exec: &Arc<hyper_executor>) -> WeakExec {
        WeakExec(Arc::downgrade(exec))
    }

    /// 将新任务加入 spawn 队列。
    ///
    /// 注意：这里只是将任务放入队列，并不立即执行。
    /// 任务会在下一次 poll_next 调用时被移入 driver 中。
    /// 使用单独的 spawn_queue 是因为此方法可能在 driver 被锁定时调用
    /// （例如在某个 Future 的 poll 方法内部 spawn 新任务）。
    fn spawn(&self, task: Box<hyper_task>) {
        self.spawn_queue
            .lock()
            .unwrap()
            .push(TaskFuture { task: Some(task) });
    }

    /// 轮询执行器，尝试获取下一个完成的任务。
    ///
    /// 返回值：
    /// - Some(task): 有一个任务完成了，返回该任务
    /// - None: 没有任务完成（所有任务都在等待中，或没有任务）
    ///
    /// 内部流程：
    /// 1. 先将 spawn 队列中的任务排入 driver
    /// 2. 轮询 driver 获取就绪的任务
    /// 3. 如果没有就绪的，检查是否有新的 spawn 或 wake 事件
    /// 4. 如果有，继续循环；否则返回 None
    fn poll_next(&self) -> Option<Box<hyper_task>> {
        // 首先将 spawn 队列中的任务排入 driver。
        self.drain_queue();

        // 创建一个基于 ExecWaker 的唤醒器，用于检测轮询期间的唤醒事件。
        let waker = futures_util::task::waker_ref(&self.is_woken);
        let mut cx = Context::from_waker(&waker);

        loop {
            {
                // 限定 driver 锁的作用域，确保在调用 drain_queue 之前释放锁。
                // 这是必要的，因为 drain_queue 也需要获取 driver 的锁。
                let mut driver = self.driver.lock().unwrap();
                // 轮询 FuturesUnordered，尝试获取下一个完成的 Future
                match Pin::new(&mut *driver).poll_next(&mut cx) {
                    // 有任务完成了，直接返回
                    Poll::Ready(val) => return val,
                    // 没有任务就绪，继续检查
                    Poll::Pending => {}
                };
            }
            // driver 锁在此处已被释放

            // poll_next 返回了 Pending。
            // 检查是否有挂起的任务尝试 spawn 了一些新任务。
            // 如果是，则将它们排入 driver 并继续循环。
            if self.drain_queue() {
                continue;
            }

            // 如果在轮询期间 driver 调用了 `wake`，
            // 我们应该立即再次轮询！
            // swap(false) 同时读取旧值并重置为 false。
            if self.is_woken.0.swap(false, Ordering::SeqCst) {
                continue;
            }

            // 确实没有任何任务就绪，返回 None
            return None;
        }
    }

    /// 将 spawn 队列中的任务排入 driver。
    ///
    /// 此方法会同时锁定 self.spawn_queue 和 self.driver，
    /// 因此调用时两者都不能已经被锁定。
    ///
    /// 返回值：
    /// - true: 队列中有任务被转移了
    /// - false: 队列为空，没有任务需要转移
    fn drain_queue(&self) -> bool {
        let mut queue = self.spawn_queue.lock().unwrap();
        if queue.is_empty() {
            return false;
        }

        let driver = self.driver.lock().unwrap();

        // 将队列中所有任务转移到 driver 中
        for task in queue.drain(..) {
            driver.push(task);
        }

        true
    }
}

/// 为 ExecWaker 实现 ArcWake trait。
///
/// ArcWake 是 futures_util 提供的 trait，
/// 允许将 Arc<ExecWaker> 用作异步 Waker。
/// 当某个 Future 调用 wake 时，只需将原子布尔设置为 true，
/// 表示有任务需要被再次轮询。
impl futures_util::task::ArcWake for ExecWaker {
    fn wake_by_ref(me: &Arc<ExecWaker>) {
        // 使用 SeqCst（顺序一致性）内存排序确保可见性
        me.0.store(true, Ordering::SeqCst);
    }
}

// ===== impl WeakExec =====
// ===== WeakExec 弱引用执行器实现 =====

impl WeakExec {
    /// 创建一个空的弱引用执行器。
    ///
    /// 通过 Weak::new() 创建的弱引用永远无法升级为强引用，
    /// 这意味着通过此实例 spawn 的任务将被静默丢弃。
    /// 主要用作默认值或占位符。
    pub(crate) fn new() -> Self {
        WeakExec(Weak::new())
    }
}

/// 为 WeakExec 实现 hyper 的 Executor trait。
///
/// 这使得 WeakExec 可以作为异步执行器使用。
/// 当需要 spawn 新的 Future 时（例如 h2 crate 需要），
/// 会通过此 trait 实现来执行。
///
/// 类型约束：
/// - F: Future + Send + 'static: Future 必须可发送且具有静态生命周期
/// - F::Output: Send + Sync + AsTaskType: 输出必须线程安全且可识别类型
impl<F> crate::rt::Executor<F> for WeakExec
where
    F: Future + Send + 'static,
    F::Output: Send + Sync + AsTaskType,
{
    fn execute(&self, fut: F) {
        // 尝试将弱引用升级为强引用
        if let Some(exec) = self.0.upgrade() {
            // 升级成功，将 Future 包装为 hyper_task 并 spawn
            exec.spawn(hyper_task::boxed(fut));
        }
        // 如果升级失败（执行器已被释放），静默丢弃任务
    }
}

ffi_fn! {
    /// 创建新的任务执行器。
    ///
    /// 为了避免内存泄漏，执行器最终必须通过 `hyper_executor_free` 来释放。
    ///
    /// 返回值：成功时返回执行器指针，失败时返回 NULL。
    fn hyper_executor_new() -> *const hyper_executor {
        // Arc::into_raw 将 Arc 转换为裸指针，引用计数不变（仍为1）
        // C 侧持有这个裸指针，相当于持有一个强引用
        Arc::into_raw(hyper_executor::new())
    } ?= ptr::null()
}

ffi_fn! {
    /// 释放执行器及其中所有未完成的任务。
    ///
    /// 这应该在执行器不再需要时调用。
    /// Arc::from_raw 重新获取 Arc 的所有权，然后 drop 释放它。
    /// 如果这是最后一个强引用，执行器及其所有任务将被释放。
    fn hyper_executor_free(exec: *const hyper_executor) {
        // non_null! 宏检查指针非空，然后从裸指针恢复 Arc
        // drop 显式释放 Arc，如果引用计数降为0则释放内存
        drop(non_null!(Arc::from_raw(exec) ?= ()));
    }
}

ffi_fn! {
    /// 将任务推入执行器。
    ///
    /// 执行器获取任务的所有权，之后不得再访问该任务。
    ///
    /// 任务的所有权最终会通过 `hyper_executor_poll` 返回给用户。
    ///
    /// 要区分在同一个执行器上运行的多个任务，
    /// 请使用 hyper_task_set_userdata。
    ///
    /// 参数：
    /// - exec: 执行器指针
    /// - task: 任务指针（所有权转移给执行器）
    ///
    /// 返回值：HYPERE_OK 成功，HYPERE_INVALID_ARG 参数无效
    fn hyper_executor_push(exec: *const hyper_executor, task: *mut hyper_task) -> hyper_code {
        // 安全地解引用执行器指针
        let exec = non_null!(&*exec ?= hyper_code::HYPERE_INVALID_ARG);
        // 从裸指针恢复 Box 的所有权
        let task = non_null!(Box::from_raw(task) ?= hyper_code::HYPERE_INVALID_ARG);
        // 将任务加入 spawn 队列
        exec.spawn(task);
        hyper_code::HYPERE_OK
    }
}

ffi_fn! {
    /// 轮询执行器，尝试推进所有可以推进的任务。
    ///
    /// 如果执行器中有任务就绪，返回其中一个。任务完成的信号机制
    /// 是 hyper 内部的。任务返回的顺序不保证。
    /// 使用 userdata 来区分不同的任务。
    ///
    /// 为了避免内存泄漏，返回的任务最终必须通过 `hyper_task_free` 来释放。
    ///
    /// 如果没有就绪的任务，返回 `NULL`。
    fn hyper_executor_poll(exec: *const hyper_executor) -> *mut hyper_task {
        let exec = non_null!(&*exec ?= ptr::null_mut());
        match exec.poll_next() {
            // 有完成的任务，转换为裸指针返回给 C 侧
            Some(task) => Box::into_raw(task),
            // 没有完成的任务
            None => ptr::null_mut(),
        }
    } ?= ptr::null_mut()
}

// ===== impl hyper_task =====
// ===== hyper_task 任务实现 =====

impl hyper_task {
    /// 创建一个装箱的任务。
    ///
    /// 将任意 Future 包装为 hyper_task。
    /// Future 的输出通过 IntoDynTaskType trait 转换为统一的 BoxAny 类型。
    ///
    /// 类型约束：
    /// - F: Future + Send + 'static: Future 必须可跨线程发送
    /// - F::Output: IntoDynTaskType + Send + Sync + 'static: 输出必须可转为动态类型
    pub(crate) fn boxed<F>(fut: F) -> Box<hyper_task>
    where
        F: Future + Send + 'static,
        F::Output: IntoDynTaskType + Send + Sync + 'static,
    {
        Box::new(hyper_task {
            // 将 Future 包装为 Pin<Box<...>>，并在完成时转换输出类型
            future: Box::pin(async move { fut.await.into_dyn_task_type() }),
            // 初始时没有输出
            output: None,
            // 初始时没有用户数据
            userdata: UserDataPointer(ptr::null_mut()),
        })
    }

    /// 获取此任务输出值的类型标识。
    ///
    /// 如果任务尚未完成（output 为 None），返回 HYPER_TASK_EMPTY。
    /// 否则返回输出值的实际类型标识。
    fn output_type(&self) -> hyper_task_return_type {
        match self.output {
            None => hyper_task_return_type::HYPER_TASK_EMPTY,
            Some(ref val) => val.as_task_type(),
        }
    }
}

/// 为 TaskFuture 实现 Future trait。
///
/// 当内部的 hyper_task 的 Future 完成时，将结果存储到 task.output 中，
/// 然后返回整个 task Box。这样执行器就可以将完成的任务返回给 C 调用者。
impl Future for TaskFuture {
    type Output = Box<hyper_task>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 轮询内部任务的 Future
        match Pin::new(&mut self.task.as_mut().unwrap().future).poll(cx) {
            Poll::Ready(val) => {
                // Future 完成了，取出 task 并设置输出值
                let mut task = self.task.take().unwrap();
                task.output = Some(val);
                Poll::Ready(task)
            }
            // Future 尚未完成，继续等待
            Poll::Pending => Poll::Pending,
        }
    }
}

ffi_fn! {
    /// 释放任务。
    ///
    /// 仅在任务未被 `hyper_clientconn_handshake` 消费
    /// 或未被 `hyper_executor_push` 获取所有权时使用。
    fn hyper_task_free(task: *mut hyper_task) {
        drop(non_null!(Box::from_raw(task) ?= ()));
    }
}

ffi_fn! {
    /// 获取此任务的输出值。
    ///
    /// 仅在执行器轮询完此任务后调用一次。
    ///
    /// 使用 `hyper_task_type` 来确定返回的 `void *` 的类型。
    ///
    /// 为避免内存泄漏，非空的返回值最终必须通过对应其类型的释放函数来释放：
    /// `hyper_error_free`、`hyper_clientconn_free`、`hyper_response_free`
    /// 或 `hyper_buf_free`。
    fn hyper_task_value(task: *mut hyper_task) -> *mut c_void {
        let task = non_null!(&mut *task ?= ptr::null_mut());

        if let Some(val) = task.output.take() {
            // 将 Box 转换为裸指针
            let p = Box::into_raw(val) as *mut c_void;
            // 防止返回指向空类型的虚假指针。
            // 零大小类型（ZST，如 ()）的 Box 使用 NonNull::dangling() 作为指针，
            // 这不是一个有效的已分配内存地址，返回给 C 侧会造成问题。
            if p == std::ptr::NonNull::<c_void>::dangling().as_ptr() {
                ptr::null_mut()
            } else {
                p
            }
        } else {
            // 没有输出值
            ptr::null_mut()
        }
    } ?= ptr::null_mut()
}

ffi_fn! {
    /// 查询此任务的返回类型。
    ///
    /// 应在从执行器获取任务后、提取值之前调用，
    /// 以确定如何正确地转换返回的 void 指针。
    fn hyper_task_type(task: *mut hyper_task) -> hyper_task_return_type {
        // 如果传入 NULL，不会崩溃，只是返回 HYPER_TASK_EMPTY
        // 表示没有值可以获取
        non_null!(&*task ?= hyper_task_return_type::HYPER_TASK_EMPTY).output_type()
    }
}

ffi_fn! {
    /// 设置与此任务关联的用户数据指针。
    ///
    /// 此值将传递给任务回调，也可以稍后通过 `hyper_task_userdata` 检查。
    ///
    /// 这对于区分在同一执行器上运行的不同请求的任务非常有用。
    /// 例如，可以将请求 ID 关联到对应的任务上。
    fn hyper_task_set_userdata(task: *mut hyper_task, userdata: *mut c_void) {
        if task.is_null() {
            return;
        }

        unsafe { (*task).userdata = UserDataPointer(userdata) };
    }
}

ffi_fn! {
    /// 检索通过 `hyper_task_set_userdata` 设置的用户数据。
    ///
    /// 如果从未设置过用户数据或任务指针为 NULL，返回 NULL。
    fn hyper_task_userdata(task: *mut hyper_task) -> *mut c_void {
        non_null!(&*task ?= ptr::null_mut()).userdata.0
    } ?= ptr::null_mut()
}

// ===== impl AsTaskType =====
// ===== AsTaskType 类型标识 trait 实现 =====

/// 为单元类型 () 实现 AsTaskType。
/// () 表示没有有意义的返回值，对应 HYPER_TASK_EMPTY。
/// unsafe 是因为必须保证类型标识的正确性。
unsafe impl AsTaskType for () {
    fn as_task_type(&self) -> hyper_task_return_type {
        hyper_task_return_type::HYPER_TASK_EMPTY
    }
}

/// 为 hyper::Error 实现 AsTaskType。
/// 错误值对应 HYPER_TASK_ERROR 类型。
unsafe impl AsTaskType for crate::Error {
    fn as_task_type(&self) -> hyper_task_return_type {
        hyper_task_return_type::HYPER_TASK_ERROR
    }
}

/// 为所有实现了 AsTaskType 的类型提供 IntoDynTaskType 的默认实现。
///
/// 直接将自身装箱为 trait 对象。
impl<T> IntoDynTaskType for T
where
    T: AsTaskType + Send + Sync + 'static,
{
    fn into_dyn_task_type(self) -> BoxAny {
        Box::new(self)
    }
}

/// 为 Result<T> 实现 IntoDynTaskType。
///
/// 这允许 Future 返回 Result 类型：
/// - Ok(val): 将成功值转换为动态类型
/// - Err(err): 将错误转换为动态类型（HYPER_TASK_ERROR）
impl<T> IntoDynTaskType for crate::Result<T>
where
    T: IntoDynTaskType + Send + Sync + 'static,
{
    fn into_dyn_task_type(self) -> BoxAny {
        match self {
            Ok(val) => val.into_dyn_task_type(),
            Err(err) => Box::new(err),
        }
    }
}

/// 为 Option<T> 实现 IntoDynTaskType。
///
/// 这允许 Future 返回 Option 类型：
/// - Some(val): 将值转换为动态类型
/// - None: 转换为空类型 ()（HYPER_TASK_EMPTY）
impl<T> IntoDynTaskType for Option<T>
where
    T: IntoDynTaskType + Send + Sync + 'static,
{
    fn into_dyn_task_type(self) -> BoxAny {
        match self {
            Some(val) => val.into_dyn_task_type(),
            None => ().into_dyn_task_type(),
        }
    }
}

// ===== impl hyper_context =====
// ===== hyper_context 异步上下文实现 =====

impl hyper_context<'_> {
    /// 将 Rust 标准库的 Context 包装为 hyper_context。
    ///
    /// 由于 hyper_context 只有一个字段（Context），
    /// 根据 Rust 的内存布局规则，单字段结构体与其字段具有相同的内存布局。
    /// 因此可以安全地使用 transmute 在两者之间转换。
    ///
    /// 这使得 C 回调函数可以通过 hyper_context 指针访问底层的 Waker。
    pub(crate) fn wrap<'a, 'b>(cx: &'a mut Context<'b>) -> &'a mut hyper_context<'b> {
        // 安全性：hyper_context 是 Context 的透明包装（单字段结构体）
        unsafe { std::mem::transmute::<&mut Context<'_>, &mut hyper_context<'_>>(cx) }
    }
}

ffi_fn! {
    /// 创建与任务上下文关联的唤醒器。
    ///
    /// 唤醒器可用于通知任务的执行器该任务已准备好推进
    /// （使用 `hyper_waker_wake`）。
    ///
    /// 通常只需要调用一次，但可以多次调用，每次返回一个新的唤醒器。
    ///
    /// 为避免内存泄漏，唤醒器最终必须通过 `hyper_waker_free` 或
    /// `hyper_waker_wake` 来消费。
    fn hyper_context_waker(cx: *mut hyper_context<'_>) -> *mut hyper_waker {
        // 从上下文中克隆一个 Waker
        let waker = non_null!(&mut *cx ?= ptr::null_mut()).0.waker().clone();
        // 将 Waker 装箱并转换为裸指针返回给 C 侧
        Box::into_raw(Box::new(hyper_waker { waker }))
    } ?= ptr::null_mut()
}

// ===== impl hyper_waker =====
// ===== hyper_waker 唤醒器实现 =====

ffi_fn! {
    /// 释放唤醒器。
    ///
    /// 仅在唤醒器未通过 `hyper_waker_wake` 消费时使用。
    /// 如果已经调用了 wake，则不需要也不应该再调用 free。
    fn hyper_waker_free(waker: *mut hyper_waker) {
        drop(non_null!(Box::from_raw(waker) ?= ()));
    }
}

ffi_fn! {
    /// 唤醒与此唤醒器关联的任务。
    ///
    /// 这不会直接执行关联任务的工作。相反，它向任务的执行器发送信号，
    /// 表明该任务已准备好推进。应用程序有责任调用 hyper_executor_poll，
    /// 后者将依次在所有准备好推进的任务上执行工作。
    ///
    /// 注意：这会消费（consume）唤醒器。之后不应再使用或释放该唤醒器。
    fn hyper_waker_wake(waker: *mut hyper_waker) {
        // 从裸指针恢复 Box 的所有权
        let waker = non_null!(Box::from_raw(waker) ?= ());
        // 调用 Rust 标准库的 wake 方法，通知执行器
        // wake() 会消费 Waker，所以之后不需要手动释放
        waker.waker.wake();
    }
}
