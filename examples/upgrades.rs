//! # hyper HTTP 协议升级（Upgrade）示例
//!
//! 本示例演示了 HTTP 协议升级机制的完整流程。
//! HTTP 升级允许客户端和服务器从 HTTP 协议切换到其他协议
//! （如 WebSocket、HTTP/2、或自定义协议）。
//!
//! 本示例实现了一个名为 "foobar" 的自定义协议：
//! 1. 客户端发送带有 `Upgrade: foobar` 头的 HTTP 请求
//! 2. 服务器返回 `101 Switching Protocols` 响应
//! 3. 连接升级后，双方使用自定义的 "foobar" 协议通信
//!    - 客户端发送 "foo=bar"
//!    - 服务器回复 "bar=foo"
//!
//! ## 使用方法
//! ```bash
//! cargo run --example upgrades
//! ```
//!
//! ## 核心知识点
//! - `hyper::upgrade::on()` 用于等待连接升级完成
//! - `conn.with_upgrades()` 启用连接的升级支持
//! - 升级后的连接变为原始的双向 I/O 流
//! - 使用 `watch::channel` 实现服务器的优雅关闭

// 拒绝所有编译警告
#![deny(warnings)]

// 注意：hyper::upgrade 的文档链接到了这个示例
use std::net::SocketAddr;
use std::str;

// AsyncReadExt/AsyncWriteExt 提供 read_exact, write_all, read_to_end 等方法
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
// watch channel: 单生产者多消费者的广播通道，用于信号通知
use tokio::sync::watch;

use bytes::Bytes;
use http_body_util::Empty;
// UPGRADE: HTTP "Upgrade" 头的常量
use hyper::header::{HeaderValue, UPGRADE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
// Upgraded: 代表已升级的连接，是一个双向 I/O 流
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, StatusCode};

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// 统一的 Result 类型别名
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 服务器端：升级后的 I/O 处理
///
/// 在连接升级为 "foobar" 协议后，服务器端的通信逻辑：
/// 1. 读取客户端发送的 7 个字节（"foo=bar"）
/// 2. 回复 "bar=foo"
async fn server_upgraded_io(upgraded: Upgraded) -> Result<()> {
    // 将 Upgraded 包装为 tokio 兼容的 I/O 类型
    let mut upgraded = TokioIo::new(upgraded);
    // 升级后的连接是原始的双向 I/O 流，
    // 可以直接读写数据（不再受 HTTP 协议约束）
    //
    // 因为这是完全受控的示例，我们确切知道客户端会发送 7 个字节，
    // 所以使用 read_exact 精确读取
    let mut vec = vec![0; 7];
    upgraded.read_exact(&mut vec).await?;
    println!("server[foobar] recv: {:?}", str::from_utf8(&vec));

    // 回复 "foobar" 协议的响应
    upgraded.write_all(b"bar=foo").await?;
    println!("server[foobar] sent");
    Ok(())
}

/// 服务器端：HTTP 升级请求处理函数
///
/// 处理来自客户端的 HTTP 请求，判断是否包含升级请求：
/// - 如果包含 Upgrade 头 → 返回 101 并在后台处理升级
/// - 如果不包含 → 返回 400 Bad Request
async fn server_upgrade(mut req: Request<hyper::body::Incoming>) -> Result<Response<Empty<Bytes>>> {
    let mut res = Response::new(Empty::new());

    // 检查请求是否包含 Upgrade 头
    // 如果没有 Upgrade 头，返回 400 Bad Request
    if !req.headers().contains_key(UPGRADE) {
        *res.status_mut() = StatusCode::BAD_REQUEST;
        return Ok(res);
    }

    // 设置升级后的处理逻辑并 spawn 到后台
    //
    // 关键点：`hyper::upgrade::on(&mut req)` 返回一个 Future，
    // 该 Future 在客户端收到 101 响应后才会完成（resolve）。
    // 因此必须 spawn 这个 Future 到后台，而不是 await 它，
    // 否则 101 响应永远无法返回给客户端（死锁）。
    tokio::task::spawn(async move {
        // 等待连接升级完成
        match hyper::upgrade::on(&mut req).await {
            Ok(upgraded) => {
                // 升级成功，使用新协议通信
                if let Err(e) = server_upgraded_io(upgraded).await {
                    eprintln!("server foobar io error: {}", e)
                };
            }
            Err(e) => eprintln!("upgrade error: {}", e),
        }
    });

    // 返回 101 Switching Protocols 响应
    // 告诉客户端服务器同意切换到 "foobar" 协议
    *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    res.headers_mut()
        .insert(UPGRADE, HeaderValue::from_static("foobar"));
    Ok(res)
}

/// 客户端：升级后的 I/O 处理
///
/// 在连接升级为 "foobar" 协议后，客户端的通信逻辑：
/// 1. 发送 "foo=bar"
/// 2. 读取服务器的回复（读取所有数据直到连接关闭）
async fn client_upgraded_io(upgraded: Upgraded) -> Result<()> {
    let mut upgraded = TokioIo::new(upgraded);
    // 发送 "foobar" 协议的请求
    upgraded.write_all(b"foo=bar").await?;
    println!("client[foobar] sent");

    // 读取服务器回复的所有数据
    let mut vec = Vec::new();
    upgraded.read_to_end(&mut vec).await?;
    println!("client[foobar] recv: {:?}", str::from_utf8(&vec));

    Ok(())
}

/// 客户端：发起 HTTP 升级请求
///
/// 完整流程：
/// 1. 建立 TCP 连接并完成 HTTP/1.1 握手
/// 2. 发送带有 `Upgrade: foobar` 头的请求
/// 3. 检查服务器是否返回 101
/// 4. 等待连接升级完成
/// 5. 在升级后的连接上执行自定义协议通信
async fn client_upgrade_request(addr: SocketAddr) -> Result<()> {
    // 构建包含 Upgrade 头的 HTTP 请求
    let req = Request::builder()
        .uri(format!("http://{}/", addr))
        .header(UPGRADE, "foobar")
        .body(Empty::<Bytes>::new())
        .expect("uri/header parse won't error");

    // 建立 TCP 连接并执行 HTTP/1.1 握手
    let stream = TcpStream::connect(addr).await?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    // 在后台驱动连接
    // 重要：必须调用 .with_upgrades() 来启用客户端的升级支持
    tokio::task::spawn(async move {
        if let Err(err) = conn.with_upgrades().await {
            println!("Connection failed: {:?}", err);
        }
    });

    // 发送升级请求
    let res = sender.send_request(req).await?;

    // 验证服务器是否同意升级（返回 101 Switching Protocols）
    if res.status() != StatusCode::SWITCHING_PROTOCOLS {
        panic!("Our server didn't upgrade: {}", res.status());
    }

    // 等待连接升级完成并使用新协议通信
    match hyper::upgrade::on(res).await {
        Ok(upgraded) => {
            if let Err(e) = client_upgraded_io(upgraded).await {
                eprintln!("client foobar io error: {}", e)
            };
        }
        Err(e) => eprintln!("upgrade error: {}", e),
    }

    Ok(())
}

/// 程序入口函数
///
/// 启动服务器和客户端，完成一次完整的协议升级流程后关闭
#[tokio::main]
async fn main() {
    // 在本示例中，我们自己创建服务器和客户端来互相通信，
    // 所以不需要指定确切的端口，让操作系统分配一个未使用的端口
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();

    let listener = TcpListener::bind(addr).await.expect("failed to bind");

    // 获取操作系统分配的实际地址（包含端口号），以便客户端连接
    let addr = listener.local_addr().unwrap();

    // 创建 watch 通道用于控制服务器关闭
    // watch channel 是单生产者多消费者的广播通道
    // tx 用于发送关闭信号，rx 用于接收关闭信号
    let (tx, mut rx) = watch::channel(false);

    // 在后台 spawn 服务器任务
    tokio::task::spawn(async move {
        loop {
            // 使用 tokio::select! 在接受连接和关闭信号之间竞争
            tokio::select! {
                // 分支1：接受新连接
                res = listener.accept() => {
                    let (stream, _) = res.expect("Failed to accept");
                    let io = TokioIo::new(stream);

                    let mut rx = rx.clone();
                    tokio::task::spawn(async move {
                        // 创建 HTTP/1.1 连接并启用升级支持
                        let conn = http1::Builder::new().serve_connection(io, service_fn(server_upgrade));

                        // 重要：必须调用 .with_upgrades() 启用服务器端的升级支持
                        let mut conn = conn.with_upgrades();

                        // 使用 Pin 固定连接对象以便在 select! 中使用
                        let mut conn = Pin::new(&mut conn);

                        tokio::select! {
                            // 分支A：连接正常完成
                            res = &mut conn => {
                                if let Err(err) = res {
                                    println!("Error serving connection: {:?}", err);
                                }
                            }
                            // 分支B：收到关闭信号，启用优雅关闭后继续轮询连接
                            _ = rx.changed() => {
                                conn.graceful_shutdown();
                            }
                        }
                    });
                }
                // 分支2：收到关闭信号，停止接受新连接
                _ = rx.changed() => {
                    break;
                }
            }
        }
    });

    // 客户端发起升级请求
    let request = client_upgrade_request(addr);
    if let Err(e) = request.await {
        eprintln!("client error: {}", e);
    }

    // 发送关闭信号，通知服务器停止监听并关闭
    let _ = tx.send(true);
}
