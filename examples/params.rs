//! # hyper 请求参数处理示例
//!
//! 本示例演示了如何在 HTTP 服务器中处理各种请求参数：
//! - **POST 表单数据**：解析 `application/x-www-form-urlencoded` 格式的表单提交
//! - **GET 查询参数**：解析 URL 中的查询字符串
//!
//! ## 使用方法
//! ```bash
//! cargo run --example params
//! # 测试 POST 表单：
//! curl -XPOST "http://127.0.0.1:1337/post" -d "name=Alice&number=42"
//! # 测试 GET 查询参数：
//! curl "http://127.0.0.1:1337/get?page=home"
//! # 查看 HTML 表单：
//! curl "http://127.0.0.1:1337/"
//! ```
//!
//! ## 核心知识点
//! - `form_urlencoded::parse` 解析 URL 编码的表单数据
//! - 参数校验和多种错误响应处理
//! - BoxBody 和 Infallible 错误类型的使用

// 注意：此文件的 deny(warnings) 被注释掉了，因为存在一个已知的 Rust 编译器问题
// 参见: https://github.com/rust-lang/rust/issues/62411
// #![deny(warnings)]
#![warn(rust_2018_idioms)]

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use tokio::net::TcpListener;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

// === 静态响应内容 ===
// HTML 表单页面，包含 name 和 number 两个输入字段
static INDEX: &[u8] = b"<html><body><form action=\"post\" method=\"post\">Name: <input type=\"text\" name=\"name\"><br>Number: <input type=\"text\" name=\"number\"><br><input type=\"submit\"></body></html>";
// 缺少必要字段时的错误消息
static MISSING: &[u8] = b"Missing field";
// 数字字段格式错误时的错误消息
static NOTNUMERIC: &[u8] = b"Number field is not numeric";

/// 参数处理示例的主服务函数
///
/// 根据 HTTP 方法和路径分发到不同的处理逻辑：
/// - `GET /` 或 `GET /post`：返回 HTML 表单页面
/// - `POST /post`：处理表单提交（name + number）
/// - `GET /get`：处理查询参数（page）
/// - 其他路由：返回 404
async fn param_example(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, hyper::Error> {
    match (req.method(), req.uri().path()) {
        // ==================== GET / 或 GET /post ====================
        // 返回包含 HTML 表单的页面
        (&Method::GET, "/") | (&Method::GET, "/post") => Ok(Response::new(full(INDEX))),

        // ==================== POST /post ====================
        // 处理表单提交的数据
        (&Method::POST, "/post") => {
            // 收集完整的请求体并转为 Bytes
            let b = req.collect().await?.to_bytes();
            // 使用 form_urlencoded 库解析 URL 编码的表单数据
            // 例如 "name=Alice&number=42" 会被解析为 HashMap
            //
            // 注意：这是简化的处理方式。在实际应用中，同一个表单字段名
            // 可能出现多次（如多选框），应该使用 HashMap<String, Vec<String>>
            // 来存储。但对于本示例，简化的方式已经足够。
            let params = form_urlencoded::parse(b.as_ref())
                .into_owned()
                .collect::<HashMap<String, String>>();

            // === 参数校验 ===
            // 验证 "name" 字段是否存在
            let name = if let Some(n) = params.get("name") {
                n
            } else {
                // 缺少 name 字段，返回 422 Unprocessable Entity
                return Ok(Response::builder()
                    .status(StatusCode::UNPROCESSABLE_ENTITY)
                    .body(full(MISSING))
                    .expect("constant status won't error"));
            };
            // 验证 "number" 字段是否存在且为有效数字
            let number = if let Some(n) = params.get("number") {
                // 尝试将字符串解析为 f64 浮点数
                if let Ok(v) = n.parse::<f64>() {
                    v
                } else {
                    // 字段存在但不是有效数字
                    return Ok(Response::builder()
                        .status(StatusCode::UNPROCESSABLE_ENTITY)
                        .body(full(NOTNUMERIC))
                        .expect("constant status won't error"));
                }
            } else {
                // 缺少 number 字段
                return Ok(Response::builder()
                    .status(StatusCode::UNPROCESSABLE_ENTITY)
                    .body(full(MISSING))
                    .expect("constant status won't error"));
            };

            // 参数校验通过，生成并返回响应
            // 在实际应用中，这里通常会涉及数据库操作或外部服务调用，
            // 这些操作可能失败，所以可能需要返回 500 InternalServerError
            let body = format!("Hello {}, your number is {}", name, number);
            Ok(Response::new(full(body)))
        }

        // ==================== GET /get ====================
        // 处理 URL 查询参数
        (&Method::GET, "/get") => {
            // 获取 URI 中的查询字符串（? 之后的部分）
            let query = if let Some(q) = req.uri().query() {
                q
            } else {
                return Ok(Response::builder()
                    .status(StatusCode::UNPROCESSABLE_ENTITY)
                    .body(full(MISSING))
                    .expect("constant status won't error"));
            };
            // 解析查询参数（与 POST 表单数据使用相同的解析方式，
            // 因为查询字符串也是 URL 编码格式）
            let params = form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<String, String>>();
            // 获取 "page" 参数
            let page = if let Some(p) = params.get("page") {
                p
            } else {
                return Ok(Response::builder()
                    .status(StatusCode::UNPROCESSABLE_ENTITY)
                    .body(full(MISSING))
                    .expect("constant status won't error"));
            };
            let body = format!("You requested {}", page);
            Ok(Response::new(full(body)))
        }

        // ==================== 默认路由 ====================
        // 返回 404 Not Found
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(empty())
            .expect("constant status won't error")),
    }
}

/// 创建空的 BoxBody
fn empty() -> BoxBody<Bytes, Infallible> {
    Empty::<Bytes>::new().boxed()
}

/// 创建包含完整数据的 BoxBody
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, Infallible> {
    Full::new(chunk.into()).boxed()
}

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 初始化日志系统
    pretty_env_logger::init();

    // 绑定到 127.0.0.1:1337
    let addr: SocketAddr = ([127, 0, 0, 1], 1337).into();

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);
    // 主循环：接受连接并处理
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        // 为每个连接 spawn 一个异步任务
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(param_example))
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
