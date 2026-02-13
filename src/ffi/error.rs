//! # 错误类型与错误码的 FFI 绑定模块
//!
//! 本模块定义了 hyper C API 中的错误处理机制，提供两层错误表示：
//!
//! - `hyper_code`：简单的枚举错误码，用于函数返回值快速指示成功或失败类别
//! - `hyper_error`：详细的错误对象，包含完整的错误信息，可打印为人类可读的描述
//!
//! ## 在 hyper FFI 中的角色
//!
//! 大多数 FFI 函数通过返回 `hyper_code` 来指示操作结果。
//! 当需要更详细的错误信息时（例如异步任务完成后的错误），
//! 通过 `hyper_error` 对象获取错误码和可读的错误描述文本。
//!
//! ## 典型使用模式
//!
//! ```c
//! // 简单错误码检查
//! hyper_code code = hyper_request_set_method(req, "GET", 3);
//! if (code != HYPERE_OK) { /* 处理错误 */ }
//!
//! // 详细错误检查（从任务中获取）
//! hyper_error *err = hyper_task_value(task);
//! hyper_error_print(err, buf, buf_len);
//! hyper_error_free(err);
//! ```

// === 内部模块导入 ===
use crate::ffi::size_t; // C 兼容的 size_t 类型别名

/// 详细的错误对象，由某些 hyper FFI 函数返回。
///
/// 与简单的 `hyper_code` 不同，`hyper_error` 包含完整的错误上下文信息，
/// 可以提取错误码或打印为人类可读的字符串。
///
/// 内部包装了 hyper 核心的 `crate::Error` 类型（newtype 模式）。
///
/// Compare with `hyper_code`, which is a simpler error returned from
/// some hyper functions.
///
/// Methods:
///
/// - hyper_error_code:  Get an equivalent hyper_code from this error.
/// - hyper_error_print: Print the details of this error to a buffer.
/// - hyper_error_free:  Frees a hyper_error.
pub struct hyper_error(crate::Error);

/// 许多 hyper FFI 方法的简单返回码枚举。
///
/// `#[repr(C)]` 确保枚举的内存布局与 C 编译器生成的枚举一致，
/// 使得 C 代码可以安全地使用这些值进行比较。
///
/// 每个变体都有固定的整数值（由编译器按声明顺序自动分配：0, 1, 2, ...）。
#[repr(C)]
pub enum hyper_code {
    /// 操作成功，一切正常。
    HYPERE_OK,
    /// 通用错误，详细信息包含在 `hyper_error *` 中。
    HYPERE_ERROR,
    /// 函数参数无效（如传入了空指针或格式错误的数据）。
    HYPERE_INVALID_ARG,
    /// IO 传输层返回了意外的 EOF。
    ///
    /// 这通常意味着期望收到 HTTP 请求或响应，
    /// 但连接在未发送完整消息的情况下被干净地关闭了。
    ///
    /// This typically means an HTTP request or response was expected, but the
    /// connection closed cleanly without sending (all of) it.
    HYPERE_UNEXPECTED_EOF,
    /// 操作被用户提供的回调函数中止。
    ///
    /// 例如 `hyper_body_foreach` 的回调返回了 `HYPER_ITER_BREAK`。
    HYPERE_ABORTED_BY_CALLBACK,
    /// 请求的 hyper 可选功能未被启用。
    ///
    /// 例如在未启用 `http2` feature 的情况下尝试使用 HTTP/2。
    /// `#[cfg_attr(...)]` 在 http2 feature 启用时允许此变体未被使用。
    #[cfg_attr(feature = "http2", allow(unused))]
    HYPERE_FEATURE_NOT_ENABLED,
    /// 对端发送的 HTTP 消息无法被解析。
    ///
    /// 表示收到了格式错误的 HTTP 协议数据。
    HYPERE_INVALID_PEER_MESSAGE,
}

// ===== impl hyper_error =====

/// `hyper_error` 的内部方法实现。
impl hyper_error {
    /// 将内部错误转换为对应的 `hyper_code` 错误码。
    ///
    /// 通过模式匹配 hyper 内部错误的 `Kind` 变体来映射到 FFI 错误码。
    /// 未能精确映射的错误类型统一归类为 `HYPERE_ERROR`。
    fn code(&self) -> hyper_code {
        use crate::error::Kind as ErrorKind; // hyper 内部错误种类枚举
        use crate::error::User; // 用户相关的错误子类型

        match self.0.kind() {
            ErrorKind::Parse(_) => hyper_code::HYPERE_INVALID_PEER_MESSAGE, // HTTP 解析失败
            ErrorKind::IncompleteMessage => hyper_code::HYPERE_UNEXPECTED_EOF, // 消息不完整
            ErrorKind::User(User::AbortedByCallback) => hyper_code::HYPERE_ABORTED_BY_CALLBACK, // 用户回调中止
            // TODO: add more variants
            _ => hyper_code::HYPERE_ERROR, // 其他所有错误归为通用错误
        }
    }

    /// 将错误详情格式化写入字节缓冲区。
    ///
    /// 使用 `std::io::Cursor` 包装目标切片，通过 `write!` 宏将错误的
    /// `Display` 实现输出到缓冲区。`Cursor` 会自动追踪写入位置。
    ///
    /// 返回实际写入的字节数。如果缓冲区空间不足，会尽可能多地写入，
    /// 不会返回错误。
    fn print_to(&self, dst: &mut [u8]) -> usize {
        use std::io::Write;

        let mut dst = std::io::Cursor::new(dst);

        // A write! error doesn't matter. As much as possible will have been
        // written, and the Cursor position will know how far that is (even
        // if that is zero).
        // write! 的错误被忽略，因为即使缓冲区写满了，Cursor 的 position
        // 仍然记录了实际写入了多少字节。
        let _ = write!(dst, "{}", &self.0);
        dst.position() as usize // Cursor 的位置就是已写入的字节数
    }
}

ffi_fn! {
    /// Frees a `hyper_error`.
    ///
    /// This should be used for any error once it is no longer needed.
    fn hyper_error_free(err: *mut hyper_error) {
        // non_null! 检查非空后 Box::from_raw 取回所有权，drop 释放内存
        drop(non_null!(Box::from_raw(err) ?= ()));
    }
}

ffi_fn! {
    /// Get an equivalent `hyper_code` from this error.
    fn hyper_error_code(err: *const hyper_error) -> hyper_code {
        // 解引用为不可变引用后调用 code() 方法
        non_null!(&*err ?= hyper_code::HYPERE_INVALID_ARG).code()
    }
}

ffi_fn! {
    /// Print the details of this error to a buffer.
    ///
    /// The `dst_len` value must be the maximum length that the buffer can
    /// store.
    ///
    /// The return value is number of bytes that were written to `dst`.
    fn hyper_error_print(err: *const hyper_error, dst: *mut u8, dst_len: size_t) -> size_t {
        // 从裸指针和长度构造可变字节切片
        let dst = unsafe {
            std::slice::from_raw_parts_mut(dst, dst_len)
        };
        // 如果 err 为空指针，返回 0（表示写入了 0 字节）
        non_null!(&*err ?= 0).print_to(dst)
    }
}
