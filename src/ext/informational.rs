//! HTTP/1 信息性响应（1xx Informational）回调模块。
//!
//! 本模块提供了在 HTTP/1 客户端接收到 1xx 信息性响应时执行回调的机制。
//! 在 HTTP/1.1 协议中，服务器可以在发送最终响应（2xx-5xx）之前发送一个或多个
//! 1xx 中间响应，例如 `100 Continue`（指示客户端可以继续发送请求体）
//! 或 `103 Early Hints`（预加载资源提示）。
//!
//! # 在 hyper 架构中的角色
//!
//! 本模块是 `ext`（扩展）子系统的一部分，通过 HTTP 请求的扩展（extensions）机制
//! 将回调注入到请求处理流程中。当 hyper 的 HTTP/1 客户端连接接收到 1xx 响应时，
//! 会检查请求扩展中是否存在 `OnInformational` 回调并调用它。
//!
//! # 设计考量
//!
//! 回调使用 `Arc<dyn OnInformationalCallback>` 包装，以支持跨线程安全共享。
//! 公开 API 使用 `Response<'_>` 门面（facade）类型而非直接暴露 `http::Response`，
//! 这样既防止用户将响应移出回调，又为未来 API 演化保留了灵活性。

/// 引入 `Arc`（原子引用计数智能指针），用于在多个地方安全地共享回调对象。
/// 这是必要的，因为回调需要跨越异步任务边界传递。
use std::sync::Arc;

/// 信息性响应回调的内部包装类型。
///
/// 使用 `Arc<dyn OnInformationalCallback + Send + Sync>` 实现类型擦除，
/// 将具体的回调闭包或结构体擦除为 trait 对象，使得 `OnInformational`
/// 可以存储在 `http::Extensions` 中（需要 `'static` 类型）。
///
/// `#[derive(Clone)]` 利用 `Arc` 的引用计数实现廉价克隆。
#[derive(Clone)]
pub(crate) struct OnInformational(Arc<dyn OnInformationalCallback + Send + Sync>);

/// 为 HTTP/1 请求注册 1xx 信息性响应回调。
///
/// 当客户端发送此请求后，若服务器返回 1xx 信息性响应，
/// 指定的回调函数将被调用，传入一个包含响应状态码和头部的 [`Response`] 对象。
///
/// # 参数
///
/// - `req`：要附加回调的 HTTP 请求的可变引用
/// - `callback`：回调闭包，接收 `Response<'_>` 参数，需满足 `Send + Sync + 'static` 约束
///
/// # 示例
///
/// ```
/// # let some_body = ();
/// let mut req = hyper::Request::new(some_body);
///
/// hyper::ext::on_informational(&mut req, |res| {
///     println!("informational: {:?}", res.status());
/// });
///
/// // 在客户端连接上发送请求...
/// ```
pub fn on_informational<B, F>(req: &mut http::Request<B>, callback: F)
where
    F: Fn(Response<'_>) + Send + Sync + 'static,
{
    // 将用户闭包包装为 OnInformationalClosure 后传递给内部实现
    on_informational_raw(req, OnInformationalClosure(callback));
}

/// 注册信息性响应回调的内部实现。
///
/// 此函数接受任何实现了 `OnInformationalCallback` trait 的类型，
/// 将其包装为 `OnInformational`（通过 `Arc` 实现类型擦除）后
/// 插入到请求的扩展映射中。
///
/// 此函数为 `pub(crate)` 可见性，供 FFI 层直接传入自定义回调类型。
pub(crate) fn on_informational_raw<B, C>(req: &mut http::Request<B>, callback: C)
where
    C: OnInformationalCallback + Send + Sync + 'static,
{
    req.extensions_mut()
        .insert(OnInformational(Arc::new(callback))); // 将回调包装为 Arc 并存入扩展
}

// 密封的 trait（sealed trait 模式），不可被外部命名和实现。
// 这确保了只有 crate 内部的类型才能实现此回调接口。
/// 信息性响应回调的内部 trait。
///
/// 定义了接收 `http::Response<()>` 参数的回调方法。
/// 使用密封 trait 模式防止外部实现，保留未来修改接口的灵活性。
pub(crate) trait OnInformationalCallback {
    /// 当收到信息性响应时被调用。
    ///
    /// 参数为 `http::Response<()>`，body 类型为 `()` 因为 1xx 响应没有消息体。
    fn on_informational(&self, res: http::Response<()>);
}

/// `OnInformational` 的方法实现。
impl OnInformational {
    /// 调用内部存储的回调。
    ///
    /// 通过 `Arc` 的解引用调用 trait 对象的 `on_informational` 方法，
    /// 实现了动态分发（dynamic dispatch）。
    pub(crate) fn call(&self, res: http::Response<()>) {
        self.0.on_informational(res);
    }
}

/// 将用户提供的闭包 `F` 适配为 `OnInformationalCallback` trait 的包装类型。
///
/// 这是适配器模式的典型应用：将闭包转换为 trait 实现，
/// 使其可以作为 trait 对象存储在 `Arc` 中。
struct OnInformationalClosure<F>(F);

/// 为闭包包装类型实现 `OnInformationalCallback` trait。
///
/// 在回调被调用时，先将 `http::Response<()>` 包装为门面类型 `Response`，
/// 再传递给用户的闭包。这样用户看到的是受限的 `Response<'_>` 类型，
/// 而非完整的 `http::Response<()>`。
impl<F> OnInformationalCallback for OnInformationalClosure<F>
where
    F: Fn(Response<'_>) + Send + Sync + 'static,
{
    fn on_informational(&self, res: http::Response<()>) {
        let res = Response(&res); // 创建门面类型，包装对原始响应的引用
        (self.0)(res); // 调用用户闭包
    }
}

// `http::Response` 上的门面类型（Facade Pattern）。
//
// 设计意图：
// - 故意隐藏将响应移出闭包的能力（不能 move）
// - 同时也不能被当作普通引用 `&Response` 使用
//   （否则用户可能写出 `|res: &_|` 这样的闭包签名，
//   当我们未来改为传值语义时会导致向后不兼容）
//
// 由于此类型不可被外部命名（因为它在一个密封的回调接口后面），
// 未来我们可以在向后兼容的前提下将其改为真正的引用或传值语义。
/// 信息性响应的门面类型。
///
/// 提供对 1xx 信息性响应的只读访问，包括状态码、HTTP 版本和头部。
/// 此类型故意限制了对底层 `http::Response<()>` 的访问方式，
/// 以便未来 API 演化时保持向后兼容。
#[derive(Debug)]
pub struct Response<'a>(&'a http::Response<()>); // 持有对原始响应的不可变引用

/// `Response` 门面类型的方法实现。
impl Response<'_> {
    /// 返回响应的 HTTP 状态码。
    ///
    /// 对于信息性响应，通常为 100（Continue）、102（Processing）、
    /// 103（Early Hints）等 1xx 范围内的状态码。
    #[inline]
    pub fn status(&self) -> http::StatusCode {
        self.0.status()
    }

    /// 返回响应的 HTTP 版本。
    ///
    /// 对于通过 HTTP/1 接收的信息性响应，通常为 `HTTP/1.1`。
    #[inline]
    pub fn version(&self) -> http::Version {
        self.0.version()
    }

    /// 返回响应头部的不可变引用。
    ///
    /// 信息性响应可能包含有用的头部，例如 `103 Early Hints` 响应
    /// 中的 `Link` 头部，可用于预加载资源。
    #[inline]
    pub fn headers(&self) -> &http::HeaderMap {
        self.0.headers()
    }
}
