//! # hyper HTTP 客户端示例
//!
//! 本示例演示了如何使用 hyper 构建一个基本的 HTTP/1.1 客户端。
//! 该客户端从命令行参数获取 URL，发起 HTTP GET 请求，
//! 并以流式方式将响应体逐块输出到标准输出。
//!
//! ## 使用方法
//! ```bash
//! cargo run --example client -- http://httpbin.org/ip
//! ```
//!
//! ## 核心知识点
//! - 使用 `TcpStream` 建立底层 TCP 连接
//! - 通过 `hyper::client::conn::http1::handshake` 执行 HTTP/1.1 握手
//! - 使用 `tokio::task::spawn` 在后台驱动连接
//! - 以流式方式（逐帧）读取响应体，避免将整个响应缓存到内存中

// 拒绝所有编译警告，确保代码质量
#![deny(warnings)]
// 启用 Rust 2018 惯用写法的警告提示
#![warn(rust_2018_idioms)]
use std::env;

// bytes 库提供了高效的字节缓冲区类型
use bytes::Bytes;
// BodyExt 提供了 HTTP Body 的扩展方法（如 frame()），Empty 表示空的请求体
use http_body_util::{BodyExt, Empty};
// Request 用于构建 HTTP 请求
use hyper::Request;
// AsyncWriteExt 提供异步写入扩展方法（如 write_all）
use tokio::io::{self, AsyncWriteExt as _};
// TcpStream 用于建立 TCP 连接
use tokio::net::TcpStream;

// 引入基准测试中的支持模块，提供 TokioIo 适配器
// TokioIo 将 tokio 的 I/O trait 适配为 hyper 所需的 I/O trait
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// 定义统一的 Result 类型别名，简化错误处理（DRY 原则：Don't Repeat Yourself）
// 使用 Box<dyn Error> 可以接受任何实现了 Error trait 的错误类型
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 程序入口函数
/// `#[tokio::main]` 宏会自动创建 tokio 异步运行时
#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统，可通过 RUST_LOG 环境变量控制日志级别
    // 例如：RUST_LOG=debug cargo run --example client
    pretty_env_logger::init();

    // 从命令行参数中获取 URL（第一个参数是程序名，第二个才是用户输入的 URL）
    let url = match env::args().nth(1) {
        Some(url) => url,
        None => {
            println!("Usage: client <url>");
            return Ok(());
        }
    };

    // 将字符串解析为 hyper::Uri 类型
    // HTTPS 需要额外的 TLS 实现（如 rustls 或 native-tls），
    // 本示例仅支持 HTTP，所以这里给出明确提示
    let url = url.parse::<hyper::Uri>().unwrap();
    if url.scheme_str() != Some("http") {
        println!("This example only works with 'http' URLs.");
        return Ok(());
    }

    // 调用核心的 URL 请求函数
    fetch_url(url).await
}

/// 发起 HTTP GET 请求并以流式方式输出响应内容
///
/// # 参数
/// - `url`: 要请求的 HTTP URL
///
/// # 流程
/// 1. 解析主机和端口，建立 TCP 连接
/// 2. 执行 HTTP/1.1 握手，获取请求发送器和连接对象
/// 3. 在后台任务中驱动连接（保持连接活跃）
/// 4. 构建并发送 HTTP GET 请求
/// 5. 逐帧读取并输出响应体
async fn fetch_url(url: hyper::Uri) -> Result<()> {
    // 从 URI 中提取主机名，如果 URI 中没有主机名则 panic
    let host = url.host().expect("uri has no host");
    // 获取端口号，如果 URI 中未指定端口则使用 HTTP 默认端口 80
    let port = url.port_u16().unwrap_or(80);
    // 拼接地址字符串，格式为 "host:port"
    let addr = format!("{}:{}", host, port);
    // 通过 tokio 的 TcpStream 异步建立 TCP 连接
    let stream = TcpStream::connect(addr).await?;
    // 使用 TokioIo 适配器将 tokio 的 TcpStream 包装为 hyper 兼容的 I/O 类型
    let io = TokioIo::new(stream);

    // 执行 HTTP/1.1 握手
    // 返回值：
    // - sender: 用于发送 HTTP 请求的 SendRequest 句柄
    // - conn: 表示底层连接的 Future，需要被持续 poll 以驱动连接
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    // 在后台任务中驱动连接
    // 这是必要的，因为 HTTP/1.1 连接需要持续被轮询来处理收发数据
    // 如果不 spawn 这个任务，连接将无法正常工作
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
    });

    // 获取 URI 中的 authority 部分（即 host:port），用于设置 Host 请求头
    let authority = url.authority().unwrap().clone();

    // 获取请求路径（不含协议和主机部分）
    let path = url.path();
    // 构建 HTTP GET 请求
    // - uri: 只需要路径部分（因为 Host 头已经指定了目标服务器）
    // - Host 头: HTTP/1.1 规范要求必须包含 Host 头
    // - body: 使用空的请求体（GET 请求通常不需要请求体）
    let req = Request::builder()
        .uri(path)
        .header(hyper::header::HOST, authority.as_str())
        .body(Empty::<Bytes>::new())?;

    // 发送请求并等待响应
    let mut res = sender.send_request(req).await?;

    // 打印响应状态码（如 200 OK、404 Not Found 等）
    println!("Response: {}", res.status());
    // 打印响应头（使用 {:#?} 格式化输出，更加美观）
    println!("Headers: {:#?}\n", res.headers());

    // 以流式方式读取响应体
    // 与一次性缓存整个响应不同，这里逐帧（frame）读取数据，
    // 每收到一个数据块就立即写入标准输出，适合处理大响应体
    while let Some(next) = res.frame().await {
        let frame = next?;
        // 检查当前帧是否包含数据（HTTP/2 中帧可能是数据帧或头帧）
        if let Some(chunk) = frame.data_ref() {
            // 将数据块异步写入标准输出
            io::stdout().write_all(chunk).await?;
        }
    }

    println!("\n\nDone!");

    Ok(())
}
