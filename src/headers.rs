//! HTTP 头部解析与操作工具模块
//!
//! 本模块提供了一系列用于解析和操作 HTTP 头部的工具函数，
//! 包括 `Connection`、`Content-Length`、`Transfer-Encoding` 等关键头部的处理。
//!
//! ## 在 hyper 中的角色
//!
//! 本模块是 hyper 协议处理层的基础组件之一，通过 `cfg_proto!` 宏进行条件编译，
//! 仅在同时启用了协议版本（http1/http2）和角色（client/server）时才编译。
//! 它被 `proto` 模块（HTTP/1 和 HTTP/2 的协议实现）广泛使用，
//! 用于在编码/解码 HTTP 消息时处理各种头部字段。
//!
//! ## 设计说明
//!
//! 所有函数都是 `pub(super)` 可见性，仅供 hyper crate 内部使用。
//! 函数通过细粒度的 `#[cfg]` 属性控制编译，确保只有在特定 feature
//! 组合下才会编译对应的函数。

// BytesMut 用于高效的字节缓冲区操作，仅在 HTTP/1 client 模式下需要（用于 add_chunked）
#[cfg(all(feature = "client", feature = "http1"))]
use bytes::BytesMut;
// HeaderValue 是 HTTP 头部值的类型，在多个函数中使用
use http::header::HeaderValue;
// Method 用于判断 HTTP 方法的载荷语义（仅 HTTP/2 client）
#[cfg(all(feature = "http2", feature = "client"))]
use http::Method;
// ValueIter 用于迭代同名头部的多个值，CONTENT_LENGTH 是预定义的头部名常量，
// HeaderMap 是头部字段的映射集合
#[cfg(any(feature = "client", all(feature = "server", feature = "http2")))]
use http::{
    header::{ValueIter, CONTENT_LENGTH},
    HeaderMap,
};

/// 检查 `Connection` 头部是否包含 `keep-alive` 指令。
///
/// HTTP/1.1 中，`Connection: keep-alive` 表示客户端希望保持连接复用。
/// 此函数委托给 `connection_has` 进行大小写不敏感的查找。
///
/// 仅在 HTTP/1 模式下编译。
#[cfg(feature = "http1")]
pub(super) fn connection_keep_alive(value: &HeaderValue) -> bool {
    connection_has(value, "keep-alive")
}

/// 检查 `Connection` 头部是否包含 `close` 指令。
///
/// HTTP/1.1 中，`Connection: close` 表示在本次请求/响应完成后关闭连接。
/// 此函数委托给 `connection_has` 进行大小写不敏感的查找。
///
/// 仅在 HTTP/1 模式下编译。
#[cfg(feature = "http1")]
pub(super) fn connection_close(value: &HeaderValue) -> bool {
    connection_has(value, "close")
}

/// 检查 `Connection` 头部值中是否包含指定的连接选项。
///
/// `Connection` 头部可以包含多个逗号分隔的值（如 `keep-alive, Upgrade`），
/// 此函数逐一检查每个值，进行大小写不敏感的比较。
///
/// # 参数
/// - `value`：Connection 头部的值
/// - `needle`：要查找的连接选项（如 "keep-alive"、"close"）
///
/// 仅在 HTTP/1 模式下编译。
#[cfg(feature = "http1")]
fn connection_has(value: &HeaderValue, needle: &str) -> bool {
    // to_str() 尝试将 HeaderValue 转换为 UTF-8 字符串
    // 如果包含非 ASCII 字符则返回 Err，此时直接返回 false
    if let Ok(s) = value.to_str() {
        for val in s.split(',') {
            // trim() 去除前后空白，eq_ignore_ascii_case 进行 ASCII 大小写不敏感比较
            if val.trim().eq_ignore_ascii_case(needle) {
                return true;
            }
        }
    }
    false
}

/// 从单个 `Content-Length` 头部值中解析出长度数值。
///
/// 仅在 HTTP/1 server 模式下使用，因为 server 需要根据 Content-Length
/// 来确定请求 body 的长度。
///
/// 委托给 `from_digits` 进行安全的数字解析。
#[cfg(all(feature = "http1", feature = "server"))]
pub(super) fn content_length_parse(value: &HeaderValue) -> Option<u64> {
    from_digits(value.as_bytes())
}

/// 从 `HeaderMap` 中解析 `Content-Length` 头部值。
///
/// 获取所有 `Content-Length` 头部（可能有多个）并委托给
/// `content_length_parse_all_values` 进行统一处理。
///
/// 在 client 模式或 HTTP/2 server 模式下可用。
#[cfg(any(feature = "client", all(feature = "server", feature = "http2")))]
pub(super) fn content_length_parse_all(headers: &HeaderMap) -> Option<u64> {
    // get_all 返回指定头部名的所有值的迭代器
    content_length_parse_all_values(headers.get_all(CONTENT_LENGTH).into_iter())
}

/// 从多个 `Content-Length` 头部值中解析出长度数值。
///
/// 根据 HTTP 规范（RFC 7230 Section 3.3.2），如果存在多个 Content-Length 头部，
/// 它们的值必须完全一致。此函数实现了这个验证逻辑：
/// - 所有值都必须能正确解析为数字
/// - 所有值必须相同
/// - 如果有任何不一致或无法解析的值，返回 `None`
///
/// 在 client 模式或 HTTP/2 server 模式下可用。
#[cfg(any(feature = "client", all(feature = "server", feature = "http2")))]
pub(super) fn content_length_parse_all_values(values: ValueIter<'_, HeaderValue>) -> Option<u64> {
    // If multiple Content-Length headers were sent, everything can still
    // be alright if they all contain the same value, and all parse
    // correctly. If not, then it's an error.

    let mut content_length: Option<u64> = None;
    for h in values {
        if let Ok(line) = h.to_str() {
            // Content-Length 值也可能是逗号分隔的（虽然不常见但规范允许）
            for v in line.split(',') {
                if let Some(n) = from_digits(v.trim().as_bytes()) {
                    if content_length.is_none() {
                        // 第一次遇到有效值，记录下来
                        content_length = Some(n)
                    } else if content_length != Some(n) {
                        // 与之前记录的值不一致，返回 None 表示错误
                        return None;
                    }
                } else {
                    // 无法解析为数字，返回 None 表示错误
                    return None;
                }
            }
        } else {
            // 头部值不是有效的 UTF-8，返回 None 表示错误
            return None;
        }
    }

    content_length
}

/// 从字节切片中安全地解析无符号 64 位整数。
///
/// 不使用 `str::parse::<u64>()` 是因为标准库的实现允许可选的前导符号（如 `+`），
/// 而 HTTP 规范中的数字字段不允许符号前缀。此外，输入可能不是有效的 UTF-8，
/// 所以直接操作字节更为安全和高效。
///
/// 使用 `checked_mul` 和 `checked_add` 来防止整数溢出，
/// 溢出时返回 `None` 而非 panic。
fn from_digits(bytes: &[u8]) -> Option<u64> {
    // cannot use FromStr for u64, since it allows a signed prefix
    let mut result = 0u64;
    const RADIX: u64 = 10;

    // 空字节串不是有效的数字
    if bytes.is_empty() {
        return None;
    }

    for &b in bytes {
        // can't use char::to_digit, since we haven't verified these bytes
        // are utf-8.
        match b {
            b'0'..=b'9' => {
                // 使用 checked 算术防止溢出
                result = result.checked_mul(RADIX)?;
                result = result.checked_add((b - b'0') as u64)?;
            }
            _ => {
                // not a DIGIT, get outta here!
                return None;
            }
        }
    }

    Some(result)
}

/// 检查给定的 HTTP 方法是否具有已定义的载荷语义。
///
/// 根据 HTTP 规范，GET、HEAD、DELETE 和 CONNECT 方法没有定义的
/// 请求 body 语义。这在 HTTP/2 client 中用于决定是否需要设置
/// Content-Length 头部。
///
/// 仅在 HTTP/2 client 模式下编译。
#[cfg(all(feature = "http2", feature = "client"))]
pub(super) fn method_has_defined_payload_semantics(method: &Method) -> bool {
    // 使用 matches! 宏进行模式匹配，取反得到"有载荷语义"的方法
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::DELETE | Method::CONNECT
    )
}

/// 如果 `Content-Length` 头部尚未设置，则设置其值。
///
/// 使用 `HeaderMap::entry` API 的 `or_insert_with` 方法，
/// 仅在头部不存在时才插入。这是一种惰性设置模式，
/// 避免覆盖用户可能已经设置的值。
///
/// 仅在 HTTP/2 模式下编译。
#[cfg(feature = "http2")]
pub(super) fn set_content_length_if_missing(headers: &mut HeaderMap, len: u64) {
    headers
        .entry(CONTENT_LENGTH)
        // or_insert_with 使用闭包惰性创建值，仅在需要时才执行
        .or_insert_with(|| HeaderValue::from(len));
}

/// 检查 `Transfer-Encoding` 头部是否使用了 chunked 编码。
///
/// 获取所有 `Transfer-Encoding` 头部值并委托给 `is_chunked` 函数处理。
///
/// 仅在 HTTP/1 client 模式下编译。
#[cfg(all(feature = "client", feature = "http1"))]
pub(super) fn transfer_encoding_is_chunked(headers: &HeaderMap) -> bool {
    is_chunked(headers.get_all(http::header::TRANSFER_ENCODING).into_iter())
}

/// 检查 Transfer-Encoding 值的迭代器中，最后一个编码是否为 chunked。
///
/// 根据 HTTP/1.1 规范（RFC 7230 Section 3.3.1），chunked 传输编码
/// 必须始终是最后一个编码。此函数使用 `next_back()` 获取最后一个值
/// 进行检查。
///
/// 仅在 HTTP/1 client 模式下编译。
#[cfg(all(feature = "client", feature = "http1"))]
pub(super) fn is_chunked(mut encodings: ValueIter<'_, HeaderValue>) -> bool {
    // chunked must always be the last encoding, according to spec
    // next_back() 获取迭代器的最后一个元素（DoubleEndedIterator）
    if let Some(line) = encodings.next_back() {
        return is_chunked_(line);
    }

    false
}

/// 检查单个 `Transfer-Encoding` 头部值中，最后一个编码是否为 chunked。
///
/// Transfer-Encoding 头部可以包含逗号分隔的多个编码（如 `gzip, chunked`），
/// 此函数使用 `rsplit` 从右向左分割，取第一个值（即最后一个编码）进行检查。
///
/// 仅在 HTTP/1 模式下编译。
#[cfg(feature = "http1")]
pub(super) fn is_chunked_(value: &HeaderValue) -> bool {
    // chunked must always be the last encoding, according to spec
    if let Ok(s) = value.to_str() {
        // rsplit 从右向左分割，next() 获取最右侧的部分（即最后一个编码）
        if let Some(encoding) = s.rsplit(',').next() {
            return encoding.trim().eq_ignore_ascii_case("chunked");
        }
    }

    false
}

/// 向已存在的 `Transfer-Encoding` 头部追加 `chunked` 编码。
///
/// 当需要为 HTTP/1 请求添加 chunked 编码时，此函数修改已有的
/// Transfer-Encoding 头部值，在末尾追加 `, chunked`。如果头部
/// 已经没有值了（不太可能发生），则直接插入 `chunked`。
///
/// # 参数
/// - `entry`：一个已占用的头部条目（OccupiedEntry），确保头部已存在
///
/// # 实现细节
/// 使用 `BytesMut` 进行高效的字节缓冲操作，避免不必要的内存分配。
/// 先将原始值和 `, chunked` 拼接到 BytesMut 中，然后通过
/// `freeze()` 将其转换为不可变的 `Bytes`，再转换为 `HeaderValue`。
///
/// 仅在 HTTP/1 client 模式下编译。
#[cfg(all(feature = "client", feature = "http1"))]
pub(super) fn add_chunked(mut entry: http::header::OccupiedEntry<'_, HeaderValue>) {
    const CHUNKED: &str = "chunked";

    // iter_mut() 获取该头部所有值的可变迭代器
    // next_back() 获取最后一个值（chunked 必须在最后）
    if let Some(line) = entry.iter_mut().next_back() {
        // + 2 for ", "
        // 预分配足够的容量，避免后续的重新分配
        let new_cap = line.as_bytes().len() + CHUNKED.len() + 2;
        let mut buf = BytesMut::with_capacity(new_cap);
        // 将原始值、分隔符和 "chunked" 依次追加到缓冲区
        buf.extend_from_slice(line.as_bytes());
        buf.extend_from_slice(b", ");
        buf.extend_from_slice(CHUNKED.as_bytes());

        // freeze() 将可变的 BytesMut 转换为不可变的 Bytes（零拷贝）
        // from_maybe_shared 从 Bytes 创建 HeaderValue
        *line = HeaderValue::from_maybe_shared(buf.freeze())
            .expect("original header value plus ascii is valid");
        return;
    }

    // 如果头部没有已有的值，直接插入 "chunked"
    // from_static 从编译时常量创建 HeaderValue（零分配）
    entry.insert(HeaderValue::from_static(CHUNKED));
}
