//! 日志追踪（tracing）宏模块
//!
//! 本模块为 hyper 提供了统一的日志和追踪（tracing）基础设施。它对 `tracing` crate
//! 的所有公共日志宏和 span 宏进行了条件包装，使得：
//!
//! 1. 当 `tracing` feature 启用时，宏会展开为实际的 `tracing::*` 调用
//! 2. 当 `tracing` feature 未启用时，宏展开为空操作（零成本抽象）
//!
//! ## 在 hyper 中的角色
//!
//! 本模块通过 `#[macro_use]` 在 `lib.rs` 中紧接着 `cfg` 模块之后被引入，
//! 使得整个 crate 内的代码都可以直接使用 `trace!`、`debug!`、`warn!` 等宏，
//! 而无需关心 `tracing` feature 是否启用。这种设计使得 hyper 的核心逻辑代码
//! 可以自由地添加调试日志，同时不会对未启用 tracing 的用户产生任何运行时开销。
//!
//! ## 不稳定特性
//!
//! `tracing` feature 被标记为不稳定特性（unstable），需要同时设置
//! `RUSTFLAGS='--cfg hyper_unstable_tracing'` 才能使用。这通过下面的
//! `compile_error!` 宏实现编译时检查。

// 为了完整性，即使某些宏当前未被使用，也提供了对 tracing 所有公共日志和 span 宏的封装。
// 此属性抑制"未使用宏"的编译警告。
#![allow(unused_macros)]

// 编译时安全检查：如果用户在 Cargo.toml 中启用了 `tracing` feature，
// 但没有设置 `--cfg hyper_unstable_tracing` 编译标志，则产生编译错误。
// 这是 hyper 对不稳定特性的保护机制——确保用户明确知道自己在使用不稳定 API。
#[cfg(all(not(hyper_unstable_tracing), feature = "tracing"))]
compile_error!(
    "\
    The `tracing` feature is unstable, and requires the \
    `RUSTFLAGS='--cfg hyper_unstable_tracing'` environment variable to be set.\
"
);

/// `debug!` —— 调试级别日志宏
///
/// 当 `tracing` feature 启用时，转发到 `tracing::debug!`；否则为空操作。
/// 用于记录有助于调试但在正常运行时不需要的信息。
macro_rules! debug {
    ($($arg:tt)+) => {
        #[cfg(feature = "tracing")]
        {
            tracing::debug!($($arg)+);
        }
    }
}

/// `debug_span!` —— 调试级别 span 创建宏
///
/// 创建一个调试级别的 tracing span 并立即进入（entered）。
/// Span 用于标记代码的执行区域，可以在日志中关联同一操作的多条消息。
/// 返回一个 `Entered` guard，当 guard 被 drop 时自动退出 span。
///
/// 注意：整个宏体被包裹在一个块 `{ }` 中，这样即使在 tracing 未启用时，
/// 宏展开的结果也是一个有效的表达式（空块 `{}` 返回 `()`）。
macro_rules! debug_span {
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "tracing")]
            {
                let _span = tracing::debug_span!($($arg)+);
                // entered() 返回一个 RAII guard，在其作用域内 span 处于活跃状态
                _span.entered()
            }
        }
    }
}

/// `error!` —— 错误级别日志宏
///
/// 当 `tracing` feature 启用时，转发到 `tracing::error!`；否则为空操作。
/// 用于记录严重错误信息。
macro_rules! error {
    ($($arg:tt)*) => {
        #[cfg(feature = "tracing")]
        {
            tracing::error!($($arg)+);
        }
    }
}

/// `error_span!` —— 错误级别 span 创建宏
///
/// 创建并进入一个错误级别的 tracing span。
macro_rules! error_span {
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "tracing")]
            {
                let _span = tracing::error_span!($($arg)+);
                _span.entered()
            }
        }
    }
}

/// `info!` —— 信息级别日志宏
///
/// 当 `tracing` feature 启用时，转发到 `tracing::info!`；否则为空操作。
/// 用于记录一般性的信息级别消息。
macro_rules! info {
    ($($arg:tt)*) => {
        #[cfg(feature = "tracing")]
        {
            tracing::info!($($arg)+);
        }
    }
}

/// `info_span!` —— 信息级别 span 创建宏
///
/// 创建并进入一个信息级别的 tracing span。
macro_rules! info_span {
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "tracing")]
            {
                let _span = tracing::info_span!($($arg)+);
                _span.entered()
            }
        }
    }
}

/// `trace!` —— 追踪级别日志宏
///
/// 当 `tracing` feature 启用时，转发到 `tracing::trace!`；否则为空操作。
/// 这是最细粒度的日志级别，用于记录非常详细的追踪信息。
/// 在 hyper 内部被广泛使用来追踪 HTTP 协议处理的每一个步骤。
macro_rules! trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "tracing")]
        {
            tracing::trace!($($arg)+);
        }
    }
}

/// `trace_span!` —— 追踪级别 span 创建宏
///
/// 创建并进入一个追踪级别的 tracing span。
macro_rules! trace_span {
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "tracing")]
            {
                let _span = tracing::trace_span!($($arg)+);
                _span.entered()
            }
        }
    }
}

/// `span!` —— 通用 span 创建宏
///
/// 创建并进入一个指定级别的 tracing span。
/// 与其他特定级别的 span 宏不同，此宏需要在第一个参数中指定 span 的级别。
macro_rules! span {
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "tracing")]
            {
                let _span = tracing::span!($($arg)+);
                _span.entered()
            }
        }
    }
}

/// `warn!` —— 警告级别日志宏
///
/// 当 `tracing` feature 启用时，转发到 `tracing::warn!`；否则为空操作。
/// 用于记录潜在的问题或需要注意的情况。
macro_rules! warn {
    ($($arg:tt)*) => {
        #[cfg(feature = "tracing")]
        {
            tracing::warn!($($arg)+);
        }
    }
}

/// `warn_span!` —— 警告级别 span 创建宏
///
/// 创建并进入一个警告级别的 tracing span。
macro_rules! warn_span {
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "tracing")]
            {
                let _span = tracing::warn_span!($($arg)+);
                _span.entered()
            }
        }
    }
}
