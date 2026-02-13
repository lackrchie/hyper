// 以下 lint 属性控制编译器的警告行为，确保代码质量：
#![deny(missing_docs)]                                           // 所有公共 API 必须有文档注释
#![deny(missing_debug_implementations)]                          // 所有公共类型必须实现 Debug trait
#![cfg_attr(test, deny(rust_2018_idioms))]                       // 测试时要求使用 Rust 2018 惯用写法
#![cfg_attr(all(test, feature = "full"), deny(unreachable_pub))] // 测试+full 模式下，禁止不可达的 pub 声明
#![cfg_attr(all(test, feature = "full"), deny(warnings))]        // 测试+full 模式下，将所有警告视为错误
#![cfg_attr(all(test, feature = "nightly"), feature(test))]      // nightly 模式下启用 test feature（用于基准测试）
#![cfg_attr(docsrs, feature(doc_cfg))]                           // docs.rs 构建时启用 doc_cfg feature，用于在文档中显示 feature 标记

//! # hyper
//!
//! hyper is a **fast** and **correct** HTTP implementation written in and for Rust.
//!
//! ## Features
//!
//! - HTTP/1 and HTTP/2
//! - Asynchronous design
//! - Leading in performance
//! - Tested and **correct**
//! - Extensive production use
//! - [Client](client/index.html) and [Server](server/index.html) APIs
//!
//! If just starting out, **check out the [Guides](https://hyper.rs/guides/1/)
//! first.**
//!
//! ## "Low-level"
//!
//! hyper is a lower-level HTTP library, meant to be a building block
//! for libraries and applications.
//!
//! If looking for just a convenient HTTP client, consider the
//! [reqwest](https://crates.io/crates/reqwest) crate.
//!
//! # Optional Features
//!
//! hyper uses a set of [feature flags] to reduce the amount of compiled code.
//! It is possible to just enable certain features over others. By default,
//! hyper does not enable any features but allows one to enable a subset for
//! their use case. Below is a list of the available feature flags. You may
//! also notice above each function, struct and trait there is listed one or
//! more feature flags that are required for that item to be used.
//!
//! If you are new to hyper it is possible to enable the `full` feature flag
//! which will enable all public APIs. Beware though that this will pull in
//! many extra dependencies that you may not need.
//!
//! The following optional features are available:
//!
//! - `http1`: Enables HTTP/1 support.
//! - `http2`: Enables HTTP/2 support.
//! - `client`: Enables the HTTP `client`.
//! - `server`: Enables the HTTP `server`.
//!
//! [feature flags]: https://doc.rust-lang.org/cargo/reference/manifest.html#the-features-section
//!
//! ## Unstable Features
//!
//! hyper includes a set of unstable optional features that can be enabled through the use of a
//! feature flag and a [configuration flag].
//!
//! The following is a list of feature flags and their corresponding `RUSTFLAG`:
//!
//! - `ffi`: Enables C API for hyper `hyper_unstable_ffi`.
//! - `tracing`: Enables debug logging with `hyper_unstable_tracing`.
//!
//! For example:
//!
//! ```notrust
//! RUSTFLAGS="--cfg hyper_unstable_tracing" cargo build
//! ```
//!
//! [configuration flag]: https://doc.rust-lang.org/reference/conditional-compilation.html
//!
//! # Stability
//!
//! It's worth talking a bit about the stability of hyper. hyper's API follows
//! [SemVer](https://semver.org). Breaking changes will only be introduced in
//! major versions, if ever. New additions to the API, such as new types,
//! methods, or traits will only be added in minor versions.
//!
//! Some parts of hyper are documented as NOT being part of the stable API. The
//! following is a brief list, you can read more about each one in the relevant
//! part of the documentation.
//!
//! - Downcasting error types from `Error::source()` is not considered stable.
//! - Private dependencies use of global variables is not considered stable.
//!   So, if a dependency uses `log` or `tracing`, hyper doesn't promise it
//!   will continue to do so.
//! - Behavior from default options is not stable. hyper reserves the right to
//!   add new options that are enabled by default which might alter the
//!   behavior, for the purposes of protection. It is also possible to _change_
//!   what the default options are set to, also in efforts to protect the
//!   most people possible.

// 隐藏地重新导出 http crate，供内部宏和依赖使用。
// `#[doc(hidden)]` 表示这个导出不出现在公共文档中，
// 但它允许下游代码（特别是 hyper-util）通过 `hyper::http` 访问 http crate。
#[doc(hidden)]
pub use http;

// 在测试 + nightly 模式下引入 test crate，用于支持基准测试（benchmark）。
// `extern crate test` 是使用 nightly-only 的 `#[bench]` 属性所必需的。
#[cfg(all(test, feature = "nightly"))]
extern crate test;

// 从 `http` crate 重新导出核心 HTTP 类型，使用户无需单独添加 `http` 依赖。
// `#[doc(no_inline)]` 确保这些类型的文档不会被内联到 hyper 文档中，
// 而是链接到 `http` crate 的原始文档。
#[doc(no_inline)]
pub use http::{header, HeaderMap, Method, Request, Response, StatusCode, Uri, Version};

// 从内部 error 模块重新导出 Error 和 Result 类型，
// 使其成为 hyper crate 的顶级公共 API。
pub use crate::error::{Error, Result};

// `#[macro_use]` 使得 cfg 模块中定义的宏（cfg_feature!、cfg_proto! 等）
// 在整个 crate 中可用。此模块必须最先声明，因为后续模块依赖这些宏。
#[macro_use]
mod cfg;

// `#[macro_use]` 使得 trace 模块中定义的日志宏（trace!、debug! 等）
// 在整个 crate 中可用。必须在 cfg 之后声明，因为 trace 模块可能使用 cfg 中的宏。
#[macro_use]
mod trace;

/// HTTP 消息体（body）相关的类型和 trait。
pub mod body;
// 内部通用工具模块，包含各种辅助类型和函数（如 Rewind I/O 包装器）。
mod common;
// 错误类型模块，定义了 Error、Kind、Parse 等类型。
// 通过上面的 `pub use` 导出公共 API。
mod error;
/// HTTP 扩展（extensions）模块，提供请求/响应扩展功能。
pub mod ext;
// 测试用 mock 模块，仅在测试时编译。
#[cfg(test)]
mod mock;
/// 异步运行时抽象模块，定义了 hyper 使用的 Read/Write trait。
pub mod rt;
/// Service trait 模块，定义了 hyper 的服务抽象。
pub mod service;
/// HTTP 升级（upgrade）模块，处理 HTTP 协议升级（如 WebSocket）。
pub mod upgrade;

// FFI（外部函数接口）模块，提供 C 语言 API。
// 这是一个不稳定特性，需要同时启用 `ffi` feature 和 `hyper_unstable_ffi` cfg flag。
// `cfg_attr(docsrs, ...)` 确保在 docs.rs 上正确标注所需的特性。
#[cfg(feature = "ffi")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "ffi", hyper_unstable_ffi))))]
pub mod ffi;

// 使用 cfg_proto! 宏条件编译：仅在启用了至少一种协议版本和至少一种角色时编译
cfg_proto! {
    // HTTP 头部工具函数模块（非公共）
    mod headers;
    // HTTP 协议实现模块（非公共），包含 HTTP/1 和 HTTP/2 的编解码器
    mod proto;
}

// 使用 cfg_feature! 宏条件编译：仅在启用 client feature 时编译
cfg_feature! {
    #![feature = "client"]

    /// HTTP 客户端模块，提供连接管理和请求发送功能。
    pub mod client;
}

// 使用 cfg_feature! 宏条件编译：仅在启用 server feature 时编译
cfg_feature! {
    #![feature = "server"]

    /// HTTP 服务端模块，提供连接接受和请求处理功能。
    pub mod server;
}
