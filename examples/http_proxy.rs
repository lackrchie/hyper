//! # hyper HTTP 代理服务器示例
//!
//! 本示例演示了如何使用 hyper 构建一个完整的 HTTP 正向代理服务器。
//! 该代理支持两种代理模式：
//! - **普通 HTTP 代理**：直接转发 HTTP 请求到目标服务器
//! - **CONNECT 隧道代理**：用于 HTTPS 流量，建立 TCP 隧道透传加密数据
//!
//! ## 使用方法
//! ```bash
//! # 1. 启动代理服务器
//! cargo run --example http_proxy
//! # 2. 配置系统代理
//! export http_proxy=http://127.0.0.1:8100
//! export https_proxy=http://127.0.0.1:8100
//! # 3. 发送请求（流量将通过代理）
//! curl -i https://www.example.com/
//! ```
//!
//! ## 核心知识点
//! - HTTP CONNECT 方法和隧道代理的工作原理
//! - `hyper::upgrade::on` 实现 HTTP 协议升级
//! - `tokio::io::copy_bidirectional` 实现双向数据透传
//! - `preserve_header_case` 和 `title_case_headers` 保持代理的透明性

// 拒绝所有编译警告
#![deny(warnings)]

use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::service::service_fn;
// Upgraded 代表一个已升级的连接（从 HTTP 协议切换为原始 TCP 通道）
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response};

use tokio::net::{TcpListener, TcpStream};

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// 类型别名：简化 HTTP/1.1 客户端和服务器构建器的类型声明
type ClientBuilder = hyper::client::conn::http1::Builder;
type ServerBuilder = hyper::server::conn::http1::Builder;

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 代理服务器监听在 8100 端口
    let addr = SocketAddr::from(([127, 0, 0, 1], 8100));

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    // 主循环：接受客户端连接
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        // 每个连接在独立的任务中处理
        tokio::task::spawn(async move {
            if let Err(err) = ServerBuilder::new()
                // 保持请求头的原始大小写（代理应该尽量透明，不改变请求内容）
                .preserve_header_case(true)
                // 使用 Title-Case 格式的请求头名称（如 Content-Type 而非 content-type）
                .title_case_headers(true)
                // 启用连接的代理服务
                .serve_connection(io, service_fn(proxy))
                // 启用升级支持（用于处理 CONNECT 隧道）
                // 没有这个调用，CONNECT 请求的升级将不会工作
                .with_upgrades()
                .await
            {
                println!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

/// 代理请求处理函数
///
/// 根据请求方法分为两种处理模式：
/// 1. **CONNECT 方法**：建立 TCP 隧道（用于 HTTPS 等需要端到端加密的场景）
/// 2. **其他方法**（GET, POST 等）：直接转发 HTTP 请求
async fn proxy(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    println!("req: {:?}", req);

    if Method::CONNECT == req.method() {
        // ==================== CONNECT 隧道代理 ====================
        //
        // CONNECT 请求的格式如下：
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // CONNECT 方法的工作流程：
        // 1. 代理收到 CONNECT 请求
        // 2. 代理返回 200 OK 空响应（表示同意建立隧道）
        // 3. 客户端收到 200 后，连接升级为原始 TCP 通道
        // 4. 代理在客户端和目标服务器之间双向透传数据
        //
        // 注意：必须先返回空的 200 响应，然后才能升级连接，
        // 所以不能在 on_upgrade 的 Future 内部返回响应。
        if let Some(addr) = host_addr(req.uri()) {
            // 在后台任务中等待连接升级完成，然后建立隧道
            tokio::task::spawn(async move {
                // 等待 HTTP 连接升级完成
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        // 升级成功，建立 TCP 隧道进行双向数据透传
                        if let Err(e) = tunnel(upgraded, addr).await {
                            eprintln!("server io error: {}", e);
                        };
                    }
                    Err(e) => eprintln!("upgrade error: {}", e),
                }
            });

            // 先返回空的 200 响应，告知客户端隧道已建立
            Ok(Response::new(empty()))
        } else {
            // CONNECT 请求的 URI 必须是 host:port 格式
            eprintln!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    } else {
        // ==================== 普通 HTTP 代理 ====================
        // 对于 GET, POST 等普通 HTTP 请求，直接转发到目标服务器

        // 从请求 URI 中提取目标主机和端口
        let host = req.uri().host().expect("uri has no host");
        let port = req.uri().port_u16().unwrap_or(80);

        // 建立到目标服务器的 TCP 连接
        let stream = TcpStream::connect((host, port)).await.unwrap();
        let io = TokioIo::new(stream);

        // 与目标服务器执行 HTTP/1.1 握手
        let (mut sender, conn) = ClientBuilder::new()
            // 保持原始头部大小写（代理透明性）
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        // 在后台驱动与目标服务器的连接
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
        });

        // 将客户端请求原样转发给目标服务器，并返回目标服务器的响应
        let resp = sender.send_request(req).await?;
        // 将响应体映射为 BoxBody 类型
        Ok(resp.map(|b| b.boxed()))
    }
}

/// 从 URI 中提取 authority（host:port）部分
///
/// 对于 CONNECT 请求，URI 的格式是 `host:port`（不是完整的 URL）
fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

/// 创建空的 BoxBody（用于 CONNECT 响应）
fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// 创建包含完整数据的 BoxBody（用于错误响应）
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

/// 建立 TCP 隧道
///
/// 在已升级的 HTTP 连接和目标服务器之间建立双向数据透传通道。
/// 这是 CONNECT 隧道代理的核心功能。
///
/// # 参数
/// - `upgraded`: 已升级的 HTTP 连接（客户端侧）
/// - `addr`: 目标服务器地址（host:port）
///
/// # 工作原理
/// 使用 `tokio::io::copy_bidirectional` 同时在两个方向上复制数据：
/// - 客户端 -> 目标服务器
/// - 目标服务器 -> 客户端
/// 这种透传方式使代理对加密流量（如 HTTPS/TLS）完全透明
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // 连接到目标远程服务器
    let mut server = TcpStream::connect(addr).await?;
    // 将升级后的连接包装为 tokio 兼容的 I/O 类型
    let mut upgraded = TokioIo::new(upgraded);

    // 双向数据透传
    // copy_bidirectional 会同时在两个方向上复制数据，直到任一方关闭连接
    // 返回 (客户端写入的字节数, 服务器写入的字节数)
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    // 隧道关闭后打印流量统计
    println!(
        "client wrote {} bytes and received {} bytes",
        from_client, from_server
    );

    Ok(())
}
