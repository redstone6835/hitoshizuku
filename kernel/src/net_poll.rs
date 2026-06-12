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

/// 上一次 poll 的时间戳（纳秒），仅用于丢弃同一时间戳下的重复钩子。
static LAST_POLL_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Timer-tick 钩子：每 tick 推一帧协议栈。
///
/// 签名必须满足 `fn(u64)`——见
/// [`arch::loongarch64::vdso::register_net_poll_hook`]。`now_ns` 是当前
/// 物理时间戳（与 vDSO 那边一致），网络层会转换成协议引擎时间。
pub fn tick_net_poll(now_ns: u64) {
    if LAST_POLL_NS.swap(now_ns, Ordering::AcqRel) == now_ns {
        return;
    }
    // 不再按固定 5ms 节流。netperf TCP_CRR 这类高频短连接在 accept/recv
    // 睡眠后依赖 timer poll 唤醒，如果跳过多个 tick，每轮小请求都会被
    // 人为放大成毫秒级延迟，严重时看起来像卡死。
    if now_ns == 0 {
        return;
    }
    // 直接传纳秒时间戳，避免进入协议栈前先丢失毫秒以下精度。
    net::stack().poll_ns(now_ns);
    // 网络 poll 可能让对端任务变为 runnable（例如 TCP_CRR 的短连接
    // accept/recv/send 交替）。即使没有显式 socket waiter，也请求一次
    // 当前 CPU 重调度，避免对端等到其它定时任务触发后才运行。
    sched::request_resched(sched::current_cpu_id());
}

/// 在 `main()` 早期注册本模块的钩子。重复注册会覆盖（与 vDSO hook 一致），
/// 但本内核只调一次。
pub fn register() {
    hal::user::register_net_poll_hook(tick_net_poll);
}
