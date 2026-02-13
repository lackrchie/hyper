//! # hyper HTTP/1.1 Hello World 服务器示例
//!
//! 这是 hyper 最基础的 HTTP 服务器示例。
//! 服务器监听本地 3000 端口，对所有请求返回 "Hello World!" 响应。
//!
//! ## 使用方法
//! ```bash
//! cargo run --example hello
//! # 在另一个终端测试：
//! curl http://127.0.0.1:3000/
//! ```
//!
//! ## 核心知识点
//! - hyper 服务器的最小化实现模板
//! - `TokioTimer` 为 HTTP/1.1 连接提供超时支持
//! - `service_fn` 将普通异步函数转为 Service trait 实现
//! - tokio 的并发模型：每个连接一个 spawn 的任务

// 拒绝所有编译警告
#![deny(warnings)]

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
// Full 表示一个包含完整数据块的 Body 类型
use http_body_util::Full;
// http1 模块提供 HTTP/1.1 服务器连接构建器
use hyper::server::conn::http1;
// service_fn 将异步函数转为 Service
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::net::TcpListener;

// 引入支持模块
// TokioIo: tokio I/O 到 hyper I/O 的适配器
// TokioTimer: 为 HTTP 连接提供基于 tokio 的定时器（用于超时控制）
#[path = "../benches/support/mod.rs"]
mod support;
use support::{TokioIo, TokioTimer};

/// 请求处理函数
///
/// 这是最简单的 HTTP 处理函数模板：
/// - 接收一个请求（此处用 `impl Body` 泛型，使其更灵活）
/// - 忽略请求内容（参数名用 `_` 表示不使用）
/// - 返回包含 "Hello World!" 的响应
///
/// 返回类型中的 `Infallible` 表示此函数永远不会返回错误
async fn hello(_: Request<impl hyper::body::Body>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from("Hello World!"))))
}

/// 程序入口函数
#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 初始化日志系统（受 RUST_LOG 环境变量控制）
    pretty_env_logger::init();

    // 绑定到本地回环地址 127.0.0.1:3000
    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();

    // 创建 TCP 监听器，绑定到指定地址
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);
    // 主循环：持续接受新的 TCP 连接
    loop {
        // 等待新的 TCP 连接到来
        //
        // 注意：这是一个 .await 点，循环会一直运行但不会忙等待。
        // .await 允许 tokio 运行时将当前任务从线程上摘下，
        // 直到有新连接到来时再唤醒此任务并放回线程上执行，
        // 最终 yield 出一个 TCP 流。
        let (tcp, _) = listener.accept().await?;
        // 使用适配器将 tokio::net::TcpStream 转换为 hyper 所需的 I/O 类型
        let io = TokioIo::new(tcp);

        // 在 tokio 中创建一个新的异步任务来处理这个连接
        // 这样主循环可以立即回到 accept() 等待下一个连接
        tokio::task::spawn(async move {
            // 使用 HTTP/1.1 构建器创建连接处理器
            // .timer() 设置连接的定时器，用于 keep-alive 超时等功能
            // .serve_connection() 开始处理连接上的 HTTP 请求
            // service_fn(hello) 将 hello 函数包装为 Service trait 的实现
            if let Err(err) = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(io, service_fn(hello))
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
