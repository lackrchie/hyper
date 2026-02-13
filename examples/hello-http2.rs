//! # hyper HTTP/2 Hello World 服务器示例
//!
//! 本示例演示了如何使用 hyper 构建一个 HTTP/2 服务器。
//! HTTP/2 相比 HTTP/1.1 的主要改进包括：
//! - 多路复用（同一个 TCP 连接上并行处理多个请求）
//! - 头部压缩（HPACK）
//! - 服务器推送
//!
//! ## 使用方法
//! ```bash
//! cargo run --example hello-http2 --features server
//! ```
//!
//! ## 核心知识点
//! - HTTP/2 需要一个 Executor 来管理后台任务的调度
//! - 使用 `http2::Builder` 而非 `http1::Builder` 来创建 HTTP/2 连接
//! - HTTP/2 的 `serve_connection` 需要传入 Executor 实例
//! - 使用条件编译 `#[cfg(feature = "server")]` 控制功能可用性

// 拒绝所有编译警告
#![deny(warnings)]
// 允许未使用的导入（因为某些导入只在特定 feature 下使用）
#![allow(unused_imports)]

use http_body_util::Full;
use hyper::body::Bytes;
// 仅在启用 "server" 特性时编译 http2 模块的导入
#[cfg(feature = "server")]
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;

// TokioIo 适配器
// 通常这会来自 hyper-util crate，但由于循环依赖的问题，
// 这里直接引用基准测试中的支持模块
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 请求处理函数
///
/// 一个简单的异步函数，忽略请求内容，直接返回 "Hello, World!" 响应
/// Infallible 作为错误类型表示这个函数永远不会返回错误
#[cfg(feature = "server")]
async fn hello(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from("Hello, World!"))))
}

/// Tokio 运行时执行器
///
/// 这是一个简单的执行器包装器，实现了 `hyper::rt::Executor` trait。
/// HTTP/2 协议需要一个执行器来 spawn 后台任务（例如处理并发的流），
/// 因为 HTTP/2 在单个连接上多路复用多个请求流。
///
/// `#[derive(Clone)]` 是必须的，因为每个连接都需要一份执行器的副本
#[derive(Clone)]
pub struct TokioExecutor;

/// 为 TokioExecutor 实现 hyper 的 Executor trait
///
/// 这使得 hyper 可以使用 tokio 的运行时来 spawn 异步任务。
/// 泛型约束：
/// - `F: Future + Send + 'static`：Future 必须可以安全地在线程间发送
/// - `F::Output: Send + 'static`：Future 的输出也必须满足 Send 约束
///
/// Executor 的作用是允许我们管理任务的执行，这有助于提高服务器的效率和可扩展性。
impl<F> hyper::rt::Executor<F> for TokioExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        // 使用 tokio 的 spawn 来执行传入的 Future
        tokio::task::spawn(fut);
    }
}

/// 程序入口函数（仅在启用 "server" 特性时编译）
#[cfg(feature = "server")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 绑定到本地回环地址的 3000 端口
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    // 创建 TCP 监听器
    let listener = TcpListener::bind(addr).await?;

    // 主循环：持续接受新连接
    loop {
        // 等待新的 TCP 连接到来
        //
        // 注意：这是一个 .await 点，循环会永远运行但并不是忙等待（busy loop）。
        // .await 允许 tokio 运行时将当前任务从线程上移除，直到有新连接到来时再唤醒。
        // 这样线程可以去执行其他任务，提高资源利用率。
        let (stream, _) = listener.accept().await?;
        // 将 tokio 的 TcpStream 适配为 hyper 的 I/O 类型
        let io = TokioIo::new(stream);

        // 为每个连接创建独立的异步任务
        // 这样主循环可以继续接受新连接而不必等待 HTTP/2 连接处理完成
        tokio::task::spawn(async move {
            // 使用 HTTP/2 构建器处理连接
            // 与 HTTP/1.1 不同，HTTP/2 需要传入一个 Executor 实例（TokioExecutor）
            // 因为 HTTP/2 需要在后台 spawn 任务来处理多路复用的请求流
            if let Err(err) = http2::Builder::new(TokioExecutor)
                .serve_connection(io, service_fn(hello))
                .await
            {
                eprintln!("Error serving connection: {}", err);
            }
        });
    }
}

/// 当未启用 "server" 特性时的 main 函数
/// 提供友好的错误提示，告诉用户需要启用该特性
#[cfg(not(feature = "server"))]
fn main() {
    panic!("This example requires the 'server' feature to be enabled");
}
