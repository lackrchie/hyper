//! HTTP/2 连接升级（Upgrade）支持模块
//!
//! 本模块实现了 HTTP/2 CONNECT 方法的连接升级机制。在 HTTP/2 中，CONNECT 方法
//! 用于建立隧道连接（例如 HTTPS 代理），成功建立后需要将 HTTP/2 流转换为
//! 类似 TCP 的双向字节流。
//!
//! 主要组件：
//! - **`H2Upgraded`**: 升级后的连接对象，实现了 hyper 的 `Read` 和 `Write` trait，
//!   将读写操作映射到底层的 h2 收发流。
//! - **`UpgradedSendStreamTask`**: 后台发送任务，负责将用户写入的数据通过 h2 的
//!   `SendStream` 发送到远端。使用 mpsc 通道桥接同步写入和异步发送。
//! - **`UpgradedSendStreamBridge`**: 写入桥接器，通过 mpsc 通道将数据传递给后台任务。
//!
//! 架构设计：
//! ```text
//! 用户代码 --写入--> H2Upgraded --mpsc通道--> UpgradedSendStreamTask --h2--> 网络
//! 用户代码 <--读取-- H2Upgraded <--h2 RecvStream-- 网络
//! ```
//!
//! 之所以需要后台任务，是因为 h2 的 `SendStream` 需要异步轮询容量和处理重置帧，
//! 而这些操作不能在同步的 `Write::poll_write` 中直接完成。

// 标准库导入
use std::future::Future; // Future trait，用于后台发送任务
use std::io::Cursor; // Cursor 用于包装 Box<[u8]> 实现 Buf trait
use std::pin::Pin; // Pin 类型，用于固定自引用类型
use std::task::{Context, Poll}; // 异步任务上下文和轮询结果

// 第三方 crate 导入
use bytes::{Buf, Bytes}; // Buf trait 和 Bytes 类型，用于高效的缓冲区操作
use futures_channel::{mpsc, oneshot}; // mpsc 用于数据传输通道，oneshot 用于错误传递
use futures_core::{ready, Stream}; // ready! 宏和 Stream trait
use h2::{Reason, RecvStream, SendStream}; // h2 的错误原因、接收流和发送流类型
use pin_project_lite::pin_project; // 轻量级 pin projection 宏

// 内部模块导入
use super::ping::Recorder; // Ping 记录器，用于跟踪 keep-alive 和 BDP 数据
use super::SendBuf; // 发送缓冲区枚举
use crate::rt::{Read, ReadBufCursor, Write}; // hyper 的异步 I/O trait

/// 创建升级连接对（H2Upgraded 和 UpgradedSendStreamTask）
///
/// 返回用户端的 `H2Upgraded`（用于读写）和后台的 `UpgradedSendStreamTask`（需要被 spawn 执行）。
///
/// # 参数
/// - `send_stream`: h2 的发送流，用于向远端发送数据
/// - `recv_stream`: h2 的接收流，用于从远端接收数据
/// - `ping`: Ping 记录器，用于追踪接收到的数据量
///
/// # 返回值
/// - `H2Upgraded`: 用户使用的升级连接，实现了 Read + Write
/// - `UpgradedSendStreamTask`: 后台任务，负责将 mpsc 通道中的数据写入 h2 SendStream
pub(super) fn pair<B>(
    send_stream: SendStream<SendBuf<B>>,
    recv_stream: RecvStream,
    ping: Recorder,
) -> (H2Upgraded, UpgradedSendStreamTask<B>) {
    // 创建有界 mpsc 通道（容量为 1），用于从 H2Upgraded 向后台任务传递写入数据
    let (tx, rx) = mpsc::channel(1);
    // 创建 oneshot 通道，用于后台任务向 H2Upgraded 传递错误信息
    let (error_tx, error_rx) = oneshot::channel();

    (
        H2Upgraded {
            send_stream: UpgradedSendStreamBridge { tx, error_rx },
            recv_stream,
            ping,
            buf: Bytes::new(), // 初始化空缓冲区
        },
        UpgradedSendStreamTask {
            h2_tx: send_stream,
            rx,
            error_tx: Some(error_tx),
        },
    )
}

/// HTTP/2 升级后的连接
///
/// 实现了 hyper 的 `Read` 和 `Write` trait，将 HTTP/2 流包装为类似 TCP 的双向字节流。
/// 读取操作直接从 h2 `RecvStream` 获取数据，写入操作通过 mpsc 通道传递给后台任务。
pub(super) struct H2Upgraded {
    /// Ping 记录器，在读取数据时记录字节数用于 BDP 计算
    ping: Recorder,
    /// 发送流桥接器，通过 mpsc 通道将数据传递给后台发送任务
    send_stream: UpgradedSendStreamBridge,
    /// h2 接收流，用于直接读取远端发送的数据
    recv_stream: RecvStream,
    /// 读取缓冲区，存储上次未完全消费的数据
    buf: Bytes,
}

/// 发送流桥接器
///
/// 将用户的写入操作通过 mpsc 通道桥接到后台的 `UpgradedSendStreamTask`。
/// 如果后台任务发生错误，会通过 oneshot 通道传回错误信息。
struct UpgradedSendStreamBridge {
    /// mpsc 发送端，用于向后台任务传递要发送的数据
    tx: mpsc::Sender<Cursor<Box<[u8]>>>,
    /// oneshot 接收端，用于接收后台任务传回的错误
    error_rx: oneshot::Receiver<crate::Error>,
}

// 使用 pin_project 宏定义后台发送任务
pin_project! {
    /// 升级连接的后台发送任务
    ///
    /// 此 Future 需要被 spawn 到执行器中运行。它从 mpsc 通道接收数据，
    /// 并通过 h2 的 `SendStream` 发送到远端。同时处理 h2 的流量控制和重置帧。
    ///
    /// `#[must_use]` 确保调用者不会忘记 spawn 这个任务。
    #[must_use = "futures do nothing unless polled"]
    pub struct UpgradedSendStreamTask<B> {
        // h2 发送流
        #[pin]
        h2_tx: SendStream<SendBuf<B>>,
        // mpsc 接收端，从 H2Upgraded 接收要发送的数据
        #[pin]
        rx: mpsc::Receiver<Cursor<Box<[u8]>>>,
        // oneshot 发送端，用于向 H2Upgraded 传递错误信息
        // 使用 Option 包装以支持 take() 操作（只发送一次）
        error_tx: Option<oneshot::Sender<crate::Error>>,
    }
}

// ===== impl UpgradedSendStreamTask =====

impl<B> UpgradedSendStreamTask<B>
where
    B: Buf,
{
    /// 执行一次完整的轮询周期
    ///
    /// 这是一个手动的 `select()` 实现，同时监控三个事件源：
    /// 1. h2 流的容量分配
    /// 2. h2 流的重置信号
    /// 3. mpsc 通道的数据到达
    ///
    /// 这样做确保任务不会在某一方已断开时继续无谓地运行。
    fn tick(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), crate::Error>> {
        let mut me = self.project();

        // this is a manual `select()` over 3 "futures", so we always need
        // to be sure they are ready and/or we are waiting notification of
        // one of the sides hanging up, so the task doesn't live around
        // longer than it's meant to.
        loop {
            // we don't have the next chunk of data yet, so just reserve 1 byte to make
            // sure there's some capacity available. h2 will handle the capacity management
            // for the actual body chunk.
            // 预留 1 字节容量以保持流活跃
            me.h2_tx.reserve_capacity(1);

            if me.h2_tx.capacity() == 0 {
                // poll_capacity oddly needs a loop
                // 等待 h2 分配发送容量
                'capacity: loop {
                    match me.h2_tx.poll_capacity(cx) {
                        Poll::Ready(Some(Ok(0))) => {} // 分配了 0 字节，继续等待
                        Poll::Ready(Some(Ok(_))) => break, // 获得了容量
                        Poll::Ready(Some(Err(e))) => {
                            return Poll::Ready(Err(crate::Error::new_body_write(e)))
                        }
                        Poll::Ready(None) => {
                            // None means the stream is no longer in a
                            // streaming state, we either finished it
                            // somehow, or the remote reset us.
                            return Poll::Ready(Err(crate::Error::new_body_write(
                                "send stream capacity unexpectedly closed",
                            )));
                        }
                        Poll::Pending => break 'capacity, // 等待容量分配，跳出内层循环
                    }
                }
            }

            // 检查远端是否发送了 RST_STREAM
            match me.h2_tx.poll_reset(cx) {
                Poll::Ready(Ok(reason)) => {
                    trace!("stream received RST_STREAM: {:?}", reason);
                    return Poll::Ready(Err(crate::Error::new_body_write(::h2::Error::from(
                        reason,
                    ))));
                }
                Poll::Ready(Err(err)) => {
                    return Poll::Ready(Err(crate::Error::new_body_write(err)))
                }
                Poll::Pending => (),
            }

            // 从 mpsc 通道接收数据并通过 h2 发送
            match me.rx.as_mut().poll_next(cx) {
                Poll::Ready(Some(cursor)) => {
                    // 收到数据，通过 h2 发送（end_of_stream = false）
                    me.h2_tx
                        .send_data(SendBuf::Cursor(cursor), false)
                        .map_err(crate::Error::new_body_write)?;
                }
                Poll::Ready(None) => {
                    // mpsc 通道关闭（H2Upgraded 被 drop 或主动关闭），
                    // 发送一个空的 EOS 帧来结束 h2 流
                    me.h2_tx
                        .send_data(SendBuf::None, true)
                        .map_err(crate::Error::new_body_write)?;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

/// `UpgradedSendStreamTask` 的 Future 实现
///
/// 将 `tick()` 的结果映射为 Future 输出：
/// - 成功完成时直接返回
/// - 发生错误时通过 oneshot 通道将错误传递给 H2Upgraded
impl<B> Future for UpgradedSendStreamTask<B>
where
    B: Buf,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.as_mut().tick(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(()),
            Poll::Ready(Err(err)) => {
                // 将错误通过 oneshot 通道发送给 H2Upgraded
                if let Some(tx) = self.error_tx.take() {
                    let _oh_well = tx.send(err); // 忽略发送失败（H2Upgraded 可能已被 drop）
                }
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ===== impl H2Upgraded =====

/// 为 H2Upgraded 实现 Read trait
///
/// 从 h2 的 RecvStream 中读取数据。使用内部缓冲区处理数据帧的分片读取。
impl Read for H2Upgraded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut read_buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // 如果内部缓冲区为空，需要从 RecvStream 获取新的数据帧
        if self.buf.is_empty() {
            self.buf = loop {
                match ready!(self.recv_stream.poll_data(cx)) {
                    None => return Poll::Ready(Ok(())), // 流已结束（EOF）
                    Some(Ok(buf)) if buf.is_empty() && !self.recv_stream.is_end_stream() => {
                        // 收到空的数据帧但流未结束，继续等待下一帧
                        continue
                    }
                    Some(Ok(buf)) => {
                        // 记录接收到的数据量（用于 BDP 计算）
                        self.ping.record_data(buf.len());
                        break buf;
                    }
                    Some(Err(e)) => {
                        // 将 h2 错误转换为 io::Error
                        return Poll::Ready(match e.reason() {
                            // NO_ERROR 和 CANCEL 视为正常关闭
                            Some(Reason::NO_ERROR) | Some(Reason::CANCEL) => Ok(()),
                            // STREAM_CLOSED 转换为 BrokenPipe 错误
                            Some(Reason::STREAM_CLOSED) => {
                                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
                            }
                            // 其他错误直接转换
                            _ => Err(h2_to_io_error(e)),
                        })
                    }
                }
            };
        }
        // 从缓冲区中复制数据到调用者的缓冲区
        let cnt = std::cmp::min(self.buf.len(), read_buf.remaining());
        read_buf.put_slice(&self.buf[..cnt]);
        self.buf.advance(cnt);
        // 释放 h2 流量控制容量，允许远端发送更多数据
        let _ = self.recv_stream.flow_control().release_capacity(cnt);
        Poll::Ready(Ok(()))
    }
}

/// 为 H2Upgraded 实现 Write trait
///
/// 通过 mpsc 通道将数据传递给后台的 UpgradedSendStreamTask。
impl Write for H2Upgraded {
    /// 异步写入数据
    ///
    /// 首先检查 mpsc 通道是否就绪（有容量），然后将数据发送到后台任务。
    /// 如果后台任务已失败，会尝试从 error_rx 获取错误信息。
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        // 空写入直接返回
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // 检查 mpsc 通道是否有容量
        match self.send_stream.tx.poll_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(_task_dropped)) => {
                // if the task dropped, check if there was an error
                // otherwise i guess its a broken pipe
                // 后台任务已终止，尝试获取具体错误信息
                return match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
                    Poll::Ready(Ok(reason)) => Poll::Ready(Err(io_error(reason))),
                    Poll::Ready(Err(_task_dropped)) => {
                        Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                    }
                    Poll::Pending => Poll::Pending,
                };
            }
            Poll::Pending => return Poll::Pending,
        }

        // 将数据复制到 Box<[u8]> 并通过 Cursor 包装后发送
        let n = buf.len();
        match self.send_stream.tx.start_send(Cursor::new(buf.into())) {
            Ok(()) => Poll::Ready(Ok(n)),
            Err(_task_dropped) => {
                // if the task dropped, check if there was an error
                // otherwise i guess its a broken pipe
                match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
                    Poll::Ready(Ok(reason)) => Poll::Ready(Err(io_error(reason))),
                    Poll::Ready(Err(_task_dropped)) => {
                        Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    /// 刷新写缓冲区
    ///
    /// 等待 mpsc 通道就绪，确保之前发送的数据已被后台任务处理。
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.send_stream.tx.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_task_dropped)) => {
                // if the task dropped, check if there was an error
                // otherwise it was a clean close
                match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
                    Poll::Ready(Ok(reason)) => Poll::Ready(Err(io_error(reason))),
                    Poll::Ready(Err(_task_dropped)) => Poll::Ready(Ok(())), // 后台任务正常结束
                    Poll::Pending => Poll::Pending,
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// 关闭写入端
    ///
    /// 关闭 mpsc 通道，通知后台任务发送 EOS 帧。
    /// 然后等待可能的错误信息。
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // 关闭 mpsc 通道，后台任务会在收到 None 时发送 EOS
        self.send_stream.tx.close_channel();
        // 检查后台任务是否产生了错误
        match Pin::new(&mut self.send_stream.error_rx).poll(cx) {
            Poll::Ready(Ok(reason)) => Poll::Ready(Err(io_error(reason))),
            Poll::Ready(Err(_task_dropped)) => Poll::Ready(Ok(())), // 后台任务已正常结束
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 将 hyper 错误转换为 io::Error
fn io_error(e: crate::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e)
}

/// 将 h2 错误转换为 io::Error
///
/// 如果 h2 错误本身包含 io::Error（例如底层 TCP 连接错误），
/// 则直接提取；否则包装为 io::ErrorKind::Other。
fn h2_to_io_error(e: h2::Error) -> std::io::Error {
    if e.is_io() {
        e.into_io().unwrap()
    } else {
        std::io::Error::new(std::io::ErrorKind::Other, e)
    }
}
