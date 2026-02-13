//! # hyper 服务器状态管理示例
//!
//! 本示例演示了如何在 HTTP 服务器中管理跨请求的共享状态。
//! 使用原子计数器（`AtomicUsize`）来记录请求次数，
//! 这是一种无锁的线程安全方式，比 `Mutex` 性能更高。
//!
//! ## 使用方法
//! ```bash
//! cargo run --example state
//! # 在另一个终端中多次发送请求，观察计数器递增：
//! curl http://127.0.0.1:3000/  # 返回 "Request #0"
//! curl http://127.0.0.1:3000/  # 返回 "Request #1"
//! curl http://127.0.0.1:3000/  # 返回 "Request #2"
//! ```
//!
//! ## 核心知识点
//! - 使用 `Arc<AtomicUsize>` 实现跨连接的无锁共享状态
//! - `Ordering::AcqRel` 内存序的含义和使用场景
//! - `fetch_add` 原子操作：读取当前值并递增，是一个不可分割的操作
//! - 注意：此示例中连接处理不是在 spawn 的任务中，所以是串行处理连接

// 拒绝所有编译警告
#![deny(warnings)]

use std::net::SocketAddr;
use std::sync::{
    // AtomicUsize: 无锁的原子计数器，支持线程安全的原子操作
    atomic::{AtomicUsize, Ordering},
    // Arc: 原子引用计数，用于在多个任务间共享数据
    Arc,
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{server::conn::http1, service::service_fn};
use hyper::{Error, Response};
use tokio::net::TcpListener;

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 绑定到 127.0.0.1:3000
    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();

    // 创建共享状态：一个原子计数器
    // Arc 使得计数器可以在多个异步任务间安全共享
    // AtomicUsize 提供无锁的原子操作，比 Mutex<usize> 性能更高
    let counter = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        // 每个连接可能发送多个请求（HTTP keep-alive），
        // 所以 Service 需要一份计数器的克隆来处理后续请求
        let counter = counter.clone();

        // 创建服务处理函数
        // `service_fn` 将闭包转换为 Service trait 的实现
        // `move` 将 counter 的所有权移入闭包
        let service = service_fn(move |_req| {
            // fetch_add(1, Ordering::AcqRel) 执行原子操作：
            // 1. 读取当前值（作为返回值）
            // 2. 将存储值加 1
            // 这两步是不可分割的原子操作，保证线程安全
            //
            // Ordering::AcqRel 是一种内存序（Memory Ordering）：
            // - Acquire（获取）：保证在此操作之后的所有读操作都能看到此操作及之前的写
            // - Release（释放）：保证此操作之前的所有写操作对其他线程可见
            // - AcqRel 同时具备 Acquire 和 Release 的语义
            let count = counter.fetch_add(1, Ordering::AcqRel);
            async move {
                Ok::<_, Error>(Response::new(Full::new(Bytes::from(format!(
                    "Request #{}",
                    count
                )))))
            }
        });

        // 注意：这里没有使用 tokio::task::spawn
        // 这意味着服务器是串行处理连接的（一次只处理一个连接）
        // 当前连接处理完毕后才会接受下一个连接
        // 对于生产环境应该使用 spawn 来实现并发处理
        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
            println!("Error serving connection: {:?}", err);
        }
    }
}
