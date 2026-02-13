//! 可回绕读取的 IO 包装器模块。
//!
//! 本模块提供 `Rewind<T>` 类型，它将一个 IO 流与一个预读缓冲区组合在一起。
//! 读取操作会优先从预读缓冲区中消费数据，缓冲区耗尽后才从底层 IO 流中读取。
//!
//! 这种设计在 HTTP 协议处理中非常有用：
//! - **协议探测**：先读取几个字节判断是 HTTP/1 还是 HTTP/2 协议，
//!   然后将这些字节"放回"，交给正确的协议解析器从头解析
//! - **HTTP 升级**：在 HTTP 升级握手完成后，可能已经读取了部分新协议的数据，
//!   需要将这些数据回绕后交给新的协议处理器
//! - **连接复用**：在 HTTP/1 keep-alive 连接中，可能在读取完一个请求后
//!   已经读到了下一个请求的部分数据

// `Pin` 用于固定指针（异步 IO trait 要求 `self: Pin<&mut Self>`）
use std::pin::Pin;
// `Context` 携带异步任务的 Waker，`Poll` 表示异步操作的就绪状态
use std::task::{Context, Poll};
// `cmp` 用于取最小值，`io` 提供 I/O 错误类型和 IoSlice
use std::{cmp, io};

// `Buf` trait 提供 advance 等缓冲区操作方法，`Bytes` 是引用计数的不可变字节容器
use bytes::{Buf, Bytes};

// hyper 自定义的异步 IO trait
use crate::rt::{Read, ReadBufCursor, Write};

/// 可回绕的 IO 包装器，支持将预读数据前置于底层 IO 流。
///
/// `Rewind` 在底层 IO 流 `T` 之上维护一个可选的预读缓冲区 `pre`。
/// 读取时优先消费 `pre` 中的数据，消费完毕后透传到底层 IO 流。
/// 写入操作始终直接委托给底层 IO 流。
///
/// # 类型参数
/// - `T`: 底层 IO 流类型，需实现 `Read + Write + Unpin`
#[derive(Debug)]
pub(crate) struct Rewind<T> {
    /// 可选的预读缓冲区。`Some(bytes)` 表示有待回绕的数据，`None` 表示无预读数据
    pre: Option<Bytes>,
    /// 底层 IO 流
    inner: T,
}

/// `Rewind` 的构造和辅助方法。
impl<T> Rewind<T> {
    /// 创建一个没有预读数据的 `Rewind` 包装器（仅测试使用）。
    ///
    /// 通过 `#[cfg]` 限制仅在测试环境中可用，因为生产代码通常使用 `new_buffered`。
    #[cfg(all(
        test,
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(crate) fn new(io: T) -> Self {
        Rewind {
            pre: None,
            inner: io,
        }
    }

    /// 创建一个带有预读缓冲区的 `Rewind` 包装器。
    ///
    /// `buf` 中的数据将在后续读取操作中被优先消费。
    pub(crate) fn new_buffered(io: T, buf: Bytes) -> Self {
        Rewind {
            pre: Some(buf),
            inner: io,
        }
    }

    /// 设置回绕数据（仅测试使用）。
    ///
    /// 将 `bs` 设置为预读缓冲区，后续读取将优先返回这些数据。
    /// 使用 `debug_assert!` 确保调用时没有残留的预读数据。
    #[cfg(all(
        test,
        any(feature = "client", feature = "server"),
        any(feature = "http1", feature = "http2")
    ))]
    pub(crate) fn rewind(&mut self, bs: Bytes) {
        debug_assert!(self.pre.is_none());
        self.pre = Some(bs);
    }

    /// 消费 `Rewind`，返回底层 IO 流和剩余的预读数据。
    ///
    /// 如果没有残留的预读数据，`Bytes` 将为空（通过 `unwrap_or_default()`）。
    /// 这在协议升级或连接移交时使用，将底层 IO 和未消费的数据一起传递给新的处理器。
    pub(crate) fn into_inner(self) -> (T, Bytes) {
        (self.inner, self.pre.unwrap_or_default())
    }

    // pub(crate) fn get_mut(&mut self) -> &mut T {
    //     &mut self.inner
    // }
}

/// 为 `Rewind<T>` 实现 hyper 的 `Read` trait。
///
/// 读取时优先从预读缓冲区 `pre` 消费数据。如果预读缓冲区中有数据，
/// 尽可能多地拷贝到调用者提供的缓冲区中；如果预读缓冲区不足以填满
/// 调用者的缓冲区，则保留剩余的预读数据供下次读取。
/// 预读缓冲区消费完毕后，后续读取直接委托给底层 IO 流。
impl<T> Read for Rewind<T>
where
    T: Read + Unpin,
{
    /// 异步读取数据。
    ///
    /// # 实现逻辑
    /// 1. 使用 `self.pre.take()` 取出预读缓冲区（如有）
    /// 2. 如果预读缓冲区非空，计算可拷贝的字节数（取缓冲区和目标空间的最小值）
    /// 3. 拷贝数据到 `buf`，推进预读缓冲区的读取位置
    /// 4. 如果预读缓冲区还有剩余数据，放回 `self.pre`
    /// 5. 如果预读缓冲区已空或不存在，委托给底层 IO 流
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // 取出预读缓冲区（Option::take 将 self.pre 置为 None 并返回原值）
        if let Some(mut prefix) = self.pre.take() {
            // If there are no remaining bytes, let the bytes get dropped.
            // 如果预读缓冲区非空，则优先提供预读数据
            if !prefix.is_empty() {
                // 取预读数据长度和目标缓冲区剩余空间的最小值
                let copy_len = cmp::min(prefix.len(), buf.remaining());
                // TODO: There should be a way to do following two lines cleaner...
                // 将预读数据拷贝到目标缓冲区
                buf.put_slice(&prefix[..copy_len]);
                // 推进预读缓冲区的读取位置
                prefix.advance(copy_len);
                // Put back what's left
                // 如果预读缓冲区还有剩余数据，放回 self.pre 供下次读取
                if !prefix.is_empty() {
                    self.pre = Some(prefix);
                }

                return Poll::Ready(Ok(()));
            }
        }
        // 预读缓冲区已空或不存在，委托给底层 IO 流
        // 由于 T: Unpin，可以安全地用 Pin::new 创建 Pin<&mut T>
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// 为 `Rewind<T>` 实现 hyper 的 `Write` trait。
///
/// 所有写入操作直接委托给底层 IO 流 `inner`，预读缓冲区仅影响读取路径。
impl<T> Write for Rewind<T>
where
    T: Write + Unpin,
{
    /// 写入数据，直接委托给底层 IO 流。
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    /// 向量化写入，直接委托给底层 IO 流。
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    /// 刷新缓冲区，直接委托给底层 IO 流。
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    /// 关闭写入端，直接委托给底层 IO 流。
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    /// 检查底层 IO 流是否支持向量化写入。
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// 单元测试模块
#[cfg(all(
    any(feature = "client", feature = "server"),
    any(feature = "http1", feature = "http2"),
))]
#[cfg(test)]
mod tests {
    // 导入 Compat 适配器，用于将 Rewind 包装为 tokio 兼容的 IO 类型
    use super::super::Compat;
    use super::Rewind;
    use bytes::Bytes;
    // tokio 的 AsyncReadExt 提供 read_exact 等便捷方法
    use tokio::io::AsyncReadExt;

    /// 测试部分回绕：先读取部分数据，回绕后重新读取完整数据。
    ///
    /// 验证流程：
    /// 1. 创建包含 "hello" 的 mock IO
    /// 2. 读取前 2 字节 ("he")
    /// 3. 将读取的 2 字节回绕
    /// 4. 重新读取全部 5 字节，验证得到完整的 "hello"
    #[cfg(not(miri))]
    #[tokio::test]
    async fn partial_rewind() {
        let underlying = [104, 101, 108, 108, 111];

        let mock = tokio_test::io::Builder::new().read(&underlying).build();

        // 多层包装：Mock -> Compat(反向适配) -> Rewind -> Compat(正向适配)
        let mut stream = Compat::new(Rewind::new(Compat::new(mock)));

        // Read off some bytes, ensure we filled o1
        let mut buf = [0; 2];
        stream.read_exact(&mut buf).await.expect("read1");

        // Rewind the stream so that it is as if we never read in the first place.
        // 将已读取的字节回绕，使得下次读取从头开始
        stream.0.rewind(Bytes::copy_from_slice(&buf[..]));

        let mut buf = [0; 5];
        stream.read_exact(&mut buf).await.expect("read1");

        // At this point we should have read everything that was in the MockStream
        // 验证回绕后读取到的数据包含回绕的字节 + 底层流的后续字节
        assert_eq!(&buf, &underlying);
    }

    /// 测试完整回绕：读取全部数据后回绕，重新读取验证一致性。
    ///
    /// 验证流程：
    /// 1. 创建包含 "hello" 的 mock IO
    /// 2. 读取全部 5 字节
    /// 3. 将全部 5 字节回绕
    /// 4. 重新读取全部 5 字节，验证数据完全一致
    #[cfg(not(miri))]
    #[tokio::test]
    async fn full_rewind() {
        let underlying = [104, 101, 108, 108, 111];

        let mock = tokio_test::io::Builder::new().read(&underlying).build();

        let mut stream = Compat::new(Rewind::new(Compat::new(mock)));

        let mut buf = [0; 5];
        stream.read_exact(&mut buf).await.expect("read1");

        // Rewind the stream so that it is as if we never read in the first place.
        stream.0.rewind(Bytes::copy_from_slice(&buf[..]));

        let mut buf = [0; 5];
        stream.read_exact(&mut buf).await.expect("read1");

        assert_eq!(&buf, &underlying);
    }
}
