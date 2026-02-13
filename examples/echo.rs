//! # hyper 回显服务器示例
//!
//! 本示例演示了一个功能丰富的 HTTP 回显（Echo）服务器，支持多种路由处理：
//! - `GET /`：返回使用说明
//! - `POST /echo`：原样返回请求体
//! - `POST /echo/uppercase`：将请求体转为大写后返回（流式处理）
//! - `POST /echo/reversed`：将请求体反转后返回（需要先缓存完整请求体）
//!
//! ## 使用方法
//! ```bash
//! cargo run --example echo
//! # 然后在另一个终端中测试：
//! curl localhost:3000/echo -XPOST -d "hello world"
//! curl localhost:3000/echo/uppercase -XPOST -d "hello world"
//! curl localhost:3000/echo/reversed -XPOST -d "hello world"
//! ```
//!
//! ## 核心知识点
//! - 基于 HTTP 方法和路径的路由匹配
//! - `BoxBody` 类型擦除（允许不同路由返回不同的 Body 类型）
//! - 流式处理 vs 缓存式处理的对比
//! - 请求体大小限制（防止大请求体攻击）

// 拒绝所有编译警告
#![deny(warnings)]

use std::net::SocketAddr;

use bytes::Bytes;
// BoxBody: 类型擦除的 Body 包装器
// BodyExt: Body 扩展方法（如 boxed(), map_frame(), collect() 等）
// Empty: 空的 Body（用于没有响应体的情况）
// Full: 包含完整数据的 Body
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
// Frame 表示 HTTP Body 中的一个帧（数据帧或 trailers 帧）
use hyper::body::Frame;
// http1 模块提供 HTTP/1.1 服务器连接构建器
use hyper::server::conn::http1;
// service_fn 将一个异步函数转换为 hyper 的 Service trait 实现
use hyper::service::service_fn;
// Body trait 提供了流式读取请求体的能力
// Method: HTTP 方法枚举（GET, POST 等）
// Request/Response: HTTP 请求和响应类型
// StatusCode: HTTP 状态码
use hyper::{body::Body, Method, Request, Response, StatusCode};
// TcpListener 用于监听 TCP 连接
use tokio::net::TcpListener;

// TokioIo 适配器：桥接 tokio I/O 和 hyper I/O
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 回显服务处理函数
///
/// 这是核心的请求处理器（Service Handler），它接收一个 HTTP 请求，
/// 根据请求的方法和路径进行路由，返回相应的响应。
///
/// # 返回类型说明
/// 使用 `BoxBody<Bytes, hyper::Error>` 作为统一的响应体类型，
/// 这是因为不同路由可能返回不同的具体 Body 类型（Empty, Full, MappedBody 等），
/// 通过 BoxBody 进行类型擦除，使它们可以统一为同一种类型。
async fn echo(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // 使用 match 对 (HTTP方法, URI路径) 的元组进行模式匹配路由
    match (req.method(), req.uri().path()) {
        // ==================== GET / ====================
        // 返回使用说明文本
        (&Method::GET, "/") => Ok(Response::new(full(
            "Try POSTing data to /echo such as: `curl localhost:3000/echo -XPOST -d \"hello world\"`",
        ))),

        // ==================== POST /echo ====================
        // 最简单的回显：直接将请求体原样作为响应体返回
        // `req.into_body()` 消费请求并取出请求体
        // `.boxed()` 将具体的 Incoming 类型转为 BoxBody
        (&Method::POST, "/echo") => Ok(Response::new(req.into_body().boxed())),

        // ==================== POST /echo/uppercase ====================
        // 流式处理：将请求体的每个数据帧转为大写后返回
        // 优点：不需要等待整个请求体到达，可以边接收边处理
        (&Method::POST, "/echo/uppercase") => {
            // map_frame 对每个帧进行转换处理
            // 这是一种零缓存的流式处理方式
            let frame_stream = req.into_body().map_frame(|frame| {
                // 尝试将帧转为数据帧
                let frame = if let Ok(data) = frame.into_data() {
                    // 将每个字节转为 ASCII 大写
                    data.iter()
                        .map(|byte| byte.to_ascii_uppercase())
                        .collect::<Bytes>()
                } else {
                    // 如果不是数据帧（例如 trailers），返回空字节
                    Bytes::new()
                };

                // 包装为新的数据帧返回
                Frame::data(frame)
            });

            Ok(Response::new(frame_stream.boxed()))
        }

        // ==================== POST /echo/reversed ====================
        // 缓存式处理：需要先接收完整的请求体才能进行反转操作
        //
        // 与上面的大写处理不同，反转操作必须知道完整内容才能执行，
        // 所以需要使用 .await 等待所有数据到达，再一次性处理。
        (&Method::POST, "/echo/reversed") => {
            // === 安全防护：限制请求体大小 ===
            // 获取请求体的大小上限提示（如果有 Content-Length 头的话）
            // 如果没有大小提示，则假设为最大值 u64::MAX
            let max = req.body().size_hint().upper().unwrap_or(u64::MAX);
            // 拒绝超过 64KB 的请求体，防止恶意大请求消耗服务器内存
            if max > 1024 * 64 {
                let mut resp = Response::new(full("Body too big"));
                *resp.status_mut() = hyper::StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(resp);
            }

            // 收集并等待完整的请求体
            // collect() 异步收集所有帧，to_bytes() 将它们合并为连续的 Bytes
            let whole_body = req.collect().await?.to_bytes();

            // 反转字节序列
            // iter() 返回字节迭代器，rev() 反转，cloned() 克隆每个字节
            let reversed_body = whole_body.iter().rev().cloned().collect::<Vec<u8>>();
            Ok(Response::new(full(reversed_body)))
        }

        // ==================== 默认路由 ====================
        // 对于不匹配的路由，返回 404 Not Found
        _ => {
            let mut not_found = Response::new(empty());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}

/// 创建一个空的 BoxBody
///
/// 用于不需要响应体的情况（如 404 响应）
/// `map_err(|never| match never {})` 是一种类型转换技巧：
/// Empty 的错误类型是 Infallible（永远不会发生的错误），
/// 通过 match never {} 将其转换为 hyper::Error 类型
fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// 创建一个包含完整数据的 BoxBody
///
/// 将任何可以转为 Bytes 的类型包装为 BoxBody
/// 泛型约束 `T: Into<Bytes>` 允许传入 &str, String, Vec<u8> 等类型
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 绑定到本地回环地址的 3000 端口
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    // 创建 TCP 监听器，绑定到指定地址
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);
    // 主循环：持续接受新的 TCP 连接
    loop {
        // accept() 等待新连接到来
        // 返回 (TcpStream, 远程地址)
        let (stream, _) = listener.accept().await?;
        // 将 TcpStream 包装为 hyper 兼容的 I/O 类型
        let io = TokioIo::new(stream);

        // 为每个连接创建一个独立的异步任务
        // 这样主循环可以立即继续接受下一个连接，实现并发处理
        tokio::task::spawn(async move {
            // 使用 HTTP/1.1 构建器处理连接
            // service_fn(echo) 将 echo 函数转为 Service
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(echo))
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
