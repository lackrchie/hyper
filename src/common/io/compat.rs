//! IO trait 兼容适配器模块。
//!
//! 本模块提供 `Compat<T>` 包装器，在 hyper 自定义的 IO trait（`crate::rt::Read` /
//! `crate::rt::Write`）与 tokio 的 IO trait（`tokio::io::AsyncRead` /
//! `tokio::io::AsyncWrite`）之间进行双向适配。
//!
//! hyper 定义了自己的 IO trait 以避免对 tokio 的硬依赖，但 `h2` crate（HTTP/2 实现）
//! 要求使用 tokio 的 IO trait。`Compat` 通过实现两套 trait 来桥接这一差异：
//! - 正向适配（生产代码）：将实现了 `crate::rt::Read/Write` 的类型适配为 `tokio::io::AsyncRead/AsyncWrite`
//! - 反向适配（仅测试）：将实现了 `tokio::io::AsyncRead/AsyncWrite` 的类型适配为 `crate::rt::Read/Write`
//!
//! 反向适配仅在测试中使用，用于将 tokio 的 mock IO 包装为 hyper 的 IO 类型进行测试。

// `Pin` 用于固定指针，异步 IO trait 的方法签名要求 `self: Pin<&mut Self>`
use std::pin::Pin;
// `Context` 携带异步任务的 Waker，`Poll` 表示异步操作的就绪状态
use std::task::{Context, Poll};

/// hyper IO trait 到 tokio IO trait 的兼容适配器。
///
/// 这是一个简单的 newtype 包装器（元组结构体），将内部类型 `T` 的 IO trait
/// 实现适配为另一套 IO trait。
///
/// # 使用场景
/// 主要用于 HTTP/2 连接，将 hyper 的 IO 类型传递给 h2 crate。
/// 在内部单元测试中，也用于反向适配，将 tokio 的 mock IO 类型用于测试 hyper 的 IO 逻辑。
#[derive(Debug)]
pub(crate) struct Compat<T>(pub(crate) T);

/// `Compat` 的基本方法。
impl<T> Compat<T> {
    /// 创建一个新的 `Compat` 包装器。
    pub(crate) fn new(io: T) -> Self {
        Compat(io)
    }

    /// 将 `Pin<&mut Compat<T>>` 投影为 `Pin<&mut T>`。
    ///
    /// 这是一个手动实现的 Pin 投影，将外层包装器的 Pin 传递到内部类型。
    /// 由于 `Compat` 只是一个简单的 newtype 包装器，不包含任何自引用结构，
    /// 因此这种投影是安全的。
    fn p(self: Pin<&mut Self>) -> Pin<&mut T> {
        // SAFETY: The simplest of projections. This is just
        // a wrapper, we don't do anything that would undo the projection.
        // SAFETY：最简单的投影。Compat 只是一个包装器，不会破坏 Pin 的不变量。
        unsafe { self.map_unchecked_mut(|me| &mut me.0) }
    }
}

/// 正向适配：将 `crate::rt::Read` 适配为 `tokio::io::AsyncRead`。
///
/// 当内部类型 `T` 实现了 hyper 的 `Read` trait 时，`Compat<T>` 自动实现
/// tokio 的 `AsyncRead` trait，使其可以被 h2 等依赖 tokio IO 的 crate 使用。
impl<T> tokio::io::AsyncRead for Compat<T>
where
    T: crate::rt::Read,
{
    /// 将 hyper 的 `poll_read` 调用适配为 tokio 的 `poll_read`。
    ///
    /// 两者的主要区别在于缓冲区类型不同：
    /// - tokio 使用 `tokio::io::ReadBuf`
    /// - hyper 使用 `crate::rt::ReadBuf` / `ReadBufCursor`
    ///
    /// 该方法在两种缓冲区之间进行转换，确保已初始化和已填充的字节计数正确同步。
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // 记录调用前的已初始化和已填充字节数
        let init = tbuf.initialized().len();
        let filled = tbuf.filled().len();
        let (new_init, new_filled) = unsafe {
            // 将 tokio 的 ReadBuf 底层缓冲区转换为 hyper 的 ReadBuf
            let mut buf = crate::rt::ReadBuf::uninit(tbuf.inner_mut());
            // 恢复之前的初始化和填充状态
            buf.set_init(init);
            buf.set_filled(filled);

            // 调用 hyper 的 poll_read 方法
            match crate::rt::Read::poll_read(self.p(), cx, buf.unfilled()) {
                Poll::Ready(Ok(())) => (buf.init_len(), buf.len()),
                other => return other,
            }
        };

        // 将 hyper 侧的读取结果同步回 tokio 的 ReadBuf
        let n_init = new_init - init;
        unsafe {
            // 告知 tokio 的 ReadBuf 新初始化了多少字节
            tbuf.assume_init(n_init);
            // 更新已填充的字节数
            tbuf.set_filled(new_filled);
        }

        Poll::Ready(Ok(()))
    }
}

/// 正向适配：将 `crate::rt::Write` 适配为 `tokio::io::AsyncWrite`。
///
/// 当内部类型 `T` 实现了 hyper 的 `Write` trait 时，`Compat<T>` 自动实现
/// tokio 的 `AsyncWrite` trait。写入操作的适配相对简单，因为两套 trait 的
/// 写入接口几乎完全一致。
impl<T> tokio::io::AsyncWrite for Compat<T>
where
    T: crate::rt::Write,
{
    /// 写入数据，委托给 hyper 的 `Write::poll_write`。
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        crate::rt::Write::poll_write(self.p(), cx, buf)
    }

    /// 刷新缓冲区，委托给 hyper 的 `Write::poll_flush`。
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        crate::rt::Write::poll_flush(self.p(), cx)
    }

    /// 关闭写入端，委托给 hyper 的 `Write::poll_shutdown`。
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        crate::rt::Write::poll_shutdown(self.p(), cx)
    }

    /// 检查是否支持向量化写入，委托给 hyper 的 `Write::is_write_vectored`。
    fn is_write_vectored(&self) -> bool {
        crate::rt::Write::is_write_vectored(&self.0)
    }

    /// 向量化写入，委托给 hyper 的 `Write::poll_write_vectored`。
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        crate::rt::Write::poll_write_vectored(self.p(), cx, bufs)
    }
}

/// 反向适配（仅测试）：将 `tokio::io::AsyncRead` 适配为 `crate::rt::Read`。
///
/// 仅在测试环境中编译，用于将 tokio 的 mock IO 类型（如 `tokio_test::io::Mock`）
/// 包装为 hyper 的 IO 类型，以便在单元测试中使用。
#[cfg(test)]
impl<T> crate::rt::Read for Compat<T>
where
    T: tokio::io::AsyncRead,
{
    /// 将 tokio 的 `AsyncRead::poll_read` 适配为 hyper 的 `Read::poll_read`。
    ///
    /// 与正向适配类似，需要在 tokio 和 hyper 的缓冲区类型之间进行转换。
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: crate::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let n = unsafe {
            // 将 hyper 的 ReadBufCursor 底层缓冲区转换为 tokio 的 ReadBuf
            let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
            match tokio::io::AsyncRead::poll_read(self.p(), cx, &mut tbuf) {
                Poll::Ready(Ok(())) => tbuf.filled().len(),
                other => return other,
            }
        };

        unsafe {
            // 将 tokio 侧的读取结果同步回 hyper 的 ReadBufCursor
            buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }
}

/// 反向适配（仅测试）：将 `tokio::io::AsyncWrite` 适配为 `crate::rt::Write`。
///
/// 仅在测试环境中编译，提供与正向适配对称的反向转换。
#[cfg(test)]
impl<T> crate::rt::Write for Compat<T>
where
    T: tokio::io::AsyncWrite,
{
    /// 写入数据，委托给 tokio 的 `AsyncWrite::poll_write`。
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write(self.p(), cx, buf)
    }

    /// 刷新缓冲区，委托给 tokio 的 `AsyncWrite::poll_flush`。
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_flush(self.p(), cx)
    }

    /// 关闭写入端，委托给 tokio 的 `AsyncWrite::poll_shutdown`。
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_shutdown(self.p(), cx)
    }

    /// 检查是否支持向量化写入，委托给 tokio 的 `AsyncWrite::is_write_vectored`。
    fn is_write_vectored(&self) -> bool {
        tokio::io::AsyncWrite::is_write_vectored(&self.0)
    }

    /// 向量化写入，委托给 tokio 的 `AsyncWrite::poll_write_vectored`。
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write_vectored(self.p(), cx, bufs)
    }
}
