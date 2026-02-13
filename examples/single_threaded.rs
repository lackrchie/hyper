//! # hyper 单线程运行时示例
//!
//! 本示例演示了如何在单线程（single-threaded）tokio 运行时中使用 hyper。
//! 这是一个高级示例，主要用于以下场景：
//! - 需要使用 `!Send` 类型（如 `Rc`, `Cell` 等不能跨线程的类型）
//! - 想要减少线程开销的轻量级服务
//! - 测试 hyper 在非多线程环境下的兼容性
//!
//! ## 包含的内容
//! - HTTP/1.1 服务器和客户端（使用 `!Send` 的 Body 和 IO 类型）
//! - HTTP/2 服务器和客户端（使用 `!Send` 的 Body、IO 和自定义 Executor）
//!
//! ## 核心知识点
//! - `tokio::runtime::Builder::new_current_thread()` 创建单线程运行时
//! - `tokio::task::LocalSet` 允许 spawn `!Send` 的 Future
//! - `tokio::task::spawn_local` 在当前线程上 spawn 任务（不要求 Send）
//! - 自定义 `!Send` 的 Body 和 IO 类型
//! - 为 HTTP/2 实现不要求 Send 的自定义 Executor

// 拒绝所有编译警告
#![deny(warnings)]
/// 本示例展示了如何在单线程运行时中使用 hyper。
/// 此示例也用于测试当 `Body` 类型不满足 `Send` 约束时代码是否能编译通过。
///
/// 本示例包含 HTTP/1 和 HTTP/2 的服务器与客户端。
///
/// 在 HTTP/1 中，可以使用 `!Send` 的 `Body` 类型。
/// 在 HTTP/2 中，可以使用 `!Send` 的 `Body` 和 `IO` 类型。
///
/// 本示例中的 `Body` 和 `IOTypeNotSend` 结构体都是 `!Send` 的。
///
/// 对于 HTTP/2，只有当 `Executor` trait 的实现不带 `Send` 约束时才能工作。
use http_body_util::BodyExt;
use hyper::server::conn::http2;
// Cell: 单线程内部可变性容器（!Send, !Sync）
use std::cell::Cell;
use std::net::SocketAddr;
// Rc: 单线程引用计数指针（!Send, !Sync）
use std::rc::Rc;
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpListener;

use hyper::body::{Body as HttpBody, Bytes, Frame};
use hyper::service::service_fn;
use hyper::Request;
use hyper::{Error, Response};
// PhantomData: 零大小类型，用于标记所有权/生命周期关系
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use tokio::net::TcpStream;

// TokioIo 适配器
#[path = "../benches/support/mod.rs"]
mod support;
use support::TokioIo;

/// 自定义的 `!Send` Body 类型
///
/// 通过 `PhantomData<*const ()>` 使这个类型变为 `!Send` 和 `!Sync`。
/// 裸指针 `*const ()` 天然不满足 Send 和 Sync，
/// PhantomData 会将这个属性传递给包含它的结构体。
struct Body {
    // 使 Body 类型变为 !Send 和 !Sync 的标记字段
    _marker: PhantomData<*const ()>,
    // 实际的响应数据（Option 用于实现一次性消费：取出后变为 None）
    data: Option<Bytes>,
}

/// 允许从 String 转换为 Body
impl From<String> for Body {
    fn from(a: String) -> Self {
        Body {
            _marker: PhantomData,
            data: Some(a.into()),
        }
    }
}

/// 为自定义 Body 实现 hyper 的 HttpBody trait
///
/// 这使得我们的 Body 类型可以作为 HTTP 响应体使用
impl HttpBody for Body {
    /// 数据块类型
    type Data = Bytes;
    /// 错误类型
    type Error = Error;

    /// 轮询下一个数据帧
    ///
    /// 使用 `self.data.take()` 实现一次性消费：
    /// 第一次调用返回 `Some(data)`，之后返回 `None`（表示数据已发送完毕）
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.get_mut().data.take().map(|d| Ok(Frame::data(d))))
    }
}

/// 程序入口函数
///
/// 注意：这里没有使用 `#[tokio::main]`，因为我们需要手动配置单线程运行时
fn main() {
    // 初始化日志系统
    pretty_env_logger::init();

    // === 启动 HTTP/2 服务器线程 ===
    let server_http2 = thread::spawn(move || {
        // 创建单线程 tokio 运行时
        // new_current_thread() 表示所有任务都在当前线程上执行
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        // 创建 LocalSet，它允许 spawn 不满足 Send 约束的 Future
        // 这是在单线程运行时中使用 !Send 类型的关键
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, http2_server()).unwrap();
    });

    // === 启动 HTTP/2 客户端线程 ===
    let client_http2 = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let local = tokio::task::LocalSet::new();
        local
            .block_on(
                &rt,
                http2_client("http://localhost:3000".parse::<hyper::Uri>().unwrap()),
            )
            .unwrap();
    });

    // === 启动 HTTP/1 服务器线程 ===
    let server_http1 = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, http1_server()).unwrap();
    });

    // === 启动 HTTP/1 客户端线程 ===
    let client_http1 = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let local = tokio::task::LocalSet::new();
        local
            .block_on(
                &rt,
                http1_client("http://localhost:3001".parse::<hyper::Uri>().unwrap()),
            )
            .unwrap();
    });

    // 等待所有线程完成
    server_http2.join().unwrap();
    client_http2.join().unwrap();

    server_http1.join().unwrap();
    client_http1.join().unwrap();
}

/// HTTP/1.1 服务器
///
/// 监听端口 3001，使用 `Rc<Cell<i32>>` 作为请求计数器。
/// `Rc` 和 `Cell` 都是 `!Send` 的，只能在单线程环境中使用。
async fn http1_server() -> Result<(), Box<dyn std::error::Error>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 3001));

    let listener = TcpListener::bind(addr).await?;

    // 使用 Rc<Cell<i32>> 作为计数器
    // Rc: 单线程引用计数（比 Arc 开销更小，但不能跨线程）
    // Cell: 提供内部可变性（比 Mutex 开销更小，但不能跨线程）
    let counter = Rc::new(Cell::new(0));

    loop {
        let (stream, _) = listener.accept().await?;

        // 使用自定义的 !Send IO 包装器
        let io = IOTypeNotSend::new(TokioIo::new(stream));

        // 克隆 Rc 计数器（增加引用计数，共享底层数据）
        let cnt = counter.clone();

        // 创建服务函数，每次请求时递增计数器
        let service = service_fn(move |_| {
            let prev = cnt.get();
            cnt.set(prev + 1);
            let value = cnt.get();
            async move { Ok::<_, Error>(Response::new(Body::from(format!("Request #{}", value)))) }
        });

        // 使用 spawn_local 而非 spawn
        // spawn_local 不要求 Future 满足 Send 约束，
        // 但只能在 LocalSet 上下文中使用
        tokio::task::spawn_local(async move {
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}

/// HTTP/1.1 客户端
///
/// 连接到 HTTP/1 服务器并发送 4 个请求，打印每个请求的响应
async fn http1_client(url: hyper::Uri) -> Result<(), Box<dyn std::error::Error>> {
    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);
    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect(addr).await?;

    // 使用 !Send 的 IO 包装器
    let io = IOTypeNotSend::new(TokioIo::new(stream));

    // HTTP/1.1 握手
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    // 在本地任务中驱动连接（使用 spawn_local 因为 IO 是 !Send）
    tokio::task::spawn_local(async move {
        if let Err(err) = conn.await {
            let mut stdout = io::stdout();
            stdout
                .write_all(format!("Connection failed: {:?}", err).as_bytes())
                .await
                .unwrap();
            stdout.flush().await.unwrap();
        }
    });

    let authority = url.authority().unwrap().clone();

    // 发送 4 个请求
    for _ in 0..4 {
        let req = Request::builder()
            .uri(url.clone())
            .header(hyper::header::HOST, authority.as_str())
            .body(Body::from("test".to_string()))?;

        let mut res = sender.send_request(req).await?;

        // 打印响应状态码和头部
        let mut stdout = io::stdout();
        stdout
            .write_all(format!("Response: {}\n", res.status()).as_bytes())
            .await
            .unwrap();
        stdout
            .write_all(format!("Headers: {:#?}\n", res.headers()).as_bytes())
            .await
            .unwrap();
        stdout.flush().await.unwrap();

        // 逐帧打印响应体
        while let Some(next) = res.frame().await {
            let frame = next?;
            if let Some(chunk) = frame.data_ref() {
                stdout.write_all(chunk).await.unwrap();
            }
        }
        stdout.write_all(b"\n-----------------\n").await.unwrap();
        stdout.flush().await.unwrap();
    }
    Ok(())
}

/// HTTP/2 服务器
///
/// 监听端口 3000，使用 `Rc<Cell<i32>>` 作为请求计数器。
/// HTTP/2 服务器需要自定义的 Executor（LocalExec），
/// 因为默认的 tokio Executor 要求 Future 满足 Send 约束。
async fn http2_server() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout();

    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();
    // 使用 !Send 的请求计数器 —— 在单线程上这是安全的
    let counter = Rc::new(Cell::new(0));

    let listener = TcpListener::bind(addr).await?;

    stdout
        .write_all(format!("Listening on http://{}", addr).as_bytes())
        .await
        .unwrap();
    stdout.flush().await.unwrap();

    loop {
        let (stream, _) = listener.accept().await?;
        let io = IOTypeNotSend::new(TokioIo::new(stream));

        // 克隆计数器供新连接使用
        let cnt = counter.clone();

        let service = service_fn(move |_| {
            let prev = cnt.get();
            cnt.set(prev + 1);
            let value = cnt.get();
            async move { Ok::<_, Error>(Response::new(Body::from(format!("Request #{}", value)))) }
        });

        // 使用自定义的 LocalExec 执行器来处理 HTTP/2 连接
        // LocalExec 使用 spawn_local 而非 spawn，因此不要求 Send
        tokio::task::spawn_local(async move {
            if let Err(err) = http2::Builder::new(LocalExec)
                .serve_connection(io, service)
                .await
            {
                let mut stdout = io::stdout();
                stdout
                    .write_all(format!("Error serving connection: {:?}", err).as_bytes())
                    .await
                    .unwrap();
                stdout.flush().await.unwrap();
            }
        });
    }
}

/// HTTP/2 客户端
///
/// 连接到 HTTP/2 服务器并发送 4 个请求
async fn http2_client(url: hyper::Uri) -> Result<(), Box<dyn std::error::Error>> {
    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);
    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect(addr).await?;

    // 使用 !Send 的 IO 包装器
    let stream = IOTypeNotSend::new(TokioIo::new(stream));

    // HTTP/2 握手，使用 LocalExec 作为执行器
    let (mut sender, conn) = hyper::client::conn::http2::handshake(LocalExec, stream).await?;

    // 在本地任务中驱动连接
    tokio::task::spawn_local(async move {
        if let Err(err) = conn.await {
            let mut stdout = io::stdout();
            stdout
                .write_all(format!("Connection failed: {:?}", err).as_bytes())
                .await
                .unwrap();
            stdout.flush().await.unwrap();
        }
    });

    let authority = url.authority().unwrap().clone();

    // 发送 4 个请求
    for _ in 0..4 {
        let req = Request::builder()
            .uri(url.clone())
            .header(hyper::header::HOST, authority.as_str())
            .body(Body::from("test".to_string()))?;

        let mut res = sender.send_request(req).await?;

        // 打印响应
        let mut stdout = io::stdout();
        stdout
            .write_all(format!("Response: {}\n", res.status()).as_bytes())
            .await
            .unwrap();
        stdout
            .write_all(format!("Headers: {:#?}\n", res.headers()).as_bytes())
            .await
            .unwrap();
        stdout.flush().await.unwrap();

        // 逐帧打印响应体
        while let Some(next) = res.frame().await {
            let frame = next?;
            if let Some(chunk) = frame.data_ref() {
                stdout.write_all(chunk).await.unwrap();
            }
        }
        stdout.write_all(b"\n-----------------\n").await.unwrap();
        stdout.flush().await.unwrap();
    }
    Ok(())
}

// ==================== 自定义 Executor ====================
//
// 注意：这部分仅 HTTP/2 需要。HTTP/1 不需要 Executor。
//
// 由于 HTTP/2 服务器需要 spawn 后台任务来处理多路复用的流，
// 我们需要一个自定义的 Executor，它使用 spawn_local 来 spawn !Send 的 Future。

/// 本地执行器 —— 使用 `spawn_local` 而非 `spawn`
///
/// 关键区别：
/// - 标准 Executor 要求 `F: Future + Send`
/// - LocalExec 只要求 `F: Future + 'static`（不要求 Send）
#[derive(Clone, Copy, Debug)]
struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: std::future::Future + 'static, // 注意：没有 Send 约束！
{
    fn execute(&self, fut: F) {
        // 使用 spawn_local 将任务 spawn 到当前运行的 LocalSet 中
        tokio::task::spawn_local(fut);
    }
}

// ==================== 自定义 !Send IO 类型 ====================

/// 不满足 Send 约束的 IO 类型包装器
///
/// 通过 `PhantomData<*const ()>` 使类型变为 `!Send`，
/// 用于演示 hyper 在 !Send 环境下的工作能力
struct IOTypeNotSend {
    // 使类型变为 !Send 的标记
    _marker: PhantomData<*const ()>,
    // 实际的 TCP 流
    stream: TokioIo<TcpStream>,
}

impl IOTypeNotSend {
    /// 包装一个 TokioIo<TcpStream> 为 !Send 类型
    fn new(stream: TokioIo<TcpStream>) -> Self {
        Self {
            _marker: PhantomData,
            stream,
        }
    }
}

/// 为 IOTypeNotSend 实现 hyper 的 Write trait
///
/// 所有写操作都委托给内部的 TokioIo<TcpStream>
impl hyper::rt::Write for IOTypeNotSend {
    /// 写入数据
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    /// 刷新写缓冲区
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    /// 关闭写端
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// 为 IOTypeNotSend 实现 hyper 的 Read trait
///
/// 读操作委托给内部的 TokioIo<TcpStream>
impl hyper::rt::Read for IOTypeNotSend {
    /// 读取数据到缓冲区
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
