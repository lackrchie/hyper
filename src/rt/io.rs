//! 异步 IO trait 模块
//!
//! 本模块定义了 hyper 自己的异步 `Read` 和 `Write` trait，以及配套的
//! `ReadBuf` 和 `ReadBufCursor` 缓冲区类型。
//!
//! ## 为什么 hyper 要定义自己的 IO trait？
//!
//! hyper 自定义 IO trait 而非直接使用 tokio 的 `AsyncRead`/`AsyncWrite`，
//! 主要基于以下设计目标：
//!
//! 1. **支持基于轮询（poll-based）的 IO 操作**
//! 2. **可选的向量化 IO（vectored IO）支持**
//! 3. **未来可接入缓冲池（buffer pool）**
//! 4. **为最终的 io-uring 完成式 IO 预留前向兼容性**
//!
//! 最后一点是做出此设计决策的核心原因：hyper 希望能够在不破坏 1.0 API
//! 的前提下，未来支持基于 io-uring 的完成式异步运行时。

// --- 标准库导入 ---

/// 导入格式化 trait，用于实现 `Debug`
use std::fmt;
/// 导入 `MaybeUninit`，用于表示可能未初始化的内存区域
/// 这是 `ReadBuf` 能够高效处理未初始化缓冲区的关键类型
use std::mem::MaybeUninit;
/// 导入 `DerefMut` trait，用于为 `Pin<P>` 等智能指针类型实现 Read/Write
use std::ops::DerefMut;
/// 导入 `Pin`，用于固定自引用类型，确保异步轮询方法中的安全性
use std::pin::Pin;
/// 导入异步任务上下文 `Context` 和轮询结果 `Poll`
use std::task::{Context, Poll};

// New IO traits? What?! Why, are you bonkers?
//
// I mean, yes, probably. But, here's the goals:
//
// 1. Supports poll-based IO operations.
// 2. Opt-in vectored IO.
// 3. Can use an optional buffer pool.
// 4. Able to add completion-based (uring) IO eventually.
//
// Frankly, the last point is the entire reason we're doing this. We want to
// have forwards-compatibility with an eventually stable io-uring runtime. We
// don't need that to work right away. But it must be possible to add in here
// without breaking hyper 1.0.
//
// While in here, if there's small tweaks to poll_read or poll_write that would
// allow even the "slow" path to be faster, such as if someone didn't remember
// to forward along an `is_completion` call.

/// 异步字节读取 trait。
///
/// 此 trait 类似于 `std::io::Read`，但支持异步读取操作。
/// 它是 hyper 中所有 IO 读取操作的基础抽象。
///
/// # 实现 `Read`
///
/// 实现者应将数据读入提供的 [`ReadBufCursor`] 并推进游标以指示写入了多少字节。
/// 最简单、最安全的方式是使用 [`ReadBufCursor::put_slice`]：
///
/// ```
/// use hyper::rt::{Read, ReadBufCursor};
/// use std::pin::Pin;
/// use std::task::{Context, Poll};
/// use std::io;
///
/// struct MyReader {
///     data: Vec<u8>,
///     position: usize,
/// }
///
/// impl Read for MyReader {
///     fn poll_read(
///         mut self: Pin<&mut Self>,
///         _cx: &mut Context<'_>,
///         mut buf: ReadBufCursor<'_>,
///     ) -> Poll<Result<(), io::Error>> {
///         let remaining_data = &self.data[self.position..];
///         if remaining_data.is_empty() {
///             // No more data to read, signal EOF by returning Ok without
///             // advancing the buffer
///             return Poll::Ready(Ok(()));
///         }
///
///         // Calculate how many bytes we can write
///         let to_copy = remaining_data.len().min(buf.remaining());
///         // Use put_slice to safely copy data and advance the cursor
///         buf.put_slice(&remaining_data[..to_copy]);
///
///         self.position += to_copy;
///         Poll::Ready(Ok(()))
///     }
/// }
/// ```
///
/// 对于更高级的使用场景（如需要直接访问缓冲区，或与直接写入指针的 API 交互），
/// 可以使用 unsafe 的 [`ReadBufCursor::as_mut`] 和 [`ReadBufCursor::advance`] 方法。
/// 请参阅它们的文档了解安全性要求。
pub trait Read {
    /// 尝试将字节读入 `buf`。
    ///
    /// 成功时返回 `Poll::Ready(Ok(()))`，并将数据放入 `buf` 的未填充部分。
    /// 如果没有读取到数据（`buf.remaining()` 未变），则表示已到达 EOF。
    ///
    /// 如果当前没有数据可读，方法返回 `Poll::Pending`，
    /// 并安排当前任务（通过 `cx.waker()`）在对象可读或关闭时收到通知。
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>>;
}

/// 异步字节写入 trait。
///
/// 此 trait 类似于 `std::io::Write`，但支持异步写入操作。
/// 它是 hyper 中所有 IO 写入操作的基础抽象。
pub trait Write {
    /// 尝试将 `buf` 中的字节写入目标。
    ///
    /// 成功时返回 `Poll::Ready(Ok(num_bytes_written))`。
    /// 保证 `n <= buf.len()`。返回值 `0` 表示底层对象不再接受字节，
    /// 或者提供的缓冲区为空。
    ///
    /// 如果对象尚未准备好写入，方法返回 `Poll::Pending`，
    /// 并安排当前任务在对象可写或关闭时收到通知。
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>>;

    /// 尝试刷新（flush）此写入器中缓冲的数据。
    ///
    /// 成功时返回 `Poll::Ready(Ok(()))`。
    /// 如果刷新无法立即完成，返回 `Poll::Pending`，
    /// 并安排当前任务在可以继续时收到通知。
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>>;

    /// 尝试关闭（shutdown）此写入器。
    ///
    /// 这通常意味着发送 EOF 信号或关闭底层的写入通道。
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>>;

    /// 返回此写入器是否拥有高效的 `poll_write_vectored` 实现。
    ///
    /// 默认实现返回 `false`。如果底层 IO 支持高效的向量化写入
    /// （如 writev 系统调用），实现者应覆盖此方法返回 `true`。
    fn is_write_vectored(&self) -> bool {
        false
    }

    /// 类似 `poll_write`，但从一组缓冲区切片中写入数据。
    ///
    /// 向量化写入（vectored write / scatter-gather IO）可以在一次系统调用中
    /// 写入多个不连续的缓冲区，减少系统调用次数。
    ///
    /// 默认实现会找到第一个非空缓冲区并委托给 `poll_write`。
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        // 找到第一个非空的缓冲区切片，如果全部为空则使用空切片
        let buf = bufs
            .iter()
            .find(|b| !b.is_empty())
            .map_or(&[][..], |b| &**b);
        self.poll_write(cx, buf)
    }
}

/// 增量填充和初始化的字节缓冲区包装器。
///
/// 此类型实现了一种"双游标"机制。它在缓冲区中跟踪三个区域：
/// - **已填充区域（filled）**：缓冲区开头，已被逻辑填充了数据的区域
/// - **已初始化但未填充区域（initialized）**：曾被初始化但尚未被逻辑填充的区域
/// - **未初始化区域（uninitialized）**：缓冲区末尾，可能未被初始化的区域
///
/// 已填充区域一定是已初始化区域的子集。
///
/// 缓冲区内容的可视化表示：
///
/// ```not_rust
/// [             capacity              ]
/// [ filled |         unfilled         ]
/// [    initialized    | uninitialized ]
/// ```
///
/// 将已知初始化的字节"反初始化"是未定义行为（UB），因为我们只是不确定
/// 该区域是否已初始化——如果它已被初始化，就必须保持初始化状态。
///
/// 这种设计的核心优势在于避免了不必要的零初始化开销：
/// 传统的 `Vec<u8>` 需要先零初始化再写入，而 `ReadBuf` 允许直接写入
/// 未初始化的内存，从而减少内存写入次数。
pub struct ReadBuf<'a> {
    /// 底层的原始字节切片，使用 `MaybeUninit<u8>` 表示可能未初始化
    raw: &'a mut [MaybeUninit<u8>],
    /// 已填充的字节数（从缓冲区开头计算）
    filled: usize,
    /// 已初始化的字节数（从缓冲区开头计算，>= filled）
    init: usize,
}

/// [`ReadBuf`] 的游标部分，表示未填充的区域。
///
/// 通过调用 [`ReadBuf::unfilled()`] 创建。
///
/// `ReadBufCursor` 提供了安全和不安全两种方式向缓冲区写入数据：
///
/// - **安全方式**：使用 [`put_slice`](Self::put_slice) 从切片复制数据。
///   它会自动处理初始化跟踪和游标推进。
///
/// - **不安全方式**：对于零拷贝场景或与底层 API 交互时，
///   使用 [`as_mut`](Self::as_mut) 获取 `MaybeUninit<u8>` 的可变切片，
///   写入后调用 [`advance`](Self::advance)。更高效但需要仔细遵守安全不变量。
///
/// # 安全方法示例
///
/// ```
/// use hyper::rt::ReadBuf;
///
/// let mut backing = [0u8; 64];
/// let mut read_buf = ReadBuf::new(&mut backing);
///
/// {
///     let mut cursor = read_buf.unfilled();
///     // put_slice 安全地处理一切
///     cursor.put_slice(b"hello");
/// }
///
/// assert_eq!(read_buf.filled(), b"hello");
/// ```
///
/// # 不安全方法示例
///
/// ```
/// use hyper::rt::ReadBuf;
///
/// let mut backing = [0u8; 64];
/// let mut read_buf = ReadBuf::new(&mut backing);
///
/// {
///     let mut cursor = read_buf.unfilled();
///     // SAFETY: 我们将恰好初始化 5 个字节
///     let slice = unsafe { cursor.as_mut() };
///     slice[0].write(b'h');
///     slice[1].write(b'e');
///     slice[2].write(b'l');
///     slice[3].write(b'l');
///     slice[4].write(b'o');
///     // SAFETY: 我们已经初始化了 5 个字节
///     unsafe { cursor.advance(5) };
/// }
///
/// assert_eq!(read_buf.filled(), b"hello");
/// ```
#[derive(Debug)]
pub struct ReadBufCursor<'a> {
    /// 对 `ReadBuf` 的可变引用，通过它来操作底层缓冲区
    buf: &'a mut ReadBuf<'a>,
}

/// `ReadBuf` 的方法实现块
impl<'data> ReadBuf<'data> {
    /// 使用已初始化的字节切片创建新的 `ReadBuf`。
    ///
    /// 由于传入的 `&mut [u8]` 已完全初始化，所以 `init` 设为切片长度。
    /// 内部通过指针转换将 `&mut [u8]` 转为 `&mut [MaybeUninit<u8>]`。
    #[inline]
    pub fn new(raw: &'data mut [u8]) -> Self {
        let len = raw.len();
        Self {
            // SAFETY: 我们从不反初始化这些字节——只要它们一开始是初始化的，
            // 将 [u8] 视为 [MaybeUninit<u8>] 是安全的
            raw: unsafe { &mut *(raw as *mut [u8] as *mut [MaybeUninit<u8>]) },
            filled: 0,
            init: len,
        }
    }

    /// 使用未初始化的字节切片创建新的 `ReadBuf`。
    ///
    /// `filled` 和 `init` 都设为 0，因为缓冲区完全未初始化。
    #[inline]
    pub fn uninit(raw: &'data mut [MaybeUninit<u8>]) -> Self {
        Self {
            raw,
            filled: 0,
            init: 0,
        }
    }

    /// 获取缓冲区中已填充数据的不可变切片引用。
    ///
    /// 返回的切片只包含已填充（且已初始化）的数据部分。
    #[inline]
    pub fn filled(&self) -> &[u8] {
        // SAFETY: 只切取 filled 范围内的数据，这部分一定已初始化，
        // 因此将 [MaybeUninit<u8>] 转为 [u8] 是安全的
        unsafe { &*(&self.raw[0..self.filled] as *const [MaybeUninit<u8>] as *const [u8]) }
    }

    /// 获取缓冲区未填充部分的游标。
    ///
    /// 返回的 `ReadBufCursor` 可用于向缓冲区写入新数据。
    /// 这里使用了 `unsafe` 的 `transmute` 来缩短生命周期——
    /// 这是安全的，因为 `ReadBuf` 的内部引用不会被重新赋值。
    #[inline]
    pub fn unfilled<'cursor>(&'cursor mut self) -> ReadBufCursor<'cursor> {
        ReadBufCursor {
            // SAFETY: self.buf 不会被重新赋值，所以缩短生命周期是安全的。
            // 这种生命周期技巧（lifetime trick）确保了 ReadBufCursor 的生命周期
            // 不会超过 ReadBuf 的可变借用期
            buf: unsafe {
                std::mem::transmute::<&'cursor mut ReadBuf<'data>, &'cursor mut ReadBuf<'cursor>>(
                    self,
                )
            },
        }
    }

    /// 设置已初始化的字节数（内部方法，用于 HTTP/2）。
    ///
    /// # Safety
    ///
    /// 调用者必须确保前 `n` 个字节确实已被初始化。
    /// 使用 `max` 确保 init 游标只会向前移动，不会后退。
    #[inline]
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(crate) unsafe fn set_init(&mut self, n: usize) {
        self.init = self.init.max(n);
    }

    /// 设置已填充的字节数（内部方法，用于 HTTP/2）。
    ///
    /// # Safety
    ///
    /// 调用者必须确保前 `n` 个字节确实已被填充（且已初始化）。
    /// 使用 `max` 确保 filled 游标只会向前移动。
    #[inline]
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(crate) unsafe fn set_filled(&mut self, n: usize) {
        self.filled = self.filled.max(n);
    }

    /// 返回已填充的字节数（内部方法，用于 HTTP/2）
    #[inline]
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(crate) fn len(&self) -> usize {
        self.filled
    }

    /// 返回已初始化的字节数（内部方法，用于 HTTP/2）
    #[inline]
    #[cfg(all(any(feature = "client", feature = "server"), feature = "http2"))]
    pub(crate) fn init_len(&self) -> usize {
        self.init
    }

    /// 返回缓冲区中剩余可用的字节数（容量 - 已填充）
    #[inline]
    fn remaining(&self) -> usize {
        self.capacity() - self.filled
    }

    /// 返回缓冲区的总容量
    #[inline]
    fn capacity(&self) -> usize {
        self.raw.len()
    }
}

/// 为 `ReadBuf` 实现 `Debug` trait。
///
/// 输出 filled、init 和 capacity 三个关键指标，不输出实际数据内容。
impl fmt::Debug for ReadBuf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadBuf")
            .field("filled", &self.filled)
            .field("init", &self.init)
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// `ReadBufCursor` 的方法实现块
impl ReadBufCursor<'_> {
    /// 获取缓冲区未填充部分的可变切片。
    ///
    /// # Safety
    ///
    /// 调用者不得将之前已初始化的字节反初始化。
    /// 返回的是 `MaybeUninit<u8>` 切片，写入时应使用 `MaybeUninit::write()` 方法。
    #[inline]
    pub unsafe fn as_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        // 返回从 filled 位置开始到缓冲区末尾的切片
        &mut self.buf.raw[self.buf.filled..]
    }

    /// 将 `filled` 游标向前推进 `n` 个字节。
    ///
    /// # Safety
    ///
    /// 调用者必须确保已经初始化了额外的 `n` 个字节。
    /// 使用 `checked_add` 防止整数溢出，并同步更新 `init` 游标。
    #[inline]
    pub unsafe fn advance(&mut self, n: usize) {
        // checked_add 检测溢出，溢出时 panic
        self.buf.filled = self.buf.filled.checked_add(n).expect("overflow");
        // 确保 init 至少与 filled 一样大
        self.buf.init = self.buf.filled.max(self.buf.init);
    }

    /// 返回从当前位置到缓冲区末尾的剩余可写字节数。
    ///
    /// 此值等于 `as_mut()` 返回的切片长度。
    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.remaining()
    }

    /// 从 `src` 切片将字节复制到缓冲区并推进游标。
    ///
    /// 这是向 `ReadBufCursor` 写入数据的推荐安全方法，
    /// 它会自动处理初始化跟踪和游标推进。
    ///
    /// # Panics
    ///
    /// 如果 `src` 的长度超过剩余容量，将会 panic。
    #[inline]
    pub fn put_slice(&mut self, src: &[u8]) {
        assert!(
            self.buf.remaining() >= src.len(),
            "src.len() must fit in remaining()"
        );

        let amt = src.len();
        // 因为上面的 assert，这里不会溢出
        let end = self.buf.filled + amt;

        // Safety: 长度已在上方断言检查，copy_from_nonoverlapping 用于
        // 高效的非重叠内存复制
        unsafe {
            self.buf.raw[self.buf.filled..end]
                .as_mut_ptr()
                .cast::<u8>()
                .copy_from_nonoverlapping(src.as_ptr(), amt);
        }

        // 更新 init 游标：如果写入的数据超过了之前的初始化范围
        if self.buf.init < end {
            self.buf.init = end;
        }
        // 更新 filled 游标
        self.buf.filled = end;
    }
}

// ========== Read trait 的委托实现宏 ==========

/// 为实现了 `Deref` 的包装类型（Box、&mut T）生成 `Read` 的委托实现。
///
/// 这是 Rust 中常见的"新类型委托"（newtype delegation）模式：
/// 通过宏为智能指针和引用类型自动生成 trait 实现，将调用委托给内部类型。
macro_rules! deref_async_read {
    () => {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: ReadBufCursor<'_>,
        ) -> Poll<std::io::Result<()>> {
            // 解引用并重新 pin，将调用委托给内部类型
            Pin::new(&mut **self).poll_read(cx, buf)
        }
    };
}

/// 为 `Box<T>` 实现 `Read`——将调用委托给 `T` 的 `Read` 实现。
///
/// `T: ?Sized` 允许 trait object（如 `Box<dyn Read>`）也能工作。
/// `T: Unpin` 约束允许在 Box 上安全地创建 Pin。
impl<T: ?Sized + Read + Unpin> Read for Box<T> {
    deref_async_read!();
}

/// 为 `&mut T` 实现 `Read`——将调用委托给 `T` 的 `Read` 实现
impl<T: ?Sized + Read + Unpin> Read for &mut T {
    deref_async_read!();
}

/// 为 `Pin<P>` 实现 `Read`——处理双重 Pin 的情况。
///
/// 当 `P` 实现了 `DerefMut` 且 `P::Target` 实现了 `Read` 时，
/// `Pin<P>` 也实现 `Read`。这允许 `Pin<Box<dyn Read>>` 等类型工作。
impl<P> Read for Pin<P>
where
    P: DerefMut,
    P::Target: Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 使用 pin_as_deref_mut 辅助函数来安全地解除双重 Pin
        pin_as_deref_mut(self).poll_read(cx, buf)
    }
}

// ========== Write trait 的委托实现宏 ==========

/// 为实现了 `Deref` 的包装类型生成 `Write` 的完整委托实现。
///
/// 包含所有五个 Write trait 方法的委托。
macro_rules! deref_async_write {
    () => {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut **self).poll_write(cx, buf)
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[std::io::IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut **self).poll_write_vectored(cx, bufs)
        }

        fn is_write_vectored(&self) -> bool {
            (**self).is_write_vectored()
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut **self).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut **self).poll_shutdown(cx)
        }
    };
}

/// 为 `Box<T>` 实现 `Write`——将所有写入操作委托给内部类型
impl<T: ?Sized + Write + Unpin> Write for Box<T> {
    deref_async_write!();
}

/// 为 `&mut T` 实现 `Write`——将所有写入操作委托给内部类型
impl<T: ?Sized + Write + Unpin> Write for &mut T {
    deref_async_write!();
}

/// 为 `Pin<P>` 实现 `Write`——处理双重 Pin 的情况
impl<P> Write for Pin<P>
where
    P: DerefMut,
    P::Target: Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        pin_as_deref_mut(self).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        pin_as_deref_mut(self).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        (**self).is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        pin_as_deref_mut(self).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        pin_as_deref_mut(self).poll_shutdown(cx)
    }
}

/// `Pin::as_deref_mut()` 的 polyfill（兼容实现）。
///
/// 由于 `Pin::as_deref_mut()` 尚未稳定（截至编写时），
/// 此函数提供了等效功能：从 `Pin<&mut Pin<P>>` 安全地获取 `Pin<&mut P::Target>`。
///
/// TODO: 一旦 `Pin::as_deref_mut()` 稳定后，应替换为标准库版本。
///
/// # Safety
///
/// 直接从 `Pin<&mut Pin<P>>` 到 `Pin<&mut P::Target>`，
/// 在此过程中不移动数据也不暴露 `&mut Pin<P>`。
/// 详见 `Pin::as_deref_mut()` 的文档说明。
fn pin_as_deref_mut<P: DerefMut>(pin: Pin<&mut Pin<P>>) -> Pin<&mut P::Target> {
    // SAFETY: 我们直接从 Pin<&mut Pin<P>> 转到 Pin<&mut P::Target>，
    // 过程中不移动数据也不暴露 &mut Pin<P>
    unsafe { pin.get_unchecked_mut() }.as_mut()
}
