//! # HTTP 核心类型的 FFI 绑定模块
//!
//! 本模块为 HTTP 请求（Request）、响应（Response）和头部映射（Headers）提供 C 语言接口。
//! 这些是 hyper FFI 中最常使用的类型，涵盖了 HTTP 消息的构建、检查和操作。
//!
//! ## 在 hyper FFI 中的角色
//!
//! - `hyper_request`：用于构建出站 HTTP 请求（设置方法、URI、版本、头部、体）
//! - `hyper_response`：用于检查入站 HTTP 响应（读取状态码、版本、原因短语、头部、体）
//! - `hyper_headers`：HTTP 头部映射的读写接口，同时保留原始大小写和顺序信息
//!
//! ## 头部大小写与顺序保留
//!
//! HTTP/1.1 规范中头部名称是大小写不敏感的，但某些旧版服务器可能依赖特定大小写。
//! hyper FFI 通过 `HeaderCaseMap` 和 `OriginalHeaderOrder` 扩展类型保留了
//! 头部的原始大小写和插入顺序，使 C 调用者可以获得与网络上完全一致的头部数据。

// === 标准库导入 ===
use std::ffi::{c_int, c_void}; // C 兼容的整数和 void 指针类型

// === 外部 crate 导入 ===
use bytes::Bytes; // 高效的引用计数字节缓冲区，用于存储头部名称的原始大小写

// === 内部模块导入 ===
use super::body::hyper_body; // FFI 层的 body 类型
use super::error::hyper_code; // FFI 错误码枚举
use super::task::{hyper_task_return_type, AsTaskType}; // 任务类型标识
use super::{UserDataPointer, HYPER_ITER_CONTINUE}; // 用户数据指针和迭代控制常量
use crate::body::Incoming as IncomingBody; // hyper 核心的入站 body 类型
use crate::ext::{HeaderCaseMap, OriginalHeaderOrder, ReasonPhrase}; // hyper 的扩展类型：头部大小写映射、原始顺序、原因短语
use crate::ffi::size_t; // C 兼容的 size_t 类型别名
use crate::header::{HeaderName, HeaderValue}; // HTTP 头部的名称和值类型
use crate::{HeaderMap, Method, Request, Response, Uri}; // http crate 的核心类型

/// HTTP 请求的 FFI 包装类型。
///
/// 通过 `hyper_request_new()` 构建，通过一系列 `hyper_request_set_*` 函数配置，
/// 最终通过 `hyper_clientconn_send()` 发送。
///
/// 内部包装了 `http::Request<IncomingBody>`（newtype 模式）。
/// `pub(super)` 允许 FFI 模块内的其他子模块访问内部字段。
///
/// Methods:
///
/// - hyper_request_new:              Construct a new HTTP request.
/// - hyper_request_headers:          Gets a mutable reference to the HTTP headers of this request
/// - hyper_request_set_body:         Set the body of the request.
/// - hyper_request_set_method:       Set the HTTP Method of the request.
/// - hyper_request_set_uri:          Set the URI of the request.
/// - hyper_request_set_uri_parts:    Set the URI of the request with separate scheme, authority, and path/query strings.
/// - hyper_request_set_version:      Set the preferred HTTP version of the request.
/// - hyper_request_on_informational: Set an informational (1xx) response callback.
/// - hyper_request_free:             Free an HTTP request.
pub struct hyper_request(pub(super) Request<IncomingBody>);

/// HTTP 响应的 FFI 包装类型。
///
/// 通过 `hyper_clientconn_send` 发送请求后，从执行器的 `HYPER_TASK_RESPONSE`
/// 类型任务中获取。提供状态码、版本、头部和 body 的访问方法。
///
/// Methods:
///
/// - hyper_response_status:            Get the HTTP-Status code of this response.
/// - hyper_response_version:           Get the HTTP version used by this response.
/// - hyper_response_reason_phrase:     Get a pointer to the reason-phrase of this response.
/// - hyper_response_reason_phrase_len: Get the length of the reason-phrase of this response.
/// - hyper_response_headers:           Gets a reference to the HTTP headers of this response.
/// - hyper_response_body:              Take ownership of the body of this response.
/// - hyper_response_free:              Free an HTTP response.
pub struct hyper_response(pub(super) Response<IncomingBody>);

/// HTTP 头部映射的 FFI 包装类型。
///
/// 除了标准的 `HeaderMap`（名称 -> 值映射），还维护了头部名称的原始大小写
/// 信息和头部的原始插入顺序，以便在 HTTP/1.1 协议中精确还原收到的头部格式。
///
/// `#[derive(Clone)]` 允许在需要时复制整个头部映射。
///
/// Methods:
///
/// - hyper_headers_add:     Adds the provided value to the list of the provided name.
/// - hyper_headers_foreach: Iterates the headers passing each name and value pair to the callback.
/// - hyper_headers_set:     Sets the header with the provided name to the provided value.
#[derive(Clone)]
pub struct hyper_headers {
    /// 标准的 HTTP 头部映射（名称全部小写化）
    pub(super) headers: HeaderMap,
    /// 头部名称的原始大小写映射（如 "Content-Type" 而非 "content-type"）
    orig_casing: HeaderCaseMap,
    /// 头部的原始插入顺序记录
    orig_order: OriginalHeaderOrder,
}

/// 1xx 信息性响应的回调上下文。
///
/// 当收到 1xx 信息性响应（如 100 Continue）时，通过此结构体保存的
/// 回调函数和用户数据来通知 C 调用者。
///
/// `#[derive(Clone)]` 因为此类型需要被存储在请求的扩展中，可能被复制。
#[derive(Clone)]
struct OnInformational {
    /// C 端的回调函数指针
    func: hyper_request_on_informational_callback,
    /// 用户数据指针，包装在 UserDataPointer 中以支持 Send/Sync
    data: UserDataPointer,
}

/// 1xx 信息性响应回调的函数签名。
///
/// 参数：
/// - `*mut c_void`：用户数据指针
/// - `*mut hyper_response`：信息性响应的借用引用（回调返回后即失效）
type hyper_request_on_informational_callback = extern "C" fn(*mut c_void, *mut hyper_response);

// ===== impl hyper_request =====

ffi_fn! {
    /// Construct a new HTTP request.
    ///
    /// The default request has an empty body. To send a body, call `hyper_request_set_body`.
    ///
    ///
    /// To avoid a memory leak, the request must eventually be consumed by
    /// `hyper_request_free` or `hyper_clientconn_send`.
    fn hyper_request_new() -> *mut hyper_request {
        // 使用空 body 创建默认请求，HTTP 方法默认为 GET
        Box::into_raw(Box::new(hyper_request(Request::new(IncomingBody::empty()))))
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Free an HTTP request.
    ///
    /// This should only be used if the request isn't consumed by
    /// `hyper_clientconn_send`.
    fn hyper_request_free(req: *mut hyper_request) {
        drop(non_null!(Box::from_raw(req) ?= ()));
    }
}

ffi_fn! {
    /// Set the HTTP Method of the request.
    fn hyper_request_set_method(req: *mut hyper_request, method: *const u8, method_len: size_t) -> hyper_code {
        // 从 C 字符串指针和长度构造字节切片
        let bytes = unsafe {
            std::slice::from_raw_parts(method, method_len as usize)
        };
        let req = non_null!(&mut *req ?= hyper_code::HYPERE_INVALID_ARG);
        // Method::from_bytes 解析 HTTP 方法（GET, POST, PUT 等）
        match Method::from_bytes(bytes) {
            Ok(m) => {
                // method_mut() 返回请求中 HTTP 方法的可变引用
                *req.0.method_mut() = m;
                hyper_code::HYPERE_OK
            },
            Err(_) => {
                hyper_code::HYPERE_INVALID_ARG // 无效的 HTTP 方法
            }
        }
    }
}

ffi_fn! {
    /// Set the URI of the request.
    ///
    /// The request's URI is best described as the `request-target` from the RFCs. So in HTTP/1,
    /// whatever is set will get sent as-is in the first line (GET $uri HTTP/1.1). It
    /// supports the 4 defined variants, origin-form, absolute-form, authority-form, and
    /// asterisk-form.
    ///
    /// The underlying type was built to efficiently support HTTP/2 where the request-target is
    /// split over :scheme, :authority, and :path. As such, each part can be set explicitly, or the
    /// type can parse a single contiguous string and if a scheme is found, that slot is "set". If
    /// the string just starts with a path, only the path portion is set. All pseudo headers that
    /// have been parsed/set are sent when the connection type is HTTP/2.
    ///
    /// To set each slot explicitly, use `hyper_request_set_uri_parts`.
    fn hyper_request_set_uri(req: *mut hyper_request, uri: *const u8, uri_len: size_t) -> hyper_code {
        let bytes = unsafe {
            std::slice::from_raw_parts(uri, uri_len as usize)
        };
        let req = non_null!(&mut *req ?= hyper_code::HYPERE_INVALID_ARG);
        // Uri::from_maybe_shared 尝试从字节序列解析 URI
        match Uri::from_maybe_shared(bytes) {
            Ok(u) => {
                *req.0.uri_mut() = u;
                hyper_code::HYPERE_OK
            },
            Err(_) => {
                hyper_code::HYPERE_INVALID_ARG // 无效的 URI
            }
        }
    }
}

ffi_fn! {
    /// Set the URI of the request with separate scheme, authority, and
    /// path/query strings.
    ///
    /// Each of `scheme`, `authority`, and `path_and_query` should either be
    /// null, to skip providing a component, or point to a UTF-8 encoded
    /// string. If any string pointer argument is non-null, its corresponding
    /// `len` parameter must be set to the string's length.
    fn hyper_request_set_uri_parts(
        req: *mut hyper_request,
        scheme: *const u8,
        scheme_len: size_t,
        authority: *const u8,
        authority_len: size_t,
        path_and_query: *const u8,
        path_and_query_len: size_t
    ) -> hyper_code {
        // 使用 Builder 模式逐部分构建 URI
        let mut builder = Uri::builder();
        // 各部分为可选：非空指针时才设置对应组件
        if !scheme.is_null() {
            let scheme_bytes = unsafe {
                std::slice::from_raw_parts(scheme, scheme_len as usize)
            };
            builder = builder.scheme(scheme_bytes);
        }
        if !authority.is_null() {
            let authority_bytes = unsafe {
                std::slice::from_raw_parts(authority, authority_len as usize)
            };
            builder = builder.authority(authority_bytes);
        }
        if !path_and_query.is_null() {
            let path_and_query_bytes = unsafe {
                std::slice::from_raw_parts(path_and_query, path_and_query_len as usize)
            };
            builder = builder.path_and_query(path_and_query_bytes);
        }
        match builder.build() {
            Ok(u) => {
                *unsafe { &mut *req }.0.uri_mut() = u;
                hyper_code::HYPERE_OK
            },
            Err(_) => {
                hyper_code::HYPERE_INVALID_ARG
            }
        }
    }
}

ffi_fn! {
    /// Set the preferred HTTP version of the request.
    ///
    /// The version value should be one of the `HYPER_HTTP_VERSION_` constants.
    ///
    /// Note that this won't change the major HTTP version of the connection,
    /// since that is determined at the handshake step.
    fn hyper_request_set_version(req: *mut hyper_request, version: c_int) -> hyper_code {
        use http::Version;

        let req = non_null!(&mut *req ?= hyper_code::HYPERE_INVALID_ARG);
        // 将 C 整数常量映射到 http::Version 枚举
        *req.0.version_mut() = match version {
            super::HYPER_HTTP_VERSION_NONE => Version::HTTP_11, // 未指定时默认 HTTP/1.1
            super::HYPER_HTTP_VERSION_1_0 => Version::HTTP_10,
            super::HYPER_HTTP_VERSION_1_1 => Version::HTTP_11,
            super::HYPER_HTTP_VERSION_2 => Version::HTTP_2,
            _ => {
                // We don't know this version
                // 未知版本号，返回无效参数错误
                return hyper_code::HYPERE_INVALID_ARG;
            }
        };
        hyper_code::HYPERE_OK
    }
}

ffi_fn! {
    /// Gets a mutable reference to the HTTP headers of this request
    ///
    /// This is not an owned reference, so it should not be accessed after the
    /// `hyper_request` has been consumed.
    fn hyper_request_headers(req: *mut hyper_request) -> *mut hyper_headers {
        // hyper_headers 存储在 HTTP 请求的 extensions 中（扩展字段机制）。
        // get_or_default 如果尚不存在则创建默认的 hyper_headers 并插入。
        hyper_headers::get_or_default(unsafe { &mut *req }.0.extensions_mut())
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Set the body of the request.
    ///
    /// You can get a `hyper_body` by calling `hyper_body_new`.
    ///
    /// This takes ownership of the `hyper_body *`, you must not use it or
    /// free it after setting it on the request.
    fn hyper_request_set_body(req: *mut hyper_request, body: *mut hyper_body) -> hyper_code {
        // 取回 body 的所有权
        let body = non_null!(Box::from_raw(body) ?= hyper_code::HYPERE_INVALID_ARG);
        let req = non_null!(&mut *req ?= hyper_code::HYPERE_INVALID_ARG);
        // 替换请求中的 body
        *req.0.body_mut() = body.0;
        hyper_code::HYPERE_OK
    }
}

ffi_fn! {
    /// Set an informational (1xx) response callback.
    ///
    /// The callback is called each time hyper receives an informational (1xx)
    /// response for this request.
    ///
    /// The third argument is an opaque user data pointer, which is passed to
    /// the callback each time.
    ///
    /// The callback is passed the `void *` data pointer, and a
    /// `hyper_response *` which can be inspected as any other response. The
    /// body of the response will always be empty.
    ///
    /// NOTE: The `hyper_response *` is just borrowed data, and will not
    /// be valid after the callback finishes. You must copy any data you wish
    /// to persist.
    fn hyper_request_on_informational(req: *mut hyper_request, callback: hyper_request_on_informational_callback, data: *mut c_void) -> hyper_code {
        #[cfg(feature = "client")]
        {
        // 构建回调上下文对象
        let ext = OnInformational {
            func: callback,
            data: UserDataPointer(data),
        };
        let req = non_null!(&mut *req ?= hyper_code::HYPERE_INVALID_ARG);
        // 将回调注册到请求的扩展中，hyper 内部会在收到 1xx 响应时调用
        crate::ext::on_informational_raw(&mut req.0, ext);
        hyper_code::HYPERE_OK
        }
        #[cfg(not(feature = "client"))]
        {
        // client feature 未启用时，drop 参数并返回 feature 未启用错误
        drop((req, callback, data));
        hyper_code::HYPERE_FEATURE_NOT_ENABLED
        }
    }
}

/// `hyper_request` 的内部方法实现。
impl hyper_request {
    /// 在发送请求前，将 FFI 层的头部信息应用到实际的 HTTP 请求中。
    ///
    /// hyper FFI 将头部信息（包括原始大小写和顺序）存储在请求的 extensions 中
    /// 作为 `hyper_headers` 对象。此方法在发送前将这些信息提取出来，
    /// 分别设置到请求的 HeaderMap 和对应的扩展字段中。
    pub(super) fn finalize_request(&mut self) {
        if let Some(headers) = self.0.extensions_mut().remove::<hyper_headers>() {
            *self.0.headers_mut() = headers.headers; // 设置标准头部映射
            self.0.extensions_mut().insert(headers.orig_casing); // 插入原始大小写映射
            self.0.extensions_mut().insert(headers.orig_order); // 插入原始顺序记录
        }
    }
}

// ===== impl hyper_response =====

ffi_fn! {
    /// Free an HTTP response.
    ///
    /// This should be used for any response once it is no longer needed.
    fn hyper_response_free(resp: *mut hyper_response) {
        drop(non_null!(Box::from_raw(resp) ?= ()));
    }
}

ffi_fn! {
    /// Get the HTTP-Status code of this response.
    ///
    /// It will always be within the range of 100-599.
    fn hyper_response_status(resp: *const hyper_response) -> u16 {
        // as_u16() 将 StatusCode 转换为原始的 u16 数值
        non_null!(&*resp ?= 0).0.status().as_u16()
    }
}

ffi_fn! {
    /// Get a pointer to the reason-phrase of this response.
    ///
    /// This buffer is not null-terminated.
    ///
    /// This buffer is owned by the response, and should not be used after
    /// the response has been freed.
    ///
    /// Use `hyper_response_reason_phrase_len()` to get the length of this
    /// buffer.
    fn hyper_response_reason_phrase(resp: *const hyper_response) -> *const u8 {
        // 获取原因短语的字节切片并返回其指针（借用语义）
        non_null!(&*resp ?= std::ptr::null()).reason_phrase().as_ptr()
    } ?= std::ptr::null()
}

ffi_fn! {
    /// Get the length of the reason-phrase of this response.
    ///
    /// Use `hyper_response_reason_phrase()` to get the buffer pointer.
    fn hyper_response_reason_phrase_len(resp: *const hyper_response) -> size_t {
        non_null!(&*resp ?= 0).reason_phrase().len()
    }
}

ffi_fn! {
    /// Get the HTTP version used by this response.
    ///
    /// The returned value could be:
    ///
    /// - `HYPER_HTTP_VERSION_1_0`
    /// - `HYPER_HTTP_VERSION_1_1`
    /// - `HYPER_HTTP_VERSION_2`
    /// - `HYPER_HTTP_VERSION_NONE` if newer (or older).
    fn hyper_response_version(resp: *const hyper_response) -> c_int {
        use http::Version;

        // 将 http::Version 枚举映射回 FFI 层的整数常量
        match non_null!(&*resp ?= 0).0.version() {
            Version::HTTP_10 => super::HYPER_HTTP_VERSION_1_0,
            Version::HTTP_11 => super::HYPER_HTTP_VERSION_1_1,
            Version::HTTP_2 => super::HYPER_HTTP_VERSION_2,
            _ => super::HYPER_HTTP_VERSION_NONE, // 未知版本
        }
    }
}

ffi_fn! {
    /// Gets a reference to the HTTP headers of this response.
    ///
    /// This is not an owned reference, so it should not be accessed after the
    /// `hyper_response` has been freed.
    fn hyper_response_headers(resp: *mut hyper_response) -> *mut hyper_headers {
        // 与 hyper_request_headers 类似，从 extensions 中获取或创建头部映射
        hyper_headers::get_or_default(unsafe { &mut *resp }.0.extensions_mut())
    } ?= std::ptr::null_mut()
}

ffi_fn! {
    /// Take ownership of the body of this response.
    ///
    /// It is safe to free the response even after taking ownership of its body.
    ///
    /// To avoid a memory leak, the body must eventually be consumed by
    /// `hyper_body_free`, `hyper_body_foreach`, or `hyper_request_set_body`.
    fn hyper_response_body(resp: *mut hyper_response) -> *mut hyper_body {
        // std::mem::replace 将响应中的 body 替换为空 body，同时返回原始 body。
        // 这实现了 body 的所有权转移：调用者拥有返回的 body，响应保留一个空 body。
        let body = std::mem::replace(non_null!(&mut *resp ?= std::ptr::null_mut()).0.body_mut(), IncomingBody::empty());
        Box::into_raw(Box::new(hyper_body(body)))
    } ?= std::ptr::null_mut()
}

/// `hyper_response` 的内部方法实现。
impl hyper_response {
    /// 将 hyper 内部的 `Response<IncomingBody>` 包装为 FFI 层的 `hyper_response`。
    ///
    /// 此方法在包装过程中将头部信息从标准的 `HeaderMap` 和扩展字段中
    /// 提取出来，合并为 FFI 层的 `hyper_headers` 对象，便于 C 调用者
    /// 通过统一的 API 访问头部数据（包括原始大小写和顺序）。
    pub(super) fn wrap(mut resp: Response<IncomingBody>) -> hyper_response {
        // take 将原始头部映射移出，留下空映射
        let headers = std::mem::take(resp.headers_mut());
        // 从扩展中提取原始大小写映射（如果不存在则使用默认值）
        let orig_casing = resp
            .extensions_mut()
            .remove::<HeaderCaseMap>()
            .unwrap_or_else(HeaderCaseMap::default);
        // 从扩展中提取原始顺序记录（如果不存在则使用默认值）
        let orig_order = resp
            .extensions_mut()
            .remove::<OriginalHeaderOrder>()
            .unwrap_or_else(OriginalHeaderOrder::default);
        // 将三者合并为 hyper_headers 并存储到 extensions 中
        resp.extensions_mut().insert(hyper_headers {
            headers,
            orig_casing,
            orig_order,
        });

        hyper_response(resp)
    }

    /// 获取响应的原因短语（如 "OK"、"Not Found"）。
    ///
    /// 优先从扩展中查找自定义的 `ReasonPhrase`（服务器可能发送非标准原因短语），
    /// 如果没有则回退到状态码的标准原因短语，
    /// 如果状态码也没有标准原因短语则返回空切片。
    fn reason_phrase(&self) -> &[u8] {
        // 优先检查是否有服务器发送的自定义原因短语
        if let Some(reason) = self.0.extensions().get::<ReasonPhrase>() {
            return reason.as_bytes();
        }

        // 回退到状态码的标准原因短语（如 200 -> "OK"）
        if let Some(reason) = self.0.status().canonical_reason() {
            return reason.as_bytes();
        }

        // 无法确定原因短语时返回空切片
        &[]
    }
}

/// 为 `hyper_response` 实现 `AsTaskType` trait。
///
/// 当发送请求的任务完成时，任务系统通过此 trait 识别输出值的类型为
/// `HYPER_TASK_RESPONSE`，使 C 调用者知道应将 `hyper_task_value()`
/// 的返回值转换为 `hyper_response*`。
unsafe impl AsTaskType for hyper_response {
    fn as_task_type(&self) -> hyper_task_return_type {
        hyper_task_return_type::HYPER_TASK_RESPONSE
    }
}

// ===== impl Headers =====

/// `hyper_headers_foreach` 的回调函数类型签名。
///
/// 参数说明：
/// - `*mut c_void`：用户数据指针
/// - `*const u8, size_t`：头部名称的指针和长度
/// - `*const u8, size_t`：头部值的指针和长度
///
/// 返回值：`HYPER_ITER_CONTINUE` 继续迭代，`HYPER_ITER_BREAK` 停止。
type hyper_headers_foreach_callback =
    extern "C" fn(*mut c_void, *const u8, size_t, *const u8, size_t) -> c_int;

/// `hyper_headers` 的内部方法实现。
impl hyper_headers {
    /// 从 HTTP 扩展字段中获取或创建默认的 `hyper_headers`。
    ///
    /// HTTP 请求和响应的 `extensions` 是一个类型映射（TypeMap），
    /// 可以存储任意类型的附加数据。此方法利用这个机制将 FFI 层的头部
    /// 对象嵌入到标准的 HTTP 消息中。
    ///
    /// 如果 extensions 中尚未存在 `hyper_headers`，会插入一个默认实例。
    pub(super) fn get_or_default(ext: &mut http::Extensions) -> &mut hyper_headers {
        if let None = ext.get_mut::<hyper_headers>() {
            ext.insert(hyper_headers::default());
        }

        ext.get_mut::<hyper_headers>().unwrap()
    }
}

ffi_fn! {
    /// Iterates the headers passing each name and value pair to the callback.
    ///
    /// The `userdata` pointer is also passed to the callback.
    ///
    /// The callback should return `HYPER_ITER_CONTINUE` to keep iterating, or
    /// `HYPER_ITER_BREAK` to stop.
    fn hyper_headers_foreach(headers: *const hyper_headers, func: hyper_headers_foreach_callback, userdata: *mut c_void) {
        let headers = non_null!(&*headers ?= ());
        // For each header name/value pair, there may be a value in the casemap
        // that corresponds to the HeaderValue. So, we iterator all the keys,
        // and for each one, try to pair the originally cased name with the value.
        //
        // 头部迭代策略：
        // 如果有原始顺序记录（ordered_iter），则按插入顺序迭代；
        // 否则按 HeaderMap 的默认顺序迭代。
        // 对于每个头部，优先使用原始大小写名称（orig_casing），
        // 如果没有原始大小写记录则回退到标准的小写名称。
        //
        // TODO: consider adding http::HeaderMap::entries() iterator
        let mut ordered_iter =  headers.orig_order.get_in_order().peekable();
        if ordered_iter.peek().is_some() {
            // 有原始顺序记录：按记录的插入顺序迭代
            for (name, idx) in ordered_iter {
                // 尝试获取头部名称的原始大小写形式
                let (name_ptr, name_len) = if let Some(orig_name) = headers.orig_casing.get_all(name).nth(*idx) {
                    (orig_name.as_ref().as_ptr(), orig_name.as_ref().len())
                } else {
                    // 回退到标准小写名称
                    (
                    name.as_str().as_bytes().as_ptr(),
                    name.as_str().as_bytes().len(),
                    )
                };

                let val_ptr;
                let val_len;
                // 通过名称和索引获取对应的头部值
                if let Some(value) = headers.headers.get_all(name).iter().nth(*idx) {
                    val_ptr = value.as_bytes().as_ptr();
                    val_len = value.as_bytes().len();
                } else {
                    // Stop iterating, something has gone wrong.
                    // 索引越界说明内部状态不一致，安全地停止迭代
                    return;
                }

                // 调用 C 回调，传递头部名称和值
                if HYPER_ITER_CONTINUE != func(userdata, name_ptr, name_len, val_ptr, val_len) {
                    return; // 回调请求停止迭代
                }
            }
        } else {
            // 无原始顺序记录：按 HeaderMap 的默认顺序迭代
            for name in headers.headers.keys() {
                let mut names = headers.orig_casing.get_all(name);

                // 一个头部名称可能有多个值（如多个 Set-Cookie）
                for value in headers.headers.get_all(name) {
                    let (name_ptr, name_len) = if let Some(orig_name) = names.next() {
                        (orig_name.as_ref().as_ptr(), orig_name.as_ref().len())
                    } else {
                        (
                            name.as_str().as_bytes().as_ptr(),
                            name.as_str().as_bytes().len(),
                        )
                    };

                    let val_ptr = value.as_bytes().as_ptr();
                    let val_len = value.as_bytes().len();

                    if HYPER_ITER_CONTINUE != func(userdata, name_ptr, name_len, val_ptr, val_len) {
                        return;
                    }
                }
            }
        }
    }
}

ffi_fn! {
    /// Sets the header with the provided name to the provided value.
    ///
    /// This overwrites any previous value set for the header.
    fn hyper_headers_set(headers: *mut hyper_headers, name: *const u8, name_len: size_t, value: *const u8, value_len: size_t) -> hyper_code {
        let headers = non_null!(&mut *headers ?= hyper_code::HYPERE_INVALID_ARG);
        // raw_name_value 从裸指针解析并验证头部名称和值
        match unsafe { raw_name_value(name, name_len, value, value_len) } {
            Ok((name, value, orig_name)) => {
                // insert 覆盖已有的同名头部值
                headers.headers.insert(&name, value);
                headers.orig_casing.insert(name.clone(), orig_name.clone());
                headers.orig_order.insert(name);
                hyper_code::HYPERE_OK
            }
            Err(code) => code,
        }
    }
}

ffi_fn! {
    /// Adds the provided value to the list of the provided name.
    ///
    /// If there were already existing values for the name, this will append the
    /// new value to the internal list.
    fn hyper_headers_add(headers: *mut hyper_headers, name: *const u8, name_len: size_t, value: *const u8, value_len: size_t) -> hyper_code {
        let headers = non_null!(&mut *headers ?= hyper_code::HYPERE_INVALID_ARG);

        match unsafe { raw_name_value(name, name_len, value, value_len) } {
            Ok((name, value, orig_name)) => {
                // append 追加值到已有列表（不覆盖），适用于可重复的头部（如 Set-Cookie）
                headers.headers.append(&name, value);
                headers.orig_casing.append(&name, orig_name.clone());
                headers.orig_order.append(name);
                hyper_code::HYPERE_OK
            }
            Err(code) => code,
        }
    }
}

/// 为 `hyper_headers` 实现 `Default` trait。
///
/// 创建一个空的头部映射，所有内部容器都使用各自的默认值。
impl Default for hyper_headers {
    fn default() -> Self {
        Self {
            headers: Default::default(),
            orig_casing: HeaderCaseMap::default(),
            orig_order: OriginalHeaderOrder::default(),
        }
    }
}

/// 从 C 传入的裸指针解析并验证 HTTP 头部名称和值。
///
/// 此函数是 `hyper_headers_set` 和 `hyper_headers_add` 的共享辅助函数。
///
/// 返回值：
/// - `Ok((HeaderName, HeaderValue, Bytes))` —— 解析后的标准化名称、值和原始名称字节
/// - `Err(hyper_code)` —— 解析失败时的错误码
///
/// # Safety
///
/// 调用者必须保证 `name` 和 `value` 指针在对应长度范围内有效。
unsafe fn raw_name_value(
    name: *const u8,
    name_len: size_t,
    value: *const u8,
    value_len: size_t,
) -> Result<(HeaderName, HeaderValue, Bytes), hyper_code> {
    // 从裸指针构造字节切片
    let name = std::slice::from_raw_parts(name, name_len);
    // 复制一份原始名称字节（保留原始大小写）
    let orig_name = Bytes::copy_from_slice(name);
    // HeaderName::from_bytes 会将名称标准化为小写
    let name = match HeaderName::from_bytes(name) {
        Ok(name) => name,
        Err(_) => return Err(hyper_code::HYPERE_INVALID_ARG),
    };
    let value = std::slice::from_raw_parts(value, value_len);
    // HeaderValue::from_bytes 验证值是否符合 HTTP 头部值的规范
    let value = match HeaderValue::from_bytes(value) {
        Ok(val) => val,
        Err(_) => return Err(hyper_code::HYPERE_INVALID_ARG),
    };

    Ok((name, value, orig_name))
}

// ===== impl OnInformational =====

/// 为 `OnInformational` 实现 hyper 内部的 `OnInformationalCallback` trait。
///
/// 这是连接 C 回调与 hyper 内部信息性响应处理机制的桥梁。
/// 当 hyper 的 HTTP/1.1 或 HTTP/2 解析器收到 1xx 响应时，
/// 会调用此 trait 的方法，然后此方法再转调 C 端注册的回调函数。
#[cfg(feature = "client")]
impl crate::ext::OnInformationalCallback for OnInformational {
    fn on_informational(&self, res: http::Response<()>) {
        // 将空 body 的响应映射为 IncomingBody::empty()
        let res = res.map(|()| IncomingBody::empty());
        // 包装为 FFI 层的 hyper_response
        let mut res = hyper_response::wrap(res);
        // 调用 C 端注册的回调函数，传递用户数据和响应的可变引用
        (self.func)(self.data.0, &mut res);
    }
}

/// 头部功能的单元测试模块。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 `hyper_headers_foreach` 是否正确保留头部名称的原始大小写。
    ///
    /// 验证场景：添加两个同名头部（不同大小写），迭代时应返回各自的原始大小写。
    #[test]
    fn test_headers_foreach_cases_preserved() {
        let mut headers = hyper_headers::default();

        let name1 = b"Set-CookiE";
        let value1 = b"a=b";
        hyper_headers_add(
            &mut headers,
            name1.as_ptr(),
            name1.len(),
            value1.as_ptr(),
            value1.len(),
        );

        let name2 = b"SET-COOKIE";
        let value2 = b"c=d";
        hyper_headers_add(
            &mut headers,
            name2.as_ptr(),
            name2.len(),
            value2.as_ptr(),
            value2.len(),
        );

        let mut vec = Vec::<u8>::new();
        hyper_headers_foreach(&headers, concat, &mut vec as *mut _ as *mut c_void);

        assert_eq!(vec, b"Set-CookiE: a=b\r\nSET-COOKIE: c=d\r\n");

        // 辅助回调函数：将头部名称和值拼接为 "Name: Value\r\n" 格式
        extern "C" fn concat(
            vec: *mut c_void,
            name: *const u8,
            name_len: usize,
            value: *const u8,
            value_len: usize,
        ) -> c_int {
            unsafe {
                let vec = &mut *(vec as *mut Vec<u8>);
                let name = std::slice::from_raw_parts(name, name_len);
                let value = std::slice::from_raw_parts(value, value_len);
                vec.extend(name);
                vec.extend(b": ");
                vec.extend(value);
                vec.extend(b"\r\n");
            }
            HYPER_ITER_CONTINUE
        }
    }

    /// 测试 `hyper_headers_foreach` 是否正确保留头部的插入顺序。
    ///
    /// 验证场景：添加三个不同名称的头部，迭代时应按插入顺序返回。
    /// 此测试仅在同时启用 `http1` 和 `ffi` feature 时运行。
    #[cfg(all(feature = "http1", feature = "ffi"))]
    #[test]
    fn test_headers_foreach_order_preserved() {
        let mut headers = hyper_headers::default();

        let name1 = b"Set-CookiE";
        let value1 = b"a=b";
        hyper_headers_add(
            &mut headers,
            name1.as_ptr(),
            name1.len(),
            value1.as_ptr(),
            value1.len(),
        );

        let name2 = b"Content-Encoding";
        let value2 = b"gzip";
        hyper_headers_add(
            &mut headers,
            name2.as_ptr(),
            name2.len(),
            value2.as_ptr(),
            value2.len(),
        );

        let name3 = b"SET-COOKIE";
        let value3 = b"c=d";
        hyper_headers_add(
            &mut headers,
            name3.as_ptr(),
            name3.len(),
            value3.as_ptr(),
            value3.len(),
        );

        let mut vec = Vec::<u8>::new();
        hyper_headers_foreach(&headers, concat, &mut vec as *mut _ as *mut c_void);

        println!("{}", std::str::from_utf8(&vec).unwrap());
        assert_eq!(
            vec,
            b"Set-CookiE: a=b\r\nContent-Encoding: gzip\r\nSET-COOKIE: c=d\r\n"
        );

        extern "C" fn concat(
            vec: *mut c_void,
            name: *const u8,
            name_len: usize,
            value: *const u8,
            value_len: usize,
        ) -> c_int {
            unsafe {
                let vec = &mut *(vec as *mut Vec<u8>);
                let name = std::slice::from_raw_parts(name, name_len);
                let value = std::slice::from_raw_parts(value, value_len);
                vec.extend(name);
                vec.extend(b": ");
                vec.extend(value);
                vec.extend(b"\r\n");
            }
            HYPER_ITER_CONTINUE
        }
    }
}
