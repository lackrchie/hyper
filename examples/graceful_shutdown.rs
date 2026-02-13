//! # hyper 优雅关闭（Graceful Shutdown）示例
//!
//! 本示例演示了如何为 HTTP 服务器实现基于超时的优雅关闭机制。
//!
//! ## 工作原理
//! 1. 服务器为每个连接设置一个初始超时时间（5秒）
//! 2. 如果在超时时间内连接没有完成，则触发优雅关闭
//! 3. 优雅关闭后，再给一个额外的宽限期（2秒）让正在处理的请求完成
//! 4. 如果宽限期结束后请求仍未完成，则强制关闭连接
//!
//! ## 使用方法
//! ```bash
//! cargo run --example graceful_shutdown
//! # 在另一个终端中发送请求（处理函数会 sleep 6秒来模拟长任务）
//! curl http://127.0.0.1:3000/
//! ```
//!
//! ## 核心知识点
//! - `tokio::select!` 宏实现超时和连接处理的并发竞争
//! - `conn.graceful_shutdown()` 通知连接停止接受新请求
//! - `tokio::pin!` 宏将 Future 固定在栈上以便被多次 poll

// 拒绝所有编译警告
#![deny(warnings)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::net::TcpListener;
// pin! 宏用于将 Future 固定在栈上（Pin<&mut T>）
// 这是必要的，因为 tokio::select! 需要对 Future 进行可变引用的轮询
use tokio::pin;

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 请求处理函数
///
/// 接收一个请求，模拟耗时 6 秒的处理过程后返回 "Hello World!"
/// 6 秒的设定是有意义的：
/// - 超过初始的 5 秒连接超时 —— 会触发优雅关闭
/// - 在额外的 2 秒宽限期内 —— 请求可以在优雅关闭后完成
async fn hello(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    // 模拟耗时处理
    println!("in hello before sleep");
    tokio::time::sleep(Duration::from_secs(6)).await;
    println!("in hello after sleep");
    Ok(Response::new(Full::new(Bytes::from("Hello World!"))))
}

/// 程序入口函数
#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 绑定到本地回环地址的 3000 端口
    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();

    // === 定义超时策略 ===
    // 使用超时时间列表来实现分阶段关闭：
    // - 第一阶段（5秒）：正常等待连接完成
    // - 第二阶段（2秒）：触发优雅关闭后的宽限期
    // 这两个超时是顺序执行的，所以总最长等待时间是 5+2=7 秒
    let connection_timeouts = vec![Duration::from_secs(5), Duration::from_secs(2)];

    // 绑定 TCP 监听器
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);
    loop {
        // 接受新的 TCP 连接
        let (tcp, remote_address) = listener.accept().await?;

        // 将 tokio 的 TcpStream 适配为 hyper 的 I/O 类型
        let io = TokioIo::new(tcp);

        // 打印客户端连接的远程地址
        println!("accepted connection from {:?}", remote_address);

        // 克隆超时配置，以便在新的异步任务中使用
        let connection_timeouts_clone = connection_timeouts.clone();

        // 为每个连接创建独立的异步任务
        // 这样主循环可以继续接受新连接而不被阻塞
        tokio::task::spawn(async move {
            // 创建 HTTP/1.1 连接处理器
            let conn = http1::Builder::new().serve_connection(io, service_fn(hello));
            // 使用 pin! 宏将连接 Future 固定在栈上
            // 这是 tokio::select! 所需要的，因为 select! 需要对 Future 进行可变引用轮询
            // 固定后可以安全地通过 conn.as_mut() 多次访问
            pin!(conn);

            // === 超时迭代循环 ===
            // 遍历每个超时阶段，使用 tokio::select! 在连接完成和超时之间竞争
            for (iter, sleep_duration) in connection_timeouts_clone.iter().enumerate() {
                println!("iter = {} sleep_duration = {:?}", iter, sleep_duration);
                tokio::select! {
                    // 分支1：连接处理完成（正常或错误）
                    res = conn.as_mut() => {
                        // 连接已完成，打印结果并退出循环
                        match res {
                            Ok(()) => println!("after polling conn, no error"),
                            Err(e) =>  println!("error serving connection: {:?}", e),
                        };
                        break;
                    }
                    // 分支2：当前阶段超时
                    _ = tokio::time::sleep(*sleep_duration) => {
                        // 超时触发，调用 graceful_shutdown 通知连接优雅关闭
                        // graceful_shutdown 会：
                        // 1. 停止接受新的请求
                        // 2. 等待当前正在处理的请求完成
                        // 3. 然后关闭连接
                        // 循环继续到下一个超时阶段（宽限期）
                        println!("iter = {} got timeout_interval, calling conn.graceful_shutdown", iter);
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }
        });
    }
}
