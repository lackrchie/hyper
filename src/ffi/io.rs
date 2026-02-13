//! # I/O 传输层的 FFI 绑定模块
//!
//! 本模块为 hyper 的底层 I/O 传输提供 C 语言接口，抽象了 TCP/TLS 等
//! 网络连接的读写操作。
//!
//! ## 在 hyper FFI 中的角色
//!
//! `hyper_io` 是 hyper FFI 中连接底层网络和上层 HTTP 协议的桥梁。
//! C 调用者通过设置读写回调函数来提供实际的网络 I/O 实现，
//! hyper 内部则通过 Rust 的 `Read`/`Write` trait 调用这些回调。
//!
//! ## 典型使用流程
//!
//! 1. `hyper_io_new()` —— 创建 IO 句柄
//! 2. `hyper_io_set_userdata()` —— 设置包含 fd/TLS 上下文的用户数据
//! 3. `hyper_io_set_read()` —— 设置读回调
//! 4. `hyper_io_set_write()` —— 设置写回调
//! 5. 将 IO 句柄传递给 `hyper_clientconn_handshake()`
//!
//! ## 非阻塞 I/O 模型
//!
//! 所有 I/O 操作都是非阻塞的。当回调无法立即完成（如 `EAGAIN`/`EWOULDBLOCK`）时，
//! 应通过 `hyper_context` 获取 waker，将其注册到事件循环中，
//! 然后返回 `HYPER_IO_PENDING`。

// === 标准库导入 ===
use std::ffi::c_void; // C 兼容的 void 指针类型
use std::pin::Pin; // Pin 包装器，用于确保自引用类型不被移动
use std::task::{Context, Poll}; // Rust 异步核心类型

// === 内部模块导入 ===
use super::task::hyper_context; // FFI 层的异步上下文类型
use crate::ffi::size_t; // C 兼容的 size_t 类型别名
use crate::rt::{Read, Write}; // hyper 运行时的读写 trait（类似于 tokio::io 的 AsyncRead/AsyncWrite）

/// 读回调返回此值表示操作挂起（数据暂不可用）。
///
/// 值为 `0xFFFFFFFF`，被选择为一个不可能的正常读取字节数，
/// 作为特殊标记值（sentinel value）使用。
pub const HYPER_IO_PENDING: size_t = 0xFFFFFFFF;
/// 读写回调返回此值表示发生了不可恢复的错误。
///
/// 值为 `0xFFFFFFFE`，与 PENDING 相邻的另一个特殊标记值。
pub const HYPER_IO_ERROR: size_t = 0xFFFFFFFE;

/// 读回调的函数签名。
///
/// 参数说明：
/// - `*mut c_void`：用户数据（通常包含 fd 和/或 TLS 上下文）
/// - `*mut hyper_context<'_>`：异步上下文（可从中获取 waker）
/// - `*mut u8`：目标缓冲区指针
/// - `size_t`：缓冲区长度
///
/// 返回值：实际读取的字节数，或 `HYPER_IO_PENDING`/`HYPER_IO_ERROR`。
type hyper_io_read_callback =
    extern "C" fn(*mut c_void, *mut hyper_context<'_>, *mut u8, size_t) -> size_t;

/// 写回调的函数签名。
///
/// 参数说明：
/// - `*mut c_void`：用户数据
/// - `*mut hyper_context<'_>`：异步上下文
/// - `*const u8`：源数据缓冲区指针（只读）
/// - `size_t`：缓冲区长度
///
/// 返回值：实际写入的字节数，或 `HYPER_IO_PENDING`/`HYPER_IO_ERROR`。
type hyper_io_write_callback =
    extern "C" fn(*mut c_void, *mut hyper_context<'_>, *const u8, size_t) -> size_t;

/// 特定连接的读写句柄。
///
/// 在其生命周期内拥有一个特定的 TCP 或 TLS 连接。
/// 它包含读回调、写回调和一个 `void*` 用户数据指针。
/// 典型情况下，用户数据指向一个包含文件描述符和 TLS 上下文的结构体。
///
/// This owns a specific TCP or TLS connection for the lifetime of
/// that connection. It contains a read and write callback, as well as a
/// void *userdata. Typically the userdata will point to a struct
/// containing a file descriptor and a TLS context.
///
/// Methods:
///
/// - hyper_io_new:          Create a new IO type used to represent a transport.
/// - hyper_io_set_read:     Set the read function for this IO transport.
/// - hyper_io_set_write:    Set the write function for this IO transport.
/// - hyper_io_set_userdata: Set the user data pointer for this IO to some value.
/// - hyper_io_free:         Free an IO handle.
pub struct hyper_io {
    /// 读回调函数指针
    read: hyper_io_read_callback,
    /// 写回调函数指针
    write: hyper_io_write_callback,
    /// 用户数据指针（通常指向 C 端管理的 fd + TLS 上下文）
    userdata: *mut c_void,
}

ffi_fn! {
    /// Create a new IO type used to represent a transport.
    ///
    /// The read and write functions of this transport should be set with
    /// `hyper_io_set_read` and `hyper_io_set_write`.
    ///
    /// It is expected that the underlying transport is non-blocking. When
    /// a read or write callback can't make progress because there is no
    /// data available yet, it should use the `hyper_waker` mechanism to
    /// arrange to be called again when data is available.
    ///
    /// To avoid a memory leak, the IO handle must eventually be consumed by
    /// `hyper_io_free` or `hyper_clientconn_handshake`.
    fn hyper_io_new() -> *mut hyper_io {
        // 默认的读写回调为 noop（空操作），返回 0 字节
        Box::into_raw(Box::new(hyper_io {
            read: read_noop,
            write: write_noop,
            userdata: std::ptr::null_mut(),
        }))
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Free an IO handle.
    ///
    /// This should only be used if the request isn't consumed by
    /// `hyper_clientconn_handshake`.
    fn hyper_io_free(io: *mut hyper_io) {
        drop(non_null!(Box::from_raw(io) ?= ()));
    }
}

ffi_fn! {
    /// Set the user data pointer for this IO to some value.
    ///
    /// This value is passed as an argument to the read and write callbacks.
    fn hyper_io_set_userdata(io: *mut hyper_io, data: *mut c_void) {
        non_null!(&mut *io ?= ()).userdata = data;
    }
}

ffi_fn! {
    /// Set the read function for this IO transport.
    ///
    /// Data that is read from the transport should be put in the `buf` pointer,
    /// up to `buf_len` bytes. The number of bytes read should be the return value.
    ///
    /// It is undefined behavior to try to access the bytes in the `buf` pointer,
    /// unless you have already written them yourself. It is also undefined behavior
    /// to return that more bytes have been written than actually set on the `buf`.
    ///
    /// If there is no data currently available, the callback should create a
    /// `hyper_waker` from its `hyper_context` argument and register the waker
    /// with whatever polling mechanism is used to signal when data is available
    /// later on. The return value should be `HYPER_IO_PENDING`. See the
    /// documentation for `hyper_waker`.
    ///
    /// If there is an irrecoverable error reading data, then `HYPER_IO_ERROR`
    /// should be the return value.
    fn hyper_io_set_read(io: *mut hyper_io, func: hyper_io_read_callback) {
        non_null!(&mut *io ?= ()).read = func;
    }
}

ffi_fn! {
    /// Set the write function for this IO transport.
    ///
    /// Data from the `buf` pointer should be written to the transport, up to
    /// `buf_len` bytes. The number of bytes written should be the return value.
    ///
    /// If there is no data currently available, the callback should create a
    /// `hyper_waker` from its `hyper_context` argument and register the waker
    /// with whatever polling mechanism is used to signal when data is available
    /// later on. The return value should be `HYPER_IO_PENDING`. See the documentation
    /// for `hyper_waker`.
    ///
    /// If there is an irrecoverable error reading data, then `HYPER_IO_ERROR`
    /// should be the return value.
    fn hyper_io_set_write(io: *mut hyper_io, func: hyper_io_write_callback) {
        non_null!(&mut *io ?= ()).write = func;
    }
}

/// 默认的读回调（空操作）。
///
/// `cbindgen:ignore` 标记使 cbindgen 不为此函数生成 C 头文件声明。
/// 返回 0 表示读取了 0 字节（等效于 EOF）。
/// cbindgen:ignore
extern "C" fn read_noop(
    _userdata: *mut c_void,
    _: *mut hyper_context<'_>,
    _buf: *mut u8,
    _buf_len: size_t,
) -> size_t {
    0
}

/// 默认的写回调（空操作）。
///
/// `cbindgen:ignore` 标记使 cbindgen 不为此函数生成 C 头文件声明。
/// 返回 0 表示写入了 0 字节。
/// cbindgen:ignore
extern "C" fn write_noop(
    _userdata: *mut c_void,
    _: *mut hyper_context<'_>,
    _buf: *const u8,
    _buf_len: size_t,
) -> size_t {
    0
}

/// 为 `hyper_io` 实现 hyper 运行时的 `Read` trait。
///
/// 这是将 C 回调适配为 Rust 异步读取接口的核心实现。
/// hyper 内部的 HTTP 解析器通过此 trait 从底层传输读取原始字节数据。
///
/// `Pin<&mut Self>` 确保 `hyper_io` 在轮询期间不会被移动（Rust 异步的安全要求）。
impl Read for hyper_io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: crate::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 获取底层缓冲区的可写区域指针和剩余容量
        let buf_ptr = unsafe { buf.as_mut() }.as_mut_ptr() as *mut u8;
        let buf_len = buf.remaining();

        // 调用 C 端注册的读回调
        match (self.read)(self.userdata, hyper_context::wrap(cx), buf_ptr, buf_len) {
            HYPER_IO_PENDING => Poll::Pending, // 数据暂不可用
            HYPER_IO_ERROR => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "io error",
            ))),
            ok => {
                // We have to trust that the user's read callback actually
                // filled in that many bytes... :(
                // 信任 C 回调报告的读取字节数，推进缓冲区游标。
                // 如果 C 端报告的字节数超过实际写入量，将导致读取未初始化内存（UB）。
                unsafe { buf.advance(ok) };
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// 为 `hyper_io` 实现 hyper 运行时的 `Write` trait。
///
/// 这是将 C 回调适配为 Rust 异步写入接口的核心实现。
/// hyper 内部的 HTTP 编码器通过此 trait 向底层传输写入 HTTP 数据。
impl Write for hyper_io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let buf_ptr = buf.as_ptr();
        let buf_len = buf.len();

        // 调用 C 端注册的写回调
        match (self.write)(self.userdata, hyper_context::wrap(cx), buf_ptr, buf_len) {
            HYPER_IO_PENDING => Poll::Pending,
            HYPER_IO_ERROR => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "io error",
            ))),
            ok => Poll::Ready(Ok(ok)), // 返回实际写入的字节数
        }
    }

    /// flush 操作直接返回成功。
    ///
    /// 假设底层传输不需要显式 flush（C 端应在写回调中自行处理缓冲）。
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// shutdown 操作直接返回成功。
    ///
    /// 实际的连接关闭由 C 端在 `hyper_io` 被释放时自行处理。
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// hyper_io 包含裸指针字段（userdata、read、write 函数指针），
// 编译器不会自动推断 Send/Sync。手动实现以允许跨线程传递，
// 安全性由 C 端调用者保证——他们必须确保 userdata 指向的数据是线程安全的，
// 或者保证不会在多个线程同时使用同一个 hyper_io。
unsafe impl Send for hyper_io {}
unsafe impl Sync for hyper_io {}
