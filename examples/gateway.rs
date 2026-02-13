//! # hyper HTTP 网关/反向代理示例
//!
//! 本示例演示了如何使用 hyper 构建一个简单的 HTTP 网关（反向代理）。
//! 该网关监听端口 3001，将所有收到的请求转发到端口 3000 的后端服务器，
//! 并将后端的响应原样返回给客户端。
//!
//! ## 使用方法
//! ```bash
//! # 先启动后端服务器（例如 hello 示例）
//! cargo run --example hello &
//! # 然后启动网关
//! cargo run --example gateway
//! # 通过网关访问后端服务
//! curl http://127.0.0.1:3001/
//! ```
//!
//! ## 核心知识点
//! - 反向代理的基本实现模式：接收请求 -> 修改 URI -> 转发到后端 -> 返回响应
//! - 在 service_fn 闭包中同时扮演"服务器"和"客户端"的角色
//! - 使用 `move` 闭包捕获外部变量

// 拒绝所有编译警告
#![deny(warnings)]

// 引入 HTTP/1.1 服务器连接构建器和 service_fn
use hyper::{server::conn::http1, service::service_fn};
use std::net::SocketAddr;
// TcpListener 用于监听入站连接，TcpStream 用于连接后端服务器
use tokio::net::{TcpListener, TcpStream};

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 网关监听地址（入站地址）—— 客户端连接到这个地址
    let in_addr: SocketAddr = ([127, 0, 0, 1], 3001).into();
    // 后端服务器地址（出站地址）—— 网关将请求转发到这个地址
    let out_addr: SocketAddr = ([127, 0, 0, 1], 3000).into();

    // 克隆出站地址，以便在闭包中使用
    // （因为闭包会 move 捕获变量，原始变量之后可能还需要使用）
    let out_addr_clone = out_addr;

    // 在入站地址上创建 TCP 监听器
    let listener = TcpListener::bind(in_addr).await?;

    println!("Listening on http://{}", in_addr);
    println!("Proxying on http://{}", out_addr);

    // 主循环：持续接受客户端连接
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        // 创建服务处理函数
        // `service_fn` 将一个闭包转换为 hyper 的 Service
        // `move` 关键字使闭包获得 out_addr_clone 的所有权
        let service = service_fn(move |mut req| {
            // === 第一步：修改请求的 URI ===
            // 将请求的 URI 重写为指向后端服务器
            // 保留原始请求的路径和查询参数
            let uri_string = format!(
                "http://{}{}",
                out_addr_clone,
                req.uri()
                    .path_and_query()
                    .map(|x| x.as_str())
                    .unwrap_or("/")
            );
            let uri = uri_string.parse().unwrap();
            // 用新的 URI 替换请求中的原始 URI
            *req.uri_mut() = uri;

            // === 第二步：从新的 URI 中提取后端地址 ===
            let host = req.uri().host().expect("uri has no host");
            let port = req.uri().port_u16().unwrap_or(80);
            let addr = format!("{}:{}", host, port);

            // === 第三步：连接后端服务器并转发请求 ===
            async move {
                // 建立到后端服务器的 TCP 连接
                let client_stream = TcpStream::connect(addr).await.unwrap();
                let io = TokioIo::new(client_stream);

                // 执行 HTTP/1.1 握手
                let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
                // 在后台驱动连接
                tokio::task::spawn(async move {
                    if let Err(err) = conn.await {
                        println!("Connection failed: {:?}", err);
                    }
                });

                // 将修改后的请求转发到后端，并返回后端的响应
                sender.send_request(req).await
            }
        });

        // 为每个客户端连接创建独立的异步任务
        tokio::task::spawn(async move {
            // 使用 HTTP/1.1 处理客户端连接，将请求交给 service 处理
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                println!("Failed to serve the connection: {:?}", err);
            }
        });
    }
}
