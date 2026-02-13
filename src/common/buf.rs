//! 缓冲区列表（BufList）模块。
//!
//! 本模块提供 `BufList<T>` 类型，它是一个基于 `VecDeque` 的多段缓冲区容器，
//! 实现了 `bytes::Buf` trait。在 hyper 的 HTTP/1 协议处理中，响应体数据
//! 可能由多个不连续的内存块组成，`BufList` 将这些块串联起来，对外呈现为
//! 一个统一的连续字节缓冲区，支持高效的零拷贝读取和向量化 I/O（vectored I/O）。
//!
//! 这种设计避免了将多个小缓冲区合并为一个大缓冲区所带来的内存拷贝开销，
//! 在网络 I/O 场景下能显著提升性能。

// 标准库的双端队列，用于存储多个缓冲区块，支持高效的头部弹出操作
use std::collections::VecDeque;
// 用于向量化 I/O（writev/readv），可一次性写入多个不连续的内存块
use std::io::IoSlice;

// bytes crate 提供的核心缓冲区抽象：
// - `Buf`: 只读缓冲区 trait，定义了 remaining/chunk/advance 等方法
// - `BufMut`: 可写缓冲区 trait
// - `Bytes`: 引用计数的不可变字节容器，支持零拷贝切片
// - `BytesMut`: 可变字节容器，可通过 freeze() 转为 Bytes
use bytes::{Buf, BufMut, Bytes, BytesMut};

/// 缓冲区列表，将多个实现了 `Buf` trait 的缓冲区块组织为一个逻辑上连续的缓冲区。
///
/// `BufList` 内部使用 `VecDeque<T>` 存储各个缓冲区块，按 FIFO 顺序消费数据。
/// 当前端缓冲区的数据被完全读取后，自动弹出并切换到下一个缓冲区。
///
/// # 类型参数
/// - `T`: 实现了 `Buf` trait 的缓冲区类型（如 `Bytes`、`&[u8]` 等）
pub(crate) struct BufList<T> {
    /// 内部双端队列，存储所有待读取的缓冲区块
    bufs: VecDeque<T>,
}

/// `BufList` 的基本操作实现。
impl<T: Buf> BufList<T> {
    /// 创建一个空的 `BufList`。
    pub(crate) fn new() -> BufList<T> {
        BufList {
            bufs: VecDeque::new(),
        }
    }

    /// 将一个缓冲区块追加到列表末尾。
    ///
    /// # Panics
    /// 在 debug 模式下，如果传入的缓冲区没有剩余数据（`has_remaining()` 为 false），
    /// 则会触发断言失败。这是一种防御性编程，避免插入空缓冲区导致后续逻辑异常。
    #[inline]
    pub(crate) fn push(&mut self, buf: T) {
        debug_assert!(buf.has_remaining());
        self.bufs.push_back(buf);
    }

    /// 返回当前列表中的缓冲区块数量。
    ///
    /// 该方法可用于监控缓冲区碎片化程度或做流量控制判断。
    #[inline]
    pub(crate) fn bufs_cnt(&self) -> usize {
        self.bufs.len()
    }
}

/// 为 `BufList<T>` 实现 `Buf` trait，使其可作为统一的只读缓冲区使用。
///
/// 这是该模块的核心实现：将多个分散的缓冲区块抽象为一个连续的字节流，
/// 使得上层代码无需关心底层数据的物理分布。
impl<T: Buf> Buf for BufList<T> {
    /// 返回所有缓冲区块中剩余可读字节的总数。
    ///
    /// 遍历所有缓冲区块，累加各自的 `remaining()` 值。
    #[inline]
    fn remaining(&self) -> usize {
        self.bufs.iter().map(|buf| buf.remaining()).sum()
    }

    /// 返回当前第一个缓冲区块的连续可读字节切片。
    ///
    /// 如果列表为空，返回空切片（通过 `unwrap_or_default()`）。
    /// 注意：此方法仅返回第一个块的数据，不会跨块合并。
    #[inline]
    fn chunk(&self) -> &[u8] {
        self.bufs.front().map(Buf::chunk).unwrap_or_default()
    }

    /// 向前推进读取位置 `cnt` 个字节。
    ///
    /// 实现逻辑：从队列前端开始消费数据，如果当前前端缓冲区的剩余数据
    /// 大于 `cnt`，则仅推进该缓冲区；否则消费完该缓冲区后弹出，继续
    /// 处理下一个，直到推进了 `cnt` 个字节。
    #[inline]
    fn advance(&mut self, mut cnt: usize) {
        while cnt > 0 {
            {
                // 获取队列前端缓冲区的可变引用
                let front = &mut self.bufs[0];
                let rem = front.remaining();
                if rem > cnt {
                    // 前端缓冲区数据足够，直接推进并返回
                    front.advance(cnt);
                    return;
                } else {
                    // 前端缓冲区数据不足或刚好，先消费完再继续
                    front.advance(rem);
                    cnt -= rem;
                }
            }
            // 前端缓冲区已消费完毕，从队列中弹出
            self.bufs.pop_front();
        }
    }

    /// 将缓冲区列表中的数据填充到多个 `IoSlice` 中，用于向量化 I/O。
    ///
    /// 向量化 I/O（vectored I/O）允许单次系统调用写入多个不连续的内存块，
    /// 这比逐块写入效率更高。该方法将各缓冲区块的数据分别映射到 `dst` 的各个槽位中。
    ///
    /// 返回实际填充的 `IoSlice` 数量。
    #[inline]
    fn chunks_vectored<'t>(&'t self, dst: &mut [IoSlice<'t>]) -> usize {
        if dst.is_empty() {
            return 0;
        }
        let mut vecs = 0;
        for buf in &self.bufs {
            // 每个缓冲区块可能贡献一个或多个 IoSlice
            vecs += buf.chunks_vectored(&mut dst[vecs..]);
            if vecs == dst.len() {
                // dst 已填满，无需继续
                break;
            }
        }
        vecs
    }

    /// 从缓冲区列表中拷贝 `len` 个字节并返回 `Bytes`。
    ///
    /// 此方法会尽量利用内部缓冲区的优化实现（如 `Bytes` 的零拷贝切片）：
    /// - 如果第一个缓冲区块的数据恰好等于 `len`，直接取出并弹出该块
    /// - 如果第一个缓冲区块的数据多于 `len`，从中切出 `len` 字节
    /// - 否则需要跨块拷贝，分配新的 `BytesMut` 并从多个块中聚合数据
    #[inline]
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        // Our inner buffer may have an optimized version of copy_to_bytes, and if the whole
        // request can be fulfilled by the front buffer, we can take advantage.
        match self.bufs.front_mut() {
            // 第一个块的剩余数据恰好等于请求长度，零拷贝取出
            Some(front) if front.remaining() == len => {
                let b = front.copy_to_bytes(len);
                self.bufs.pop_front();
                b
            }
            // 第一个块的剩余数据多于请求长度，从中切片
            Some(front) if front.remaining() > len => front.copy_to_bytes(len),
            _ => {
                // 需要跨多个块聚合数据，先断言长度合法
                assert!(len <= self.remaining(), "`len` greater than remaining");
                // 分配足够容量的 BytesMut，通过 Buf::take 限制读取长度后拷贝
                let mut bm = BytesMut::with_capacity(len);
                bm.put(self.take(len));
                // freeze() 将可变的 BytesMut 转为不可变的 Bytes
                bm.freeze()
            }
        }
    }
}

/// 单元测试模块
#[cfg(test)]
mod tests {
    // 用于指针比较，验证零拷贝行为
    use std::ptr;

    use super::*;

    /// 创建一个包含 "Hello World" 三段数据的测试用 BufList
    fn hello_world_buf() -> BufList<Bytes> {
        BufList {
            bufs: vec![Bytes::from("Hello"), Bytes::from(" "), Bytes::from("World")].into(),
        }
    }

    /// 测试：请求的字节数小于第一个缓冲区块的长度
    /// 验证零拷贝切片行为——返回的 Bytes 应指向原始内存
    #[test]
    fn to_bytes_shorter() {
        let mut bufs = hello_world_buf();
        let old_ptr = bufs.chunk().as_ptr();
        let start = bufs.copy_to_bytes(4);
        assert_eq!(start, "Hell");
        // 验证返回的 Bytes 与原始缓冲区共享同一内存（零拷贝）
        assert!(ptr::eq(old_ptr, start.as_ptr()));
        assert_eq!(bufs.chunk(), b"o");
        // 验证剩余部分的指针也正确偏移
        assert!(ptr::eq(old_ptr.wrapping_add(4), bufs.chunk().as_ptr()));
        assert_eq!(bufs.remaining(), 7);
    }

    /// 测试：请求的字节数恰好等于第一个缓冲区块的长度
    /// 验证整块取出并弹出的行为
    #[test]
    fn to_bytes_eq() {
        let mut bufs = hello_world_buf();
        let old_ptr = bufs.chunk().as_ptr();
        let start = bufs.copy_to_bytes(5);
        assert_eq!(start, "Hello");
        assert!(ptr::eq(old_ptr, start.as_ptr()));
        assert_eq!(bufs.chunk(), b" ");
        assert_eq!(bufs.remaining(), 6);
    }

    /// 测试：请求的字节数跨越多个缓冲区块
    /// 此情况下需要分配新内存并拷贝数据
    #[test]
    fn to_bytes_longer() {
        let mut bufs = hello_world_buf();
        let start = bufs.copy_to_bytes(7);
        assert_eq!(start, "Hello W");
        assert_eq!(bufs.remaining(), 4);
    }

    /// 测试：只有一个缓冲区块时的 copy_to_bytes 行为
    #[test]
    fn one_long_buf_to_bytes() {
        let mut buf = BufList::new();
        buf.push(b"Hello World" as &[_]);
        assert_eq!(buf.copy_to_bytes(5), "Hello");
        assert_eq!(buf.chunk(), b" World");
    }

    /// 测试：请求的字节数超过剩余数据量时应触发 panic
    #[test]
    #[should_panic(expected = "`len` greater than remaining")]
    fn buf_to_bytes_too_many() {
        hello_world_buf().copy_to_bytes(42);
    }
}
