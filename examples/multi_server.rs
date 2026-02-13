//! # hyper 多服务器并行运行示例
//!
//! 本示例演示了如何在同一个进程中同时运行多个 HTTP 服务器，
//! 每个服务器监听不同的端口并提供不同的服务。
//!
//! ## 使用方法
//! ```bash
//! cargo run --example multi_server
//! # 在另一个终端测试两个服务器：
//! curl http://127.0.0.1:1337/  # 返回 "The 1st service!"
//! curl http://127.0.0.1:1338/  # 返回 "The 2nd service!"
//! ```
//!
//! ## 核心知识点
//! - 使用 `futures_util::future::join` 并发运行多个异步任务
//! - 在同一个 tokio 运行时中运行多个独立的服务器
//! - 每个服务器作为一个独立的 async block 运行

// 拒绝所有编译警告
#![deny(warnings)]
// 启用 Rust 2018 惯用写法的警告
#![warn(rust_2018_idioms)]

use std::net::SocketAddr;

use bytes::Bytes;
// join 函数可以同时运行两个 Future，并等待它们都完成
// 类似于 tokio::join! 宏，但来自 futures 库
use futures_util::future::join;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::net::TcpListener;

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// 第一个服务器的响应内容（静态字节切片，编译时确定，零开销）
static INDEX1: &[u8] = b"The 1st service!";
// 第二个服务器的响应内容
static INDEX2: &[u8] = b"The 2nd service!";

/// 第一个服务器的请求处理函数
/// 对所有请求返回 "The 1st service!"
async fn index1(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    Ok(Response::new(Full::new(Bytes::from(INDEX1))))
}

/// 第二个服务器的请求处理函数
/// 对所有请求返回 "The 2nd service!"
async fn index2(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    Ok(Response::new(Full::new(Bytes::from(INDEX2))))
}

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 定义两个服务器的监听地址
    let addr1: SocketAddr = ([127, 0, 0, 1], 1337).into();
    let addr2: SocketAddr = ([127, 0, 0, 1], 1338).into();

    // === 第一个服务器 ===
    // 使用 async move 块定义一个异步任务
    // move 确保 addr1 被移入闭包中
    let srv1 = async move {
        let listener = TcpListener::bind(addr1).await.unwrap();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);

            // 为每个连接 spawn 一个任务
            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(io, service_fn(index1))
                    .await
                {
                    println!("Error serving connection: {:?}", err);
                }
            });
        }
    };

    // === 第二个服务器 ===
    // 与第一个服务器结构完全相同，只是监听不同端口、使用不同处理函数
    let srv2 = async move {
        let listener = TcpListener::bind(addr2).await.unwrap();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);

            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(io, service_fn(index2))
                    .await
                {
                    println!("Error serving connection: {:?}", err);
                }
            });
        }
    };

    println!("Listening on http://{} and http://{}", addr1, addr2);

    // === 并发运行两个服务器 ===
    // `join` 同时驱动两个 Future，两个服务器同时开始监听和处理请求
    // 由于两个服务器的循环都是无限循环（永远不会返回），
    // 所以 join 也永远不会返回
    let _ret = join(srv1, srv2).await;

    Ok(())
}
