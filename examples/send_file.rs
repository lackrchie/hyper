//! # hyper 文件发送服务器示例
//!
//! 本示例演示了如何使用 hyper 构建一个静态文件服务器，
//! 以流式方式发送文件内容给客户端。
//!
//! ## 使用方法
//! ```bash
//! cargo run --example send_file
//! # 在浏览器或终端中访问：
//! curl http://127.0.0.1:1337/
//! curl http://127.0.0.1:1337/no_file.html  # 测试文件不存在的情况
//! ```
//!
//! ## 核心知识点
//! - 使用 `tokio::fs::File` 异步读取文件
//! - 使用 `tokio_util::io::ReaderStream` 将文件转为异步字节流
//! - 使用 `StreamBody` 将字节流转为 HTTP Body
//! - 流式发送文件（不需要将整个文件加载到内存中，适合大文件）

// 拒绝所有编译警告
#![deny(warnings)]

use std::net::SocketAddr;

use bytes::Bytes;
// TryStreamExt 提供 map_ok 等流操作方法
use futures_util::TryStreamExt;
// BoxBody: 类型擦除的 Body，BodyExt: 扩展方法
// Full: 完整数据体，StreamBody: 将 Stream 包装为 Body
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
// Frame 表示 HTTP Body 中的一个帧
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, Result, StatusCode};
// tokio 的异步文件 I/O 和 TCP 监听器
use tokio::{fs::File, net::TcpListener};
// ReaderStream 将实现了 AsyncRead 的类型转为 Stream
use tokio_util::io::ReaderStream;

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// 默认的索引文件路径（相对于项目根目录）
static INDEX: &str = "examples/send_file_index.html";
// 文件未找到时的响应内容
static NOTFOUND: &[u8] = b"Not Found";

/// 程序入口函数
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 绑定到 127.0.0.1:1337
    let addr: SocketAddr = "127.0.0.1:1337".parse().unwrap();

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    // 主循环：接受连接
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        // 为每个连接 spawn 一个异步任务
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(response_examples))
                .await
            {
                println!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

/// 路由分发函数
///
/// 根据请求的方法和路径决定发送哪个文件：
/// - `GET /` 或 `GET /index.html`：发送索引页面
/// - `GET /no_file.html`：故意请求一个不存在的文件（测试错误处理）
/// - 其他路由：返回 404
async fn response_examples(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, std::io::Error>>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") | (&Method::GET, "/index.html") => simple_file_send(INDEX).await,
        (&Method::GET, "/no_file.html") => {
            // 测试文件不存在时的处理
            simple_file_send("this_file_should_not_exist.html").await
        }
        _ => Ok(not_found()),
    }
}

/// 构建 404 Not Found 响应
fn not_found() -> Response<BoxBody<Bytes, std::io::Error>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        // Full 的错误类型是 Infallible，通过 map_err 转换为 std::io::Error
        .body(Full::new(NOTFOUND.into()).map_err(|e| match e {}).boxed())
        .expect("constant status won't error")
}

/// 以流式方式发送文件内容
///
/// # 参数
/// - `filename`: 要发送的文件路径
///
/// # 流程
/// 1. 异步打开文件
/// 2. 将文件包装为 ReaderStream（异步字节流）
/// 3. 将字节流转为 StreamBody（HTTP Body）
/// 4. 将 StreamBody 包装为 BoxBody 并返回
///
/// ## 流式发送的优势
/// 文件内容不会被一次性加载到内存中，而是按块读取和发送。
/// 对于大文件来说，这种方式可以显著降低内存使用。
async fn simple_file_send(filename: &str) -> Result<Response<BoxBody<Bytes, std::io::Error>>> {
    // 异步打开文件
    let file = File::open(filename).await;
    if file.is_err() {
        eprintln!("ERROR: Unable to open file.");
        return Ok(not_found());
    }

    let file: File = file.unwrap();

    // 将 tokio::fs::File 包装为 ReaderStream
    // ReaderStream 会自动将文件内容按块（默认 4KB）读取为 Bytes 流
    let reader_stream = ReaderStream::new(file);

    // 将字节流转换为 HTTP Body
    // map_ok(Frame::data) 将每个 Bytes 块包装为 HTTP 数据帧
    // StreamBody 接受一个产生 Frame 的 Stream，将其适配为 HTTP Body
    let stream_body = StreamBody::new(reader_stream.map_ok(Frame::data));
    // 使用 boxed() 进行类型擦除，统一返回类型
    let boxed_body = stream_body.boxed();

    // 构建 200 OK 响应并附带文件内容作为响应体
    let response = Response::builder()
        .status(StatusCode::OK)
        .body(boxed_body)
        .expect("constant status won't error");

    Ok(response)
}
