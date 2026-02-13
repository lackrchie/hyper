//! # HTTP 请求/响应体的 FFI 绑定模块
//!
//! 本模块为 hyper 的 HTTP body 提供 C 语言接口，支持流式读写 HTTP 请求和响应体。
//!
//! ## 在 hyper FFI 中的角色
//!
//! HTTP body 是 HTTP 消息的核心组成部分。本模块封装了两个主要类型：
//! - `hyper_body`：HTTP 消息体的流式句柄，用于发送请求体和接收响应体
//! - `hyper_buf`：字节缓冲区，承载 body 的实际数据块
//!
//! ## 典型使用流程
//!
//! **发送请求体（出站）**：
//! 1. 调用 `hyper_body_new()` 创建空 body
//! 2. 调用 `hyper_body_set_userdata()` 设置用户数据
//! 3. 调用 `hyper_body_set_data_func()` 设置数据提供回调
//! 4. 调用 `hyper_request_set_body()` 将 body 绑定到请求
//!
//! **接收响应体（入站）**：
//! 1. 从响应中通过 `hyper_response_body()` 获取 body
//! 2. 调用 `hyper_body_data()` 创建数据轮询任务
//! 3. 将任务推入执行器，轮询直到获得 `HYPER_TASK_BUF` 或 `HYPER_TASK_EMPTY`

// === 标准库导入 ===
use std::ffi::{c_int, c_void}; // C 兼容的整数和 void 指针类型
use std::mem::ManuallyDrop; // 手动管理析构的包装器，防止值被自动 drop
use std::ptr; // 裸指针操作工具（如 null_mut）
use std::task::{Context, Poll}; // Rust 异步运行时的核心类型：任务上下文和轮询结果

// === 外部 crate 导入 ===
use http_body_util::BodyExt as _; // 导入 Body 扩展 trait，使 `.frame()` 方法可用
                                    // `as _` 表示只引入 trait 的方法，不引入名称本身

// === 内部模块导入 ===
use super::task::{hyper_context, hyper_task, hyper_task_return_type, AsTaskType}; // 异步任务系统类型
use super::{UserDataPointer, HYPER_ITER_CONTINUE}; // 用户数据指针包装和迭代控制常量
use crate::body::{Bytes, Frame, Incoming as IncomingBody}; // hyper 核心 body 类型
use crate::ffi::size_t; // C 兼容的 size_t 类型别名

/// 流式 HTTP 消息体。
///
/// 这是 hyper FFI 中的核心类型之一，既用于发送请求（通过 `hyper_request_set_body`），
/// 也用于接收响应（通过 `hyper_response_body`）。
///
/// 对于出站请求体，调用 `hyper_body_set_data_func` 来提供数据。
///
/// 对于入站响应体，调用 `hyper_body_data` 获取一个任务，该任务每次被轮询时
/// 会产生一块数据。然后必须通过 `hyper_executor_push` 将该任务添加到执行器中。
///
/// 内部包装了 hyper 的 `Incoming` body 类型（一个新类型模式，即 newtype pattern）。
///
/// Methods:
///
/// - hyper_body_new:           Create a new "empty" body.
/// - hyper_body_set_userdata:  Set userdata on this body, which will be passed to callback functions.
/// - hyper_body_set_data_func: Set the data callback for this body.
/// - hyper_body_data:          Creates a task that will poll a response body for the next buffer of data.
/// - hyper_body_foreach:       Creates a task to execute the callback with each body chunk received.
/// - hyper_body_free:          Free a body.
pub struct hyper_body(pub(super) IncomingBody);

/// HTTP body 数据的字节缓冲区。
///
/// 这是对 `bytes::Bytes` 的 FFI 包装，用于在 C 和 Rust 之间传递 body 数据块。
///
/// 获取方式：
/// - 在 `hyper_body_foreach` 的回调中接收
/// - 从执行器轮询得到 `HYPER_TASK_BUF` 类型的任务后提取
///
/// `pub(crate)` 可见性允许 crate 内的其他模块直接访问内部的 `Bytes`。
///
/// Methods:
///
/// - hyper_buf_bytes: Get a pointer to the bytes in this buffer.
/// - hyper_buf_copy:  Create a new hyper_buf * by copying the provided bytes.
/// - hyper_buf_free:  Free this buffer.
/// - hyper_buf_len:   Get the length of the bytes this buffer contains.
pub struct hyper_buf(pub(crate) Bytes);

/// 用户自定义的出站 body 数据源。
///
/// 当 C 代码需要发送带有自定义数据的 HTTP 请求体时，通过此结构体提供数据。
/// 它包含一个数据回调函数和一个用户数据指针，hyper 会在需要更多数据时调用回调。
///
/// `pub(crate)` 可见性因为此类型需要在 `crate::body` 模块中被使用。
pub(crate) struct UserBody {
    /// 数据提供回调函数，当 hyper 需要更多 body 数据时被调用
    data_func: hyper_body_data_callback,
    /// 传递给回调的用户数据指针（对应 C 中的 void*）
    userdata: *mut c_void,
}

// ===== Body =====

/// `hyper_body_foreach` 的回调函数类型。
///
/// 参数说明：
/// - `*mut c_void`：用户数据指针
/// - `*const hyper_buf`：当前数据块的借用引用（回调返回后即失效）
///
/// 返回值：`HYPER_ITER_CONTINUE` 继续迭代，`HYPER_ITER_BREAK` 停止迭代。
type hyper_body_foreach_callback = extern "C" fn(*mut c_void, *const hyper_buf) -> c_int;

/// `hyper_body_set_data_func` 的回调函数类型。
///
/// 参数说明：
/// - `*mut c_void`：用户数据指针
/// - `*mut hyper_context<'_>`：异步上下文，可从中获取 waker
/// - `*mut *mut hyper_buf`：输出参数，回调应将数据写入此处
///
/// 返回值：`HYPER_POLL_READY`（数据就绪）、`HYPER_POLL_PENDING`（数据未就绪）
/// 或 `HYPER_POLL_ERROR`（出错）。
type hyper_body_data_callback =
    extern "C" fn(*mut c_void, *mut hyper_context<'_>, *mut *mut hyper_buf) -> c_int;

ffi_fn! {
    /// Creates a new "empty" body.
    ///
    /// If not configured, this body acts as an empty payload.
    ///
    /// To avoid a memory leak, the body must eventually be consumed by
    /// `hyper_body_free`, `hyper_body_foreach`, or `hyper_request_set_body`.
    fn hyper_body_new() -> *mut hyper_body {
        // Box::new 在堆上分配 hyper_body，Box::into_raw 将所有权转移给 C 调用者。
        // IncomingBody::ffi() 创建一个专门用于 FFI 的空 body 实例。
        Box::into_raw(Box::new(hyper_body(IncomingBody::ffi())))
    } ?= ptr::null_mut()
}

ffi_fn! {
    /// Free a body.
    ///
    /// This should only be used if the request isn't consumed by
    /// `hyper_body_foreach` or `hyper_request_set_body`.
    fn hyper_body_free(body: *mut hyper_body) {
        // non_null! 检查指针非空，Box::from_raw 取回所有权，drop 释放内存。
        // `?= ()` 表示如果指针为空，返回 `()`（即什么都不做）。
        drop(non_null!(Box::from_raw(body) ?= ()));
    }
}

ffi_fn! {
    /// Creates a task that will poll a response body for the next buffer of data.
    ///
    /// The task may have different types depending on the outcome:
    ///
    /// - `HYPER_TASK_BUF`: Success, and more data was received.
    /// - `HYPER_TASK_ERROR`: An error retrieving the data.
    /// - `HYPER_TASK_EMPTY`: The body has finished streaming data.
    ///
    /// When the application receives the task from `hyper_executor_poll`,
    /// if the task type is `HYPER_TASK_BUF`, it should cast the task to
    /// `hyper_buf *` and consume all the bytes in the buffer. Then
    /// the application should call `hyper_body_data` again for the same
    /// `hyper_body *`, to create a task for the next buffer of data.
    /// Repeat until the polled task type is `HYPER_TASK_ERROR` or
    /// `HYPER_TASK_EMPTY`.
    ///
    /// To avoid a memory leak, the task must eventually be consumed by
    /// `hyper_task_free`, or taken ownership of by `hyper_executor_push`
    /// without subsequently being given back by `hyper_executor_poll`.
    ///
    /// This does not consume the `hyper_body *`, so it may be used again.
    /// However, the `hyper_body *` MUST NOT be used or freed until the
    /// related task is returned from `hyper_executor_poll`.
    ///
    /// For a more convenient method, see also `hyper_body_foreach`.
    fn hyper_body_data(body: *mut hyper_body) -> *mut hyper_task {
        // This doesn't take ownership of the Body, so don't allow destructor
        // ManuallyDrop 包裹 Box，阻止 Box 的析构函数运行。
        // 这是因为 hyper_body_data 不获取 body 的所有权——body 仍由 C 调用者持有。
        // 如果不用 ManuallyDrop，Box 的 drop 会在此函数返回时释放 body 的内存。
        let mut body = ManuallyDrop::new(non_null!(Box::from_raw(body) ?= ptr::null_mut()));

        // 创建一个异步任务，该任务在被轮询时从 body 读取下一帧数据。
        // hyper_task::boxed 将 Future 封装为可通过 FFI 操作的任务对象。
        Box::into_raw(hyper_task::boxed(async move {
            loop {
                match body.0.frame().await {
                    Some(Ok(frame)) => {
                        // frame 可能是数据帧或 trailers 帧。
                        // into_data() 尝试将其转换为数据帧，如果是 trailers 则跳过继续。
                        if let Ok(data) = frame.into_data() {
                            return Ok(Some(hyper_buf(data)));
                        } else {
                            // 非数据帧（如 trailers），继续轮询下一帧
                            continue;
                        }
                    },
                    Some(Err(e)) => return Err(e), // 读取出错
                    None => return Ok(None),        // body 数据流结束
                }
            }
        }))
    } ?= ptr::null_mut()
}

ffi_fn! {
    /// Creates a task to execute the callback with each body chunk received.
    ///
    /// To avoid a memory leak, the task must eventually be consumed by
    /// `hyper_task_free`, or taken ownership of by `hyper_executor_push`
    /// without subsequently being given back by `hyper_executor_poll`.
    ///
    /// The `hyper_buf` pointer is only a borrowed reference. It cannot live outside
    /// the execution of the callback. You must make a copy of the bytes to retain them.
    ///
    /// The callback should return `HYPER_ITER_CONTINUE` to continue iterating
    /// chunks as they are received, or `HYPER_ITER_BREAK` to cancel. Each
    /// invocation of the callback must consume all the bytes it is provided.
    /// There is no mechanism to signal to Hyper that only a subset of bytes were
    /// consumed.
    ///
    /// This will consume the `hyper_body *`, you shouldn't use it anymore or free it.
    fn hyper_body_foreach(body: *mut hyper_body, func: hyper_body_foreach_callback, userdata: *mut c_void) -> *mut hyper_task {
        // 注意：与 hyper_body_data 不同，此函数获取 body 的所有权（没有 ManuallyDrop）。
        let mut body = non_null!(Box::from_raw(body) ?= ptr::null_mut());
        // 将 userdata 包装到 UserDataPointer 中以便跨 await 点安全传递。
        let userdata = UserDataPointer(userdata);

        Box::into_raw(hyper_task::boxed(async move {
            // `let _ = &userdata;` 确保 userdata 被 async 块捕获（move 语义）。
            // 这是一个常见的 Rust async 模式，防止编译器优化掉对 userdata 的捕获。
            let _ = &userdata;
            // 循环读取 body 的每一帧数据
            while let Some(item) = body.0.frame().await {
                let frame = item?; // 使用 ? 操作符传播错误
                if let Ok(chunk) = frame.into_data() {
                    // 调用 C 回调函数处理数据块。
                    // `&hyper_buf(chunk)` 创建临时的 hyper_buf 引用，回调结束后即销毁。
                    if HYPER_ITER_CONTINUE != func(userdata.0, &hyper_buf(chunk)) {
                        // 回调返回非 CONTINUE 值，表示用户请求中止迭代
                        return Err(crate::Error::new_user_aborted_by_callback());
                    }
                }
            }
            Ok(()) // 所有数据块处理完毕
        }))
    } ?= ptr::null_mut()
}

ffi_fn! {
    /// Set userdata on this body, which will be passed to callback functions.
    fn hyper_body_set_userdata(body: *mut hyper_body, userdata: *mut c_void) {
        let b = non_null!(&mut *body ?= ());
        // as_ffi_mut() 获取 body 内部 FFI 专用字段的可变引用
        b.0.as_ffi_mut().userdata = userdata;
    }
}

ffi_fn! {
    /// Set the outgoing data callback for this body.
    ///
    /// The callback is called each time hyper needs to send more data for the
    /// body. It is passed the value from `hyper_body_set_userdata`.
    ///
    /// If there is data available, the `hyper_buf **` argument should be set
    /// to a `hyper_buf *` containing the data, and `HYPER_POLL_READY` should
    /// be returned.
    ///
    /// Returning `HYPER_POLL_READY` while the `hyper_buf **` argument points
    /// to `NULL` will indicate the body has completed all data.
    ///
    /// If there is more data to send, but it isn't yet available, a
    /// `hyper_waker` should be saved from the `hyper_context *` argument, and
    /// `HYPER_POLL_PENDING` should be returned. You must wake the saved waker
    /// to signal the task when data is available.
    ///
    /// If some error has occurred, you can return `HYPER_POLL_ERROR` to abort
    /// the body.
    fn hyper_body_set_data_func(body: *mut hyper_body, func: hyper_body_data_callback) {
        let b = non_null!{ &mut *body ?= () };
        b.0.as_ffi_mut().data_func = func;
    }
}

// ===== impl UserBody =====

/// `UserBody` 的方法实现
impl UserBody {
    /// 创建一个新的 `UserBody`，默认数据回调为空操作（noop）。
    ///
    /// 空操作回调立即返回 `HYPER_POLL_READY`，表示没有数据可提供。
    pub(crate) fn new() -> UserBody {
        UserBody {
            data_func: data_noop, // 默认的无操作回调
            userdata: std::ptr::null_mut(), // 默认无用户数据
        }
    }

    /// 轮询用户提供的数据回调以获取下一块 body 数据。
    ///
    /// 此方法是 Rust 异步体系与 C 回调之间的桥梁：
    /// - 将 Rust 的 `Context`（包含 waker）传递给 C 回调
    /// - 将 C 回调的返回值映射为 Rust 的 `Poll` 枚举
    ///
    /// 返回值语义：
    /// - `Poll::Ready(None)` —— body 数据流结束
    /// - `Poll::Ready(Some(Ok(frame)))` —— 获得一块数据
    /// - `Poll::Ready(Some(Err(e)))` —— 发生错误
    /// - `Poll::Pending` —— 数据暂不可用，需等待 waker 唤醒
    pub(crate) fn poll_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<crate::Result<Frame<Bytes>>>> {
        let mut out = std::ptr::null_mut(); // 输出参数，回调将数据指针写入此处
        // 调用 C 回调函数，传入用户数据、异步上下文和输出指针
        match (self.data_func)(self.userdata, hyper_context::wrap(cx), &mut out) {
            super::task::HYPER_POLL_READY => {
                if out.is_null() {
                    // 回调返回 READY 但输出为 NULL，表示 body 已完成所有数据传输
                    Poll::Ready(None)
                } else {
                    // 回调返回了数据，通过 Box::from_raw 取回 hyper_buf 的所有权
                    let buf = unsafe { Box::from_raw(out) };
                    // 将原始字节数据包装为 HTTP 数据帧
                    Poll::Ready(Some(Ok(Frame::data(buf.0))))
                }
            }
            super::task::HYPER_POLL_PENDING => Poll::Pending, // 数据暂不可用
            super::task::HYPER_POLL_ERROR => {
                // C 回调报告了错误
                Poll::Ready(Some(Err(crate::Error::new_body_write_aborted())))
            }
            unexpected => Poll::Ready(Some(Err(crate::Error::new_body_write(format!(
                "unexpected hyper_body_data_func return code {}",
                unexpected
            ))))),
        }
    }
}

/// 空操作数据回调（默认回调）。
///
/// `cbindgen:ignore` 告诉 cbindgen 不要为此函数生成 C 头文件声明，
/// 因为它是内部实现细节，不暴露给 C 调用者。
/// cbindgen:ignore
extern "C" fn data_noop(
    _userdata: *mut c_void,
    _: *mut hyper_context<'_>,
    _: *mut *mut hyper_buf,
) -> c_int {
    // 直接返回 READY，表示没有数据（相当于空 body）
    super::task::HYPER_POLL_READY
}

// UserBody 包含裸指针（userdata 和 data_func），编译器默认不会为其实现 Send/Sync。
// 由于 FFI 场景下需要跨线程传递（异步任务可能在不同线程执行），
// 这里手动声明其线程安全性，实际安全性由 C 端调用者保证。
unsafe impl Send for UserBody {}
unsafe impl Sync for UserBody {}

// ===== Bytes =====

ffi_fn! {
    /// Create a new `hyper_buf *` by copying the provided bytes.
    ///
    /// This makes an owned copy of the bytes, so the `buf` argument can be
    /// freed (with `hyper_buf_free`) or changed afterwards.
    ///
    /// To avoid a memory leak, the copy must eventually be consumed by
    /// `hyper_buf_free`.
    ///
    /// This returns `NULL` if allocating a new buffer fails.
    fn hyper_buf_copy(buf: *const u8, len: size_t) -> *mut hyper_buf {
        // 从 C 传入的裸指针和长度构造 Rust 切片（借用语义，不拥有数据）
        let slice = unsafe {
            std::slice::from_raw_parts(buf, len)
        };
        // copy_from_slice 会分配新内存并复制数据，确保 Rust 端拥有独立的数据副本。
        // 然后通过 Box::into_raw 将所有权转移给 C 调用者。
        Box::into_raw(Box::new(hyper_buf(Bytes::copy_from_slice(slice))))
    } ?= ptr::null_mut()
}

ffi_fn! {
    /// Get a pointer to the bytes in this buffer.
    ///
    /// This should be used in conjunction with `hyper_buf_len` to get the length
    /// of the bytes data.
    ///
    /// This pointer is borrowed data, and not valid once the `hyper_buf` is
    /// consumed/freed.
    fn hyper_buf_bytes(buf: *const hyper_buf) -> *const u8 {
        // 返回底层 Bytes 的数据指针，这是借用语义——指针的有效性取决于 hyper_buf 的生命周期。
        unsafe { (*buf).0.as_ptr() }
    } ?= ptr::null()
}

ffi_fn! {
    /// Get the length of the bytes this buffer contains.
    fn hyper_buf_len(buf: *const hyper_buf) -> size_t {
        unsafe { (*buf).0.len() }
    }
}

ffi_fn! {
    /// Free this buffer.
    ///
    /// This should be used for any buffer once it is no longer needed.
    fn hyper_buf_free(buf: *mut hyper_buf) {
        // Box::from_raw 取回所有权，drop 触发析构释放内存
        drop(unsafe { Box::from_raw(buf) });
    }
}

/// 为 `hyper_buf` 实现 `AsTaskType` trait。
///
/// 这使得当 `hyper_buf` 作为异步任务的输出值时，任务系统能够正确标识其类型。
/// C 调用者可以通过 `hyper_task_type()` 查询到 `HYPER_TASK_BUF`，
/// 从而知道应将 `hyper_task_value()` 的返回值转换为 `hyper_buf*`。
///
/// `unsafe` 是因为 `AsTaskType` trait 本身被声明为 `unsafe trait`，
/// 要求实现者保证类型标识的正确性。
unsafe impl AsTaskType for hyper_buf {
    fn as_task_type(&self) -> hyper_task_return_type {
        hyper_task_return_type::HYPER_TASK_BUF
    }
}
