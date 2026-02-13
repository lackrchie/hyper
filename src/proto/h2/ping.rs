//! HTTP/2 Ping 帧的使用与管理
//!
//! hyper 使用 HTTP/2 PING 帧实现两个目的：
//!
//! 1. **自适应流量控制（BDP，带宽延迟积）**：通过 PING 帧测量 RTT（往返时间），
//!    结合已接收的字节数动态调整流量控制窗口大小，以最大化吞吐量。
//! 2. **连接保活（Keep-Alive）**：定期发送 PING 帧检测连接是否仍然存活，
//!    如果在超时时间内未收到 PONG 响应，则关闭连接。
//!
//! 两种功能都是可选的，可以独立启用或禁用。
//!
//! # BDP 算法
//!
//! 1. 当接收到 DATA 帧时，如果没有正在进行的 BDP ping：
//!    1a. 记录当前时间。
//!    1b. 发送一个 BDP ping。
//! 2. 累加已接收的字节数。
//! 3. 当收到 BDP ping 的确认（pong）时：
//!    3a. 计算从发送到接收的时间差（RTT）。
//!    3b. 使用移动平均法合并 RTT。
//!    3c. 计算 BDP = 字节数 / RTT。
//!    3d. 如果 BDP 超过当前最大值的 2/3，则更新窗口大小。

// 标准库导入
use std::fmt; // 格式化 trait，用于实现 Display
use std::future::Future; // Future trait，用于异步编程
use std::pin::Pin; // Pin 类型，用于固定内存位置
use std::sync::{Arc, Mutex}; // 原子引用计数和互斥锁，用于线程安全的共享状态
use std::task::{self, Poll}; // 异步任务上下文和轮询结果
use std::time::{Duration, Instant}; // 时间间隔和时间点类型

// h2 crate 的 Ping 和 PingPong 类型，用于发送和接收 HTTP/2 PING 帧
use h2::{Ping, PingPong};

// 内部 crate 导入
use crate::common::time::Time; // hyper 的时间抽象，支持可替换的计时器实现
use crate::rt::Sleep; // hyper 运行时的 Sleep trait，提供异步休眠功能

/// 窗口大小类型别名，与 HTTP/2 规范一致使用 u32
type WindowSize = u32;

/// 创建一个禁用状态的 Recorder
///
/// 当不需要 BDP 或 keep-alive 功能时，使用此函数创建一个空的 Recorder。
/// 所有对该 Recorder 的操作都将直接返回，不产生任何副作用。
pub(super) fn disabled() -> Recorder {
    Recorder { shared: None }
}

/// 创建 Ping 通道，返回记录器（Recorder）和响应器（Ponger）对
///
/// Recorder 用于在接收数据时记录信息（由流的接收端使用），
/// Ponger 用于处理 PING/PONG 帧并计算 BDP（由连接任务使用）。
///
/// # 参数
/// - `ping_pong`: h2 提供的 PingPong 句柄，用于实际的 PING 帧收发
/// - `config`: Ping 配置，包含 BDP 和 keep-alive 设置
/// - `timer`: 时间抽象，用于获取当前时间和创建定时器
///
/// # Panics
/// 如果配置中 BDP 和 keep-alive 都未启用，则触发 debug_assert
pub(super) fn channel(ping_pong: PingPong, config: Config, timer: Time) -> (Recorder, Ponger) {
    debug_assert!(
        config.is_enabled(),
        "ping channel requires bdp or keep-alive config",
    );

    // 初始化 BDP 状态（如果启用）
    let bdp = config.bdp_initial_window.map(|wnd| Bdp {
        bdp: wnd,
        max_bandwidth: 0.0,
        rtt: 0.0,
        ping_delay: Duration::from_millis(100), // 初始 ping 间隔为 100ms
        stable_count: 0,
    });

    let now = timer.now();

    // 只有在 BDP 启用时才需要跟踪字节数和下次 BDP ping 时间
    let (bytes, next_bdp_at) = if bdp.is_some() {
        (Some(0), Some(now))
    } else {
        (None, None)
    };

    // 初始化 keep-alive 状态（如果启用）
    let keep_alive = config.keep_alive_interval.map(|interval| KeepAlive {
        interval,
        timeout: config.keep_alive_timeout,
        while_idle: config.keep_alive_while_idle,
        sleep: timer.sleep(interval), // 创建初始定时器
        state: KeepAliveState::Init,
        timer: timer.clone(),
    });

    // 只有启用 keep-alive 时才需要跟踪最后读取时间
    let last_read_at = keep_alive.as_ref().map(|_| now);

    // 创建共享状态，使用 Arc<Mutex<>> 实现线程安全共享
    // Recorder 和 Ponger 都持有对此共享状态的引用
    let shared = Arc::new(Mutex::new(Shared {
        bytes,
        last_read_at,
        is_keep_alive_timed_out: false,
        ping_pong,
        ping_sent_at: None,
        next_bdp_at,
        timer,
    }));

    (
        Recorder {
            shared: Some(shared.clone()),
        },
        Ponger {
            bdp,
            keep_alive,
            shared,
        },
    )
}

/// Ping 配置结构体
///
/// 包含 BDP 自适应窗口和 keep-alive 的所有配置选项。
#[derive(Clone)]
pub(super) struct Config {
    /// BDP 初始窗口大小。如果为 Some，则启用自适应流量控制
    pub(super) bdp_initial_window: Option<WindowSize>,
    /// If no frames are received in this amount of time, a PING frame is sent.
    /// keep-alive ping 间隔。如果在此时间内未收到任何帧，则发送 PING
    pub(super) keep_alive_interval: Option<Duration>,
    /// After sending a keepalive PING, the connection will be closed if
    /// a pong is not received in this amount of time.
    /// keep-alive 超时时间。发送 PING 后，如果在此时间内未收到 PONG，则关闭连接
    pub(super) keep_alive_timeout: Duration,
    /// If true, sends pings even when there are no active streams.
    /// 是否在没有活跃流时也发送 keep-alive ping
    pub(super) keep_alive_while_idle: bool,
}

/// Ping 记录器，用于记录数据接收事件
///
/// 在接收 HTTP/2 数据帧和非数据帧时调用，以触发 BDP 计算和更新 keep-alive 时间戳。
/// 可以安全地克隆，多个 Recorder 共享同一个内部状态。
///
/// 如果 `shared` 为 None，表示 ping 功能已禁用，所有方法都是空操作。
#[derive(Clone)]
pub(crate) struct Recorder {
    shared: Option<Arc<Mutex<Shared>>>,
}

/// Ping 响应器，负责处理 PING/PONG 帧和执行 BDP 计算
///
/// 由连接任务（Connection task）持有并轮询。它会：
/// - 管理 keep-alive 定时器的调度和超时检测
/// - 接收 PONG 响应并计算 BDP
/// - 在需要时返回窗口大小更新或超时通知
pub(super) struct Ponger {
    /// BDP 计算状态（如果启用 BDP）
    bdp: Option<Bdp>,
    /// Keep-alive 状态（如果启用 keep-alive）
    keep_alive: Option<KeepAlive>,
    /// 与 Recorder 共享的可变状态
    shared: Arc<Mutex<Shared>>,
}

/// Recorder 和 Ponger 之间的共享状态
///
/// 使用 `Mutex` 保护，因为 Recorder 可能在不同的任务/线程中被使用。
struct Shared {
    /// h2 的 PingPong 句柄，用于发送 PING 和接收 PONG
    ping_pong: PingPong,
    /// PING 帧发送的时间点，用于计算 RTT
    ping_sent_at: Option<Instant>,

    // bdp 相关字段
    /// If `Some`, bdp is enabled, and this tracks how many bytes have been
    /// read during the current sample.
    /// 当前采样周期内已接收的字节数（None 表示 BDP 已禁用）
    bytes: Option<usize>,
    /// We delay a variable amount of time between BDP pings. This allows us
    /// to send less pings as the bandwidth stabilizes.
    /// 下次 BDP ping 的预定时间
    next_bdp_at: Option<Instant>,

    // keep-alive 相关字段
    /// If `Some`, keep-alive is enabled, and the Instant is how long ago
    /// the connection read the last frame.
    /// 最后一次读取帧的时间（None 表示 keep-alive 已禁用）
    last_read_at: Option<Instant>,

    /// keep-alive 是否已超时
    is_keep_alive_timed_out: bool,
    /// 时间抽象，用于获取当前时间
    timer: Time,
}

/// BDP（带宽延迟积）计算状态
///
/// 通过测量 RTT 和已传输字节数来估算最优的流控窗口大小。
/// 使用移动平均法平滑 RTT 值，并在带宽稳定后逐渐减少 ping 频率。
struct Bdp {
    /// Current BDP in bytes
    /// 当前估算的 BDP 值（字节）
    bdp: u32,
    /// Largest bandwidth we've seen so far.
    /// 观察到的最大带宽值（字节/秒）
    max_bandwidth: f64,
    /// Round trip time in seconds
    /// 移动平均 RTT（秒）
    rtt: f64,
    /// Delay the next ping by this amount.
    ///
    /// This will change depending on how stable the current bandwidth is.
    /// 下次 ping 的延迟时间。当带宽稳定时，延迟会增大以减少不必要的 ping
    ping_delay: Duration,
    /// The count of ping round trips where BDP has stayed the same.
    /// 连续未更新 BDP 的 ping 往返次数，用于判断带宽是否稳定
    stable_count: u32,
}

/// Keep-Alive 保活机制状态
///
/// 管理 keep-alive ping 的调度、发送和超时检测。
struct KeepAlive {
    /// If no frames are received in this amount of time, a PING frame is sent.
    /// keep-alive ping 间隔
    interval: Duration,
    /// After sending a keepalive PING, the connection will be closed if
    /// a pong is not received in this amount of time.
    /// PONG 响应的超时时间
    timeout: Duration,
    /// If true, sends pings even when there are no active streams.
    /// 是否在空闲时也发送 ping
    while_idle: bool,
    /// 当前的 keep-alive 状态机状态
    state: KeepAliveState,
    /// 异步定时器，用于调度 ping 发送和超时检测
    sleep: Pin<Box<dyn Sleep>>,
    /// 时间抽象
    timer: Time,
}

/// Keep-Alive 状态机
///
/// 表示 keep-alive 机制的三种状态：
/// - `Init`: 初始状态，尚未调度任何 ping
/// - `Scheduled`: 已调度了 ping，等待定时器到期
/// - `PingSent`: PING 已发送，等待 PONG 响应
enum KeepAliveState {
    /// 初始化状态
    Init,
    /// 已调度，参数为预计发送 ping 的时间点
    Scheduled(Instant),
    /// PING 已发送，等待 PONG
    PingSent,
}

/// Ponger 的轮询结果枚举
///
/// 表示 Ponger 在被轮询时可能产生的两种事件：
/// - `SizeUpdate`: 需要更新流控窗口大小（BDP 计算结果）
/// - `KeepAliveTimedOut`: keep-alive 超时，应关闭连接
pub(super) enum Ponged {
    /// BDP 计算出了新的窗口大小
    SizeUpdate(WindowSize),
    /// Keep-alive PING 超时未收到 PONG
    KeepAliveTimedOut,
}

/// Keep-Alive 超时错误
///
/// 当 keep-alive PING 在超时时间内未收到 PONG 响应时产生此错误。
#[derive(Debug)]
pub(super) struct KeepAliveTimedOut;

// ===== impl Config =====

impl Config {
    /// 检查 ping 功能是否已启用（BDP 或 keep-alive 至少一个启用）
    pub(super) fn is_enabled(&self) -> bool {
        self.bdp_initial_window.is_some() || self.keep_alive_interval.is_some()
    }
}

// ===== impl Recorder =====

impl Recorder {
    /// 记录收到的数据帧
    ///
    /// 在接收到 HTTP/2 DATA 帧时调用。会更新 keep-alive 的最后读取时间，
    /// 并在适当时机触发 BDP ping 的发送。
    ///
    /// # 参数
    /// - `len`: 收到的数据字节数
    pub(crate) fn record_data(&self, len: usize) {
        let shared = if let Some(ref shared) = self.shared {
            shared
        } else {
            // 如果 shared 为 None，说明 ping 已禁用，直接返回
            return;
        };

        let mut locked = shared.lock().unwrap();

        // 更新 keep-alive 的最后读取时间
        locked.update_last_read_at();

        // are we ready to send another bdp ping?
        // if not, we don't need to record bytes either
        // 检查是否到了发送下一个 BDP ping 的时间
        if let Some(ref next_bdp_at) = locked.next_bdp_at {
            if locked.timer.now() < *next_bdp_at {
                // 还未到预定时间，跳过
                return;
            } else {
                // 已到预定时间，清除标记以允许新的 ping
                locked.next_bdp_at = None;
            }
        }

        // 累加已接收的字节数
        if let Some(ref mut bytes) = locked.bytes {
            *bytes += len;
        } else {
            // no need to send bdp ping if bdp is disabled
            // BDP 未启用，无需记录字节数
            return;
        }

        // 如果当前没有正在等待响应的 ping，则发送新的 BDP ping
        if !locked.is_ping_sent() {
            locked.send_ping();
        }
    }

    /// 记录收到的非数据帧（如 HEADERS、SETTINGS 等）
    ///
    /// 仅更新 keep-alive 的最后读取时间，不影响 BDP 计算。
    pub(crate) fn record_non_data(&self) {
        let shared = if let Some(ref shared) = self.shared {
            shared
        } else {
            return;
        };

        let mut locked = shared.lock().unwrap();

        locked.update_last_read_at();
    }

    /// If the incoming stream is already closed, convert self into
    /// a disabled reporter.
    /// 如果传入流已关闭，将自身转换为禁用状态的 Recorder
    ///
    /// 这是一个优化：对于已经结束的流，不需要继续进行 BDP 跟踪。
    #[cfg(feature = "client")]
    pub(super) fn for_stream(self, stream: &h2::RecvStream) -> Self {
        if stream.is_end_stream() {
            disabled()
        } else {
            self
        }
    }

    /// 检查 keep-alive 是否已超时
    ///
    /// 如果已超时，返回一个 keep-alive 超时错误。
    /// 此方法用于在发现连接错误时区分是 keep-alive 超时还是其他原因。
    pub(super) fn ensure_not_timed_out(&self) -> crate::Result<()> {
        if let Some(ref shared) = self.shared {
            let locked = shared.lock().unwrap();
            if locked.is_keep_alive_timed_out {
                return Err(KeepAliveTimedOut.crate_error());
            }
        }

        // else
        Ok(())
    }
}

// ===== impl Ponger =====

impl Ponger {
    /// 轮询 Ponger 以处理 PING/PONG 事件
    ///
    /// 该方法执行以下逻辑：
    /// 1. 检查并调度 keep-alive ping
    /// 2. 等待 PONG 响应
    /// 3. 收到 PONG 后计算 BDP 并可能返回窗口大小更新
    /// 4. 检测 keep-alive 超时
    ///
    /// # 返回值
    /// - `Poll::Ready(Ponged::SizeUpdate(wnd))`: 需要更新窗口大小
    /// - `Poll::Ready(Ponged::KeepAliveTimedOut)`: keep-alive 超时
    /// - `Poll::Pending`: 没有需要处理的事件
    pub(super) fn poll(&mut self, cx: &mut task::Context<'_>) -> Poll<Ponged> {
        let mut locked = self.shared.lock().unwrap();
        let now = locked.timer.now(); // hoping this is fine to move within the lock
        let is_idle = self.is_idle();

        // 调度和发送 keep-alive ping
        if let Some(ref mut ka) = self.keep_alive {
            ka.maybe_schedule(is_idle, &locked);
            ka.maybe_ping(cx, is_idle, &mut locked);
        }

        // 如果没有正在等待的 ping，则暂时挂起
        if !locked.is_ping_sent() {
            // XXX: this doesn't register a waker...?
            return Poll::Pending;
        }

        // 轮询 PONG 响应
        match locked.ping_pong.poll_pong(cx) {
            Poll::Ready(Ok(_pong)) => {
                // 收到 PONG，计算 RTT
                let start = locked
                    .ping_sent_at
                    .expect("pong received implies ping_sent_at");
                locked.ping_sent_at = None;
                let rtt = now - start;
                trace!("recv pong");

                // 更新 keep-alive 状态
                if let Some(ref mut ka) = self.keep_alive {
                    locked.update_last_read_at();
                    ka.maybe_schedule(is_idle, &locked);
                    ka.maybe_ping(cx, is_idle, &mut locked);
                }

                // 执行 BDP 计算
                if let Some(ref mut bdp) = self.bdp {
                    let bytes = locked.bytes.expect("bdp enabled implies bytes");
                    locked.bytes = Some(0); // reset 重置字节计数器
                    trace!("received BDP ack; bytes = {}, rtt = {:?}", bytes, rtt);

                    let update = bdp.calculate(bytes, rtt);
                    // 设置下次 BDP ping 的时间
                    locked.next_bdp_at = Some(now + bdp.ping_delay);
                    if let Some(update) = update {
                        return Poll::Ready(Ponged::SizeUpdate(update));
                    }
                }
            }
            Poll::Ready(Err(_e)) => {
                debug!("pong error: {}", _e);
            }
            Poll::Pending => {
                // PONG 尚未到达，检查 keep-alive 是否超时
                if let Some(ref mut ka) = self.keep_alive {
                    if let Err(KeepAliveTimedOut) = ka.maybe_timeout(cx) {
                        // keep-alive 超时，标记状态并通知上层
                        self.keep_alive = None;
                        locked.is_keep_alive_timed_out = true;
                        return Poll::Ready(Ponged::KeepAliveTimedOut);
                    }
                }
            }
        }

        // XXX: this doesn't register a waker...?
        Poll::Pending
    }

    /// 检查连接是否处于空闲状态
    ///
    /// 通过 Arc 的强引用计数来判断：如果只剩下 Ponger 自身和 Shared 内部
    /// 的引用（<= 2），说明没有活跃的 Recorder 克隆在使用，连接处于空闲。
    fn is_idle(&self) -> bool {
        Arc::strong_count(&self.shared) <= 2
    }
}

// ===== impl Shared =====

impl Shared {
    /// 发送一个 PING 帧
    ///
    /// 使用 h2 的 PingPong 接口发送一个不透明的 PING 帧，
    /// 并记录发送时间用于后续 RTT 计算。
    fn send_ping(&mut self) {
        match self.ping_pong.send_ping(Ping::opaque()) {
            Ok(()) => {
                self.ping_sent_at = Some(self.timer.now());
                trace!("sent ping");
            }
            Err(_err) => {
                debug!("error sending ping: {}", _err);
            }
        }
    }

    /// 检查是否有正在等待响应的 PING
    fn is_ping_sent(&self) -> bool {
        self.ping_sent_at.is_some()
    }

    /// 更新最后读取帧的时间戳
    ///
    /// 仅在 keep-alive 启用（last_read_at 为 Some）时才更新。
    fn update_last_read_at(&mut self) {
        if self.last_read_at.is_some() {
            self.last_read_at = Some(self.timer.now());
        }
    }

    /// 获取最后读取帧的时间戳
    ///
    /// # Panics
    /// 如果 keep-alive 未启用（last_read_at 为 None），则 panic
    fn last_read_at(&self) -> Instant {
        self.last_read_at.expect("keep_alive expects last_read_at")
    }
}

// ===== impl Bdp =====

/// BDP 的上限值（16MB）
///
/// 超过此值可能会触及 TCP 层的流量控制限制，因此不再继续增大窗口。
const BDP_LIMIT: usize = 1024 * 1024 * 16;

impl Bdp {
    /// 根据接收到的字节数和 RTT 计算新的 BDP 窗口大小
    ///
    /// # 参数
    /// - `bytes`: 本次采样周期内接收到的总字节数
    /// - `rtt`: 本次 PING/PONG 的往返时间
    ///
    /// # 返回值
    /// - `Some(window_size)`: 需要更新窗口大小
    /// - `None`: 当前窗口大小已足够，无需更新
    fn calculate(&mut self, bytes: usize, rtt: Duration) -> Option<WindowSize> {
        // No need to do any math if we're at the limit.
        // 已达到上限，不再增大
        if self.bdp as usize == BDP_LIMIT {
            self.stabilize_delay();
            return None;
        }

        // average the rtt
        // 使用指数移动平均法平滑 RTT 值
        let rtt = seconds(rtt);
        if self.rtt == 0.0 {
            // First sample means rtt is first rtt.
            // 第一次采样，直接使用测量值
            self.rtt = rtt;
        } else {
            // Weigh this rtt as 1/8 for a moving average.
            // 以 1/8 权重更新移动平均 RTT
            self.rtt += (rtt - self.rtt) * 0.125;
        }

        // calculate the current bandwidth
        // 计算当前带宽，使用 1.5 倍 RTT 作为除数以留有余量
        let bw = (bytes as f64) / (self.rtt * 1.5);
        trace!("current bandwidth = {:.1}B/s", bw);

        if bw < self.max_bandwidth {
            // not a faster bandwidth, so don't update
            // 带宽未超过历史最大值，不更新窗口
            self.stabilize_delay();
            return None;
        } else {
            self.max_bandwidth = bw;
        }

        // if the current `bytes` sample is at least 2/3 the previous
        // bdp, increase to double the current sample.
        // 如果当前采样字节数至少达到上次 BDP 的 2/3，
        // 则将 BDP 更新为当前采样的 2 倍（但不超过上限）
        if bytes >= self.bdp as usize * 2 / 3 {
            self.bdp = (bytes * 2).min(BDP_LIMIT) as WindowSize;
            trace!("BDP increased to {}", self.bdp);

            // BDP 更新了，重置稳定计数并缩短 ping 间隔以更快适应
            self.stable_count = 0;
            self.ping_delay /= 2;
            Some(self.bdp)
        } else {
            self.stabilize_delay();
            None
        }
    }

    /// 当 BDP 保持稳定时，逐渐增大 ping 间隔以减少开销
    ///
    /// 连续 2 次未更新 BDP 后，将 ping 间隔增大 4 倍。
    /// ping 间隔的上限为 10 秒。
    fn stabilize_delay(&mut self) {
        if self.ping_delay < Duration::from_secs(10) {
            self.stable_count += 1;

            if self.stable_count >= 2 {
                self.ping_delay *= 4;
                self.stable_count = 0;
            }
        }
    }
}

/// 将 Duration 转换为浮点数秒
fn seconds(dur: Duration) -> f64 {
    const NANOS_PER_SEC: f64 = 1_000_000_000.0;
    let secs = dur.as_secs() as f64;
    secs + (dur.subsec_nanos() as f64) / NANOS_PER_SEC
}

// ===== impl KeepAlive =====

impl KeepAlive {
    /// 尝试调度下一次 keep-alive ping
    ///
    /// 根据当前状态决定是否需要调度新的 ping：
    /// - `Init` 状态：如果连接不空闲或允许空闲时发送 ping，则调度
    /// - `PingSent` 状态：如果 PONG 已收到（ping 不再处于发送状态），则重新调度
    /// - `Scheduled` 状态：已有调度，无需操作
    fn maybe_schedule(&mut self, is_idle: bool, shared: &Shared) {
        match self.state {
            KeepAliveState::Init => {
                if !self.while_idle && is_idle {
                    return;
                }

                self.schedule(shared);
            }
            KeepAliveState::PingSent => {
                if shared.is_ping_sent() {
                    return;
                }
                self.schedule(shared);
            }
            KeepAliveState::Scheduled(..) => (),
        }
    }

    /// 调度一次 keep-alive ping
    ///
    /// 计算下次 ping 的时间点（最后读取时间 + 间隔），并重置定时器。
    fn schedule(&mut self, shared: &Shared) {
        let interval = shared.last_read_at() + self.interval;
        self.state = KeepAliveState::Scheduled(interval);
        self.timer.reset(&mut self.sleep, interval);
    }

    /// 在适当时机发送 keep-alive PING 帧
    ///
    /// 仅在 `Scheduled` 状态且定时器到期时发送。发送前会再次检查
    /// 是否在等待期间收到了新的帧（避免不必要的 ping）。
    fn maybe_ping(&mut self, cx: &mut task::Context<'_>, is_idle: bool, shared: &mut Shared) {
        match self.state {
            KeepAliveState::Scheduled(at) => {
                // 检查定时器是否到期
                if Pin::new(&mut self.sleep).poll(cx).is_pending() {
                    return;
                }
                // check if we've received a frame while we were scheduled
                // 检查在等待期间是否收到了新帧
                if shared.last_read_at() + self.interval > at {
                    // 收到了新帧，回到初始状态重新调度
                    self.state = KeepAliveState::Init;
                    cx.waker().wake_by_ref(); // schedule us again 唤醒以重新调度
                    return;
                }
                // 如果连接空闲且不允许空闲时 ping，则跳过
                if !self.while_idle && is_idle {
                    trace!("keep-alive no need to ping when idle and while_idle=false");
                    return;
                }
                // 发送 keep-alive PING
                trace!("keep-alive interval ({:?}) reached", self.interval);
                shared.send_ping();
                self.state = KeepAliveState::PingSent;
                // 设置超时定时器
                let timeout = self.timer.now() + self.timeout;
                self.timer.reset(&mut self.sleep, timeout);
            }
            KeepAliveState::Init | KeepAliveState::PingSent => (),
        }
    }

    /// 检查 keep-alive 是否超时
    ///
    /// 仅在 `PingSent` 状态下检查。如果超时定时器到期，返回超时错误。
    fn maybe_timeout(&mut self, cx: &mut task::Context<'_>) -> Result<(), KeepAliveTimedOut> {
        match self.state {
            KeepAliveState::PingSent => {
                if Pin::new(&mut self.sleep).poll(cx).is_pending() {
                    return Ok(());
                }
                trace!("keep-alive timeout ({:?}) reached", self.timeout);
                Err(KeepAliveTimedOut)
            }
            KeepAliveState::Init | KeepAliveState::Scheduled(..) => Ok(()),
        }
    }
}

// ===== impl KeepAliveTimedOut =====

impl KeepAliveTimedOut {
    /// 将 KeepAliveTimedOut 转换为 hyper 的 crate::Error
    pub(super) fn crate_error(self) -> crate::Error {
        crate::Error::new(crate::error::Kind::Http2).with(self)
    }
}

/// 实现 Display trait，用于错误消息格式化
impl fmt::Display for KeepAliveTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("keep-alive timed out")
    }
}

/// 实现标准 Error trait，支持错误链
///
/// `source()` 返回 `TimedOut` 错误作为根因，
/// 这使得上层可以通过错误链追踪到超时的根本原因。
impl std::error::Error for KeepAliveTimedOut {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&crate::error::TimedOut)
    }
}
