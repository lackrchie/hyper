//! HTTP 客户端模块
//!
//! 本模块是 hyper 库中客户端功能的顶层入口。hyper 的客户端设计围绕"单连接"
//! （single connection）模型展开，即每个连接对象代表与服务器之间的一条 HTTP 连接。
//! 连接池管理、DNS 解析等高层功能不在本模块范围内，而是由 `hyper-util` 等上层库提供。
//!
//! ## 子模块结构
//!
//! - [`conn`] — 低层连接 API，提供基于单连接的 HTTP/1 和 HTTP/2 客户端实现。
//!   用户可以在此基础上自定义连接管理策略。
//! - `dispatch` — 内部调度模块，提供请求/响应的通道（channel）机制，
//!   用于在连接的发送端（`SendRequest`）和连接状态机之间传递消息。
//! - `tests` — 客户端相关的集成测试（仅在测试构建时编译）。
//!
//! ## 示例
//!
//! * [`client`] — 一个简单的 CLI HTTP 客户端，向指定 URL 发送请求，
//!   并将响应内容和详细信息逐块（chunk-by-chunk）输出到标准输出。
//!
//! * [`client_json`] — 一个简单的程序，发送 GET 请求获取 JSON 数据，
//!   异步读取响应体，使用 serde 解析后输出结果。
//!
//! [`client`]: https://github.com/hyperium/hyper/blob/master/examples/client.rs
//! [`client_json`]: https://github.com/hyperium/hyper/blob/master/examples/client_json.rs

// 条件编译：仅在测试模式下引入 tests 子模块
#[cfg(test)]
mod tests;

// cfg_feature! 是 hyper 自定义的条件编译宏，用于根据 feature flag 有选择地编译代码块。
// 此处要求启用 "http1" 或 "http2" feature 中的至少一个，才会编译以下子模块。
cfg_feature! {
    #![any(feature = "http1", feature = "http2")]

    // conn 模块：公开的低层客户端连接 API，包含 HTTP/1 和 HTTP/2 的握手与发送逻辑
    pub mod conn;
    // dispatch 模块：crate 内部使用的请求调度通道，
    // pub(super) 表示仅对父模块（即 client 模块）及同级子模块可见
    pub(super) mod dispatch;
}
