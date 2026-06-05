//! 协议栈定时 poll 钩子。
//!
//! # 背景
//!
//! smoltcp 的协议栈是**纯被动**的——`NetStack::poll` 没人调就没人推进
//! 任何一帧。结果就是：
//!
//! 1. 网卡收到的包永远进不了 `SocketSet`（没人调 `Interface::poll` 把
//!    `Device::receive()` 拉回来的帧喂给 socket）；
//! 2. TCP 状态机不前进，连接卡在 SYN_SENT / SYN_RCVD 永远不 Established；
//! 3. soft-remove 标记的 socket 永远不真正从 `SocketSet` 摘掉，长期
//!    累积会让 `SocketHandle` index 单调增长到几千几万。
//!
//! 在用户态 netperf/iperf 场景下，三者全中：握手不完成，data 路径走不
//! 通，netperf 一直 retry，最后用户侧超时打印失败信息。
//!
//! # 方案
//!
//! 利用 HAL 层在 timer 中断路径上预留的"独立于 vDSO tick hook"的那
//! 条旁路——`hal::user::register_net_poll_hook`，每个 timer tick 由
//! 陷阱入口 [`arch::loongarch64::vdso::run_net_poll_hook`] 调一次
//! 本模块注册好的钩子 [`tick_net_poll`]，再转发到 `net::stack().poll()`。
//!
//! # 调频
//!
//! smoltcp 自己的 `Interface::poll` 在无包时几乎零成本（一次 mutex
//! 拿 + 几次内部状态读取）；本钩子直接每 tick 调一次，不再节流。如
//! 未来要节流（例如想减少锁争用），改成在钩子里用 `now_ns` 与上次
//! 记录比对（`>= 5ms` 才推一帧）即可。
//!
//! # wake-up
//!
//! `NetStack::poll` 内部在收尾会 `wake_all` 全局 socket 事件通知队列
//! ——所有阻塞在 `recv/send/accept/connect` 的用户任务都会重新检查
//! socket 状态，可读/可写/可 accept 的就接着走完 syscall 返回。

use core::sync::atomic::{AtomicU64, Ordering};

/// 协议栈 poll 节流：距上次 poll 不足此间隔则跳过。
///
/// timer tick 在 loongarch 上通常 100Hz~1kHz，但 smoltcp 的协议栈
/// 状态推进在 RX/TX 队列空时成本极低——但锁还是要拿，wake_socket_waiters
/// 也要走（即使队列空也走无害的 wake_all）。节流到 5ms 既能覆盖
/// 100Mbit~1Gbit 链路，又不会让 wake_socket_waiters 路径成为热点。
///
/// 同时也避免在每个 timer tick 都重做"遍历所有 socket + 检查状态"
/// 这种即便空载也要花时间的工作。
const NET_POLL_INTERVAL_NS: u64 = 5 * 1_000_000; // 5ms

/// 上一次 poll 的时间戳（纳秒）。初值 0 保证第一次 tick 一定执行。
static LAST_POLL_NS: AtomicU64 = AtomicU64::new(0);

/// Timer-tick 钩子：每 tick 推一帧协议栈。
///
/// 签名必须满足 `fn(u64)`——见
/// [`arch::loongarch64::vdso::register_net_poll_hook`]。`now_ns` 是当前
/// 物理时间戳（与 vDSO 那边一致），smoltcp 用 `Instant::from_millis` 接收。
pub fn tick_net_poll(now_ns: u64) {
    // 节流：与上次 poll 间隔不足 5ms 就跳过。
    let prev = LAST_POLL_NS.load(Ordering::Acquire);
    if now_ns.saturating_sub(prev) < NET_POLL_INTERVAL_NS && prev != 0 {
        return;
    }
    // 竞争更新：只有先到的才记下 LAST_POLL_NS；后到的会发现 prev 已经
    // 更新到比自己的 now_ns 还新的值，下一次 tick 才会被允许通过。
    if LAST_POLL_NS
        .compare_exchange(prev, now_ns, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    // smoltcp 的 Instant::from_millis 取毫秒，向下取整即可。
    net::stack().poll_ms((now_ns / 1_000_000) as i64);
}

/// 在 `main()` 早期注册本模块的钩子。重复注册会覆盖（与 vDSO hook 一致），
/// 但本内核只调一次。
pub fn register() {
    hal::user::register_net_poll_hook(tick_net_poll);
}
