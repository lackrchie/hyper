//! HTTP 消息体解码长度模块
//!
//! 本模块定义了 `DecodedLength` 类型，用于统一表示 HTTP 消息体的各种长度状态。
//! 在 HTTP 协议中，消息体的长度可能是：
//!
//! - **精确已知的**：通过 `Content-Length` 头部指定
//! - **分块传输编码（chunked）**：通过 `Transfer-Encoding: chunked` 指定，长度在传输前未知
//! - **连接关闭分隔（close-delimited）**：在 HTTP/1.0 中，通过关闭连接来标识 body 结束
//!
//! `DecodedLength` 通过一个 `u64` 值和特殊的哨兵值（sentinel values）来编码这三种状态，
//! 这是一种紧凑的空间优化设计——仅占用 8 字节就能表示所有可能的长度状态。

// --- 标准库导入 ---

/// 导入格式化 trait，用于实现 `Debug` 和 `Display`
use std::fmt;

/// 解码后的 HTTP 消息体长度。
///
/// 本类型使用 `u64` 内部值来表示三种状态：
/// - `0..=(u64::MAX - 2)`：精确的字节长度（Content-Length）
/// - `u64::MAX - 1`：分块传输编码（CHUNKED），长度未知
/// - `u64::MAX`：连接关闭分隔（CLOSE_DELIMITED），长度未知
///
/// 这种设计利用了 `u64` 的最大值范围来编码特殊状态，
/// 避免了使用 `Option` 或枚举带来的额外内存开销。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedLength(u64);

/// 从 `Option<u64>` 转换为 `DecodedLength` 的实现。
///
/// - `Some(len)`：尝试创建精确长度，如果超出允许范围则回退为 `CHUNKED`
/// - `None`：返回 `CHUNKED`（长度未知）
#[cfg(any(feature = "http1", feature = "http2"))]
impl From<Option<u64>> for DecodedLength {
    fn from(len: Option<u64>) -> Self {
        len.and_then(|len| {
            // 如果长度恰好是 u64::MAX，它会与哨兵值冲突，所以视为分块编码
            Self::checked_new(len).ok()
        })
        .unwrap_or(DecodedLength::CHUNKED)
    }
}

/// Content-Length 允许的最大值。
///
/// 由于 `u64::MAX` 和 `u64::MAX - 1` 被用作哨兵值（分别表示 CLOSE_DELIMITED 和 CHUNKED），
/// 实际允许的最大精确长度为 `u64::MAX - 2`。
/// 对于实际应用而言，这个限制几乎不会被触及（约 18.4 EB）。
#[cfg(any(feature = "http1", feature = "http2", test))]
const MAX_LEN: u64 = u64::MAX - 2;

/// `DecodedLength` 的方法实现块
impl DecodedLength {
    /// 连接关闭分隔模式：body 的长度由关闭连接来确定（HTTP/1.0 风格）。
    /// 使用 `u64::MAX` 作为哨兵值。
    pub(crate) const CLOSE_DELIMITED: DecodedLength = DecodedLength(u64::MAX);
    /// 分块传输编码模式：body 使用 chunked 编码，长度在传输前未知。
    /// 使用 `u64::MAX - 1` 作为哨兵值。
    pub(crate) const CHUNKED: DecodedLength = DecodedLength(u64::MAX - 1);
    /// 零长度 body。
    pub(crate) const ZERO: DecodedLength = DecodedLength(0);

    /// 创建一个新的 `DecodedLength`（仅测试用）。
    ///
    /// 使用 `debug_assert!` 确保值不超过 `MAX_LEN`，
    /// 这意味着此断言仅在 debug 模式下生效。
    #[cfg(test)]
    pub(crate) fn new(len: u64) -> Self {
        debug_assert!(len <= MAX_LEN);
        DecodedLength(len)
    }

    /// 将内部值直接作为 content-length 返回，不做额外检查。
    ///
    /// **注意**：仅应在已确认当前值不是 `CLOSE_DELIMITED` 或 `CHUNKED` 后才调用此方法。
    /// 使用 `debug_assert!` 进行 debug 模式下的安全检查。
    #[inline]
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http1"))]
    pub(crate) fn danger_len(self) -> u64 {
        // 确保内部值小于 CHUNKED 的哨兵值，即为精确长度
        debug_assert!(self.0 < Self::CHUNKED.0);
        self.0
    }

    /// 转换为 `Option<u64>`，表示已知或未知的长度。
    ///
    /// - 精确长度返回 `Some(len)`
    /// - `CHUNKED` 或 `CLOSE_DELIMITED` 返回 `None`
    #[cfg(all(
        any(feature = "http1", feature = "http2"),
        any(feature = "client", feature = "server")
    ))]
    pub(crate) fn into_opt(self) -> Option<u64> {
        match self {
            // 利用 Rust 的常量模式匹配来检查哨兵值
            DecodedLength::CHUNKED | DecodedLength::CLOSE_DELIMITED => None,
            DecodedLength(known) => Some(known),
        }
    }

    /// 检查 `u64` 值是否在 content-length 允许的范围内。
    ///
    /// 如果超出 `MAX_LEN`，记录警告日志并返回 `TooLarge` 解析错误。
    #[cfg(any(feature = "http1", feature = "http2"))]
    pub(crate) fn checked_new(len: u64) -> Result<Self, crate::error::Parse> {
        if len <= MAX_LEN {
            Ok(DecodedLength(len))
        } else {
            warn!("content-length bigger than maximum: {} > {}", len, MAX_LEN);
            Err(crate::error::Parse::TooLarge)
        }
    }

    /// 从剩余长度中减去已接收的字节数。
    ///
    /// 如果当前是 `CHUNKED` 或 `CLOSE_DELIMITED`（长度未知），则不做任何操作。
    /// 如果是精确长度，则从中减去 `amt`。
    ///
    /// 注意：这里没有下溢检查——在 debug 模式下 Rust 会 panic，
    /// 但在 release 模式下会发生 wrapping 行为。
    /// hyper 依赖上层逻辑确保不会减去超过剩余量的值。
    #[cfg(all(
        any(feature = "http1", feature = "http2"),
        any(feature = "client", feature = "server")
    ))]
    pub(crate) fn sub_if(&mut self, amt: u64) {
        match *self {
            // 对于未知长度的模式，不做任何操作
            DecodedLength::CHUNKED | DecodedLength::CLOSE_DELIMITED => (),
            DecodedLength(ref mut known) => {
                *known -= amt;
            }
        }
    }

    /// 判断当前是否表示精确长度。
    ///
    /// 返回 `true` 如果内部值在 `0..=MAX_LEN` 范围内（包括 0）。
    /// 对于 `CHUNKED` 或 `CLOSE_DELIMITED` 返回 `false`。
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(crate) fn is_exact(&self) -> bool {
        self.0 <= MAX_LEN
    }
}

/// 为 `DecodedLength` 实现 `Debug` trait。
///
/// 对于哨兵值显示可读的名称（如 "CLOSE_DELIMITED"、"CHUNKED"），
/// 对于精确长度显示具体数值。
impl fmt::Debug for DecodedLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DecodedLength::CLOSE_DELIMITED => f.write_str("CLOSE_DELIMITED"),
            DecodedLength::CHUNKED => f.write_str("CHUNKED"),
            DecodedLength(n) => f.debug_tuple("DecodedLength").field(&n).finish(),
        }
    }
}

/// 为 `DecodedLength` 实现 `Display` trait。
///
/// 提供面向用户的友好格式化输出，适用于日志和错误消息。
impl fmt::Display for DecodedLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DecodedLength::CLOSE_DELIMITED => f.write_str("close-delimited"),
            DecodedLength::CHUNKED => f.write_str("chunked encoding"),
            DecodedLength::ZERO => f.write_str("empty"),
            DecodedLength(n) => write!(f, "content-length ({} bytes)", n),
        }
    }
}

// ========== 测试模块 ==========

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试对精确长度执行 sub_if 操作
    #[test]
    fn sub_if_known() {
        let mut len = DecodedLength::new(30);
        len.sub_if(20);

        assert_eq!(len.0, 10);
    }

    /// 测试对 CHUNKED 模式执行 sub_if 操作——应保持不变
    #[test]
    fn sub_if_chunked() {
        let mut len = DecodedLength::CHUNKED;
        len.sub_if(20);

        // CHUNKED 模式下 sub_if 不做任何操作
        assert_eq!(len, DecodedLength::CHUNKED);
    }
}
