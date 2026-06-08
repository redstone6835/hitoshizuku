//! 网络设备驱动 trait 与底层缓冲区抽象。
//!
//! 这是 `libs/net` 与具体硬件驱动之间的契约。接口只表达本协议栈需要的
//! 设备能力，不暴露任何用户态 ABI 或具体驱动模型：
//!
//! - 驱动只暴露**收发硬件帧**的操作，不感知 IP/TCP/UDP 协议。
//! - 接收端是被动的（协议栈主动 `poll_rx`），发送端是主动的
//!   （`alloc_tx` → 填充 → `commit_tx`）。
//! - 缓冲区所有权清晰：`RxBuf` 由驱动产出、协议栈消费；`TxBuf`
//!   由驱动分配、协议栈填充后归还驱动。
//! - 链路状态、MAC、MTU 等元数据通过单独的访问器查询，不放进每帧路径。
//!
//! 任何想接入本协议栈的硬件驱动只需要实现 [`NetDriver`]，无需关心
//! smoltcp 或者具体协议。`general/src/dev/drivers/virtio_net.rs` 是
//! 第一个实例。

use alloc::boxed::Box;
use core::any::Any;

// ── 链路状态 ─────────────────────────────────────────────────────────────────

/// 链路双工模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duplex {
    Half,
    Full,
}

/// 链路状态。
///
/// 协议栈在每次 poll 之前会查询一次，以决定是否驱动该接口。
/// 驱动应当从硬件实际状态（PHY 寄存器、VirtIO config 字段等）派生。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// 链路未就绪——网线未插、PHY 未协商完成、对端未连接。
    Down,
    /// 链路就绪。
    Up {
        /// 链路速率（兆比特每秒）。`None` 表示未知或未协商。
        speed_mbps: Option<u32>,
        /// 双工模式。
        duplex: Duplex,
    },
}

/// 设备向协议栈暴露的二层介质类型。
///
/// 普通网卡使用 Ethernet，loopback 这类没有二层头和 ARP 的虚拟接口使用 Ip。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMedium {
    Ethernet,
    Ip,
}

// ── 缓冲区抽象 ───────────────────────────────────────────────────────────────

/// 接收缓冲区。
///
/// 驱动 [`NetDriver::poll_rx`] 时把硬件已接收的一帧封装到此结构返回。
/// 协议栈消费完毕（拆包、转发、丢弃）后，缓冲区会自动 drop——驱动可以
/// 在内部 `Drop` 实现里把 DMA 描述符回收到接收环。
///
/// 当前实现为简单的 `Box<[u8]>`，未来可以扩展为引用计数 / 零拷贝 DMA
/// buffer，而不影响协议栈代码。
pub struct RxBuf {
    data: Box<[u8]>,
    len: usize,
}

impl RxBuf {
    /// 用一段堆分配的 `Box<[u8]>` 构造接收缓冲区。
    ///
    /// 帧长度自动裁剪为 `min(len, data.len(), MAX_FRAME_LEN)`——这是防御
    /// 深度的关键：即使硬件或驱动 bug 报告异常长度，上层也只能读到合法范围。
    ///
    /// # Panics
    ///
    /// `len > data.len()` 时 panic——这是驱动层的编程错误，不应静默截断。
    pub fn new(data: Box<[u8]>, len: usize) -> Self {
        assert!(
            len <= data.len(),
            "RxBuf::new: len ({}) exceeds buffer capacity ({})",
            len,
            data.len()
        );
        // 防御性截断：保护协议栈不被异常大帧攻击
        let safe_len = len.min(MAX_FRAME_LEN);
        Self {
            data,
            len: safe_len,
        }
    }

    /// 帧数据切片（只读）。
    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// 帧长度。
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 取出底层 `Box<[u8]>`（消费缓冲区）。
    ///
    /// 用于驱动内部 buffer 复用——比如把同一块 DMA 内存重新挂回 RX 环。
    pub fn into_storage(self) -> Box<[u8]> {
        self.data
    }
}

/// 发送缓冲区。
///
/// 驱动 [`NetDriver::alloc_tx`] 分配；协议栈填充数据后通过
/// [`NetDriver::commit_tx`] 归还，由驱动写到硬件 TX 队列。
///
/// 协议栈承诺：在 `commit_tx` 之前不会让 `TxBuf` 离开当前线程，也不会
/// 在持有 `TxBuf` 时进行可能阻塞的操作。
pub struct TxBuf {
    data: Box<[u8]>,
    /// 实际写入的字节数（由协议栈在填充后通过 [`set_len`](Self::set_len) 设置）。
    len: usize,
}

impl TxBuf {
    /// 创建一个新的发送缓冲区，初始 `len = 0`。
    pub fn new(data: Box<[u8]>) -> Self {
        Self { data, len: 0 }
    }

    /// 缓冲区总容量。
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// 当前已填充字节数。
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 协议栈用此切片向缓冲区写入帧数据。
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// 设置已写入字节数。
    ///
    /// # Panics
    ///
    /// `len > capacity()` 时 panic——这是协议栈的编程错误，不允许越界。
    /// 在 release 构建中也会触发，避免堆破坏。
    pub fn set_len(&mut self, len: usize) {
        assert!(
            len <= self.data.len(),
            "TxBuf::set_len: len ({}) exceeds capacity ({})",
            len,
            self.data.len()
        );
        self.len = len;
    }

    /// 已填充部分的只读视图。
    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// 取出底层 `Box<[u8]>`（消费缓冲区）。
    pub fn into_storage(self) -> Box<[u8]> {
        self.data
    }
}

// ── 帧长度上限 ───────────────────────────────────────────────────────────────

/// 协议栈接受的单帧最大长度（jumbo frame + 安全余量）。
///
/// 任何驱动报告 `len > MAX_FRAME_LEN` 的接收帧会被强制截断。
/// 这是防御深度——即使硬件或驱动 bug 报告异常长度，协议栈也不会读出
/// buffer 边界。
pub const MAX_FRAME_LEN: usize = 16 * 1024;

// ── 设备统计 ─────────────────────────────────────────────────────────────────

/// 网络设备统计计数器。
///
/// 高强度 I/O 场景下用于诊断丢包、错误率、吞吐量。所有字段单调递增——
/// 调用方对差值采样得到瞬时速率。
#[derive(Debug, Default, Clone, Copy)]
pub struct NetStats {
    /// 接收帧数。
    pub rx_packets: u64,
    /// 接收字节数。
    pub rx_bytes: u64,
    /// 接收错误数（CRC 错、长度错、硬件错）。
    pub rx_errors: u64,
    /// 接收丢弃数（队列满、buffer 不足）。
    pub rx_dropped: u64,
    /// 发送帧数。
    pub tx_packets: u64,
    /// 发送字节数。
    pub tx_bytes: u64,
    /// 发送错误数。
    pub tx_errors: u64,
    /// 发送丢弃数（背压时丢帧）。
    pub tx_dropped: u64,
}

// ── NetDriver trait ──────────────────────────────────────────────────────────

/// 网络设备驱动接口。
///
/// 硬件细节封装在驱动内部，上层（本 crate 的 [`adapter`](crate::adapter)）
/// 只通过这个 trait 与硬件交互。
///
/// **线程模型**：所有方法可在任意 CPU 上并发调用——驱动内部需要自己
/// 用 `Mutex` / 原子操作保护硬件队列和共享状态。
///
/// **错误语义**：本 trait 不返回错误——`poll_rx`/`alloc_tx` 在
/// 资源不可用时返回 `None`。硬件级故障（DMA 错误、寄存器异常）应当
/// 在驱动内部记录并通过 [`link_state`](Self::link_state) 切换为 `Down`。
pub trait NetDriver: Send + Sync {
    /// 当前驱动承载的链路介质。默认是普通以太网设备。
    fn medium(&self) -> LinkMedium {
        LinkMedium::Ethernet
    }

    /// 尝试从接收队列取出一帧。
    ///
    /// 返回 `Some(buf)` 表示成功取出一帧，协议栈应当解析并消费。
    /// 返回 `None` 表示当前没有已完成的接收帧（队列空）。
    ///
    /// 实现应当是非阻塞的：如果 RX 环没有 used descriptor，立刻返回 `None`。
    fn poll_rx(&self) -> Option<RxBuf>;

    /// 批量从接收队列取出多帧。
    ///
    /// 批量收包，减少高强度 I/O 下的函数调用开销和锁争用。
    /// 默认实现循环调 `poll_rx`——驱动可覆盖为单次锁定下批量取多帧的
    /// 优化版本。
    ///
    /// 调用方提供一个 buffer 数组 `out`，返回实际填充的数量（≤ `out.len()`）。
    /// 提前结束（队列空）会返回较小的数量。
    fn poll_rx_batch(&self, out: &mut [Option<RxBuf>]) -> usize {
        let mut count = 0;
        for slot in out.iter_mut() {
            match self.poll_rx() {
                Some(buf) => {
                    *slot = Some(buf);
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    /// 分配一个最大可写 `len` 字节的发送缓冲区。
    ///
    /// 返回 `None` 表示 TX 队列满 / 描述符耗尽——协议栈应当稍后重试。
    /// 实现可以选择：
    /// - 从 DMA 池分配（推荐，零拷贝）；
    /// - 临时 `Box::new(vec![0; len].into_boxed_slice())`（简单但有一次拷贝）。
    ///
    /// **安全约束**：实现必须拒绝 `len > MAX_FRAME_LEN`，返回 `None`。
    fn alloc_tx(&self, len: usize) -> Option<TxBuf>;

    /// 提交已填充的发送缓冲区。
    ///
    /// 调用此方法后，缓冲区所有权完全转移到驱动。驱动应：
    /// 1. 将 `buf.as_slice()` 中的数据拷贝/映射到 TX descriptor；
    /// 2. 通知设备开始发送（写 doorbell / kick）；
    /// 3. 在 TX 完成中断里回收描述符。
    fn commit_tx(&self, buf: TxBuf);

    /// 把未消费的接收缓冲区还给驱动。
    ///
    /// 协议栈可能因为帧错误、未匹配等原因不消费 `RxBuf`——此时调用本方法
    /// 让驱动有机会复用底层存储（如重新挂回 RX 环）。默认实现 drop 即可。
    fn recycle_rx(&self, _buf: RxBuf) {}

    /// 当前链路状态（同步查询，不应阻塞）。
    fn link_state(&self) -> LinkState;

    /// 设备 MAC 地址（6 字节，OUI + 设备号）。
    fn mac_address(&self) -> [u8; 6];

    /// 最大传输单元。默认 1500（标准以太网）。
    ///
    /// Jumbo frame 设备应当覆盖返回 9000 等更大值。协议栈在创建 smoltcp
    /// `Interface` 时会读取一次，运行期不变。
    fn mtu(&self) -> usize {
        1500
    }

    /// 向下转型支持。
    ///
    /// 用于在持有 `Arc<dyn NetDriver>` 时恢复具体驱动类型，
    /// 调用驱动私有的诊断/控制接口。实现者只需写 `fn as_any(&self) -> &dyn Any { self }`。
    fn as_any(&self) -> &dyn Any;

    /// 获取设备统计计数器快照。
    ///
    /// 用于高强度 I/O 场景下的性能诊断和监控。所有字段单调递增——
    /// 差值采样可得瞬时吞吐/丢包率。默认实现返回全零（驱动未实现统计）。
    fn stats(&self) -> NetStats {
        NetStats::default()
    }

    /// 禁用接收中断（进入主动 poll 模式时调用）。
    ///
    /// 高强度 I/O 场景下，协议栈在 poll 循环内禁用中断，批量处理
    /// RX 队列直到空，然后重新启用。避免每帧一次中断的开销。
    /// 默认实现为空——不支持中断控制的驱动无需覆盖。
    fn disable_rx_irq(&self) {}

    /// 启用接收中断（poll 循环结束、RX 队列空时调用）。
    fn enable_rx_irq(&self) {}
}
