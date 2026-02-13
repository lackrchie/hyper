// We have a lot of c-types in here, stop warning about their names!
// 在 FFI 模块中使用了大量 C 风格的类型命名（如 snake_case 的结构体名），
// 因此禁止编译器对非驼峰命名发出警告。
#![allow(non_camel_case_types)]
// fmt::Debug isn't helpful on FFI types
// FFI 类型通常是不透明的指针包装器，实现 Debug trait 意义不大，
// 因此抑制缺少 Debug 实现的警告。
#![allow(missing_debug_implementations)]
// unreachable_pub warns `#[no_mangle] pub extern fn` in private mod.
// `#[no_mangle] pub extern "C" fn` 在私有模块中会触发 unreachable_pub 警告，
// 但 FFI 函数必须是 pub 的才能被 C 代码链接，因此这里抑制该警告。
#![allow(unreachable_pub)]

//! # hyper C API — hyper 的 C 语言外部函数接口（FFI）模块
//!
//! 本模块是 hyper HTTP 库的 C 语言绑定层，提供了一套完整的 C API，
//! 使得 C/C++ 等非 Rust 语言能够调用 hyper 的核心 HTTP 客户端功能。
//!
//! ## 模块在 hyper 中的角色
//!
//! hyper 本身是一个纯 Rust 实现的高性能 HTTP 库。本 FFI 模块作为桥梁层，
//! 将 Rust 的异步 HTTP 客户端功能封装为 C 兼容的同步/回调式 API。
//! 它涵盖了以下核心子模块：
//!
//! - `body`：HTTP 请求/响应体的流式读写
//! - `client`：HTTP 客户端连接的建立与请求发送
//! - `error`：错误类型与错误码
//! - `http_types`：HTTP 请求、响应、头部映射等类型
//! - `io`：底层 I/O 传输抽象（TCP/TLS）
//! - `task`：异步任务执行器与 waker 机制
//! - `macros`：FFI 辅助宏（`ffi_fn!` 和 `non_null!`）
//!
//! ## 不稳定性说明
//!
//! This part of the documentation describes the C API for hyper. That is, how
//! to *use* the hyper library in C code. This is **not** a regular Rust
//! module, and thus it is not accessible in Rust.
//!
//! ## Unstable
//!
//! The C API of hyper is currently **unstable**, which means it's not part of
//! the semver contract as the rest of the Rust API is. Because of that, it's
//! only accessible if `--cfg hyper_unstable_ffi` is passed to `rustc` when
//! compiling. The easiest way to do that is setting the `RUSTFLAGS`
//! environment variable.
//!
//! ## Building
//!
//! The C API is part of the Rust library, but isn't compiled by default. Using
//! `cargo`, staring with `1.64.0`, it can be compiled with the following command:
//!
//! ```notrust
//! RUSTFLAGS="--cfg hyper_unstable_ffi" cargo rustc --crate-type cdylib --features client,http1,http2,ffi
//! ```

// We may eventually allow the FFI to be enabled without `client` or `http1`,
// that is why we don't auto enable them as `ffi = ["client", "http1"]` in
// the `Cargo.toml`.
//
// But for now, give a clear message that this compile error is expected.
// 编译时检查：FFI 模块当前依赖 `client` 和 `http1` 两个 feature。
// 未来可能解耦，但目前如果缺少这些 feature 则直接给出明确的编译错误信息。
#[cfg(not(all(feature = "client", feature = "http1")))]
compile_error!("The `ffi` feature currently requires the `client` and `http1` features.");

// 编译时检查：FFI 功能被标记为不稳定（unstable），
// 必须通过 `RUSTFLAGS='--cfg hyper_unstable_ffi'` 显式启用。
// 这是一种门控机制，防止用户在不了解不稳定性的情况下意外使用 FFI 功能。
#[cfg(not(hyper_unstable_ffi))]
compile_error!(
    "\
    The `ffi` feature is unstable, and requires the \
    `RUSTFLAGS='--cfg hyper_unstable_ffi'` environment variable to be set.\
"
);

// 引入宏模块，`#[macro_use]` 使得其中定义的 `ffi_fn!` 和 `non_null!` 宏
// 在本模块及其子模块中均可使用。
#[macro_use]
mod macros;

// 各子模块声明：
mod body; // HTTP 请求/响应体的流式数据处理
mod client; // HTTP 客户端连接管理
mod error; // 错误类型与错误码定义
mod http_types; // HTTP 请求、响应、头部等核心类型
mod io; // 底层 I/O 传输层抽象
mod task; // 异步任务执行器与轮询机制

// 重新导出各子模块的公开符号，使 C 代码可以通过单一模块路径访问所有 FFI 函数。
// 使用 glob re-export（`*`）将所有 `pub` 项扁平化到 `ffi` 命名空间下。
pub use self::body::*;
pub use self::client::*;
pub use self::error::*;
pub use self::http_types::*;
pub use self::io::*;
pub use self::task::*;

/// 在迭代回调函数中返回此值表示继续迭代。
///
/// 用于 `hyper_body_foreach` 和 `hyper_headers_foreach` 等迭代类 API 的回调返回值。
pub const HYPER_ITER_CONTINUE: std::ffi::c_int = 0;
/// 在迭代回调函数中返回此值表示停止迭代。
///
/// 当回调函数希望提前终止迭代时返回此值。
#[allow(unused)]
pub const HYPER_ITER_BREAK: std::ffi::c_int = 1;

/// 表示未指定的 HTTP 版本。
///
/// 当无法识别或不关心具体 HTTP 版本时使用此值。
pub const HYPER_HTTP_VERSION_NONE: std::ffi::c_int = 0;
/// HTTP/1.0 版本标识。
pub const HYPER_HTTP_VERSION_1_0: std::ffi::c_int = 10;
/// HTTP/1.1 版本标识。
pub const HYPER_HTTP_VERSION_1_1: std::ffi::c_int = 11;
/// HTTP/2 版本标识。
pub const HYPER_HTTP_VERSION_2: std::ffi::c_int = 20;

/// 用户数据指针的包装类型。
///
/// 在 FFI 边界上，C 代码经常通过 `void*` 传递用户自定义数据（userdata）。
/// 此结构体将原始的 `*mut c_void` 指针包装起来，以便在 Rust 的异步任务中安全传递。
/// `Clone` derive 允许在需要时复制指针值（仅复制指针本身，不复制指向的数据）。
#[derive(Clone)]
struct UserDataPointer(*mut std::ffi::c_void);

// We don't actually know anything about this pointer, it's up to the user
// to do the right thing.
// 手动实现 Send 和 Sync —— 我们无法验证指针指向的数据是否线程安全，
// 这完全依赖于 C 端调用者的正确使用。这是 FFI 边界上常见的"信任调用者"模式。
unsafe impl Send for UserDataPointer {}
unsafe impl Sync for UserDataPointer {}

/// hyper 版本号的 C 字符串常量。
///
/// `cbindgen:ignore` 注解告诉 cbindgen（C 头文件生成工具）忽略此项，
/// 因为它是内部实现细节。
/// 使用 `concat!` 宏在编译时将 Cargo 包版本号与空字符 `\0` 拼接，
/// 生成一个以 null 结尾的 C 兼容字符串。
/// cbindgen:ignore
static VERSION_CSTR: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

// `core::ffi::c_size_t` is a nightly-only experimental API.
// https://github.com/rust-lang/rust/issues/88345
// 由于 `c_size_t` 尚未稳定，这里直接使用 `usize` 作为 C 的 `size_t` 的等价类型。
// 在绝大多数平台上，`usize` 与 `size_t` 具有相同的大小和对齐方式。
type size_t = usize;

ffi_fn! {
    /// 返回 hyper 库版本号的静态 ASCII 字符串（以 null 结尾）。
    ///
    /// 返回的指针指向静态存储区，在整个程序生命周期内有效。
    /// C 调用者可以直接将返回值作为 `const char*` 使用。
    fn hyper_version() -> *const std::ffi::c_char {
        // 将 Rust 字符串切片的指针转换为 C 字符指针。
        // `as _` 是 Rust 的类型推断简写，此处等价于 `as *const c_char`。
        VERSION_CSTR.as_ptr() as _
    } ?= std::ptr::null()
    // `?= std::ptr::null()` 是 `ffi_fn!` 宏的 panic 安全默认值：
    // 如果函数体内发生 panic，将被 catch_unwind 捕获并返回此默认值（空指针）。
}
