//! # hyper Web API 服务器示例
//!
//! 本示例演示了如何使用 hyper 构建一个提供 JSON API 的 Web 服务器。
//! 该服务器同时扮演 API 服务器和 API 消费者的角色：
//! - 对外提供 JSON API 端点
//! - 内部通过 HTTP 客户端调用自身的 API（模拟微服务间调用）
//!
//! ## 路由说明
//! - `GET /`：返回一个指向 test.html 的链接
//! - `GET /test.html`：作为客户端向自身的 `/json_api` 发送 POST 请求，返回结果
//! - `POST /json_api`：接收 JSON 数据，添加 "test" 字段后返回
//! - `GET /json_api`：返回一个 JSON 数组 `["foo", "bar"]`
//!
//! ## 使用方法
//! ```bash
//! cargo run --example web_api
//! # 测试 GET JSON API：
//! curl http://127.0.0.1:1337/json_api
//! # 测试 POST JSON API：
//! curl -XPOST http://127.0.0.1:1337/json_api -H "Content-Type: application/json" -d '{"key":"value"}'
//! # 测试内部 API 调用：
//! curl http://127.0.0.1:1337/test.html
//! ```
//!
//! ## 核心知识点
//! - 在同一个服务器中同时处理静态内容和 JSON API
//! - 使用 serde_json 进行 JSON 序列化和反序列化
//! - 服务器内部发起 HTTP 客户端请求（自调用）
//! - 正确设置 `Content-Type: application/json` 头

// 拒绝所有编译警告
#![deny(warnings)]

use std::net::SocketAddr;

// Buf trait 提供 reader() 方法，将字节缓冲区转为 Read 接口
use bytes::{Buf, Bytes};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
// IncomingBody: 服务器收到的请求体类型
// header: HTTP 头部常量模块
use hyper::{body::Incoming as IncomingBody, header, Method, Request, Response, StatusCode};
use tokio::net::{TcpListener, TcpStream};

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// === 类型别名 ===
/// 通用错误类型
type GenericError = Box<dyn std::error::Error + Send + Sync>;
/// 统一的 Result 类型
type Result<T> = std::result::Result<T, GenericError>;
/// BoxBody 类型别名：类型擦除的 HTTP Body
type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

// === 静态常量 ===
/// 首页 HTML 内容
static INDEX: &[u8] = b"<a href=\"test.html\">test.html</a>";
/// 内部服务器错误的响应内容
static INTERNAL_SERVER_ERROR: &[u8] = b"Internal Server Error";
/// 404 响应内容
static NOTFOUND: &[u8] = b"Not Found";
/// 客户端请求的 POST 数据
static POST_DATA: &str = r#"{"original": "data"}"#;
/// 内部 API 调用的目标 URL
static URL: &str = "http://127.0.0.1:1337/json_api";

/// 客户端请求：向自身的 JSON API 发送 POST 请求
///
/// 这个函数模拟了微服务间的 HTTP 调用。
/// 它向 `http://127.0.0.1:1337/json_api` 发送一个 POST 请求，
/// 携带 JSON 数据，然后将收到的响应返回给调用者。
async fn client_request_response() -> Result<Response<BoxBody>> {
    // 构建 POST 请求，携带 JSON 数据
    let req = Request::builder()
        .method(Method::POST)
        .uri(URL)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(POST_DATA)))
        .expect("uri/header parse from constants won't error");

    // 从请求 URI 中提取目标地址
    let host = req.uri().host().expect("uri has no host");
    let port = req.uri().port_u16().expect("uri has no port");
    // 建立 TCP 连接
    let stream = TcpStream::connect(format!("{}:{}", host, port)).await?;
    let io = TokioIo::new(stream);

    // HTTP/1.1 握手
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    // 在后台驱动连接
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection error: {:?}", err);
        }
    });

    // 发送请求并获取响应
    let web_res = sender.send_request(req).await?;

    // 将响应体转为 BoxBody 返回
    let res_body = web_res.into_body().boxed();

    Ok(Response::new(res_body))
}

/// 处理 POST /json_api 请求
///
/// 接收 JSON 请求体，解析后添加一个 "test" 字段，再返回修改后的 JSON
async fn api_post_response(req: Request<IncomingBody>) -> Result<Response<BoxBody>> {
    // 异步收集并聚合请求体
    let whole_body = req.collect().await?.aggregate();
    // 使用 serde_json 从字节流中反序列化 JSON
    let mut data: serde_json::Value = serde_json::from_reader(whole_body.reader())?;
    // 向 JSON 对象中添加新字段
    data["test"] = serde_json::Value::from("test_value");
    // 将修改后的 JSON 序列化为字符串
    let json = serde_json::to_string(&data)?;
    // 构建 JSON 响应，设置正确的 Content-Type 头
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(json))?;
    Ok(response)
}

/// 处理 GET /json_api 请求
///
/// 返回一个固定的 JSON 数组 `["foo", "bar"]`
async fn api_get_response() -> Result<Response<BoxBody>> {
    let data = vec!["foo", "bar"];
    let res = match serde_json::to_string(&data) {
        Ok(json) => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(full(json))
            .expect("header parse from constant won't error"),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(full(INTERNAL_SERVER_ERROR))
            .expect("constant status won't error"),
    };
    Ok(res)
}

/// 主路由分发函数
///
/// 根据 HTTP 方法和路径将请求分发到对应的处理函数
async fn response_examples(req: Request<IncomingBody>) -> Result<Response<BoxBody>> {
    match (req.method(), req.uri().path()) {
        // GET / 或 GET /index.html → 返回首页
        (&Method::GET, "/") | (&Method::GET, "/index.html") => Ok(Response::new(full(INDEX))),
        // GET /test.html → 发起内部 API 调用并返回结果
        (&Method::GET, "/test.html") => client_request_response().await,
        // POST /json_api → 处理 JSON POST 请求
        (&Method::POST, "/json_api") => api_post_response(req).await,
        // GET /json_api → 返回 JSON 数组
        (&Method::GET, "/json_api") => api_get_response().await,
        // 其他路由 → 返回 404 Not Found
        _ => {
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(full(NOTFOUND))
                .expect("constant status won't error"))
        }
    }
}

/// 将任意可转为 Bytes 的类型包装为 BoxBody
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 绑定到 127.0.0.1:1337
    let addr: SocketAddr = "127.0.0.1:1337".parse().unwrap();

    let listener = TcpListener::bind(&addr).await?;
    println!("Listening on http://{}", addr);
    // 主循环：接受连接并处理
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        // 为每个连接 spawn 一个异步任务
        tokio::task::spawn(async move {
            let service = service_fn(response_examples);

            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                println!("Failed to serve connection: {:?}", err);
            }
        });
    }
}
