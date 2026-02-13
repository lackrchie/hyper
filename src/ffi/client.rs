//! # HTTP 客户端连接的 FFI 绑定模块
//!
//! 本模块提供了 HTTP 客户端连接管理的 C 语言接口，涵盖连接建立（握手）、
//! 请求发送、连接选项配置等核心功能。
//!
//! ## 在 hyper FFI 中的角色
//!
//! 这是 FFI 层中最重要的模块之一，它将 hyper 的异步 HTTP 客户端抽象为
//! C 可调用的接口。支持 HTTP/1.1 和 HTTP/2（需启用对应 feature）。
//!
//! ## 典型使用流程
//!
//! 1. `hyper_clientconn_options_new()` —— 创建连接选项
//! 2. `hyper_clientconn_options_exec()` —— 绑定任务执行器
//! 3. `hyper_clientconn_handshake()` —— 在 IO 传输层上进行 HTTP 握手
//! 4. 将握手任务推入执行器并轮询
//! 5. `hyper_clientconn_send()` —— 在已建立的连接上发送请求
//! 6. `hyper_clientconn_free()` —— 释放连接

// === 标准库导入 ===
use std::ffi::c_int; // C 兼容的 int 类型
use std::ptr; // 裸指针工具函数
use std::sync::Arc; // 原子引用计数智能指针，用于共享执行器的所有权

// === 内部模块导入 ===
use crate::client::conn; // hyper 核心的 HTTP 客户端连接模块
use crate::rt::Executor as _; // 导入 Executor trait 的方法（`as _` 只引入方法不引入名称）

use super::error::hyper_code; // FFI 错误码枚举
use super::http_types::{hyper_request, hyper_response}; // FFI 层的请求/响应类型
use super::io::hyper_io; // FFI 层的 IO 传输类型
use super::task::{hyper_executor, hyper_task, hyper_task_return_type, AsTaskType, WeakExec}; // 任务系统类型

/// HTTP 客户端连接的选项配置构建器。
///
/// 用于在 HTTP 握手之前配置连接参数，如 HTTP 版本、头部大小写保留等。
/// 通过一系列 `hyper_clientconn_options_*` 函数进行配置。
///
/// Methods:
///
/// - hyper_clientconn_options_new:     Creates a new set of HTTP clientconn options to be used in a handshake.
/// - hyper_clientconn_options_exec:    Set the client background task executor.
/// - hyper_clientconn_options_http2:   Set whether to use HTTP2.
/// - hyper_clientconn_options_set_preserve_header_case:  Set whether header case is preserved.
/// - hyper_clientconn_options_set_preserve_header_order: Set whether header order is preserved.
/// - hyper_clientconn_options_http1_allow_multiline_headers: Set whether HTTP/1 connections accept obsolete line folding for header values.
/// - hyper_clientconn_options_free:    Free a set of HTTP clientconn options.
pub struct hyper_clientconn_options {
    /// 是否允许 HTTP/1 响应中的过时多行头部折叠（RFC 7230 已弃用的特性）
    http1_allow_obsolete_multiline_headers_in_responses: bool,
    /// 是否保留 HTTP 头部名称的原始大小写（默认会被规范化为小写）
    http1_preserve_header_case: bool,
    /// 是否保留 HTTP 头部的原始顺序
    http1_preserve_header_order: bool,
    /// 是否使用 HTTP/2 协议
    http2: bool,
    /// Use a `Weak` to prevent cycles.
    /// 使用 `Weak` 弱引用指向执行器，避免 executor -> task -> options -> executor 的循环引用。
    /// 弱引用不会增加引用计数，因此不会阻止执行器被释放。
    exec: WeakExec,
}

/// HTTP 客户端连接句柄。
///
/// 代表一个已完成握手的 HTTP 连接，可以在其上发送一个或多个请求。
/// 当使用 HTTP/1.1 keep-alive 或 HTTP/2 多路复用时，单个连接可以发送多个请求。
///
/// ## 创建流程
///
/// 1. 用 `hyper_io_new` 创建 `hyper_io`
/// 2. 用 `hyper_clientconn_options_new` 创建选项
/// 3. 调用 `hyper_clientconn_handshake` 发起握手（消耗 io 和 options）
/// 4. 将握手任务推入执行器轮询
/// 5. 从完成的 `HYPER_TASK_CLIENTCONN` 类型任务中提取连接
///
/// ## 所有权模型
///
/// `hyper_clientconn` 永久拥有传入的 `hyper_io`。由于 `hyper_io` 拥有底层
/// TCP/TLS 连接，因此连接的生命周期由 `hyper_clientconn` 管理。
///
/// An HTTP client connection handle.
///
/// These are used to send one or more requests on a single connection.
///
/// It's possible to send multiple requests on a single connection, such
/// as when HTTP/1 keep-alive or HTTP/2 is used.
///
/// To create a `hyper_clientconn`:
///
///   1. Create a `hyper_io` with `hyper_io_new`.
///   2. Create a `hyper_clientconn_options` with `hyper_clientconn_options_new`.
///   3. Call `hyper_clientconn_handshake` with the `hyper_io` and `hyper_clientconn_options`.
///      This creates a `hyper_task`.
///   5. Call `hyper_task_set_userdata` to assign an application-specific pointer to the task.
///      This allows keeping track of multiple connections that may be handshaking
///      simultaneously.
///   4. Add the `hyper_task` to an executor with `hyper_executor_push`.
///   5. Poll that executor until it yields a task of type `HYPER_TASK_CLIENTCONN`.
///   6. Extract the `hyper_clientconn` from the task with `hyper_task_value`.
///      This will require a cast from `void *` to `hyper_clientconn *`.
///
/// This process results in a `hyper_clientconn` that permanently owns the
/// `hyper_io`. Because the `hyper_io` in turn owns a TCP or TLS connection, that means
/// the `hyper_clientconn` owns the connection for both the clientconn's lifetime
/// and the connection's lifetime.
///
/// In other words, each connection (`hyper_io`) must have exactly one `hyper_clientconn`
/// associated with it. That's because `hyper_clientconn_handshake` sends the
/// [HTTP/2 Connection Preface] (for HTTP/2 connections). Since that preface can't
/// be sent twice, handshake can't be called twice.
///
/// [HTTP/2 Connection Preface]: https://datatracker.ietf.org/doc/html/rfc9113#name-http-2-connection-preface
///
/// Methods:
///
/// - hyper_clientconn_handshake:  Creates an HTTP client handshake task.
/// - hyper_clientconn_send:       Creates a task to send a request on the client connection.
/// - hyper_clientconn_free:       Free a hyper_clientconn *.
pub struct hyper_clientconn {
    /// 内部的发送器句柄，可以是 HTTP/1 或 HTTP/2 的发送端。
    tx: Tx,
}

/// HTTP 连接的发送端枚举。
///
/// 使用 Rust 的枚举来统一 HTTP/1 和 HTTP/2 两种不同的发送器类型。
/// 各变体通过 `#[cfg(feature = "...")]` 条件编译控制。
enum Tx {
    /// HTTP/1.x 的发送请求句柄
    #[cfg(feature = "http1")]
    Http1(conn::http1::SendRequest<crate::body::Incoming>),
    /// HTTP/2 的发送请求句柄
    #[cfg(feature = "http2")]
    Http2(conn::http2::SendRequest<crate::body::Incoming>),
}

// ===== impl hyper_clientconn =====

ffi_fn! {
    /// Creates an HTTP client handshake task.
    ///
    /// Both the `io` and the `options` are consumed in this function call.
    /// They should not be used or freed afterwards.
    ///
    /// The returned task must be polled with an executor until the handshake
    /// completes, at which point the value can be taken.
    ///
    /// To avoid a memory leak, the task must eventually be consumed by
    /// `hyper_task_free`, or taken ownership of by `hyper_executor_push`
    /// without subsequently being given back by `hyper_executor_poll`.
    fn hyper_clientconn_handshake(io: *mut hyper_io, options: *mut hyper_clientconn_options) -> *mut hyper_task {
        // 取回 options 和 io 的所有权（消耗它们）
        let options = non_null! { Box::from_raw(options) ?= ptr::null_mut() };
        let io = non_null! { Box::from_raw(io) ?= ptr::null_mut() };

        // 创建异步握手任务
        Box::into_raw(hyper_task::boxed(async move {
            // 如果启用了 http2 feature 且用户请求 HTTP/2，则使用 HTTP/2 握手
            #[cfg(feature = "http2")]
            {
            if options.http2 {
                return conn::http2::Builder::new(options.exec.clone())
                    .handshake::<_, crate::body::Incoming>(io)
                    .await
                    .map(|(tx, conn)| {
                        // 握手成功后，将连接的后台驱动任务交给执行器运行。
                        // conn 是一个 Future，需要持续被 poll 以驱动 HTTP/2 帧处理。
                        options.exec.execute(Box::pin(async move {
                            let _ = conn.await; // 忽略连接关闭时的结果
                        }));
                        hyper_clientconn { tx: Tx::Http2(tx) }
                    });
                }
            }

            // 默认使用 HTTP/1 握手
            conn::http1::Builder::new()
                .allow_obsolete_multiline_headers_in_responses(options.http1_allow_obsolete_multiline_headers_in_responses)
                .preserve_header_case(options.http1_preserve_header_case)
                .preserve_header_order(options.http1_preserve_header_order)
                .handshake::<_, crate::body::Incoming>(io)
                .await
                .map(|(tx, conn)| {
                    // 同样将 HTTP/1 连接的后台驱动任务提交给执行器
                    options.exec.execute(Box::pin(async move {
                        let _ = conn.await;
                    }));
                    hyper_clientconn { tx: Tx::Http1(tx) }
                })
        }))
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Creates a task to send a request on the client connection.
    ///
    /// This consumes the request. You should not use or free the request
    /// afterwards.
    ///
    /// Returns a task that needs to be polled until it is ready. When ready, the
    /// task yields a `hyper_response *`.
    ///
    /// To avoid a memory leak, the task must eventually be consumed by
    /// `hyper_task_free`, or taken ownership of by `hyper_executor_push`
    /// without subsequently being given back by `hyper_executor_poll`.
    fn hyper_clientconn_send(conn: *mut hyper_clientconn, req: *mut hyper_request) -> *mut hyper_task {
        // 取回 request 的所有权（消耗它）
        let mut req = non_null! { Box::from_raw(req) ?= ptr::null_mut() };

        // Update request with original-case map of headers
        // 在发送前，将 hyper_headers 中保存的原始大小写映射应用到实际的 HTTP 头部
        req.finalize_request();

        // 根据连接类型（HTTP/1 或 HTTP/2）选择对应的发送方法。
        // 使用 futures_util::future::Either 将两种不同类型的 Future 统一为一种类型，
        // 这是 Rust 中处理条件 Future 的常见模式。
        let fut = match non_null! { &mut *conn ?= ptr::null_mut() }.tx {
            Tx::Http1(ref mut tx) => futures_util::future::Either::Left(tx.send_request(req.0)),
            Tx::Http2(ref mut tx) => futures_util::future::Either::Right(tx.send_request(req.0)),
        };

        // 包装 Future：将原始 Response<Incoming> 转换为 FFI 层的 hyper_response
        let fut = async move {
            fut.await.map(hyper_response::wrap)
        };

        Box::into_raw(hyper_task::boxed(fut))
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Free a `hyper_clientconn *`.
    ///
    /// This should be used for any connection once it is no longer needed.
    fn hyper_clientconn_free(conn: *mut hyper_clientconn) {
        drop(non_null! { Box::from_raw(conn) ?= () });
    }
}

/// 为 `hyper_clientconn` 实现 `AsTaskType` trait。
///
/// 当握手任务完成时，任务系统通过此 trait 识别输出值的类型为 `HYPER_TASK_CLIENTCONN`，
/// 使 C 调用者知道应将 `hyper_task_value()` 的返回值转换为 `hyper_clientconn*`。
unsafe impl AsTaskType for hyper_clientconn {
    fn as_task_type(&self) -> hyper_task_return_type {
        hyper_task_return_type::HYPER_TASK_CLIENTCONN
    }
}

// ===== impl hyper_clientconn_options =====

ffi_fn! {
    /// Creates a new set of HTTP clientconn options to be used in a handshake.
    ///
    /// To avoid a memory leak, the options must eventually be consumed by
    /// `hyper_clientconn_options_free` or `hyper_clientconn_handshake`.
    fn hyper_clientconn_options_new() -> *mut hyper_clientconn_options {
        // 所有选项均使用保守的默认值：不保留头部大小写/顺序，使用 HTTP/1，无执行器
        Box::into_raw(Box::new(hyper_clientconn_options {
            http1_allow_obsolete_multiline_headers_in_responses: false,
            http1_preserve_header_case: false,
            http1_preserve_header_order: false,
            http2: false,
            exec: WeakExec::new(), // 创建一个空的弱引用（尚未绑定执行器）
        }))
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Set whether header case is preserved.
    ///
    /// Pass `0` to allow lowercase normalization (default), `1` to retain original case.
    fn hyper_clientconn_options_set_preserve_header_case(opts: *mut hyper_clientconn_options, enabled: c_int) {
        let opts = non_null! { &mut *opts ?= () };
        // C 惯例：0 为 false，非 0 为 true
        opts.http1_preserve_header_case = enabled != 0;
    }
}

ffi_fn! {
    /// Set whether header order is preserved.
    ///
    /// Pass `0` to allow reordering (default), `1` to retain original ordering.
    fn hyper_clientconn_options_set_preserve_header_order(opts: *mut hyper_clientconn_options, enabled: c_int) {
        let opts = non_null! { &mut *opts ?= () };
        opts.http1_preserve_header_order = enabled != 0;
    }
}

ffi_fn! {
    /// Free a set of HTTP clientconn options.
    ///
    /// This should only be used if the options aren't consumed by
    /// `hyper_clientconn_handshake`.
    fn hyper_clientconn_options_free(opts: *mut hyper_clientconn_options) {
        drop(non_null! { Box::from_raw(opts) ?= () });
    }
}

ffi_fn! {
    /// Set the client background task executor.
    ///
    /// This does not consume the `options` or the `exec`.
    fn hyper_clientconn_options_exec(opts: *mut hyper_clientconn_options, exec: *const hyper_executor) {
        let opts = non_null! { &mut *opts ?= () };

        // Arc::from_raw 从裸指针重建 Arc，这会"拿回"一个引用计数。
        // 但由于这个函数不应消耗 exec 的所有权，因此需要立即 forget 来
        // 阻止 Arc 的 drop（否则会减少引用计数）。
        let exec = non_null! { Arc::from_raw(exec) ?= () };
        // downgrade 创建 Weak 弱引用，不增加强引用计数
        let weak_exec = hyper_executor::downgrade(&exec);
        // forget 阻止 Arc 析构，保持原有的引用计数不变
        std::mem::forget(exec);

        opts.exec = weak_exec;
    }
}

ffi_fn! {
    /// Set whether to use HTTP2.
    ///
    /// Pass `0` to disable, `1` to enable.
    fn hyper_clientconn_options_http2(opts: *mut hyper_clientconn_options, enabled: c_int) -> hyper_code {
        // 使用条件编译：如果 http2 feature 启用，则允许配置 HTTP/2
        #[cfg(feature = "http2")]
        {
            let opts = non_null! { &mut *opts ?= hyper_code::HYPERE_INVALID_ARG };
            opts.http2 = enabled != 0;
            hyper_code::HYPERE_OK
        }

        // 如果 http2 feature 未启用，返回 FEATURE_NOT_ENABLED 错误码
        #[cfg(not(feature = "http2"))]
        {
            // drop 参数以避免 unused 警告
            drop(opts);
            drop(enabled);
            hyper_code::HYPERE_FEATURE_NOT_ENABLED
        }
    }
}

ffi_fn! {
    /// Set whether HTTP/1 connections accept obsolete line folding for header values.
    ///
    /// Newline codepoints (\r and \n) will be transformed to spaces when parsing.
    ///
    /// Pass `0` to disable, `1` to enable.
    ///
    fn hyper_clientconn_options_http1_allow_multiline_headers(opts: *mut hyper_clientconn_options, enabled: c_int) -> hyper_code {
        let opts = non_null! { &mut *opts ?= hyper_code::HYPERE_INVALID_ARG };
        opts.http1_allow_obsolete_multiline_headers_in_responses = enabled != 0;
        hyper_code::HYPERE_OK
    }
}
