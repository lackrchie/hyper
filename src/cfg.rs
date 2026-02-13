//! 条件编译配置宏模块
//!
//! 本模块定义了一系列声明式宏（declarative macros），用于在 hyper 的整个代码库中
//! 管理基于 Cargo feature flag 的条件编译。hyper 采用了细粒度的 feature flag 体系
//!（如 `http1`、`http2`、`client`、`server`），使得用户可以只编译所需功能，减少
//! 最终二进制文件的体积和编译时间。
//!
//! 这些宏的核心作用有两个：
//! 1. 简化重复的 `#[cfg(...)]` 属性书写，避免在每个条件编译项上手动添加复杂的特性组合
//! 2. 通过 `#[cfg_attr(docsrs, doc(cfg(...)))]` 自动在文档中标注每个 API 所需的 feature，
//!    使得 docs.rs 上生成的文档能清晰地展示功能依赖关系
//!
//! ## 模块在 hyper 中的角色
//!
//! 本模块通过 `#[macro_use]` 在 `lib.rs` 中最先被引入，因此其中定义的宏在整个 crate 内
//! 均可使用。这是 hyper 条件编译基础设施的核心部分。

/// `cfg_feature!` —— 基础条件编译宏
///
/// 这是所有其他 `cfg_*` 宏的基础构建块。它接受一个 `#![meta]` 形式的属性和
/// 一组 item（结构体、函数、模块等），然后为每个 item 同时添加：
/// - `#[cfg($meta)]`：实际的条件编译控制
/// - `#[cfg_attr(docsrs, doc(cfg($meta)))]`：在 docs.rs 构建时自动添加文档标记
///
/// # 宏参数
/// - `$meta`：条件编译的元数据表达式，如 `feature = "http1"` 或 `all(feature = "http1", feature = "client")`
/// - `$item`：零个或多个 Rust item（函数、结构体、模块声明等）
///
/// # 设计说明
/// 使用 `$(  )*` 重复模式，使得单次宏调用可以包裹多个 item，每个 item 都会
/// 独立地被添加条件编译属性。这避免了需要为每个 item 分别调用宏的冗余。
macro_rules! cfg_feature {
    (
        #![$meta:meta]
        $($item:item)*
    ) => {
        $(
            // 为每个 item 添加条件编译属性
            #[cfg($meta)]
            // docsrs 是 docs.rs 构建时自动设置的 cfg flag，
            // doc(cfg(...)) 会在生成的文档中显示该 item 所需的 feature
            #[cfg_attr(docsrs, doc(cfg($meta)))]
            $item
        )*
    }
}

/// `cfg_proto!` —— 协议层条件编译宏
///
/// 当代码同时依赖"至少一种 HTTP 协议版本"和"至少一种角色（客户端/服务端）"时使用此宏。
/// 它等价于 `#[cfg(all(any(feature = "http1", feature = "http2"), any(feature = "client", feature = "server")))]`。
///
/// 这个组合在 hyper 中非常常见——许多核心的协议处理逻辑只有在同时启用了协议版本
/// 和角色时才有意义。例如，HTTP 头部解析和序列化既需要知道协议版本，也需要知道
/// 是在客户端还是服务端使用。
macro_rules! cfg_proto {
    ($($item:item)*) => {
        cfg_feature! {
            // all(...) 要求所有内部条件同时满足：
            // - any(http1, http2)：至少启用一种 HTTP 协议版本
            // - any(client, server)：至少启用一种使用角色
            #![all(
                any(feature = "http1", feature = "http2"),
                any(feature = "client", feature = "server"),
            )]
            $($item)*
        }
    }
}

// 以下两个宏（cfg_client! 和 cfg_server!）本身被包裹在 cfg_proto! 中，
// 这意味着它们的定义只有在满足协议层条件（至少一种协议+至少一种角色）时才存在。
// 这是一种"宏的条件编译定义"模式——宏自身的存在也受 feature flag 控制。
cfg_proto! {
    /// `cfg_client!` —— 客户端功能条件编译宏
    ///
    /// 用于标记仅在启用 `client` feature 时才编译的代码。
    /// 注意：此宏自身的定义已经被 `cfg_proto!` 包裹，因此它只在
    /// 同时满足协议版本和角色条件时才可用。
    ///
    /// 在实际使用中，被此宏包裹的代码需要同时满足：
    /// - `feature = "client"` （由本宏添加）
    /// - `any(feature = "http1", feature = "http2")` 和 `any(feature = "client", feature = "server")`（由外层 `cfg_proto!` 保证）
    macro_rules! cfg_client {
        ($($item:item)*) => {
            cfg_feature! {
                #![feature = "client"]
                $($item)*
            }
        }
    }

    /// `cfg_server!` —— 服务端功能条件编译宏
    ///
    /// 用于标记仅在启用 `server` feature 时才编译的代码。
    /// 与 `cfg_client!` 对称，此宏自身也受 `cfg_proto!` 的条件约束。
    ///
    /// 在实际使用中，被此宏包裹的代码需要同时满足：
    /// - `feature = "server"` （由本宏添加）
    /// - `any(feature = "http1", feature = "http2")` 和 `any(feature = "client", feature = "server")`（由外层 `cfg_proto!` 保证）
    macro_rules! cfg_server {
        ($($item:item)*) => {
            cfg_feature! {
                #![feature = "server"]
                $($item)*
            }
        }
    }
}
