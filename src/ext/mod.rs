//! hyper HTTP 消息扩展模块。
//!
//! 本模块提供了用于扩展 HTTP 请求和响应功能的类型与工具。
//! 在 hyper 的架构中，扩展（Extensions）是通过 [`http::Extensions`] 映射表
//! 附加到 HTTP 消息上的额外元数据或行为，可通过 [`http::Request::extensions`]
//! 和 [`http::Response::extensions`] 方法访问。
//!
//! # 什么是扩展？
//!
//! 扩展允许 hyper 将标准头部和请求体之外的额外元数据或行为关联到 HTTP 消息上。
//! 高级用户和库作者可以利用扩展来访问协议特定的功能、追踪原始头部大小写、
//! 处理信息性响应（1xx）等。
//!
//! # 如何访问扩展
//!
//! 扩展存储在请求或响应的 `Extensions` 映射表中，可以通过类型化的 `get` 方法访问：
//!
//! ```rust
//! # let response = http::Response::new(());
//! if let Some(ext) = response.extensions().get::<hyper::ext::ReasonPhrase>() {
//!     // 使用该扩展
//! }
//! ```
//!
//! # 扩展分组
//!
//! 本模块中的扩展可分为以下几组：
//!
//! - **HTTP/1 原因短语**：[`ReasonPhrase`] — 访问 HTTP/1 响应中的非标准原因短语。
//! - **信息性响应**：[`on_informational`] — 在客户端注册针对 1xx HTTP/1 响应的回调。
//! - **头部大小写追踪**：内部类型，用于追踪接收到的头部的原始大小写和顺序。
//! - **HTTP/2 协议扩展**：[`Protocol`] — 访问 HTTP/2 Extended CONNECT 中的 `:protocol` 伪头部。
//!
//! 某些扩展仅适用于特定协议（HTTP/1 或 HTTP/2）或特定使用场景（客户端、服务端、FFI）。
//!
//! 请参阅每个条目的文档以了解其用法和要求。

// --- 条件编译的外部依赖导入 ---

/// 引入 `bytes::Bytes`，用于零拷贝字节缓冲区，存储头部原始大小写等信息。
/// 仅在启用了 client/server 且启用 http1 特性时编译。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
use bytes::Bytes;

/// 引入 `http::header::HeaderName`，表示标准化的 HTTP 头部名称。
/// 在 http1（client/server）或 ffi 特性启用时需要。
#[cfg(any(
    all(any(feature = "client", feature = "server"), feature = "http1"),
    feature = "ffi"
))]
use http::header::HeaderName;

/// 引入 `HeaderMap`（多值头部映射表）、`IntoHeaderName`（头部名称转换 trait）
/// 和 `ValueIter`（头部值迭代器），用于 `HeaderCaseMap` 的内部实现。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
use http::header::{HeaderMap, IntoHeaderName, ValueIter};

/// 引入标准库 `HashMap`，用于 FFI 模式下的 `OriginalHeaderOrder` 记录头部出现次数。
#[cfg(feature = "ffi")]
use std::collections::HashMap;

/// 引入 `fmt` 模块，用于为 `Protocol` 实现 `Debug` trait。
#[cfg(feature = "http2")]
use std::fmt;

// --- 子模块声明与公开导出 ---

/// 声明 HTTP/1 原因短语子模块。
/// 仅在 http1 或 ffi 特性启用时编译。
#[cfg(any(feature = "http1", feature = "ffi"))]
mod h1_reason_phrase;

/// 公开导出 `ReasonPhrase` 类型，允许用户访问 HTTP/1 响应中的自定义原因短语。
#[cfg(any(feature = "http1", feature = "ffi"))]
pub use h1_reason_phrase::ReasonPhrase;

/// 声明信息性响应（1xx）子模块。
/// 仅在同时启用 http1 和 client 特性时编译（1xx 回调仅在客户端有意义）。
#[cfg(all(feature = "http1", feature = "client"))]
mod informational;

/// 公开导出 `on_informational` 函数，允许用户为 HTTP/1 请求注册 1xx 响应回调。
#[cfg(all(feature = "http1", feature = "client"))]
pub use informational::on_informational;

/// crate 内部导出 `OnInformational` 类型，供 hyper 内部的 HTTP/1 客户端连接使用。
#[cfg(all(feature = "http1", feature = "client"))]
pub(crate) use informational::OnInformational;

/// crate 内部导出 FFI 版本的信息性响应回调相关类型。
/// 仅在同时启用 http1、client 和 ffi 三个特性时编译。
#[cfg(all(feature = "http1", feature = "client", feature = "ffi"))]
pub(crate) use informational::{on_informational_raw, OnInformationalCallback};

// --- HTTP/2 Protocol 扩展类型 ---

/// HTTP/2 Extended CONNECT 协议扩展类型。
///
/// `Protocol` 代表 HTTP/2 中 `:protocol` 伪头部的值，
/// 由 [Extended CONNECT Protocol (RFC 8441)](https://datatracker.ietf.org/doc/html/rfc8441#section-4) 定义。
/// 此扩展仅在 HTTP/2 CONNECT 请求中发送，最常见的值是 `websocket`。
///
/// # 示例
///
/// ```rust
/// use hyper::ext::Protocol;
/// use http::{Request, Method, Version};
///
/// let mut req = Request::new(());
/// *req.method_mut() = Method::CONNECT;
/// *req.version_mut() = Version::HTTP_2;
/// req.extensions_mut().insert(Protocol::from_static("websocket"));
/// // 此时请求将包含 `:protocol` 伪头部，值为 "websocket"
/// ```
#[cfg(feature = "http2")]
#[derive(Clone, Eq, PartialEq)]
pub struct Protocol {
    /// 内部委托给 h2 crate 的 `Protocol` 类型，实现零成本抽象。
    inner: h2::ext::Protocol,
}

/// `Protocol` 的方法实现。
#[cfg(feature = "http2")]
impl Protocol {
    /// 从静态字符串创建 `Protocol` 实例。
    ///
    /// 这是一个 `const fn`，可在编译期求值，适合用于定义常量协议名称。
    pub const fn from_static(value: &'static str) -> Self {
        Self {
            inner: h2::ext::Protocol::from_static(value),
        }
    }

    /// 返回协议名称的字符串引用。
    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    /// 从 h2 crate 的内部 `Protocol` 类型创建 hyper 的 `Protocol`。
    /// 仅在服务端使用，将 h2 层解析出的协议信息传递给 hyper 层。
    #[cfg(feature = "server")]
    pub(crate) fn from_inner(inner: h2::ext::Protocol) -> Self {
        Self { inner }
    }

    /// 将 hyper 的 `Protocol` 转换为 h2 crate 的内部类型。
    /// 仅在客户端使用，将 hyper 层的协议信息传递给 h2 层发送。
    #[cfg(all(feature = "client", feature = "http2"))]
    pub(crate) fn into_inner(self) -> h2::ext::Protocol {
        self.inner
    }
}

/// 实现从字符串引用到 `Protocol` 的转换。
///
/// 允许用户使用普通字符串创建 `Protocol` 实例，
/// 如 `Protocol::from("websocket")`。
#[cfg(feature = "http2")]
impl<'a> From<&'a str> for Protocol {
    fn from(value: &'a str) -> Self {
        Self {
            inner: h2::ext::Protocol::from(value),
        }
    }
}

/// 实现 `AsRef<[u8]>`，允许将 `Protocol` 作为字节切片引用。
/// 这使得 `Protocol` 可以方便地用于需要字节数据的场景。
#[cfg(feature = "http2")]
impl AsRef<[u8]> for Protocol {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

/// 实现 `Debug` trait，委托给内部 h2 `Protocol` 的格式化逻辑。
#[cfg(feature = "http2")]
impl fmt::Debug for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

// --- 头部大小写映射（HeaderCaseMap） ---

/// 从头部名称到其原始大小写形式的映射表。
///
/// 当 HTTP/1 连接启用了 `preserve_header_case` 选项时，
/// hyper 会在解析响应时保留头部名称的原始大小写形式，
/// 并将其存储在此映射表中作为响应扩展。
///
/// 例如，如果收到以下头部：
///
/// ```ignore
/// x-Bread: Baguette
/// X-BREAD: Pain
/// x-bread: Ficelle
/// ```
///
/// 则 `res.extensions().get::<HeaderCaseMap>()` 将返回：
///
/// ```ignore
/// HeaderCaseMap({
///     "x-bread": ["x-Bread", "X-BREAD", "x-bread"],
/// })
/// ```
///
/// [`preserve_header_case`]: /client/struct.Client.html#method.preserve_header_case
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
#[derive(Clone, Debug)]
pub(crate) struct HeaderCaseMap(HeaderMap<Bytes>);

/// `HeaderCaseMap` 的方法实现。
///
/// 该类型使用 `HeaderMap<Bytes>` 作为底层存储，
/// 其中键是标准化的头部名称，值是对应的原始大小写形式（以 `Bytes` 存储）。
/// `HeaderMap` 本身支持多值映射，因此同一头部名称的多个大小写形式可以按序存储。
#[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
impl HeaderCaseMap {
    /// 返回与指定头部名称关联的所有原始大小写形式的迭代器，按发现顺序排列。
    ///
    /// 仅在客户端特性启用时可用。返回的迭代器元素实现了 `AsRef<[u8]>`。
    #[cfg(feature = "client")]
    pub(crate) fn get_all<'a>(
        &'a self,
        name: &HeaderName,
    ) -> impl Iterator<Item = impl AsRef<[u8]> + 'a> + 'a {
        self.get_all_internal(name)
    }

    /// 内部方法：返回指定头部名称的所有原始大小写形式的迭代器。
    ///
    /// 返回类型为 `ValueIter<'_, Bytes>`，这是 `HeaderMap` 提供的多值迭代器。
    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn get_all_internal(&self, name: &HeaderName) -> ValueIter<'_, Bytes> {
        // get_all() 返回该头部名称的所有值，into_iter() 转换为迭代器
        self.0.get_all(name).into_iter()
    }

    /// 创建一个空的 `HeaderCaseMap`。
    ///
    /// 使用关联函数而非实现 `Default` trait，是因为此类型仅在 crate 内部使用，
    /// 不需要暴露 `Default` trait 的公开接口。
    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn default() -> Self {
        Self(Default::default()) // 委托给 HeaderMap 的 Default 实现
    }

    /// 插入一个头部名称及其原始大小写形式（覆盖已有的同名条目）。
    ///
    /// 仅在测试和 FFI 场景下使用。
    #[cfg(any(test, feature = "ffi"))]
    pub(crate) fn insert(&mut self, name: HeaderName, orig: Bytes) {
        self.0.insert(name, orig);
    }

    /// 追加一个头部名称及其原始大小写形式（不覆盖已有条目，添加到多值列表末尾）。
    ///
    /// 这是解析 HTTP/1 消息时主要使用的方法，因为同一头部可能出现多次，
    /// 每次的大小写可能不同。
    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn append<N>(&mut self, name: N, orig: Bytes)
    where
        N: IntoHeaderName, // IntoHeaderName 允许接受 HeaderName、&str 等多种类型
    {
        self.0.append(name, orig);
    }
}

// --- 原始头部顺序记录（OriginalHeaderOrder，FFI 专用） ---

/// 记录 HTTP 头部原始出现顺序的结构体。
///
/// 此类型仅在 FFI（Foreign Function Interface）模式下使用，
/// 用于向 C/C++ 等外部语言消费者提供头部的原始顺序信息。
///
/// 设计思路：
/// - `num_entries` 记录每个头部名称已出现的次数，用于生成索引。
/// - `entry_order` 按接收顺序存储 `(HeaderName, index)` 对，
///   其中 `index` 表示该名称的第几次出现（从 0 开始），
///   可与 `HeaderMap::get_all()` 配合使用以按原始顺序访问值。
#[cfg(feature = "ffi")]
#[derive(Clone, Debug)]
/// Hashmap<Headername, numheaders with that name>
pub(crate) struct OriginalHeaderOrder {
    /// 存储每个头部名称对应的条目数量，用于索引计算。
    num_entries: HashMap<HeaderName, usize>,
    /// 按原始顺序存储头部条目。`vec[i] = (headerName, idx)` 表示
    /// 第 i 个接收到的头部的名称及其在同名头部中的索引位置。
    /// 可与 `HeaderMap::get_all(name).nth(idx)` 配合使用来还原原始顺序。
    entry_order: Vec<(HeaderName, usize)>,
}

/// `OriginalHeaderOrder` 的方法实现。
///
/// 仅在同时启用 http1 和 ffi 特性时编译。
#[cfg(all(feature = "http1", feature = "ffi"))]
impl OriginalHeaderOrder {
    /// 创建一个空的 `OriginalHeaderOrder` 实例。
    pub(crate) fn default() -> Self {
        OriginalHeaderOrder {
            num_entries: HashMap::new(),
            entry_order: Vec::new(),
        }
    }

    /// 插入一个头部名称（仅记录首次出现，用于 `insert` 语义——覆盖式插入）。
    ///
    /// 如果该名称已存在，则不修改顺序记录，因为 `insert` 语义是替换而非追加。
    pub(crate) fn insert(&mut self, name: HeaderName) {
        if !self.num_entries.contains_key(&name) {
            let idx = 0; // 第一次出现，索引为 0
            self.num_entries.insert(name.clone(), 1);
            self.entry_order.push((name, idx));
        }
        // 替换已存在的元素不会改变顺序，
        // 所以我们只在首次遇到该头部名称时才记录
    }

    /// 追加一个头部名称（用于 `append` 语义——多值追加）。
    ///
    /// 每次调用都会在顺序记录中添加一个新条目，索引递增。
    /// 泛型约束 `N: IntoHeaderName + Into<HeaderName> + Clone`
    /// 确保参数可以转换为 `HeaderName`。
    pub(crate) fn append<N>(&mut self, name: N)
    where
        N: IntoHeaderName + Into<HeaderName> + Clone,
    {
        let name: HeaderName = name.into(); // 将泛型参数转换为具体的 HeaderName
        let idx;
        if self.num_entries.contains_key(&name) {
            // 该名称已出现过，取当前计数作为新条目的索引
            idx = self.num_entries[&name];
            *self.num_entries.get_mut(&name).unwrap() += 1; // 计数加一
        } else {
            // 首次出现，索引从 0 开始
            idx = 0;
            self.num_entries.insert(name.clone(), 1);
        }
        self.entry_order.push((name, idx)); // 记录到顺序列表中
    }

    // 此处不运行文档测试，因为需要 `RUSTFLAGS='--cfg hyper_unstable_ffi'`
    // 才能编译。一旦 ffi 特性稳定后，应移除 `no_run` 标注。
    /// 返回一个迭代器，按原始接收顺序提供头部名称和索引。
    ///
    /// 配合 `HeaderMap::get_all(name).nth(idx)` 使用，可以按照
    /// HTTP 消息中头部的原始顺序遍历所有头部值。
    ///
    /// # 示例
    /// ```no_run
    /// use hyper::ext::OriginalHeaderOrder;
    /// use hyper::header::{HeaderName, HeaderValue, HeaderMap};
    ///
    /// let mut h_order = OriginalHeaderOrder::default();
    /// let mut h_map = Headermap::new();
    ///
    /// let name1 = b"Set-CookiE";
    /// let value1 = b"a=b";
    /// h_map.append(name1);
    /// h_order.append(name1);
    ///
    /// let name2 = b"Content-Encoding";
    /// let value2 = b"gzip";
    /// h_map.append(name2, value2);
    /// h_order.append(name2);
    ///
    /// let name3 = b"SET-COOKIE";
    /// let value3 = b"c=d";
    /// h_map.append(name3, value3);
    /// h_order.append(name3)
    ///
    /// let mut iter = h_order.get_in_order()
    ///
    /// let (name, idx) = iter.next();
    /// assert_eq!(b"a=b", h_map.get_all(name).nth(idx).unwrap());
    ///
    /// let (name, idx) = iter.next();
    /// assert_eq!(b"gzip", h_map.get_all(name).nth(idx).unwrap());
    ///
    /// let (name, idx) = iter.next();
    /// assert_eq!(b"c=d", h_map.get_all(name).nth(idx).unwrap());
    /// ```
    pub(crate) fn get_in_order(&self) -> impl Iterator<Item = &(HeaderName, usize)> {
        self.entry_order.iter()
    }
}
