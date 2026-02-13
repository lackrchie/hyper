//! # hyper 自定义 Service 结构体实现示例
//!
//! 本示例演示了如何通过为自定义结构体实现 `hyper::service::Service` trait
//! 来创建 HTTP 服务，而不是使用 `service_fn` 函数。
//!
//! 这种方式的优势：
//! - 可以在结构体中封装状态（如请求计数器）
//! - 更适合复杂的服务逻辑
//! - 更灵活的类型控制
//!
//! ## 使用方法
//! ```bash
//! cargo run --example service_struct_impl
//! # 在另一个终端测试：
//! curl http://127.0.0.1:3000/       # 返回 "home! counter = ..."
//! curl http://127.0.0.1:3000/posts   # 返回 "posts, of course! counter = ..."
//! curl http://127.0.0.1:3000/authors # 返回 "authors extraordinare! counter = ..."
//! ```
//!
//! ## 核心知识点
//! - 手动实现 `Service` trait（而非使用 service_fn 辅助函数）
//! - 使用 `Arc<Mutex<T>>` 在多个连接之间共享可变状态
//! - `Pin<Box<dyn Future>>` 作为 Service::Future 的关联类型

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
// Service trait 是 hyper 服务处理的核心抽象
use hyper::service::Service;
// Incoming 是服务器接收到的请求体类型
use hyper::{body::Incoming as IncomingBody, Request, Response};
use tokio::net::TcpListener;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
// Arc: 原子引用计数智能指针，用于跨线程共享数据
// Mutex: 互斥锁，用于保护共享的可变数据
use std::sync::{Arc, Mutex};

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 计数器类型别名
type Counter = i32;

/// 程序入口函数
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 绑定到 127.0.0.1:3000
    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    // 创建服务实例，包含一个线程安全的共享计数器
    // Arc<Mutex<Counter>> 允许多个连接安全地共享和修改计数器
    let svc = Svc {
        counter: Arc::new(Mutex::new(0)),
    };

    // 主循环：接受连接并处理
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        // 克隆服务实例（由于 counter 是 Arc，克隆的是引用计数而非数据本身）
        let svc_clone = svc.clone();
        tokio::task::spawn(async move {
            // 直接将 Svc 实例传给 serve_connection（而非 service_fn）
            if let Err(err) = http1::Builder::new().serve_connection(io, svc_clone).await {
                println!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

/// 自定义服务结构体
///
/// 包含一个线程安全的共享计数器，用于记录处理过的请求数量。
/// `#[derive(Clone)]` 使其可以被多个连接共享
/// （实际上克隆的是 Arc 的引用计数，底层数据只有一份）
#[derive(Debug, Clone)]
struct Svc {
    /// 请求计数器，使用 Arc<Mutex<T>> 实现跨连接的线程安全共享
    counter: Arc<Mutex<Counter>>,
}

/// 为 Svc 实现 Service trait
///
/// Service trait 是 hyper 中处理请求的核心抽象，类似于其他框架中的 Handler。
///
/// 泛型参数 `Request<IncomingBody>` 表示服务处理的是 HTTP 请求
impl Service<Request<IncomingBody>> for Svc {
    /// 响应类型
    type Response = Response<Full<Bytes>>;
    /// 错误类型
    type Error = hyper::Error;
    /// Future 类型 —— 因为不同路由可能返回不同的 Future，
    /// 这里使用 `Pin<Box<dyn Future>>` 进行类型擦除
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    /// 处理请求的核心方法
    ///
    /// 接收 &self（不可变引用），因此需要内部可变性（Mutex）来修改状态
    fn call(&self, req: Request<IncomingBody>) -> Self::Future {
        /// 辅助函数：创建包含字符串内容的 HTTP 响应
        fn mk_response(s: String) -> Result<Response<Full<Bytes>>, hyper::Error> {
            Ok(Response::new(Full::new(Bytes::from(s))))
        }

        // 对非 favicon 请求增加计数器
        // （浏览器会自动请求 /favicon.ico，排除它可以更准确地统计"真实"请求数）
        if req.uri().path() != "/favicon.ico" {
            *self.counter.lock().expect("lock poisoned") += 1;
        }

        // 根据路径进行路由
        let res = match req.uri().path() {
            "/" => mk_response(format!("home! counter = {:?}", self.counter)),
            "/posts" => mk_response(format!("posts, of course! counter = {:?}", self.counter)),
            "/authors" => mk_response(format!(
                "authors extraordinare! counter = {:?}",
                self.counter
            )),
            // 未知路由
            _ => mk_response("oh no! not found".into()),
        };

        // 将结果包装为 Pin<Box<Future>>
        // 由于 res 已经是确定值（不需要异步操作），
        // 直接用 async { res } 创建一个立即完成的 Future
        Box::pin(async { res })
    }
}
