//! HTTP 日期格式化与缓存模块。
//!
//! 本模块实现了 HTTP 响应中 `Date` 头部值的高效生成与缓存机制。
//! 根据 RFC 7231 规定，HTTP 服务器应在每个响应中包含 `Date` 头部，
//! 格式为 IMF-fixdate（例如 `"Sun, 06 Nov 1994 08:49:37 GMT"`）。
//!
//! 由于日期字符串每秒才需要更新一次，本模块采用线程局部缓存（`thread_local!`）策略，
//! 将格式化后的日期字符串缓存在固定大小的字节数组中。每次获取时仅检查是否需要更新，
//! 避免了每个响应都重新格式化日期字符串带来的性能开销。
//!
//! 在高并发场景下，这种设计可以显著减少字符串格式化和内存分配的次数。

// `RefCell` 提供内部可变性，配合 `thread_local!` 实现线程安全的可变缓存
use std::cell::RefCell;
// `fmt::Write` trait 允许将格式化输出写入自定义目标（这里是固定大小的字节数组）
use std::fmt::{self, Write};
// `str` 模块用于字符串处理（虽然此处未直接使用，但被编译器隐式需要）
use std::str;
// `Duration` 用于计算下次更新时间，`SystemTime` 和 `UNIX_EPOCH` 用于获取当前时间
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// HTTP/2 场景下需要将日期直接作为 `HeaderValue` 缓存
#[cfg(feature = "http2")]
use http::header::HeaderValue;
// `httpdate` crate 提供符合 HTTP 规范的日期格式化实现
use httpdate::HttpDate;

// HTTP 日期字符串的固定长度（"Sun, 06 Nov 1994 08:49:37 GMT" 恰好 29 字节）
pub(crate) const DATE_VALUE_LENGTH: usize = 29;

/// 将当前缓存的日期字符串追加到目标字节向量中。
///
/// 该函数用于 HTTP/1 响应头的序列化，直接将日期字节拷贝到输出缓冲区。
/// 通过 `thread_local!` 访问当前线程的缓存实例，避免锁竞争。
#[cfg(feature = "http1")]
pub(crate) fn extend(dst: &mut Vec<u8>) {
    CACHED.with(|cache| {
        dst.extend_from_slice(cache.borrow().buffer());
    })
}

/// 检查并更新当前线程的日期缓存。
///
/// 如果当前时间已超过下次更新时间点，则重新格式化日期字符串。
/// 该函数通常在处理请求的开始阶段调用，确保后续的日期输出是最新的。
#[cfg(feature = "http1")]
pub(crate) fn update() {
    CACHED.with(|cache| {
        cache.borrow_mut().check();
    })
}

/// 检查并更新日期缓存，同时返回用于 HTTP/2 的 `HeaderValue`。
///
/// HTTP/2 使用 HPACK 头部压缩，需要 `HeaderValue` 类型而非原始字节。
/// 此方法同时完成缓存更新和 `HeaderValue` 克隆，减少重复操作。
#[cfg(feature = "http2")]
pub(crate) fn update_and_header_value() -> HeaderValue {
    CACHED.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.check();
        cache.header_value.clone()
    })
}

/// 缓存的日期数据结构。
///
/// 使用固定大小的字节数组存储格式化后的日期字符串，避免堆分配。
/// `pos` 字段用于在格式化写入时追踪当前写入位置。
struct CachedDate {
    /// 存储格式化后的日期字符串的固定大小字节数组
    bytes: [u8; DATE_VALUE_LENGTH],
    /// 当前写入位置，在 `render` 时从 0 开始递增
    pos: usize,
    /// HTTP/2 专用：预格式化的 HeaderValue，避免每次请求时重复构造
    #[cfg(feature = "http2")]
    header_value: HeaderValue,
    /// 下次需要更新缓存的时间点（精确到秒边界）
    next_update: SystemTime,
}

// 线程局部存储的缓存实例。
// 使用 `thread_local!` 而非全局锁，每个线程拥有独立的缓存副本，
// 完全消除了多线程竞争，这在高并发 HTTP 服务器中至关重要。
thread_local!(static CACHED: RefCell<CachedDate> = RefCell::new(CachedDate::new()));

/// `CachedDate` 的方法实现。
impl CachedDate {
    /// 创建并初始化一个新的 `CachedDate` 实例。
    ///
    /// 构造时立即执行一次更新，确保缓存中有有效的日期数据。
    fn new() -> Self {
        let mut cache = CachedDate {
            bytes: [0; DATE_VALUE_LENGTH],
            pos: 0,
            #[cfg(feature = "http2")]
            header_value: HeaderValue::from_static(""),
            next_update: SystemTime::now(),
        };
        // 立即执行首次更新，使缓存就绪
        cache.update(cache.next_update);
        cache
    }

    /// 返回缓存的日期字符串的字节切片引用。
    fn buffer(&self) -> &[u8] {
        &self.bytes[..]
    }

    /// 检查缓存是否过期，如过期则更新。
    ///
    /// 仅在当前时间超过 `next_update` 时才执行格式化操作，
    /// 大部分调用只需一次时间比较即可返回，开销极小。
    fn check(&mut self) {
        let now = SystemTime::now();
        if now > self.next_update {
            self.update(now);
        }
    }

    /// 更新缓存的日期字符串和下次更新时间。
    ///
    /// 计算当前时间的亚秒偏移量，将下次更新时间对齐到下一秒的整秒边界，
    /// 确保在整秒切换时及时更新日期字符串。
    fn update(&mut self, now: SystemTime) {
        // 获取当前时间的纳秒部分（亚秒偏移）
        let nanos = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();

        self.render(now);
        // 下次更新时间 = 当前时间 + 1秒 - 亚秒偏移，即对齐到下一个整秒边界
        self.next_update = now + Duration::new(1, 0) - Duration::from_nanos(nanos as u64);
    }

    /// 将日期格式化为 HTTP 规范的字符串并写入字节数组。
    ///
    /// 通过实现 `fmt::Write` trait，利用 `write!` 宏将 `HttpDate` 格式化输出
    /// 直接写入内部字节数组，避免了中间字符串分配。
    fn render(&mut self, now: SystemTime) {
        // 重置写入位置
        self.pos = 0;
        // 利用 fmt::Write trait 将格式化结果直接写入 self.bytes
        let _ = write!(self, "{}", HttpDate::from(now));
        // 确保写入的字节数恰好等于预期长度
        debug_assert!(self.pos == DATE_VALUE_LENGTH);
        // 同步更新 HTTP/2 的 HeaderValue 缓存
        self.render_http2();
    }

    /// HTTP/2 场景下，将字节数组转换为 `HeaderValue` 并缓存。
    #[cfg(feature = "http2")]
    fn render_http2(&mut self) {
        self.header_value = HeaderValue::from_bytes(self.buffer())
            .expect("Date format should be valid HeaderValue");
    }

    /// 非 HTTP/2 场景下的空实现（编译期消除）。
    #[cfg(not(feature = "http2"))]
    fn render_http2(&mut self) {}
}

/// 为 `CachedDate` 实现 `fmt::Write` trait。
///
/// 这使得 `write!` 宏可以直接将格式化输出写入 `CachedDate` 内部的固定大小字节数组，
/// 无需经过中间 `String` 分配。这是一种常见的零分配格式化技巧。
impl fmt::Write for CachedDate {
    /// 将字符串切片写入内部字节数组的当前位置。
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let len = s.len();
        // 将字符串字节直接拷贝到目标位置
        self.bytes[self.pos..self.pos + len].copy_from_slice(s.as_bytes());
        self.pos += len;
        Ok(())
    }
}

/// 单元测试模块
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "nightly")]
    use test::Bencher;

    /// 验证 DATE_VALUE_LENGTH 常量与实际日期字符串长度一致
    #[test]
    fn test_date_len() {
        assert_eq!(DATE_VALUE_LENGTH, "Sun, 06 Nov 1994 08:49:37 GMT".len());
    }

    /// 基准测试：检查缓存是否需要更新的性能
    /// 大部分情况下 check() 只是一次时间比较，应极快
    #[cfg(feature = "nightly")]
    #[bench]
    fn bench_date_check(b: &mut Bencher) {
        let mut date = CachedDate::new();
        // cache the first update
        date.check();

        b.iter(|| {
            date.check();
        });
    }

    /// 基准测试：日期格式化渲染的性能
    #[cfg(feature = "nightly")]
    #[bench]
    fn bench_date_render(b: &mut Bencher) {
        let mut date = CachedDate::new();
        let now = SystemTime::now();
        date.render(now);
        b.bytes = date.buffer().len() as u64;

        b.iter(|| {
            date.render(now);
            test::black_box(&date);
        });
    }
}
