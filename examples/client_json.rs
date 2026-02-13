//! # hyper JSON 客户端示例
//!
//! 本示例演示了如何使用 hyper 发起 HTTP 请求并将响应体解析为 JSON。
//! 该客户端请求一个返回 JSON 数据的 API，将响应体完整收集后，
//! 使用 serde_json 反序列化为 Rust 结构体。
//!
//! ## 使用方法
//! ```bash
//! cargo run --example client_json
//! ```
//!
//! ## 核心知识点
//! - 使用 `collect().await?.aggregate()` 将流式响应体聚合为连续的字节缓冲区
//! - 使用 `serde_json::from_reader` 从缓冲区中反序列化 JSON
//! - 使用 `serde::Deserialize` derive 宏自动生成反序列化代码

// 拒绝所有编译警告
#![deny(warnings)]
// 启用 Rust 2018 惯用写法的警告
#![warn(rust_2018_idioms)]

// bytes 库提供高效的字节缓冲区
use bytes::Bytes;
// BodyExt 提供 collect() 等扩展方法，Empty 表示空的请求体
use http_body_util::{BodyExt, Empty};
// Buf trait 提供 reader() 方法，用于将字节缓冲区转换为 Read 接口
// Request 用于构建 HTTP 请求
use hyper::{body::Buf, Request};
// Deserialize trait 用于 JSON 反序列化
use serde::Deserialize;
// TcpStream 用于建立异步 TCP 连接
use tokio::net::TcpStream;

// 引入 TokioIo 适配器，将 tokio 的 I/O 接口适配为 hyper 所需的接口
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// 统一的 Result 类型别名，简化函数签名中的错误类型声明
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<()> {
    // 解析目标 URL —— 这是一个公开的 JSON 测试 API，返回用户列表
    let url = "http://jsonplaceholder.typicode.com/users".parse().unwrap();
    // 调用 fetch_json 获取用户列表
    let users = fetch_json(url).await?;
    // 使用 {:#?} 格式化打印用户列表（带缩进的 Debug 输出）
    println!("users: {:#?}", users);

    // 使用 fold 迭代器方法计算所有用户 ID 的总和
    // fold(初始值, 累加器闭包) —— 这里从 0 开始累加每个用户的 id
    let sum = users.iter().fold(0, |acc, user| acc + user.id);
    println!("sum of ids: {}", sum);
    Ok(())
}

/// 发送 HTTP GET 请求并将响应体解析为 User 结构体的 Vec
///
/// # 参数
/// - `url`: 目标 API 的 URI
///
/// # 流程
/// 1. 建立 TCP 连接并执行 HTTP/1.1 握手
/// 2. 发送 GET 请求
/// 3. 将响应体的所有数据块聚合为一个连续的缓冲区
/// 4. 使用 serde_json 将缓冲区中的 JSON 数据反序列化为 Vec<User>
async fn fetch_json(url: hyper::Uri) -> Result<Vec<User>> {
    // 从 URI 中提取主机名
    let host = url.host().expect("uri has no host");
    // 获取端口号，默认使用 HTTP 的 80 端口
    let port = url.port_u16().unwrap_or(80);
    let addr = format!("{}:{}", host, port);

    // 异步建立 TCP 连接
    let stream = TcpStream::connect(addr).await?;
    // 包装为 hyper 兼容的 I/O 类型
    let io = TokioIo::new(stream);

    // 执行 HTTP/1.1 握手，获取请求发送器和连接对象
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    // 在后台任务中持续驱动连接，使其保持活跃
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
    });

    // 获取 authority（host:port）用于 Host 请求头
    let authority = url.authority().unwrap().clone();

    // 构建 HTTP GET 请求
    // 请求体为空（Empty），因为 GET 请求不需要发送数据
    let req = Request::builder()
        .uri(url)
        .header(hyper::header::HOST, authority.as_str())
        .body(Empty::<Bytes>::new())?;

    // 发送请求并等待响应
    let res = sender.send_request(req).await?;

    // 异步收集响应体的所有数据块并聚合为连续的缓冲区
    // collect() 收集所有帧，aggregate() 将多个数据块合并为一个连续的 Buf
    // 这与 client.rs 中的逐帧流式读取不同 —— 这里需要完整的 JSON 才能解析
    let body = res.collect().await?.aggregate();

    // 使用 serde_json 从 Buf 的 reader 接口反序列化 JSON
    // body.reader() 返回一个实现了 std::io::Read 的适配器
    // 这样就不需要先将数据转为 String，可以直接从字节流中解析 JSON
    let users = serde_json::from_reader(body.reader())?;

    Ok(users)
}

/// 用户数据结构体
/// 对应 API 返回的 JSON 对象中的字段
///
/// `#[derive(Deserialize)]` 自动为结构体生成 JSON 反序列化代码
/// `#[derive(Debug)]` 允许使用 `{:?}` 格式化打印
#[derive(Deserialize, Debug)]
struct User {
    /// 用户 ID
    id: i32,
    /// 用户名（标记为 unused 以消除未使用字段的编译警告）
    #[allow(unused)]
    name: String,
}
