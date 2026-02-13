//! HTTP/1 响应原因短语（Reason Phrase）模块。
//!
//! 本模块提供了 `ReasonPhrase` 类型，用于表示和操作 HTTP/1 响应状态行中的原因短语。
//! 在 HTTP/1.1 协议中，响应状态行格式为 `HTTP/1.1 <状态码> <原因短语>\r\n`，
//! 例如 `HTTP/1.1 200 OK`。虽然 HTTP/2 已移除原因短语，但在 HTTP/1 场景下，
//! 服务器可能会发送非标准的原因短语（如 `HTTP/1.1 200 Awesome`）。
//!
//! 在 hyper 架构中，本模块作为 `ext`（扩展）子系统的一部分，允许客户端读取
//! 非标准原因短语，也允许服务端在 HTTP/1 响应中写入自定义原因短语。
//!
//! # 安全性
//!
//! 原因短语的字节内容必须符合 RFC 规范。本模块提供了严格的验证逻辑，
//! 防止注入包含换行符等危险字符的原因短语，避免 HTTP 响应拆分攻击。

/// 引入 `bytes::Bytes`，一种引用计数的字节缓冲区，支持零拷贝切片。
/// `ReasonPhrase` 内部使用 `Bytes` 存储原因短语数据，兼顾性能和灵活性。
use bytes::Bytes;

/// HTTP/1 响应中的原因短语。
///
/// `ReasonPhrase` 封装了 HTTP/1 响应状态行中状态码之后的文本部分。
/// 它通过 `http::Response` 的扩展（extensions）机制与响应关联。
///
/// # 客户端用法
///
/// 对于客户端，当服务器返回的原因短语与状态码的标准原因短语不同时，
/// `ReasonPhrase` 会出现在 `http::Response` 的扩展中。例如：
/// - 服务器返回 `HTTP/1.1 200 Awesome` 时，扩展中会包含值为 `Awesome` 的 `ReasonPhrase`
/// - 服务器返回 `HTTP/1.1 200 OK` 时，由于 `OK` 是 200 的标准原因短语，不会添加扩展
///
/// ```no_run
/// # #[cfg(all(feature = "tcp", feature = "client", feature = "http1"))]
/// # async fn fake_fetch() -> hyper::Result<()> {
/// use hyper::{Client, Uri};
/// use hyper::ext::ReasonPhrase;
///
/// let res = Client::new().get(Uri::from_static("http://example.com/non_canonical_reason")).await?;
///
/// // 打印非标准原因短语（如果存在）
/// if let Some(reason) = res.extensions().get::<ReasonPhrase>() {
///     println!("non-canonical reason: {}", std::str::from_utf8(reason.as_bytes()).unwrap());
/// }
/// # Ok(())
/// # }
/// ```
///
/// # 服务端用法
///
/// 当 `ReasonPhrase` 出现在服务端写入的 `http::Response` 扩展中时，
/// 其内容将替代标准原因短语写入 HTTP/1 响应。
///
/// # 内部实现
///
/// 使用 `Bytes` 作为内部存储，支持零拷贝和引用计数共享。
/// 派生了 `Clone`、`Debug`、`PartialEq`、`Eq`、`PartialOrd`、`Ord`、`Hash`，
/// 使其可以在各种集合和比较场景中使用。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReasonPhrase(Bytes); // 新类型模式（newtype pattern），封装 Bytes 以提供类型安全

/// `ReasonPhrase` 的核心方法实现。
impl ReasonPhrase {
    /// 获取原因短语的字节切片引用。
    ///
    /// 返回原始字节数据，调用者可根据需要转换为字符串。
    /// 注意：原因短语可能包含非 UTF-8 字节（如 obs-text 范围 0x80-0xFF），
    /// 因此返回 `&[u8]` 而非 `&str`。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// 从静态字节切片创建 `ReasonPhrase`。
    ///
    /// 这是一个 `const fn`，可在编译期求值。如果输入包含无效字节，
    /// 将在编译期触发 `panic!`，从而在编译阶段就能发现错误。
    ///
    /// 内部使用 `Bytes::from_static` 创建零拷贝的静态引用，
    /// 不会分配堆内存。
    pub const fn from_static(reason: &'static [u8]) -> Self {
        // TODO: 当 MSRV（最低支持 Rust 版本）升级到 >= 1.57.0 时，可以改为 const
        if find_invalid_byte(reason).is_some() {
            panic!("invalid byte in static reason phrase");
        }
        Self(Bytes::from_static(reason))
    }

    // 故意不设为公开方法。
    /// 直接从 `Bytes` 创建 `ReasonPhrase`，不进行有效性验证。
    ///
    /// **注意**：此方法跳过验证，仅供 crate 内部使用。
    /// 如果原因短语中包含无效字节（如换行符），
    /// 在响应中发送可能导致严重的安全问题（如 HTTP 响应拆分攻击）。
    ///
    /// 仅在客户端解析已接收的响应时使用——此时服务器发来的数据已经过
    /// HTTP/1 解析器的基本验证。
    #[cfg(feature = "client")]
    pub(crate) fn from_bytes_unchecked(reason: Bytes) -> Self {
        Self(reason)
    }
}

/// 实现从字节切片 `&[u8]` 到 `ReasonPhrase` 的可失败转换。
///
/// 验证所有字节是否符合 RFC 规范，不符合则返回 `InvalidReasonPhrase` 错误。
/// 使用 `Bytes::copy_from_slice` 将输入数据拷贝到新分配的缓冲区中。
impl TryFrom<&[u8]> for ReasonPhrase {
    type Error = InvalidReasonPhrase;

    fn try_from(reason: &[u8]) -> Result<Self, Self::Error> {
        if let Some(bad_byte) = find_invalid_byte(reason) {
            Err(InvalidReasonPhrase { bad_byte })
        } else {
            Ok(Self(Bytes::copy_from_slice(reason))) // 拷贝数据到 Bytes 中
        }
    }
}

/// 实现从 `Vec<u8>` 到 `ReasonPhrase` 的可失败转换。
///
/// 与 `&[u8]` 版本类似，但使用 `Bytes::from(Vec<u8>)` 可以复用
/// Vec 的堆分配，避免额外拷贝（零拷贝转换）。
impl TryFrom<Vec<u8>> for ReasonPhrase {
    type Error = InvalidReasonPhrase;

    fn try_from(reason: Vec<u8>) -> Result<Self, Self::Error> {
        if let Some(bad_byte) = find_invalid_byte(&reason) {
            Err(InvalidReasonPhrase { bad_byte })
        } else {
            Ok(Self(Bytes::from(reason))) // 零拷贝：复用 Vec 的内存分配
        }
    }
}

/// 实现从 `String` 到 `ReasonPhrase` 的可失败转换。
///
/// 先将 `String` 视为字节进行验证，通过后利用 `Bytes::from(String)`
/// 进行零拷贝转换。由于 `String` 保证是有效 UTF-8，这里的验证
/// 主要排除控制字符（如换行符 `\n`、回车符 `\r`）。
impl TryFrom<String> for ReasonPhrase {
    type Error = InvalidReasonPhrase;

    fn try_from(reason: String) -> Result<Self, Self::Error> {
        if let Some(bad_byte) = find_invalid_byte(reason.as_bytes()) {
            Err(InvalidReasonPhrase { bad_byte })
        } else {
            Ok(Self(Bytes::from(reason))) // 零拷贝：复用 String 的内存分配
        }
    }
}

/// 实现从 `Bytes` 到 `ReasonPhrase` 的可失败转换。
///
/// 直接验证 `Bytes` 中的内容，通过后将所有权转移到 `ReasonPhrase` 中，
/// 无需任何数据拷贝。
impl TryFrom<Bytes> for ReasonPhrase {
    type Error = InvalidReasonPhrase;

    fn try_from(reason: Bytes) -> Result<Self, Self::Error> {
        if let Some(bad_byte) = find_invalid_byte(&reason) {
            Err(InvalidReasonPhrase { bad_byte })
        } else {
            Ok(Self(reason)) // 直接转移所有权，零拷贝
        }
    }
}

/// 实现从 `ReasonPhrase` 到 `Bytes` 的转换。
///
/// 消耗 `ReasonPhrase` 并返回内部的 `Bytes`，允许用户在不需要
/// `ReasonPhrase` 类型包装时直接操作底层字节数据。
impl From<ReasonPhrase> for Bytes {
    fn from(reason: ReasonPhrase) -> Self {
        reason.0 // 解构新类型，返回内部 Bytes
    }
}

/// 实现 `AsRef<[u8]>` trait，允许将 `ReasonPhrase` 作为字节切片引用。
///
/// 这使得 `ReasonPhrase` 可以直接传递给接受 `AsRef<[u8]>` 的泛型函数，
/// 提高了与其他 API 的互操作性。
impl AsRef<[u8]> for ReasonPhrase {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// 构造 `ReasonPhrase` 时遇到无效字节的错误类型。
///
/// 包含导致验证失败的具体字节值，便于调试和错误报告。
///
/// 允许的字节范围参见 [HTTP 规范][spec]。
///
/// [spec]: https://httpwg.org/http-core/draft-ietf-httpbis-messaging-latest.html#rfc.section.4.p.7
#[derive(Debug)]
pub struct InvalidReasonPhrase {
    /// 导致验证失败的无效字节值
    bad_byte: u8,
}

/// 为 `InvalidReasonPhrase` 实现用户友好的显示格式。
impl std::fmt::Display for InvalidReasonPhrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid byte in reason phrase: {}", self.bad_byte)
    }
}

/// 实现标准错误 trait，使 `InvalidReasonPhrase` 可以与 `?` 操作符
/// 和其他错误处理机制配合使用。
impl std::error::Error for InvalidReasonPhrase {}

/// 检查单个字节是否为原因短语中的合法字节。
///
/// 根据 HTTP 规范，原因短语允许以下字节：
/// - HTAB（水平制表符，`\t`，0x09）
/// - SP（空格，` `，0x20）
/// - VCHAR（可见字符，0x21-0x7E）
/// - obs-text（遗留文本，0x80-0xFF）
///
/// 不允许的字节包括控制字符（0x00-0x08, 0x0A-0x1F）和 DEL（0x7F）。
/// 特别是 CR（`\r`）和 LF（`\n`）不允许出现，这对于防止 HTTP 响应拆分攻击至关重要。
///
/// 此函数为 `const fn`，可在编译期使用（如 `from_static` 方法中）。
const fn is_valid_byte(b: u8) -> bool {
    // 参见 https://www.rfc-editor.org/rfc/rfc5234.html#appendix-B.1
    /// 检查是否为 VCHAR（可见字符）：范围 0x21 到 0x7E
    const fn is_vchar(b: u8) -> bool {
        0x21 <= b && b <= 0x7E
    }

    // 参见 https://httpwg.org/http-core/draft-ietf-httpbis-semantics-latest.html#fields.values
    //
    // 0xFF 的比较在技术上是多余的（u8 最大值就是 0xFF），
    // 但保留它可以更清晰地对应规范文本，编译器会将其优化掉。
    #[allow(unused_comparisons, clippy::absurd_extreme_comparisons)]
    /// 检查是否为 obs-text（遗留文本）：范围 0x80 到 0xFF
    const fn is_obs_text(b: u8) -> bool {
        0x80 <= b && b <= 0xFF
    }

    // 参见 https://httpwg.org/http-core/draft-ietf-httpbis-messaging-latest.html#rfc.section.4.p.7
    // reason-phrase = *( HTAB / SP / VCHAR / obs-text )
    b == b'\t' || b == b' ' || is_vchar(b) || is_obs_text(b)
}

/// 在字节切片中查找第一个无效字节。
///
/// 遍历整个字节切片，返回遇到的第一个不符合原因短语规范的字节。
/// 如果所有字节都合法，返回 `None`。
///
/// 使用 `while` 循环而非迭代器，是因为 `const fn` 中不支持
/// `for` 循环和迭代器（截至当前 MSRV）。
const fn find_invalid_byte(bytes: &[u8]) -> Option<u8> {
    let mut i = 0;
    // const fn 中不能使用 for 循环，因此使用 while + 手动索引递增
    while i < bytes.len() {
        let b = bytes[i];
        if !is_valid_byte(b) {
            return Some(b); // 返回第一个无效字节
        }
        i += 1;
    }
    None // 所有字节均合法
}

// --- 单元测试 ---

/// `ReasonPhrase` 的单元测试模块。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试基本的有效原因短语 "OK"。
    #[test]
    fn basic_valid() {
        const PHRASE: &[u8] = b"OK";
        assert_eq!(ReasonPhrase::from_static(PHRASE).as_bytes(), PHRASE);
        assert_eq!(ReasonPhrase::try_from(PHRASE).unwrap().as_bytes(), PHRASE);
    }

    /// 测试空字符串作为有效的原因短语（规范允许零长度的原因短语）。
    #[test]
    fn empty_valid() {
        const PHRASE: &[u8] = b"";
        assert_eq!(ReasonPhrase::from_static(PHRASE).as_bytes(), PHRASE);
        assert_eq!(ReasonPhrase::try_from(PHRASE).unwrap().as_bytes(), PHRASE);
    }

    /// 测试包含 obs-text 字节（0x80-0xFF 范围）的有效原因短语。
    /// `\xe9` 是 Latin-1 编码中的 "e" 带锐音符。
    #[test]
    fn obs_text_valid() {
        const PHRASE: &[u8] = b"hyp\xe9r";
        assert_eq!(ReasonPhrase::from_static(PHRASE).as_bytes(), PHRASE);
        assert_eq!(ReasonPhrase::try_from(PHRASE).unwrap().as_bytes(), PHRASE);
    }

    /// 包含换行符 `\n` 的测试用原因短语（应被判定为无效）。
    const NEWLINE_PHRASE: &[u8] = b"hyp\ner";

    /// 测试 `from_static` 在遇到换行符时触发 panic（编译期验证）。
    #[test]
    #[should_panic]
    fn newline_invalid_panic() {
        ReasonPhrase::from_static(NEWLINE_PHRASE);
    }

    /// 测试 `try_from` 在遇到换行符时返回错误。
    #[test]
    fn newline_invalid_err() {
        assert!(ReasonPhrase::try_from(NEWLINE_PHRASE).is_err());
    }

    /// 包含回车符 `\r` 的测试用原因短语（应被判定为无效）。
    const CR_PHRASE: &[u8] = b"hyp\rer";

    /// 测试 `from_static` 在遇到回车符时触发 panic。
    #[test]
    #[should_panic]
    fn cr_invalid_panic() {
        ReasonPhrase::from_static(CR_PHRASE);
    }

    /// 测试 `try_from` 在遇到回车符时返回错误。
    #[test]
    fn cr_invalid_err() {
        assert!(ReasonPhrase::try_from(CR_PHRASE).is_err());
    }
}
