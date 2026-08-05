//! Platform-neutral DTB firmware parser.
//!
//! The independent [`fdt`] crate exposes validated views and a stable tree index.
//! This module turns that tree into normalized firmware descriptors that
//! kernel startup code can consume without hardcoding paths such as `/soc`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{fmt, str};
use spin::Mutex as SpinMutex;

use fdt::{
    AddressError as FdtAddressError, CellRangeMapping, CellValue, Fdt, GraphEndpoint, IdMap,
    MemoryDescription, NamedPhandleArgs, Node, NodeId, PhandleArgs, Tree,
};

use super::SerialPortInfo;
use super::power::{
    PowerAccessWidth, PowerControlInfo, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};

mod memory;
mod reserved;

pub use memory::{
    DtbMemoryLayout, DtbMemoryLayoutError, DtbNumaMemoryLayout, DtbResolvedReservedMemory,
    DtbUefiReservationError, apply_chosen_usable_ranges, apply_no_map_granule,
    described_memory_segments, resolve_memory_layout, split_numa_memory_segments,
    validate_uefi_reserved_memory,
};
pub use reserved::{
    DtbReservedDmaAllocation, DtbReservedMemoryDescriptor, DtbReservedMemoryError,
    DtbReservedMemoryHandle, DtbReservedMemoryRuntimeSnapshot, acquire_memory_region_by_index,
    acquire_memory_region_by_name, reserved_memory_runtime_snapshot,
};

/// 启动阶段解析出的 reserved-memory 运行期快照安装错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbReservedMemoryInstallError {
    /// 已经安装了内容不同的快照。
    AlreadyInstalled,
    /// 运行期租用表拒绝了有歧义或重叠的保留区。
    InvalidRuntime(DtbReservedMemoryError),
}

static RESERVED_MEMORY_SNAPSHOT: SpinMutex<Option<Box<[DtbResolvedReservedMemory]>>> =
    SpinMutex::new(None);

/// 启动 DT 全节点图安装错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbNodeGraphInstallError {
    AlreadyInstalled,
}

/// 开始 live 节点图事务时的错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbNodeGraphUpdateError {
    NotInstalled,
    BaselineMismatch,
    Busy,
}

struct DtbNodeGraphState {
    generation: u64,
    update_generation: Option<u64>,
    nodes: Option<Box<[DtbNodeInfo]>>,
}

static NODE_GRAPH_SNAPSHOT: SpinMutex<DtbNodeGraphState> = SpinMutex::new(DtbNodeGraphState {
    generation: 0,
    update_generation: None,
    nodes: None,
});

/// 已准备、尚未提交的 live 节点图更新。
///
/// guard 存活期间读者继续看到旧快照；未调用 [`DtbNodeGraphUpdate::commit`] 就释放
/// guard 会取消事务，使 overlay 的其它准备步骤可以直接用 `?` 回滚。
pub struct DtbNodeGraphUpdate {
    generation: u64,
    active: bool,
}

/// live 节点图提交结果。
pub struct DtbNodeGraphCommit {
    pub previous: Box<[DtbNodeInfo]>,
    pub generation: u64,
}

/// 安装启动 DT 的完整拥有型节点图。
pub fn install_node_graph(nodes: Vec<DtbNodeInfo>) -> Result<(), DtbNodeGraphInstallError> {
    let candidate = nodes.into_boxed_slice();
    let mut installed = NODE_GRAPH_SNAPSHOT.lock();
    if let Some(current) = installed.nodes.as_ref() {
        if current.as_ref() == candidate.as_ref() {
            return Ok(());
        }
        return Err(DtbNodeGraphInstallError::AlreadyInstalled);
    }
    installed.generation = 1;
    installed.nodes = Some(candidate);
    Ok(())
}

/// 返回当前 live 节点图 generation。
pub fn node_graph_generation() -> Option<u64> {
    let state = NODE_GRAPH_SNAPSHOT.lock();
    state.nodes.as_ref().map(|_| state.generation)
}

/// 为给定 generation 准备一次 live 节点图更新。
///
/// 准备阶段不隐藏旧图，也不持锁跨越驱动 probe；同一时刻只允许一个更新者。
pub fn begin_node_graph_update(
    expected_generation: u64,
) -> Result<DtbNodeGraphUpdate, DtbNodeGraphUpdateError> {
    let mut state = NODE_GRAPH_SNAPSHOT.lock();
    if state.nodes.is_none() {
        return Err(DtbNodeGraphUpdateError::NotInstalled);
    }
    if state.generation != expected_generation {
        return Err(DtbNodeGraphUpdateError::BaselineMismatch);
    }
    if state.update_generation.is_some() {
        return Err(DtbNodeGraphUpdateError::Busy);
    }
    state.update_generation = Some(expected_generation);
    Ok(DtbNodeGraphUpdate {
        generation: expected_generation,
        active: true,
    })
}

impl DtbNodeGraphUpdate {
    /// 原子提交已经完整验证并预分配的候选图。
    ///
    /// 合法 guard 独占当前 generation 的写权限，因此交换本身没有可恢复错误。旧图
    /// 返回给调用方并在锁外释放，避免大树析构阻塞固件读者。
    pub fn commit(mut self, nodes: Box<[DtbNodeInfo]>) -> DtbNodeGraphCommit {
        let mut state = NODE_GRAPH_SNAPSHOT.lock();
        assert_eq!(state.update_generation, Some(self.generation));
        assert_eq!(state.generation, self.generation);
        let previous = state
            .nodes
            .replace(nodes)
            .expect("prepared node graph update must retain its baseline");
        state.generation = state.generation.wrapping_add(1);
        if state.generation == 0 {
            state.generation = 1;
        }
        state.update_generation = None;
        let generation = state.generation;
        self.active = false;
        drop(state);
        DtbNodeGraphCommit {
            previous,
            generation,
        }
    }
}

impl Drop for DtbNodeGraphUpdate {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = NODE_GRAPH_SNAPSHOT.lock();
        if state.update_generation == Some(self.generation) {
            state.update_generation = None;
        }
    }
}

/// 返回所有 DT 节点的稳定拥有型快照，供总线 ELM 枚举专用子节点。
#[kernel_symbols::export(
    name = "general.firmware.dtb.node_graph_snapshot",
    contract = "kernel.general.dtb-node-graph@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn node_graph_snapshot() -> Vec<DtbNodeInfo> {
    NODE_GRAPH_SNAPSHOT
        .lock()
        .nodes
        .as_deref()
        .map_or_else(Vec::new, |nodes| nodes.to_vec())
}

/// 按绝对路径查询一个 DT 节点。
#[kernel_symbols::export(
    name = "general.firmware.dtb.node_by_path",
    contract = "kernel.general.dtb-node-graph@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn node_by_path(path: &str) -> Option<DtbNodeInfo> {
    NODE_GRAPH_SNAPSHOT
        .lock()
        .nodes
        .as_deref()
        .and_then(|nodes| nodes.iter().find(|node| node.path.as_ref() == path))
        .cloned()
}

/// 安装一次性 reserved-memory 运行期快照。
pub fn install_reserved_memory(
    regions: Vec<DtbResolvedReservedMemory>,
) -> Result<(), DtbReservedMemoryInstallError> {
    let candidate: Box<[DtbResolvedReservedMemory]> = regions.into_boxed_slice();
    let mut installed = RESERVED_MEMORY_SNAPSHOT.lock();
    if let Some(current) = installed.as_ref() {
        if current.as_ref() == candidate.as_ref() {
            return Ok(());
        }
        return Err(DtbReservedMemoryInstallError::AlreadyInstalled);
    }
    reserved::install_reserved_memory_runtime(&candidate)
        .map_err(DtbReservedMemoryInstallError::InvalidRuntime)?;
    *installed = Some(candidate);
    Ok(())
}

/// 返回 reserved-memory 运行期快照的拥有副本。
pub fn reserved_memory_snapshot() -> Vec<DtbResolvedReservedMemory> {
    RESERVED_MEMORY_SNAPSHOT
        .lock()
        .as_deref()
        .map_or_else(Vec::new, |regions| regions.to_vec())
}

/// 按 phandle 查询一个 reserved-memory 运行期快照。
pub fn reserved_memory_by_phandle(phandle: u32) -> Option<DtbResolvedReservedMemory> {
    RESERVED_MEMORY_SNAPSHOT
        .lock()
        .as_deref()
        .and_then(|regions| find_reserved_memory_by_phandle(regions, phandle))
}

fn find_reserved_memory_by_phandle(
    regions: &[DtbResolvedReservedMemory],
    phandle: u32,
) -> Option<DtbResolvedReservedMemory> {
    regions
        .iter()
        .find(|region| region.request.phandle == Some(phandle))
        .cloned()
}

const COMPAT_SYSCON_POWEROFF: &[u8] = b"syscon-poweroff";
const COMPAT_SYSCON_REBOOT: &[u8] = b"syscon-reboot";
const COMPAT_NS16550: &[u8] = b"ns16550";
const COMPAT_NS16550A: &[u8] = b"ns16550a";
const COMPAT_PCI_ECAM: &[u8] = b"pci-host-ecam-generic";
const COMPAT_PCIE_ECAM: &[u8] = b"pcie-host-ecam-generic";
const COMPAT_PCI_CAM: &[u8] = b"pci-host-cam-generic";
const COMPAT_SIMPLE_BUS: &[u8] = b"simple-bus";
const COMPAT_SIMPLE_MFD: &[u8] = b"simple-mfd";
const COMPAT_SIMPLE_PM_BUS: &[u8] = b"simple-pm-bus";
const COMPAT_QEMU_PLATFORM: &[u8] = b"qemu,platform";
const COMPAT_ARM_AMBA_BUS: &[u8] = b"arm,amba-bus";
const PCI_DMA_SPACE_MASK: u32 = 0x0300_0000;
const PCI_DMA_MEMORY32: u32 = 0x0200_0000;
const PCI_DMA_MEMORY64: u32 = 0x0300_0000;

#[derive(Clone, Copy)]
enum DmaChildAddressKind {
    Generic,
    Pci,
}

fn cell_value_to_usize(value: &CellValue) -> Option<usize> {
    usize::try_from(value.to_u128()?).ok()
}

fn dma_child_address_to_usize(value: &CellValue, kind: DmaChildAddressKind) -> Option<usize> {
    match kind {
        DmaChildAddressKind::Generic => cell_value_to_usize(value),
        DmaChildAddressKind::Pci => {
            let [phys_hi, middle, low] = value.cells() else {
                return None;
            };
            if !matches!(
                phys_hi & PCI_DMA_SPACE_MASK,
                PCI_DMA_MEMORY32 | PCI_DMA_MEMORY64
            ) {
                return None;
            }
            usize::try_from((u64::from(*middle) << 32) | u64::from(*low)).ok()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbMmioRangeInfo {
    pub phys_addr: usize,
    pub size: usize,
}

/// 一个无损保留、可选择投影为本机 MMIO 的 `reg` 条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbRegEntry {
    pub address: DtbCellValue,
    /// `#size-cells == 0` 时保持 `None`。
    pub size: Option<DtbCellValue>,
    /// 普通总线地址成功翻译到根地址空间时保留完整宽度结果。
    pub translated_address: Option<DtbCellValue>,
    /// 仅当地址、非零 size 和区间都能由本机 `usize` 完整表示时存在。
    pub mmio: Option<DtbMmioRangeInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbInterruptInfo {
    /// `interrupt-names` 中与该 specifier 同索引的稳定名称。
    pub name: Option<Box<str>>,
    /// interrupt provider 在索引树中的稳定编号。
    pub provider: NodeId,
    /// provider 的绝对路径；在临时索引树销毁后仍可用于稳定识别。
    pub provider_path: Box<str>,
    pub parent: Option<u32>,
    pub specifier: Box<[u32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbDeviceProperty {
    pub name: Box<str>,
    /// DTB property 的完整原始值。具体 binding 的消费者负责选择解码类型。
    pub value: Box<[u8]>,
}

/// 一个已经按 provider binding 完整切分的 DT 引用。
///
/// `property` 保留引用来自哪个原始属性；同一种 provider 类型可能由多个属性
/// 表达（尤其是任意 `*-gpios`），驱动不能只根据 `#*-cells` 猜测语义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbProviderReference {
    pub property: Box<str>,
    pub name: Option<Box<str>>,
    /// provider 在本次 DT 索引树中的树内编号；跨重解析/overlay 请使用路径。
    pub provider: Option<NodeId>,
    /// provider 的绝对路径；phandle 0 空槽为 `None`。
    pub provider_path: Option<Box<str>>,
    /// 最终 provider 的 `status` 是否可用；空槽为 `None`。
    pub provider_available: Option<bool>,
    /// nexus 翻译后的最终 provider phandle；标准空槽保留为 0。
    pub phandle: u32,
    /// 按最终 provider `#*-cells` 边界完整切分、已应用 nexus map 的参数。
    pub args: Box<[u32]>,
}

/// 任意宽度 DT cell 数值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbCellValue {
    cells: Box<[u32]>,
}

impl DtbCellValue {
    pub fn cells(&self) -> &[u32] {
        &self.cells
    }
}

/// 一个无损的 `dma-ranges` 映射。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbDmaRangeMapping {
    pub child_address: DtbCellValue,
    pub parent_address: DtbCellValue,
    /// `#size-cells == 0` 时没有 size 字段，不能伪造为零长度。
    pub size: Option<DtbCellValue>,
}

/// 已组合到 CPU 根物理地址空间的有效 DMA 窗口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbDmaWindow {
    pub cpu_start: usize,
    pub dma_start: usize,
    pub size: usize,
}

/// 设备最终可执行的 DMA 固件策略。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DtbEffectiveDmaInfo {
    pub coherent: bool,
    /// `None` 表示没有固件窗口约束；`Some([])` 表示显式 identity。
    pub windows: Option<Vec<DtbDmaWindow>>,
    /// 非空的直接 `iommus` 或无法由通用总线安全求值的 requester map 要求隔离域。
    pub iommu_required: bool,
    /// 标准 DT 关系存在，但 provider 不可用或当前内核无法安全表示/求值。
    pub unsupported: bool,
}

/// `iommu-map` 中的一条 requester-ID 映射。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbIommuMapEntry {
    pub input_base: u32,
    pub provider: NodeId,
    pub provider_path: Box<str>,
    pub provider_phandle: u32,
    pub output_base: Box<[u32]>,
    pub length: u32,
    pub provider_available: bool,
}

/// 完整的 `iommu-map` 与掩码。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbIommuMap {
    pub mask: u32,
    pub entries: Vec<DtbIommuMapEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbIommuMapMatch<'a> {
    pub entry: &'a DtbIommuMapEntry,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbMappedIommuId<'a> {
    pub provider: NodeId,
    pub provider_path: &'a str,
    pub provider_phandle: u32,
    pub args: Vec<u32>,
    pub provider_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DtbIommuMapTranslationError {
    OutputOverflow {
        provider: NodeId,
        provider_phandle: u32,
        output_base: u32,
        offset: u32,
    },
    AmbiguousMultiCellRange {
        provider: NodeId,
        provider_phandle: u32,
        cells: usize,
        length: u32,
        offset: u32,
    },
}

impl DtbIommuMap {
    /// 应用 mask 并返回第一个命中项，不解释 provider 定义的输出 cell。
    pub fn match_id(&self, requester_id: u32) -> Option<DtbIommuMapMatch<'_>> {
        let requester_id = requester_id & self.mask;
        self.entries.iter().find_map(|entry| {
            let offset = requester_id.checked_sub(entry.input_base)?;
            if offset >= entry.length {
                return None;
            }
            Some(DtbIommuMapMatch { entry, offset })
        })
    }

    /// 仅执行 DT 通用层能够无歧义定义的 requester-ID 翻译。
    pub fn map_id(
        &self,
        requester_id: u32,
    ) -> Result<Option<DtbMappedIommuId<'_>>, DtbIommuMapTranslationError> {
        let Some(matched) = self.match_id(requester_id) else {
            return Ok(None);
        };
        let entry = matched.entry;
        let args = match entry.output_base.as_ref() {
            [] => Vec::new(),
            &[output_base] => Vec::from([output_base.checked_add(matched.offset).ok_or(
                DtbIommuMapTranslationError::OutputOverflow {
                    provider: entry.provider,
                    provider_phandle: entry.provider_phandle,
                    output_base,
                    offset: matched.offset,
                },
            )?]),
            _ if entry.length == 1 => entry.output_base.to_vec(),
            output_base => {
                return Err(DtbIommuMapTranslationError::AmbiguousMultiCellRange {
                    provider: entry.provider,
                    provider_phandle: entry.provider_phandle,
                    cells: output_base.len(),
                    length: entry.length,
                    offset: matched.offset,
                });
            }
        };
        Ok(Some(DtbMappedIommuId {
            provider: entry.provider,
            provider_path: &entry.provider_path,
            provider_phandle: entry.provider_phandle,
            args,
            provider_available: entry.provider_available,
        }))
    }
}

/// 一个 DT graph endpoint 及其规范化远端关系。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbGraphEndpoint {
    pub node: NodeId,
    pub node_path: Box<str>,
    pub port: NodeId,
    pub port_path: Box<str>,
    pub port_id: Option<u32>,
    pub endpoint_id: Option<u32>,
    pub phandle: Option<u32>,
    pub remote: Option<NodeId>,
    pub remote_path: Option<Box<str>>,
    pub remote_phandle: Option<u32>,
}

/// platform 驱动可直接消费的 DT binding 关系。
///
/// 所有字段都在设备进入平台总线前完成全属性校验；任意一项失败都会让整个固件
/// 解析失败，不会向驱动暴露只解析了一半的依赖集合。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DtbPlatformBindings {
    pub references: Vec<DtbProviderReference>,
    /// `None` 表示属性缺失，`Some([])` 表示显式 identity DMA 映射。
    pub dma_ranges: Option<Vec<DtbDmaRangeMapping>>,
    pub iommu_map: Option<DtbIommuMap>,
    pub graph_endpoints: Vec<DtbGraphEndpoint>,
    pub effective_dma: DtbEffectiveDmaInfo,
}

#[derive(Debug)]
pub struct DtbPlatformDeviceInfo {
    pub name: Box<str>,
    pub path: Box<str>,
    pub parent_path: Option<Box<str>>,
    pub phandle: Option<u32>,
    /// 沿 DT 父链解析出的有效 NUMA node ID。
    pub numa_node_id: Option<u32>,
    pub interrupt_parent: Option<u32>,
    pub address_cells: usize,
    pub size_cells: usize,
    pub parent_address_cells: usize,
    pub parent_size_cells: usize,
    pub compatible: Vec<Box<str>>,
    pub reg_entries: Vec<DtbRegEntry>,
    pub reg_ranges: Vec<DtbMmioRangeInfo>,
    pub interrupts: Vec<DtbInterruptInfo>,
    pub interrupt_controller: bool,
    pub clock_hz: Option<u32>,
    pub bindings: DtbPlatformBindings,
    pub properties: Vec<DtbDeviceProperty>,
}

/// 一份可由 PCI host ELM 在实际 BDF/INTx key 上重复求值的拥有型 DT 路由快照。
///
/// 快照与产生它的 overlay candidate 绑定，不依赖全局 live tree；blob 中的
/// `/chosen` 启动 seed 已在构造时移除。私有字段防止 ELM 伪造宽度或替换 blob。
#[derive(Clone)]
struct DtbPciInterruptResolverInner {
    blob: Arc<[u8]>,
    host_path: Box<str>,
    child_address_cells: usize,
    child_interrupt_cells: usize,
}

pub struct DtbPciInterruptResolver {
    inner: Arc<DtbPciInterruptResolverInner>,
}

impl DtbPciInterruptResolver {
    fn new(
        blob: Arc<[u8]>,
        host_path: Box<str>,
        child_address_cells: usize,
        child_interrupt_cells: usize,
    ) -> Self {
        Self {
            inner: Arc::new(DtbPciInterruptResolverInner {
                blob,
                host_path,
                child_address_cells,
                child_interrupt_cells,
            }),
        }
    }
}

#[kernel_symbols::export]
impl DtbPciInterruptResolver {
    /// 为 ELM 路由表复制一个共享同代 blob 的拥有型 resolver。
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbPciInterruptResolver.clone_owned",
        contract = "kernel.general.dtb-pci-interrupt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn clone_owned(&self) -> Self {
        self.clone()
    }
}

impl Clone for DtbPciInterruptResolver {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl fmt::Debug for DtbPciInterruptResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DtbPciInterruptResolver")
            .field("host_path", &self.inner.host_path)
            .field("child_address_cells", &self.inner.child_address_cells)
            .field("child_interrupt_cells", &self.inner.child_interrupt_cells)
            .finish_non_exhaustive()
    }
}

impl PartialEq for DtbPciInterruptResolver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
            || (self.inner.host_path == other.inner.host_path
                && self.inner.child_address_cells == other.inner.child_address_cells
                && self.inner.child_interrupt_cells == other.inner.child_interrupt_cells
                && (Arc::ptr_eq(&self.inner.blob, &other.inner.blob)
                    || self.inner.blob.as_ref() == other.inner.blob.as_ref()))
    }
}

impl Eq for DtbPciInterruptResolver {}

#[kernel_symbols::export]
impl Drop for DtbPciInterruptResolver {
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbPciInterruptResolver.drop",
        contract = "kernel.general.dtb-pci-interrupt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        // 字段 drop glue 在常驻内核中释放 Arc，ELM 不直接接触 blob 所有权。
    }
}

/// 对拥有型 PCI interrupt resolver 求值时的稳定错误边界。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbPciInterruptResolveError {
    /// resolver 内部的去敏 FDT 已损坏或无法重新建立索引。
    InvalidSnapshot,
    /// 快照中不存在 resolver 构造时记录的 host 路径。
    MissingHost,
    /// 调用方提供的 child key 宽度与 PCI host binding 不一致。
    InvalidKey {
        /// host 声明的 child address cell 数。
        address_expected: usize,
        /// 调用方提供的 child address cell 数。
        address_actual: usize,
        /// host 声明的 child interrupt cell 数。
        interrupt_expected: usize,
        /// 调用方提供的 child interrupt cell 数。
        interrupt_actual: usize,
    },
    /// 快照重新解析出的 host 宽度与 resolver 元数据不一致。
    ResolverMismatch,
    /// PCI interrupt-map 或后续 interrupt nexus 无法完成规范翻译。
    InvalidMap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPcieHostInfo {
    pub name: Box<str>,
    pub path: Box<str>,
    /// 沿 host 父链继承后的 NUMA 分配亲和节点。
    pub numa_node_id: Option<u32>,
    pub ecam_phys: usize,
    pub ecam_size: usize,
    pub config_space: DtbPciConfigSpace,
    pub domain: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    pub dma_coherent: bool,
    pub address_cells: usize,
    pub interrupt_cells: usize,
    pub ranges: Vec<DtbPciRangeInfo>,
    pub interrupt_map_mask: Option<Box<[u32]>>,
    /// 原始 `interrupt-map-pass-thru`；缺失与显式全零必须可区分。
    pub interrupt_map_pass_thru: Option<Box<[u32]>>,
    /// 与该 host 所属 DT generation 绑定的动态 INTx 路由解析器。
    pub interrupt_resolver: Option<DtbPciInterruptResolver>,
    pub interrupt_map: Vec<DtbPciInterruptMapEntry>,
    pub msi_map_present: bool,
    pub msi_map_mask: u32,
    pub msi_map: Vec<DtbPciMsiMapEntry>,
    pub msi_parents: Vec<DtbMsiParent>,
    /// PCI child address cell（含空间标志）必须按原编码宽度保存。
    pub dma_ranges: Option<Vec<DtbDmaRangeMapping>>,
    pub iommu_map: Option<DtbIommuMap>,
    pub iommus: Vec<DtbProviderReference>,
    pub graph_endpoints: Vec<DtbGraphEndpoint>,
    pub effective_dma: DtbEffectiveDmaInfo,
    pub children: Vec<DtbPciChildInfo>,
}

/// Generic PCI host 的标准配置空间布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbPciConfigSpace {
    /// Conventional PCI CAM：每条 bus 占 64 KiB，每个 function 暴露 256 bytes。
    Cam,
    /// PCIe ECAM：每条 bus 占 1 MiB，每个 function 暴露 4 KiB。
    Ecam,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciChildInfo {
    pub name: Box<str>,
    pub path: Box<str>,
    pub parent_path: Option<Box<str>>,
    pub phandle: Option<u32>,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub compatible: Vec<Box<str>>,
    pub reg_entries: Vec<DtbRegEntry>,
    pub interrupts: Vec<DtbInterruptInfo>,
    pub properties: Vec<DtbDeviceProperty>,
    pub bindings: DtbPlatformBindings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciRangeInfo {
    pub space: DtbPciAddressSpace,
    pub phys_hi: u32,
    pub memory_64: bool,
    pub relocatable: bool,
    pub aliased: bool,
    pub child_addr: u64,
    pub parent_addr: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbPciAddressSpace {
    Io,
    Memory,
    PrefetchableMemory,
    Unknown(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciMsiMapEntry {
    pub requester_base: u32,
    pub controller: u32,
    pub msi_specifier: Box<[u32]>,
    pub length: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbMsiParent {
    pub controller: u32,
    pub msi_specifier: Box<[u32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciInterruptMapEntry {
    pub child_address: Box<[u32]>,
    pub child_interrupt: Box<[u32]>,
    /// `interrupt-map` 行中编码的直接父 nexus。
    pub parent: u32,
    pub parent_address: Box<[u32]>,
    pub parent_specifier: Box<[u32]>,
    /// pass-thru 全零时预先折叠出的最终 IRQ 域；非零时必须动态求值并保持 `None`。
    pub resolved: Option<DtbPciInterruptRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciInterruptRoute {
    pub parent: u32,
    pub parent_address: Box<[u32]>,
    pub parent_specifier: Box<[u32]>,
}

/// 在实际 PCI child address/interrupt key 上执行完整 interrupt-map 翻译。
///
/// 每次调用只借用 resolver 内不可变的去敏快照，重新建立 FDT 索引并递归应用所有
/// nexus map。任何快照、宽度或 map 错误均显式失败，调用方不得回退到未翻译 IRQ。
#[kernel_symbols::export(
    name = "general.firmware.dtb.resolve_pci_interrupt",
    contract = "kernel.general.dtb-pci-interrupt@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn resolve_pci_interrupt(
    resolver: &DtbPciInterruptResolver,
    child_address: &[u32],
    child_interrupt: &[u32],
) -> Result<Option<DtbPciInterruptRoute>, DtbPciInterruptResolveError> {
    let inner = resolver.inner.as_ref();
    if child_address.len() != inner.child_address_cells
        || child_interrupt.len() != inner.child_interrupt_cells
    {
        return Err(DtbPciInterruptResolveError::InvalidKey {
            address_expected: inner.child_address_cells,
            address_actual: child_address.len(),
            interrupt_expected: inner.child_interrupt_cells,
            interrupt_actual: child_interrupt.len(),
        });
    }

    let fdt = Fdt::parse(inner.blob.as_ref())
        .map_err(|_| DtbPciInterruptResolveError::InvalidSnapshot)?;
    let tree = Tree::from_fdt(fdt).map_err(|_| DtbPciInterruptResolveError::InvalidSnapshot)?;
    let host = tree
        .find_node(&inner.host_path)
        .ok_or(DtbPciInterruptResolveError::MissingHost)?;
    let Some(map) = tree
        .pci_interrupt_map(host)
        .map_err(|_| DtbPciInterruptResolveError::InvalidMap)?
    else {
        return Ok(None);
    };
    if map.child_address_cells != inner.child_address_cells
        || map.child_interrupt_cells != inner.child_interrupt_cells
    {
        return Err(DtbPciInterruptResolveError::ResolverMismatch);
    }
    tree.resolve_pci_interrupt(&map, child_address, child_interrupt)
        .map(|route| {
            route.map(|route| DtbPciInterruptRoute {
                parent: route.provider_phandle,
                parent_address: route.address.into_boxed_slice(),
                parent_specifier: route.specifier.into_boxed_slice(),
            })
        })
        .map_err(|_| DtbPciInterruptResolveError::InvalidMap)
}

/// 一棵已索引 DT 中任意节点的拥有型规范化快照。
///
/// 该层不把 I2C chip-select、SPI 地址或 PCI phys.hi 强行伪装为 platform MMIO；
/// 总线 ELM 可以按 `path`/`parent_path` 恢复拓扑，再消费原始属性、无损 `reg` 和
/// 已切分 provider binding。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbNodeInfo {
    pub node: NodeId,
    pub name: Box<str>,
    pub path: Box<str>,
    pub parent_path: Option<Box<str>>,
    pub phandle: Option<u32>,
    /// 沿父链继承后的有效 NUMA node ID。
    pub numa_node_id: Option<u32>,
    pub enabled: bool,
    pub compatible: Vec<Box<str>>,
    pub address_cells: usize,
    pub size_cells: usize,
    pub parent_address_cells: usize,
    pub parent_size_cells: usize,
    pub interrupt_controller: bool,
    pub reg_entries: Vec<DtbRegEntry>,
    pub interrupts: Vec<DtbInterruptInfo>,
    pub bindings: DtbPlatformBindings,
    pub properties: Vec<DtbDeviceProperty>,
}

#[derive(Debug)]
pub struct DtbFirmwareInfo {
    pub root_compatible: Vec<Box<str>>,
    pub cpu_count: usize,
    pub cpus: Vec<DtbCpuInfo>,
    pub numa: DtbNumaInfo,
    /// 无损的 DT 内存来源与保留请求；启动协议策略在内核入口统一解析。
    pub memory: MemoryDescription,
    pub external_initramfs_range: Option<(usize, usize)>,
    pub rng_seed: Option<Box<[u8]>>,
    pub stdout_serial: Option<SerialPortInfo>,
    pub power_controls: PowerControlInfo,
    pub serial_ports: Vec<SerialPortInfo>,
    pub nodes: Vec<DtbNodeInfo>,
    pub platform_devices: Vec<DtbPlatformDeviceInfo>,
    pub pcie_hosts: Vec<DtbPcieHostInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbCpuInfo {
    pub logical_id: u32,
    pub reg: u64,
    pub phandle: Option<u32>,
    pub numa_node_id: Option<u32>,
    pub interrupt_controller_phandles: Box<[u32]>,
    pub compatible: Vec<Box<str>>,
    pub socket_id: Option<u32>,
    pub cluster_path: Box<[u32]>,
    pub core_id: Option<u32>,
    pub thread_id: Option<u32>,
    pub capacity_dmips_mhz: Option<u32>,
}

/// 一个 DT 节点直接声明的 NUMA 归属。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbNumaAssignment {
    pub path: Box<str>,
    pub node_id: u32,
}

/// NUMA 节点之间的一条固件距离。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbNumaDistance {
    pub from: u32,
    pub to: u32,
    pub distance: u32,
}

/// 与临时 FDT 索引解耦的 NUMA 固件快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DtbNumaInfo {
    pub assignments: Vec<DtbNumaAssignment>,
    pub distances: Vec<DtbNumaDistance>,
}

impl DtbNumaInfo {
    pub fn node_id_for_path(&self, path: &str) -> Option<u32> {
        self.assignments
            .iter()
            .find(|assignment| assignment.path.as_ref() == path)
            .map(|assignment| assignment.node_id)
    }

    pub fn distance(&self, from: u32, to: u32) -> Option<u32> {
        self.distances
            .iter()
            .find(|entry| entry.from == from && entry.to == to)
            .or_else(|| {
                self.distances
                    .iter()
                    .find(|entry| entry.from == to && entry.to == from)
            })
            .map(|entry| entry.distance)
            .or_else(|| (from == to).then_some(fdt::NUMA_LOCAL_DISTANCE))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DtbFirmwareError {
    InvalidTree,
    InvalidMemory(fdt::MemoryError),
    InvalidNuma(fdt::NumaError),
    InvalidAddress(fdt::AddressError),
    InvalidInterrupt(fdt::InterruptError),
    InvalidMsi(fdt::MsiError),
    InvalidPci(fdt::PciError),
    InvalidSpecifier(fdt::SpecifierError),
    InvalidIdMap(fdt::IdMapError),
    InvalidGraph(fdt::GraphError),
    InvalidCellAddress(fdt::CellAddressError),
    DmaParentCycle(NodeId),
    InvalidProperty {
        node: NodeId,
        property: &'static str,
        error: fdt::PropertyError,
    },
    InvalidValue {
        node: NodeId,
        property: &'static str,
    },
    NativeAddressOverflow {
        node: NodeId,
        property: &'static str,
    },
}

/// 从任意生命周期的 DTB 字节建立固件摘要时的分层错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DtbFirmwareBlobError {
    InvalidFdt(fdt::Error),
    InvalidFirmware(DtbFirmwareError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AddressRange {
    start: usize,
    size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DtbAddressError {
    MissingReg,
    InvalidReg,
    UnsupportedCells,
    UnsupportedBus,
    UnmatchedRange,
    Overflow,
}

#[derive(Clone)]
struct DtbCpuMapEntry {
    cpu: u32,
    socket_id: Option<u32>,
    cluster_path: Box<[u32]>,
    core_id: Option<u32>,
    thread_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DtbCpuMapNodeKind {
    Socket,
    Cluster,
    Core,
    Thread,
}

struct DtbCpuMapWork {
    node: NodeId,
    kind: DtbCpuMapNodeKind,
    socket_id: Option<u32>,
    cluster_path: Vec<u32>,
    core_id: Option<u32>,
    thread_id: Option<u32>,
}

struct DtbTree<'a> {
    tree: Tree<'a>,
    enabled: Vec<bool>,
    pci_interrupt_blob: Arc<[u8]>,
}

pub fn parse(dtb: Fdt<'_>) -> Result<DtbFirmwareInfo, DtbFirmwareError> {
    let pci_interrupt_blob = sanitized_pci_interrupt_blob(dtb)?;
    let tree = DtbTree::new(dtb, pci_interrupt_blob)?;

    let root_compatible = tree.root_compatible();
    let cpus = tree.cpus()?;
    let numa = tree.numa()?;
    let cpu_count = cpus.len().max(1);
    let stdout_serial = tree.stdout_serial();
    let power_controls = tree.power_controls();
    let external_initramfs_range = tree.external_initramfs_range();
    let rng_seed = tree.rng_seed();
    let memory = tree
        .tree
        .memory_description()
        .map_err(DtbFirmwareError::InvalidMemory)?;
    let nodes = tree.firmware_nodes()?;

    Ok(DtbFirmwareInfo {
        root_compatible,
        cpu_count,
        cpus,
        numa,
        memory,
        external_initramfs_range,
        rng_seed,
        stdout_serial,
        power_controls,
        serial_ports: tree.serial_ports(),
        nodes,
        platform_devices: tree.platform_devices()?,
        pcie_hosts: tree.pcie_hosts()?,
    })
}

/// 解析由调用方临时持有的完整 DTB blob。
pub fn parse_blob(blob: &[u8]) -> Result<DtbFirmwareInfo, DtbFirmwareBlobError> {
    let dtb = Fdt::parse(blob).map_err(DtbFirmwareBlobError::InvalidFdt)?;
    parse(dtb).map_err(DtbFirmwareBlobError::InvalidFirmware)
}

fn sanitized_pci_interrupt_blob(dtb: Fdt<'_>) -> Result<Arc<[u8]>, DtbFirmwareError> {
    const FDT_NOP_BYTES: [u8; 4] = 4u32.to_be_bytes();

    let mut blob = dtb.as_bytes().to_vec();
    let secret_records = dtb
        .root()
        .children()
        .filter(|node| matches!(node.name(), "chosen" | "chosen@0"))
        .flat_map(|chosen| {
            chosen
                .properties()
                .filter(|property| matches!(property.name(), "rng-seed" | "kaslr-seed"))
                .map(|property| property.encoded_structure_range())
        })
        .collect::<Vec<_>>();
    let structure_start = dtb.header().off_dt_struct as usize;
    for encoded in secret_records {
        let start = structure_start
            .checked_add(encoded.start)
            .ok_or(DtbFirmwareError::InvalidTree)?;
        let end = structure_start
            .checked_add(encoded.end)
            .ok_or(DtbFirmwareError::InvalidTree)?;
        let record = blob
            .get_mut(start..end)
            .ok_or(DtbFirmwareError::InvalidTree)?;
        if !record.len().is_multiple_of(4) {
            return Err(DtbFirmwareError::InvalidTree);
        }
        for token in record.chunks_exact_mut(4) {
            token.copy_from_slice(&FDT_NOP_BYTES);
        }
    }
    Fdt::parse(&blob).map_err(|_| DtbFirmwareError::InvalidTree)?;
    Ok(blob.into())
}

impl<'a> DtbTree<'a> {
    fn numa(&self) -> Result<DtbNumaInfo, DtbFirmwareError> {
        let description = self
            .tree
            .numa_description()
            .map_err(DtbFirmwareError::InvalidNuma)?;
        Ok(DtbNumaInfo {
            assignments: description
                .assignments
                .into_iter()
                .map(|assignment| DtbNumaAssignment {
                    path: self.path(assignment.node).into_boxed_str(),
                    node_id: assignment.node_id,
                })
                .collect(),
            distances: description
                .distances
                .into_iter()
                .map(|distance| DtbNumaDistance {
                    from: distance.from,
                    to: distance.to,
                    distance: distance.distance,
                })
                .collect(),
        })
    }

    fn new(dtb: Fdt<'a>, pci_interrupt_blob: Arc<[u8]>) -> Result<Self, DtbFirmwareError> {
        let tree = Tree::from_fdt(dtb).map_err(|_| DtbFirmwareError::InvalidTree)?;
        let mut enabled = Vec::with_capacity(tree.len());
        for node_id in tree.node_ids() {
            let parent_enabled = tree
                .parent(node_id)
                .map_or(true, |parent| enabled[parent.index()]);
            let local_enabled = tree
                .is_available(node_id)
                .map_err(|_| DtbFirmwareError::InvalidTree)?;
            enabled.push(parent_enabled && local_enabled);
        }
        Ok(Self {
            tree,
            enabled,
            pci_interrupt_blob,
        })
    }

    fn root_compatible(&self) -> Vec<Box<str>> {
        compatible_strings(self.node(self.tree.root_id()))
    }

    fn firmware_nodes(&self) -> Result<Vec<DtbNodeInfo>, DtbFirmwareError> {
        let mut nodes = Vec::new();
        nodes
            .try_reserve(self.tree.len())
            .map_err(|_| DtbFirmwareError::InvalidTree)?;
        for node_id in self.tree.node_ids() {
            let enabled = self.is_enabled(node_id);
            let node = self.node(node_id);
            let compatible = if enabled {
                self.compatible_strings_strict(node_id)?
            } else {
                compatible_strings(node)
            };
            let interrupt_controller =
                if enabled {
                    match node.property("interrupt-controller") {
                        None => false,
                        Some(property) => property.as_bool().map_err(|error| {
                            DtbFirmwareError::InvalidProperty {
                                node: node_id,
                                property: "interrupt-controller",
                                error,
                            }
                        })?,
                    }
                } else {
                    false
                };
            let under_pci = enabled && self.is_under_pci_host(node_id)?;
            let reg_entries = if enabled {
                // `reg` 的元组布局由所属总线 binding 决定；graph port、I2C/SPI
                // 子节点等不一定使用普通地址+size 格式。全节点图保留原始属性，
                // 只有能按普通总线无损解码的节点才附加结构化投影。
                self.normalized_reg_entries(node_id, !under_pci)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let interrupts = if enabled {
                self.normalized_interrupts(node_id)?
            } else {
                Vec::new()
            };
            let bindings = if enabled {
                self.platform_bindings(
                    node_id,
                    if under_pci {
                        DmaChildAddressKind::Pci
                    } else {
                        DmaChildAddressKind::Generic
                    },
                )?
            } else {
                DtbPlatformBindings::default()
            };
            let parent = self.tree.parent(node_id);
            nodes.push(DtbNodeInfo {
                node: node_id,
                name: self.node_name_or_path(node_id),
                path: self.path(node_id).into_boxed_str(),
                parent_path: parent.map(|parent| self.path(parent).into_boxed_str()),
                phandle: self.tree.phandle(node_id),
                numa_node_id: self
                    .tree
                    .effective_numa_node_id(node_id)
                    .map_err(DtbFirmwareError::InvalidNuma)?,
                enabled,
                compatible,
                address_cells: self.cell_count(node_id, "#address-cells", 2)?,
                size_cells: self.cell_count(node_id, "#size-cells", 1)?,
                parent_address_cells: parent
                    .map(|parent| self.cell_count(parent, "#address-cells", 2))
                    .transpose()?
                    .unwrap_or(0),
                parent_size_cells: parent
                    .map(|parent| self.cell_count(parent, "#size-cells", 1))
                    .transpose()?
                    .unwrap_or(0),
                interrupt_controller,
                reg_entries,
                interrupts,
                bindings,
                properties: raw_properties(node),
            });
        }
        Ok(nodes)
    }

    fn cpus(&self) -> Result<Vec<DtbCpuInfo>, DtbFirmwareError> {
        let topology = self.cpu_map_entries();
        let mut capacities = Vec::new();
        for node_id in self.all_cpu_node_ids() {
            let capacity =
                match self.node(node_id).property("capacity-dmips-mhz") {
                    Some(property) => Some(property.as_u32().map_err(|error| {
                        DtbFirmwareError::InvalidProperty {
                            node: node_id,
                            property: "capacity-dmips-mhz",
                            error,
                        }
                    })?),
                    None => None,
                };
            capacities.push((node_id, capacity));
        }
        // Linux binding 规定 capacity-dmips-mhz 是 all-or-nothing；缺失或零值时
        // 整机统一退回默认容量，避免只给部分 CPU 加权。
        let use_firmware_capacity = !capacities.is_empty()
            && capacities
                .iter()
                .all(|(_, capacity)| capacity.is_some_and(|value| value != 0));
        let mut cpus = Vec::new();
        for node_id in self.cpu_node_ids() {
            let node = self.node(node_id);
            let phandle = self.tree.phandle(node_id);
            let topology =
                phandle.and_then(|phandle| topology.iter().find(|entry| entry.cpu == phandle));
            let logical_id = u32::try_from(cpus.len()).unwrap_or(u32::MAX);
            cpus.push(DtbCpuInfo {
                logical_id,
                reg: self.read_cpu_reg(node_id)?,
                phandle,
                numa_node_id: self
                    .tree
                    .numa_node_id(node_id)
                    .map_err(DtbFirmwareError::InvalidNuma)?,
                interrupt_controller_phandles: self.cpu_interrupt_controller_phandles(node_id)?,
                compatible: compatible_strings(node),
                socket_id: topology.and_then(|entry| entry.socket_id),
                cluster_path: topology
                    .map(|entry| entry.cluster_path.clone())
                    .unwrap_or_default(),
                core_id: topology.and_then(|entry| entry.core_id),
                thread_id: topology.and_then(|entry| entry.thread_id),
                capacity_dmips_mhz: use_firmware_capacity.then(|| {
                    capacities
                        .iter()
                        .find(|(cpu, _)| *cpu == node_id)
                        .and_then(|(_, capacity)| *capacity)
                        .expect("complete CPU capacity table must contain every CPU node")
                }),
            });
        }
        Ok(cpus)
    }

    fn cpu_interrupt_controller_phandles(
        &self,
        cpu: NodeId,
    ) -> Result<Box<[u32]>, DtbFirmwareError> {
        let mut providers = Vec::new();
        for &child in self.children(cpu) {
            if !self.is_enabled(child) {
                continue;
            }
            let Some(property) = self.node(child).property("interrupt-controller") else {
                continue;
            };
            property
                .as_bool()
                .map_err(|error| DtbFirmwareError::InvalidProperty {
                    node: child,
                    property: "interrupt-controller",
                    error,
                })?;
            let phandle = self
                .tree
                .phandle(child)
                .ok_or(DtbFirmwareError::InvalidValue {
                    node: child,
                    property: "phandle",
                })?;
            if providers.contains(&phandle) {
                return Err(DtbFirmwareError::InvalidValue {
                    node: child,
                    property: "phandle",
                });
            }
            providers.push(phandle);
        }
        Ok(providers.into_boxed_slice())
    }

    fn cpu_map_entries(&self) -> Vec<DtbCpuMapEntry> {
        let Some(cpus_id) = self.cpus_node_id() else {
            return Vec::new();
        };
        let mut roots = self
            .children(cpus_id)
            .iter()
            .copied()
            .filter(|&node_id| self.node(node_id).name() == "cpu-map");
        let Some(root_id) = roots.next() else {
            return Vec::new();
        };
        if roots.next().is_some()
            || !self.is_enabled(root_id)
            || self.node(root_id).property("cpu").is_some()
        {
            return Vec::new();
        }

        let mut entries = Vec::new();
        let Some(root_children) = self.cpu_map_children(root_id, None) else {
            return Vec::new();
        };
        if root_children.is_empty() {
            return Vec::new();
        }
        let mut stack = Vec::new();
        for (node, kind, id) in root_children.into_iter().rev() {
            let (socket_id, cluster_path) = match kind {
                DtbCpuMapNodeKind::Socket => (Some(id), Vec::new()),
                DtbCpuMapNodeKind::Cluster => (None, alloc::vec![id]),
                DtbCpuMapNodeKind::Core | DtbCpuMapNodeKind::Thread => return Vec::new(),
            };
            stack.push(DtbCpuMapWork {
                node,
                kind,
                socket_id,
                cluster_path,
                core_id: None,
                thread_id: None,
            });
        }

        while let Some(work) = stack.pop() {
            let cpu_property = self.node(work.node).property("cpu");
            let cpu = cpu_property.and_then(|property| property.as_u32().ok());
            if cpu_property.is_some() && cpu.is_none() {
                return Vec::new();
            }
            let Some(children) = self.cpu_map_children(work.node, Some(work.kind)) else {
                return Vec::new();
            };

            match work.kind {
                DtbCpuMapNodeKind::Socket | DtbCpuMapNodeKind::Cluster => {
                    if cpu.is_some() || children.is_empty() {
                        return Vec::new();
                    }
                    for (node, kind, id) in children.into_iter().rev() {
                        let mut cluster_path = work.cluster_path.clone();
                        let mut core_id = work.core_id;
                        match kind {
                            DtbCpuMapNodeKind::Cluster => cluster_path.push(id),
                            DtbCpuMapNodeKind::Core => core_id = Some(id),
                            DtbCpuMapNodeKind::Socket | DtbCpuMapNodeKind::Thread => {
                                return Vec::new();
                            }
                        }
                        stack.push(DtbCpuMapWork {
                            node,
                            kind,
                            socket_id: work.socket_id,
                            cluster_path,
                            core_id,
                            thread_id: None,
                        });
                    }
                }
                DtbCpuMapNodeKind::Core => match (cpu, children.is_empty()) {
                    (Some(cpu), true) => entries.push(DtbCpuMapEntry {
                        cpu,
                        socket_id: work.socket_id,
                        cluster_path: work.cluster_path.into_boxed_slice(),
                        core_id: work.core_id,
                        thread_id: None,
                    }),
                    (None, false) => {
                        for (node, kind, id) in children.into_iter().rev() {
                            if kind != DtbCpuMapNodeKind::Thread {
                                return Vec::new();
                            }
                            stack.push(DtbCpuMapWork {
                                node,
                                kind,
                                socket_id: work.socket_id,
                                cluster_path: work.cluster_path.clone(),
                                core_id: work.core_id,
                                thread_id: Some(id),
                            });
                        }
                    }
                    _ => return Vec::new(),
                },
                DtbCpuMapNodeKind::Thread => {
                    let (Some(cpu), true) = (cpu, children.is_empty()) else {
                        return Vec::new();
                    };
                    entries.push(DtbCpuMapEntry {
                        cpu,
                        socket_id: work.socket_id,
                        cluster_path: work.cluster_path.into_boxed_slice(),
                        core_id: work.core_id,
                        thread_id: work.thread_id,
                    });
                }
            }
        }

        let all_cpus: Vec<NodeId> = self.all_cpu_node_ids().collect();
        if entries.len() != all_cpus.len() {
            return Vec::new();
        }
        let mut seen = Vec::new();
        for entry in &entries {
            if seen.contains(&entry.cpu) {
                return Vec::new();
            }
            let Some(target) = self.tree.node_by_phandle(entry.cpu) else {
                return Vec::new();
            };
            if self.tree.parent(target) != Some(cpus_id) || !self.node_is_cpu(target) {
                return Vec::new();
            }
            seen.push(entry.cpu);
        }
        if all_cpus.iter().any(|cpu| {
            self.tree
                .phandle(*cpu)
                .is_none_or(|phandle| !seen.contains(&phandle))
        }) {
            return Vec::new();
        }
        entries
    }

    fn cpu_map_children(
        &self,
        node_id: NodeId,
        parent: Option<DtbCpuMapNodeKind>,
    ) -> Option<Vec<(NodeId, DtbCpuMapNodeKind, u32)>> {
        let mut children = Vec::new();
        for &child in self.children(node_id) {
            let (kind, id) = cpu_map_node_name(self.node(child).name())?;
            let allowed = match parent {
                None => matches!(kind, DtbCpuMapNodeKind::Socket | DtbCpuMapNodeKind::Cluster),
                Some(DtbCpuMapNodeKind::Socket | DtbCpuMapNodeKind::Cluster) => {
                    matches!(kind, DtbCpuMapNodeKind::Cluster | DtbCpuMapNodeKind::Core)
                }
                Some(DtbCpuMapNodeKind::Core) => kind == DtbCpuMapNodeKind::Thread,
                Some(DtbCpuMapNodeKind::Thread) => false,
            };
            if !allowed
                || children
                    .first()
                    .is_some_and(|(_, first_kind, _)| *first_kind != kind)
            {
                return None;
            }
            children.push((child, kind, id));
        }
        let mut ids: Vec<u32> = children.iter().map(|(_, _, id)| *id).collect();
        ids.sort_unstable();
        if ids
            .iter()
            .enumerate()
            .any(|(expected, id)| *id != expected as u32)
        {
            return None;
        }
        Some(children)
    }

    fn stdout_serial(&self) -> Option<SerialPortInfo> {
        let node_id = self.tree.chosen_stdout().ok().flatten()?.node;
        if !self.is_enabled(node_id) || !self.node_is_serial(node_id) {
            return None;
        }
        let node = self.node(node_id);
        let range = self.first_reg_range(node_id)?;
        Some(SerialPortInfo {
            name: self.node_name_or_path(node_id),
            phys_addr: range.start,
            reg_size: Some(range.size),
            clock_hz: read_clock_hz(node),
            baud: read_current_speed(node),
        })
    }

    fn power_controls(&self) -> PowerControlInfo {
        PowerControlInfo {
            shutdown: self.syscon_power_action(COMPAT_SYSCON_POWEROFF),
            reboot: self.syscon_power_action(COMPAT_SYSCON_REBOOT),
        }
    }

    fn syscon_power_action(&self, compatible: &[u8]) -> Option<PowerControlMethod> {
        let action_id = self.tree.node_ids().find(|&node_id| {
            self.is_enabled(node_id) && compatible_contains(self.node(node_id), compatible)
        })?;
        let action_node = self.node(action_id);
        let regmap_phandle = read_be_u32_prop(action_node.find_property("regmap")?.value())?;
        let offset = read_usize_scalar(action_node.find_property("offset")?.value())?;
        let value = read_u64_scalar(action_node.find_property("value")?.value())?;
        let regmap_id = self.tree.node_by_phandle(regmap_phandle)?;
        if !self.is_enabled(regmap_id) {
            return None;
        }

        let regmap_node = self.node(regmap_id);
        let range = self.first_reg_range(regmap_id)?;
        let width_bytes = regmap_node
            .find_property("reg-io-width")
            .and_then(|prop| read_usize_scalar(prop.value()))
            .or_else(|| {
                if range.size != 0 && offset < range.size && range.size - offset < 4 {
                    Some(1)
                } else {
                    Some(4)
                }
            })?;
        let access_width = PowerAccessWidth::from_bytes(width_bytes)?;
        let address = range.start.checked_add(offset)?;

        Some(PowerControlMethod::RegisterWrite {
            register: PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address,
                access_width,
            },
            value,
        })
    }

    fn external_initramfs_range(&self) -> Option<(usize, usize)> {
        let chosen = self.chosen_node()?;
        let start = read_usize_scalar(chosen.find_property("linux,initrd-start")?.value())?;
        let end = read_usize_scalar(chosen.find_property("linux,initrd-end")?.value())?;
        (end > start).then_some((start, end))
    }

    fn rng_seed(&self) -> Option<Box<[u8]>> {
        let chosen = self.chosen_node()?;
        let seed = chosen.find_property("rng-seed")?.value();
        (!seed.is_empty()).then(|| seed.to_vec().into_boxed_slice())
    }

    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        let mut ports = Vec::new();
        for node_id in self.tree.node_ids() {
            if !self.is_enabled(node_id) || !self.node_is_serial(node_id) {
                continue;
            }
            let Some(range) = self.first_reg_range(node_id) else {
                continue;
            };
            if ports
                .iter()
                .any(|port: &SerialPortInfo| port.phys_addr == range.start)
            {
                continue;
            }
            ports.push(SerialPortInfo {
                name: self.node_name_or_path(node_id),
                phys_addr: range.start,
                reg_size: Some(range.size),
                clock_hz: read_clock_hz(self.node(node_id)),
                baud: read_current_speed(self.node(node_id)),
            });
        }
        ports
    }

    fn platform_devices(&self) -> Result<Vec<DtbPlatformDeviceInfo>, DtbFirmwareError> {
        let mut devices = Vec::new();
        let mut pending = Vec::new();
        pending.extend(self.children(self.tree.root_id()).iter().rev().copied());

        while let Some(node_id) = pending.pop() {
            if !self.is_enabled(node_id) {
                continue;
            }
            if self.is_under_pci_host(node_id)? {
                continue;
            }
            let node = self.node(node_id);
            let compatible = self.compatible_strings_strict(node_id)?;
            if compatible.is_empty() {
                // 与 Linux 的 strict OF platform population 一致：没有 compatible
                // 的容器不是 platform bus，也不能据此递归枚举其 binding 子节点。
                // 这会把 /cpus、reserved-memory、I2C/SPI/MDIO 等命名空间留给
                // 对应的专用解析器，避免把 hart ID、片选或从地址伪造成 MMIO。
                continue;
            };
            let populate_children = is_default_platform_bus(&compatible);
            let interrupt_controller = match node.property("interrupt-controller") {
                None => false,
                Some(property) => {
                    property
                        .as_bool()
                        .map_err(|error| DtbFirmwareError::InvalidProperty {
                            node: node_id,
                            property: "interrupt-controller",
                            error,
                        })?
                }
            };
            let reg_entries = self.normalized_reg_entries(node_id, true)?;
            let reg_ranges = reg_entries.iter().filter_map(|entry| entry.mmio).collect();
            let interrupts = self.normalized_interrupts(node_id)?;
            let interrupt_parent = interrupts
                .first()
                .and_then(|interrupt| interrupt.parent)
                .or(self
                    .tree
                    .interrupt_provider(node_id)
                    .map_err(DtbFirmwareError::InvalidInterrupt)?
                    .and_then(|provider| self.tree.phandle(provider)));
            let bindings = self.platform_bindings(node_id, DmaChildAddressKind::Generic)?;
            devices.push(DtbPlatformDeviceInfo {
                name: self.node_name_or_path(node_id),
                path: self.path(node_id).into_boxed_str(),
                parent_path: self
                    .tree
                    .parent(node_id)
                    .map(|parent| self.path(parent).into_boxed_str()),
                phandle: self.tree.phandle(node_id),
                numa_node_id: self
                    .tree
                    .effective_numa_node_id(node_id)
                    .map_err(DtbFirmwareError::InvalidNuma)?,
                interrupt_parent,
                address_cells: self.cell_count(node_id, "#address-cells", 2)?,
                size_cells: self.cell_count(node_id, "#size-cells", 1)?,
                parent_address_cells: self
                    .tree
                    .parent(node_id)
                    .map(|parent| self.cell_count(parent, "#address-cells", 2))
                    .transpose()?
                    .unwrap_or(0),
                parent_size_cells: self
                    .tree
                    .parent(node_id)
                    .map(|parent| self.cell_count(parent, "#size-cells", 1))
                    .transpose()?
                    .unwrap_or(0),
                compatible,
                reg_entries,
                reg_ranges,
                interrupts,
                interrupt_controller,
                clock_hz: read_clock_hz(node),
                bindings,
                properties: raw_properties(node),
            });

            if populate_children {
                pending.extend(self.children(node_id).iter().rev().copied());
            }
        }
        Ok(devices)
    }

    /// 原子解析一个 platform 节点的通用 provider、DMA、IOMMU 与 graph binding。
    fn platform_bindings(
        &self,
        node_id: NodeId,
        dma_address_kind: DmaChildAddressKind,
    ) -> Result<DtbPlatformBindings, DtbFirmwareError> {
        let mut references = Vec::new();

        append_named_references(
            &mut references,
            "clocks",
            self.tree
                .clocks(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "assigned-clocks",
            self.tree
                .mapped_phandle_array(node_id, "assigned-clocks", "clock")
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "assigned-clock-parents",
            self.tree
                .mapped_phandle_array(node_id, "assigned-clock-parents", "clock")
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "resets",
            self.tree
                .resets(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "dmas",
            self.tree
                .dmas(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "iommus",
            self.tree
                .iommus(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "phys",
            self.tree
                .phys(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "power-domains",
            self.tree
                .named_mapped_phandle_array(
                    node_id,
                    "power-domains",
                    "power-domain",
                    "power-domain-names",
                )
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "interconnects",
            self.tree
                .named_mapped_phandle_array(
                    node_id,
                    "interconnects",
                    "interconnect",
                    "interconnect-names",
                )
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "pwms",
            self.tree
                .named_mapped_phandle_array(node_id, "pwms", "pwm", "pwm-names")
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "mboxes",
            self.tree
                .mboxes(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "io-channels",
            self.tree
                .io_channels(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "thermal-sensors",
            self.tree
                .thermal_sensors(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "sound-dai",
            self.tree
                .sound_dais(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "memory-region",
            self.tree
                .named_memory_regions(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_named_references(
            &mut references,
            "nvmem-cells",
            self.tree
                .named_nvmem_cells(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "operating-points-v2",
            self.tree
                .operating_points(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "interrupt-affinity",
            self.tree
                .interrupt_affinity(node_id)
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        append_references(
            &mut references,
            "wakeup-parent",
            self.tree
                .phandle_list(node_id, "wakeup-parent")
                .map_err(DtbFirmwareError::InvalidSpecifier)?,
        );
        if let Some(parents) = self
            .tree
            .msi_parents(node_id)
            .map_err(DtbFirmwareError::InvalidMsi)?
        {
            references.extend(parents.into_iter().map(|parent| DtbProviderReference {
                property: "msi-parent".into(),
                name: None,
                provider: Some(parent.controller),
                provider_path: None,
                provider_available: None,
                phandle: parent.controller_phandle,
                args: parent.msi_specifier.into_boxed_slice(),
            }));
        }

        // GPIO、regulator supply 与 pinctrl state 的前缀/编号属于具体设备 binding，
        // 必须动态发现并保留原始属性名。
        let dynamic_properties = self
            .node(node_id)
            .properties()
            .map(|property| property.name())
            .collect::<Vec<_>>();
        for &property in dynamic_properties
            .iter()
            .filter(|name| **name == "gpios" || name.ends_with("-gpios"))
        {
            append_references(
                &mut references,
                property,
                self.tree
                    .gpios(node_id, property)
                    .map_err(DtbFirmwareError::InvalidSpecifier)?,
            );
        }
        for &property in dynamic_properties
            .iter()
            .filter(|name| name.ends_with("-supply"))
        {
            append_references(
                &mut references,
                property,
                self.tree
                    .phandle_list(node_id, property)
                    .map_err(DtbFirmwareError::InvalidSpecifier)?,
            );
        }
        let pinctrl_names = self
            .node(node_id)
            .property("pinctrl-names")
            .map(|property| {
                property
                    .as_string_list()
                    .map(|names| names.map(String::from).collect::<Vec<_>>())
                    .map_err(|error| DtbFirmwareError::InvalidProperty {
                        node: node_id,
                        property: "pinctrl-names",
                        error,
                    })
            })
            .transpose()?;
        for (&property, state) in dynamic_properties
            .iter()
            .filter_map(|property| pinctrl_state_index(property).map(|state| (property, state)))
        {
            let entries = self
                .tree
                .phandle_list(node_id, property)
                .map_err(DtbFirmwareError::InvalidSpecifier)?;
            let Some(entries) = entries else {
                continue;
            };
            references.extend(entries.into_iter().map(|entry| {
                provider_reference(
                    property,
                    pinctrl_names
                        .as_ref()
                        .and_then(|names| names.get(state))
                        .cloned()
                        .map(String::into_boxed_str),
                    entry,
                )
            }));
        }
        for reference in &mut references {
            reference.provider_path = reference
                .provider
                .map(|provider| self.path(provider).into_boxed_str());
            reference.provider_available =
                reference.provider.map(|provider| self.is_enabled(provider));
        }

        let dma_ranges = self
            .tree
            .dma_ranges_cells(node_id)
            .map_err(DtbFirmwareError::InvalidCellAddress)?
            .map(|ranges| ranges.into_iter().map(map_dma_range).collect());
        let iommu_map = self
            .tree
            .iommu_map(node_id)
            .map_err(DtbFirmwareError::InvalidIdMap)?
            .map(|map| self.map_iommu_map(map));
        let graph_endpoints = self
            .tree
            .graph_endpoints(node_id)
            .map_err(DtbFirmwareError::InvalidGraph)?
            .into_iter()
            .map(|endpoint| map_graph_endpoint(&self.tree, endpoint))
            .collect();

        let effective_dma = self.effective_dma_info(
            node_id,
            self.dma_parent(node_id)?,
            &references,
            dma_address_kind,
        )?;
        Ok(DtbPlatformBindings {
            references,
            dma_ranges,
            iommu_map,
            graph_endpoints,
            effective_dma,
        })
    }

    fn effective_dma_info(
        &self,
        device: NodeId,
        start: Option<NodeId>,
        references: &[DtbProviderReference],
        address_kind: DmaChildAddressKind,
    ) -> Result<DtbEffectiveDmaInfo, DtbFirmwareError> {
        let coherent = self.dma_is_coherent(device)?;
        let direct_iommu_required = references.iter().any(|reference| {
            reference.property.as_ref() == "iommus" && reference.provider.is_some()
        });
        let direct_iommu_unavailable = references.iter().any(|reference| {
            reference.property.as_ref() == "iommus"
                && reference.provider.is_some()
                && reference.provider_available != Some(true)
        });
        let generic_map_required = self.validate_dma_parent_iommu_maps(start, address_kind)?;
        let (windows, window_unsupported) = self.effective_dma_windows(start, address_kind)?;
        Ok(DtbEffectiveDmaInfo {
            coherent,
            windows,
            iommu_required: direct_iommu_required || generic_map_required,
            unsupported: direct_iommu_unavailable || window_unsupported || generic_map_required,
        })
    }

    /// 严格校验完整 DMA parent 链上的 `iommu-map`，并判断 generic requester
    /// 是否存在当前内核无法安全推导的有效 IOMMU 映射。
    ///
    /// PCI requester ID 可由 BDF 精确生成，host/function 的实际命中由 PCI 层
    /// 处理；这里仍遍历并校验整条链，但不能把 host map 变成全局阻断。generic
    /// 总线尚无稳定 requester-ID 抽象，只要任一 map 项指向可用 provider，就必须
    /// fail closed。指向 disabled provider 的项不生效，设备仍可使用 `dma-ranges`。
    fn validate_dma_parent_iommu_maps(
        &self,
        mut current: Option<NodeId>,
        address_kind: DmaChildAddressKind,
    ) -> Result<bool, DtbFirmwareError> {
        let mut generic_map_required = false;
        let mut visited = Vec::new();
        while let Some(bus) = current {
            if visited.contains(&bus) {
                return Err(DtbFirmwareError::DmaParentCycle(bus));
            }
            visited.push(bus);
            if let Some(map) = self
                .tree
                .iommu_map(bus)
                .map_err(DtbFirmwareError::InvalidIdMap)?
            {
                if matches!(address_kind, DmaChildAddressKind::Generic)
                    && map
                        .entries
                        .iter()
                        .any(|entry| self.is_enabled(entry.provider))
                {
                    generic_map_required = true;
                }
            }
            current = self.dma_parent(bus)?;
        }
        Ok(generic_map_required)
    }

    fn dma_is_coherent(&self, node: NodeId) -> Result<bool, DtbFirmwareError> {
        let mut current = Some(node);
        let mut visited = Vec::new();
        while let Some(node_id) = current {
            if visited.contains(&node_id) {
                return Err(DtbFirmwareError::DmaParentCycle(node_id));
            }
            visited.push(node_id);
            let node = self.node(node_id);
            if let Some(property) = node.property("dma-coherent") {
                property
                    .as_bool()
                    .map_err(|error| DtbFirmwareError::InvalidProperty {
                        node: node_id,
                        property: "dma-coherent",
                        error,
                    })?;
                return Ok(true);
            }
            if let Some(property) = node.property("dma-noncoherent") {
                property
                    .as_bool()
                    .map_err(|error| DtbFirmwareError::InvalidProperty {
                        node: node_id,
                        property: "dma-noncoherent",
                        error,
                    })?;
                return Ok(false);
            }
            current = self.dma_parent(node_id)?;
        }
        Ok(false)
    }

    fn dma_parent(&self, node: NodeId) -> Result<Option<NodeId>, DtbFirmwareError> {
        let dma_mem = self
            .tree
            .named_mapped_phandle_array(node, "interconnects", "interconnect", "interconnect-names")
            .map_err(DtbFirmwareError::InvalidSpecifier)?
            .and_then(|entries| {
                entries
                    .into_iter()
                    .find(|entry| entry.name.as_deref() == Some("dma-mem"))
            })
            .and_then(|entry| entry.specifier.provider);
        Ok(dma_mem.or_else(|| self.tree.parent(node)))
    }

    fn effective_dma_windows(
        &self,
        mut current: Option<NodeId>,
        address_kind: DmaChildAddressKind,
    ) -> Result<(Option<Vec<DtbDmaWindow>>, bool), DtbFirmwareError> {
        let mut saw_identity = false;
        let mut visited = Vec::new();
        while let Some(bus) = current {
            if visited.contains(&bus) {
                return Err(DtbFirmwareError::DmaParentCycle(bus));
            }
            visited.push(bus);
            let node = self.node(bus);
            let Some(property) = node.property("dma-ranges") else {
                // `dma-ranges` 约束由最近一个声明该属性的 DMA parent 提供；普通
                // 中间总线省略属性时继续向上查找，不能把祖先窗口误判为无限制。
                current = self.dma_parent(bus)?;
                continue;
            };
            // 先严格校验 boolean/tuple 编码；空属性表示本级 identity，继续寻找上级
            // 非空窗口并与其组合。
            let mappings = self
                .tree
                .dma_ranges_cells(bus)
                .map_err(DtbFirmwareError::InvalidCellAddress)?
                .expect("property presence was checked");
            if property.value().is_empty() {
                debug_assert!(mappings.is_empty());
                saw_identity = true;
                current = self.dma_parent(bus)?;
                continue;
            }

            let Some(parent) = self.tree.parent(bus) else {
                return Ok((None, true));
            };
            let mut windows = Vec::new();
            for mapping in mappings {
                let Some(size_value) = mapping.size.as_ref() else {
                    return Ok((None, true));
                };
                let Some(size) = cell_value_to_usize(size_value) else {
                    return Ok((None, true));
                };
                let Some(dma_start) =
                    dma_child_address_to_usize(&mapping.child_address, address_kind)
                else {
                    return Ok((None, true));
                };
                let cpu_address = self
                    .tree
                    .translate_dma_address_cells(
                        parent,
                        &mapping.parent_address,
                        mapping.size.as_ref(),
                    )
                    .map_err(DtbFirmwareError::InvalidCellAddress)?;
                let Some(cpu_start) = cell_value_to_usize(&cpu_address) else {
                    return Ok((None, true));
                };
                if size == 0
                    || cpu_start.checked_add(size).is_none()
                    || dma_start.checked_add(size).is_none()
                {
                    return Ok((None, true));
                }
                windows.push(DtbDmaWindow {
                    cpu_start,
                    dma_start,
                    size,
                });
            }
            return Ok((Some(windows), false));
        }
        Ok((saw_identity.then(Vec::new), false))
    }

    fn pci_children(
        &self,
        host: NodeId,
        bus_start: u8,
        bus_end: u8,
    ) -> Result<Vec<DtbPciChildInfo>, DtbFirmwareError> {
        let mut result = Vec::new();
        let mut pending = self.children(host).to_vec();
        while let Some(node_id) = pending.pop() {
            pending.extend_from_slice(self.children(node_id));
            if !self.is_enabled(node_id) {
                continue;
            }
            let node = self.node(node_id);
            let Some(reg) = node.property("reg") else {
                continue;
            };
            // PCI function `reg` 由 3 address + 2 size cells 组成。host 下的 graph
            // port/endpoint 也常有单-cell `reg`，不能把它误当成 BDF。
            if reg.value().len() < 20 || !reg.value().len().is_multiple_of(20) {
                continue;
            }
            let entries = self
                .tree
                .reg_cells(node_id)
                .map_err(DtbFirmwareError::InvalidCellAddress)?;
            let Some(address) = entries.first().map(|entry| &entry.address) else {
                continue;
            };
            let [phys_hi, _, _] = address.cells() else {
                continue;
            };
            let bus = ((phys_hi >> 16) & 0xff) as u8;
            let device = ((phys_hi >> 11) & 0x1f) as u8;
            let function = ((phys_hi >> 8) & 0x7) as u8;
            if bus < bus_start || bus > bus_end {
                continue;
            }
            result.push(DtbPciChildInfo {
                name: self.node_name_or_path(node_id),
                path: self.path(node_id).into_boxed_str(),
                parent_path: self
                    .tree
                    .parent(node_id)
                    .map(|parent| self.path(parent).into_boxed_str()),
                phandle: self.tree.phandle(node_id),
                bus,
                device,
                function,
                compatible: self.compatible_strings_strict(node_id)?,
                reg_entries: self.normalized_reg_entries(node_id, false)?,
                interrupts: self.normalized_interrupts(node_id)?,
                properties: raw_properties(node),
                bindings: self.platform_bindings(node_id, DmaChildAddressKind::Pci)?,
            });
        }
        result.sort_by_key(|child| (child.bus, child.device, child.function));
        Ok(result)
    }

    fn pcie_hosts(&self) -> Result<Vec<DtbPcieHostInfo>, DtbFirmwareError> {
        let mut hosts = Vec::new();
        for node_id in self.tree.node_ids() {
            if !self.is_enabled(node_id) {
                continue;
            }
            let compatible = self.compatible_strings_strict(node_id)?;
            let Some(config_space) = compatible.iter().find_map(|value| {
                if value.as_bytes() == COMPAT_PCI_CAM {
                    Some(DtbPciConfigSpace::Cam)
                } else if value.as_bytes() == COMPAT_PCI_ECAM
                    || value.as_bytes() == COMPAT_PCIE_ECAM
                {
                    Some(DtbPciConfigSpace::Ecam)
                } else {
                    None
                }
            }) else {
                continue;
            };

            let (ecam_phys, ecam_size) = self.pci_config_range(node_id)?;
            let (bus_start, bus_end) = self.pci_bus_range(node_id)?;
            let bytes_per_bus = match config_space {
                DtbPciConfigSpace::Cam => 1 << 16,
                DtbPciConfigSpace::Ecam => 1 << 20,
            };
            let required_ecam = (usize::from(bus_end) - usize::from(bus_start) + 1)
                .checked_mul(bytes_per_bus)
                .ok_or_else(|| pci_overflow(node_id, "reg", 0))?;
            if ecam_size < required_ecam {
                return Err(DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                    node: node_id,
                    property: "reg",
                    entry: 0,
                    value: ecam_size as u128,
                }));
            }
            let domain = self.pci_domain(node_id)?;
            let ranges = self
                .tree
                .pci_ranges(node_id)
                .map_err(DtbFirmwareError::InvalidPci)?
                .ok_or(DtbFirmwareError::InvalidPci(
                    fdt::PciError::MissingRequired {
                        node: node_id,
                        property: "ranges",
                    },
                ))?;
            if ranges.is_empty() {
                return Err(DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                    node: node_id,
                    property: "ranges",
                    entry: 0,
                    value: 0,
                }));
            }
            let ranges = ranges
                .into_iter()
                .map(|range| {
                    let parent_addr = usize::try_from(range.parent_address)
                        .map_err(|_| pci_overflow(node_id, "ranges", 0))?;
                    let size = usize::try_from(range.size)
                        .map_err(|_| pci_overflow(node_id, "ranges", 0))?;
                    let space = match range.space {
                        fdt::PciAddressSpace::Io => DtbPciAddressSpace::Io,
                        fdt::PciAddressSpace::Memory32 | fdt::PciAddressSpace::Memory64 => {
                            if range.prefetchable {
                                DtbPciAddressSpace::PrefetchableMemory
                            } else {
                                DtbPciAddressSpace::Memory
                            }
                        }
                    };
                    Ok(DtbPciRangeInfo {
                        space,
                        phys_hi: range.phys_hi,
                        memory_64: matches!(range.space, fdt::PciAddressSpace::Memory64),
                        relocatable: range.relocatable,
                        aliased: range.aliased,
                        child_addr: range.child_address,
                        parent_addr,
                        size,
                    })
                })
                .collect::<Result<Vec<_>, DtbFirmwareError>>()?;
            let interrupt_map = self
                .tree
                .pci_interrupt_map(node_id)
                .map_err(DtbFirmwareError::InvalidPci)?;
            let interrupt_cells = interrupt_map.as_ref().map_or_else(
                || self.optional_u32(node_id, "#interrupt-cells", 1),
                |map| Ok(map.child_interrupt_cells as u32),
            )? as usize;
            let interrupt_map_mask = interrupt_map
                .as_ref()
                .map(|map| map.mask.clone().into_boxed_slice());
            let interrupt_map_pass_thru = interrupt_map.as_ref().and_then(|map| {
                self.node(node_id)
                    .property("interrupt-map-pass-thru")
                    .map(|_| map.pass_thru.clone().into_boxed_slice())
            });
            let host_path = self.path(node_id).into_boxed_str();
            let interrupt_resolver = interrupt_map.as_ref().map(|map| {
                DtbPciInterruptResolver::new(
                    Arc::clone(&self.pci_interrupt_blob),
                    host_path.clone(),
                    map.child_address_cells,
                    map.child_interrupt_cells,
                )
            });
            let interrupt_map = match interrupt_map {
                None => Vec::new(),
                Some(map) => {
                    let can_resolve_statically = map.pass_thru.iter().all(|&cell| cell == 0);
                    map.entries
                        .iter()
                        .map(|entry| {
                            let resolved = if can_resolve_statically {
                                self.tree
                                    .resolve_pci_interrupt(
                                        &map,
                                        &entry.child_address,
                                        &entry.child_interrupt,
                                    )
                                    .map_err(DtbFirmwareError::InvalidPci)?
                                    .map(|route| DtbPciInterruptRoute {
                                        parent: route.provider_phandle,
                                        parent_address: route.address.into_boxed_slice(),
                                        parent_specifier: route.specifier.into_boxed_slice(),
                                    })
                            } else {
                                None
                            };
                            Ok(DtbPciInterruptMapEntry {
                                child_address: entry.child_address.clone().into_boxed_slice(),
                                child_interrupt: entry.child_interrupt.clone().into_boxed_slice(),
                                parent: entry.parent_phandle,
                                parent_address: entry.parent_address.clone().into_boxed_slice(),
                                parent_specifier: entry.parent_specifier.clone().into_boxed_slice(),
                                resolved,
                            })
                        })
                        .collect::<Result<Vec<_>, DtbFirmwareError>>()?
                }
            };
            let msi_map = self
                .tree
                .pci_msi_map(node_id)
                .map_err(DtbFirmwareError::InvalidPci)?;
            let msi_map_present = msi_map.is_some();
            let msi_map_mask = msi_map.as_ref().map_or(u32::MAX, |map| map.mask);
            let msi_map = msi_map
                .map(|map| {
                    map.entries
                        .into_iter()
                        .map(|entry| DtbPciMsiMapEntry {
                            requester_base: entry.requester_base,
                            controller: entry.controller_phandle,
                            msi_specifier: entry.msi_specifier.into_boxed_slice(),
                            length: entry.length,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let msi_parents = self
                .tree
                .msi_parents(node_id)
                .map_err(DtbFirmwareError::InvalidMsi)?
                .map(|parents| {
                    parents
                        .into_iter()
                        .map(|parent| DtbMsiParent {
                            controller: parent.controller_phandle,
                            msi_specifier: parent.msi_specifier.into_boxed_slice(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let dma_ranges = self
                .tree
                .dma_ranges_cells(node_id)
                .map_err(DtbFirmwareError::InvalidCellAddress)?
                .map(|ranges| ranges.into_iter().map(map_dma_range).collect());
            let iommu_map = self
                .tree
                .iommu_map(node_id)
                .map_err(DtbFirmwareError::InvalidIdMap)?
                .map(|map| self.map_iommu_map(map));
            let mut iommus = Vec::new();
            append_references(
                &mut iommus,
                "iommus",
                self.tree
                    .iommus(node_id)
                    .map_err(DtbFirmwareError::InvalidSpecifier)?,
            );
            for reference in &mut iommus {
                reference.provider_path = reference
                    .provider
                    .map(|provider| self.path(provider).into_boxed_str());
                reference.provider_available =
                    reference.provider.map(|provider| self.is_enabled(provider));
            }
            let graph_endpoints = self
                .tree
                .graph_endpoints(node_id)
                .map_err(DtbFirmwareError::InvalidGraph)?
                .into_iter()
                .map(|endpoint| map_graph_endpoint(&self.tree, endpoint))
                .collect();
            let effective_dma =
                self.effective_dma_info(node_id, Some(node_id), &iommus, DmaChildAddressKind::Pci)?;
            let children = self.pci_children(node_id, bus_start, bus_end)?;
            hosts.push(DtbPcieHostInfo {
                name: self.node_name_or_path(node_id),
                path: host_path,
                numa_node_id: self
                    .tree
                    .effective_numa_node_id(node_id)
                    .map_err(DtbFirmwareError::InvalidNuma)?,
                ecam_phys,
                ecam_size,
                config_space,
                domain,
                bus_start,
                bus_end,
                dma_coherent: effective_dma.coherent,
                address_cells: 3,
                interrupt_cells,
                ranges,
                interrupt_map_mask,
                interrupt_map_pass_thru,
                interrupt_resolver,
                interrupt_map,
                msi_map_present,
                msi_map_mask,
                msi_map,
                msi_parents,
                dma_ranges,
                iommu_map,
                iommus,
                graph_endpoints,
                effective_dma,
                children,
            });
        }
        Ok(hosts)
    }

    fn compatible_strings_strict(
        &self,
        node_id: NodeId,
    ) -> Result<Vec<Box<str>>, DtbFirmwareError> {
        let node = self.node(node_id);
        let Some(property) = node.property("compatible") else {
            return Ok(Vec::new());
        };
        property
            .as_string_list()
            .map(|values| values.map(Into::into).collect())
            .map_err(|error| DtbFirmwareError::InvalidProperty {
                node: node_id,
                property: "compatible",
                error,
            })
    }

    fn normalized_reg_entries(
        &self,
        node_id: NodeId,
        translate: bool,
    ) -> Result<Vec<DtbRegEntry>, DtbFirmwareError> {
        let entries = self
            .tree
            .reg_cells(node_id)
            .map_err(DtbFirmwareError::InvalidCellAddress)?;
        let parent = self.tree.parent(node_id);
        Ok(entries
            .into_iter()
            .map(|entry| {
                let translated = if translate {
                    parent.and_then(|parent| {
                        self.tree
                            .translate_address_cells(parent, &entry.address, entry.size.as_ref())
                            .ok()
                    })
                } else {
                    None
                };
                let mmio =
                    translated
                        .as_ref()
                        .zip(entry.size.as_ref())
                        .and_then(|(address, size)| {
                            let phys_addr = cell_value_to_usize(address)?;
                            let size = cell_value_to_usize(size)?;
                            if size == 0 || phys_addr.checked_add(size).is_none() {
                                return None;
                            }
                            Some(DtbMmioRangeInfo { phys_addr, size })
                        });
                DtbRegEntry {
                    address: map_cell_value(entry.address),
                    size: entry.size.map(map_cell_value),
                    translated_address: translated.map(map_cell_value),
                    mmio,
                }
            })
            .collect())
    }

    fn normalized_interrupts(
        &self,
        node_id: NodeId,
    ) -> Result<Vec<DtbInterruptInfo>, DtbFirmwareError> {
        let interrupts = self
            .tree
            .interrupts(node_id)
            .map_err(DtbFirmwareError::InvalidInterrupt)?
            .unwrap_or_default();
        let names = self
            .node(node_id)
            .property("interrupt-names")
            .map(|property| {
                property
                    .as_string_list()
                    .map(|names| names.map(Box::<str>::from).collect::<Vec<_>>())
                    .map_err(|error| DtbFirmwareError::InvalidProperty {
                        node: node_id,
                        property: "interrupt-names",
                        error,
                    })
            })
            .transpose()?;
        if names
            .as_ref()
            .is_some_and(|names| names.len() != interrupts.len())
        {
            return Err(DtbFirmwareError::InvalidValue {
                node: node_id,
                property: "interrupt-names",
            });
        }
        Ok(interrupts
            .into_iter()
            .enumerate()
            .map(|(index, interrupt)| DtbInterruptInfo {
                name: names.as_ref().map(|names| names[index].clone()),
                provider: interrupt.provider,
                provider_path: self.path(interrupt.provider).into_boxed_str(),
                parent: interrupt.phandle,
                specifier: interrupt.cells.into_boxed_slice(),
            })
            .collect())
    }

    fn is_under_pci_host(&self, node_id: NodeId) -> Result<bool, DtbFirmwareError> {
        let mut current = self.tree.parent(node_id);
        while let Some(parent) = current {
            let compatible = self.compatible_strings_strict(parent)?;
            if compatible.iter().any(|value| {
                value.as_bytes() == COMPAT_PCI_ECAM
                    || value.as_bytes() == COMPAT_PCIE_ECAM
                    || value.as_bytes() == COMPAT_PCI_CAM
            }) {
                return Ok(true);
            }
            current = self.tree.parent(parent);
        }
        Ok(false)
    }

    fn optional_u32(
        &self,
        node_id: NodeId,
        property_name: &'static str,
        default: u32,
    ) -> Result<u32, DtbFirmwareError> {
        let Some(property) = self.node(node_id).property(property_name) else {
            return Ok(default);
        };
        property
            .as_u32()
            .map_err(|error| DtbFirmwareError::InvalidProperty {
                node: node_id,
                property: property_name,
                error,
            })
    }

    /// 读取任意宽地址 binding 的 cell 数。这里只校验属性是单个 u32；具体属性
    /// 的长度计算由 `fdt` 宽地址解析器使用 checked arithmetic 完成。
    fn cell_count(
        &self,
        node_id: NodeId,
        property_name: &'static str,
        default: usize,
    ) -> Result<usize, DtbFirmwareError> {
        let Some(property) = self.node(node_id).property(property_name) else {
            return Ok(default);
        };
        let count = property
            .as_u32()
            .map_err(|error| DtbFirmwareError::InvalidProperty {
                node: node_id,
                property: property_name,
                error,
            })?;
        usize::try_from(count).map_err(|_| DtbFirmwareError::NativeAddressOverflow {
            node: node_id,
            property: property_name,
        })
    }

    fn pci_config_range(&self, node_id: NodeId) -> Result<(usize, usize), DtbFirmwareError> {
        let node = self.node(node_id);
        if node.property("reg").is_none() {
            return Err(DtbFirmwareError::InvalidPci(
                fdt::PciError::MissingRequired {
                    node: node_id,
                    property: "reg",
                },
            ));
        }
        let ranges = self
            .tree
            .translated_reg(node_id)
            .map_err(fdt::PciError::InvalidAddress)
            .map_err(DtbFirmwareError::InvalidPci)?;
        if ranges.len() != 1 {
            return Err(DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                node: node_id,
                property: "reg",
                entry: 0,
                value: ranges.len() as u128,
            }));
        }
        let range = ranges[0];
        let size = range
            .size
            .ok_or(DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                node: node_id,
                property: "reg",
                entry: 0,
                value: 0,
            }))?;
        if size == 0 {
            return Err(DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                node: node_id,
                property: "reg",
                entry: 0,
                value: 0,
            }));
        }
        let start = usize::try_from(range.address).map_err(|_| pci_overflow(node_id, "reg", 0))?;
        let size = usize::try_from(size).map_err(|_| pci_overflow(node_id, "reg", 0))?;
        start
            .checked_add(size)
            .ok_or_else(|| pci_overflow(node_id, "reg", 0))?;
        Ok((start, size))
    }

    fn pci_bus_range(&self, node_id: NodeId) -> Result<(u8, u8), DtbFirmwareError> {
        let Some(property) = self.node(node_id).property("bus-range") else {
            return Ok((0, u8::MAX));
        };
        let mut cells = property
            .cells()
            .map_err(|error| DtbFirmwareError::InvalidProperty {
                node: node_id,
                property: "bus-range",
                error,
            })?;
        if cells.len() != 2 {
            return Err(DtbFirmwareError::InvalidProperty {
                node: node_id,
                property: "bus-range",
                error: fdt::PropertyError::InvalidLength {
                    actual: property.value().len(),
                    expected: Some(8),
                },
            });
        }
        let start = cells.next().expect("two cells were checked");
        let end = cells.next().expect("two cells were checked");
        if start > u32::from(u8::MAX) || end > u32::from(u8::MAX) || start > end {
            return Err(DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                node: node_id,
                property: "bus-range",
                entry: 0,
                value: (u128::from(start) << 32) | u128::from(end),
            }));
        }
        Ok((start as u8, end as u8))
    }

    fn pci_domain(&self, node_id: NodeId) -> Result<u16, DtbFirmwareError> {
        let domain = self.optional_u32(node_id, "linux,pci-domain", 0)?;
        u16::try_from(domain).map_err(|_| {
            DtbFirmwareError::InvalidPci(fdt::PciError::InvalidValue {
                node: node_id,
                property: "linux,pci-domain",
                entry: 0,
                value: u128::from(domain),
            })
        })
    }

    fn first_reg_range(&self, node_id: NodeId) -> Option<AddressRange> {
        self.reg_ranges(node_id).ok()?.into_iter().next()
    }

    fn reg_ranges(&self, node_id: NodeId) -> Result<Vec<AddressRange>, DtbAddressError> {
        let node = self.tree.node(node_id).ok_or(DtbAddressError::InvalidReg)?;
        if node.find_property("reg").is_none() {
            return Err(DtbAddressError::MissingReg);
        }

        let mut current_parent = self.tree.parent(node_id);
        while let Some(bus_id) = current_parent {
            if self.node_is_pcie_host(bus_id) {
                return Err(DtbAddressError::UnsupportedBus);
            }
            current_parent = self.tree.parent(bus_id);
        }

        let mut ranges = Vec::new();
        for range in self
            .tree
            .translated_reg(node_id)
            .map_err(map_fdt_address_error)?
        {
            let Some(size) = range.size else {
                return Err(DtbAddressError::InvalidReg);
            };
            if size == 0 {
                continue;
            }
            let start = u128_to_usize(range.address)?;
            let size = u128_to_usize(size)?;
            start.checked_add(size).ok_or(DtbAddressError::Overflow)?;
            ranges.push(AddressRange { start, size });
        }

        (!ranges.is_empty())
            .then_some(ranges)
            .ok_or(DtbAddressError::InvalidReg)
    }

    fn node_is_cpu(&self, node_id: NodeId) -> bool {
        let node = self.node(node_id);
        node.base_name_bytes() == b"cpu" || property_first_string_eq(node, "device_type", "cpu")
    }

    fn cpus_node_id(&self) -> Option<NodeId> {
        self.children(self.tree.root_id())
            .iter()
            .copied()
            .find(|&node_id| self.node(node_id).base_name_bytes() == b"cpus")
    }

    fn cpu_node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.all_cpu_node_ids()
            .filter(|&node_id| self.is_enabled(node_id))
    }

    fn all_cpu_node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.cpus_node_id()
            .and_then(|cpus_id| self.tree.children(cpus_id))
            .into_iter()
            .flatten()
            .copied()
            .filter(|&node_id| self.node_is_cpu(node_id))
    }

    fn node_is_serial(&self, node_id: NodeId) -> bool {
        let node = self.node(node_id);
        node.base_name_bytes() == b"serial"
            || compatible_contains(node, COMPAT_NS16550)
            || compatible_contains(node, COMPAT_NS16550A)
    }

    fn node_is_pcie_host(&self, node_id: NodeId) -> bool {
        let node = self.node(node_id);
        compatible_contains(node, COMPAT_PCI_ECAM)
            || compatible_contains(node, COMPAT_PCIE_ECAM)
            || compatible_contains(node, COMPAT_PCI_CAM)
    }

    fn node_name_or_path(&self, node_id: NodeId) -> Box<str> {
        let name = self.node(node_id).name();
        if name.is_empty() {
            self.path(node_id).into_boxed_str()
        } else {
            name.into()
        }
    }

    #[inline]
    fn node(&self, node_id: NodeId) -> Node<'a> {
        self.tree
            .node(node_id)
            .expect("fdt::Tree node id must remain valid")
    }

    fn chosen_node(&self) -> Option<Node<'a>> {
        self.tree
            .find_node("/chosen")
            .or_else(|| self.tree.find_node("/chosen@0"))
            .map(|node_id| self.node(node_id))
    }

    #[inline]
    fn path(&self, node_id: NodeId) -> String {
        self.tree
            .path(node_id)
            .expect("fdt::Tree node path must remain valid")
    }

    #[inline]
    fn children(&self, node_id: NodeId) -> &[NodeId] {
        self.tree
            .children(node_id)
            .expect("fdt::Tree node children must remain valid")
    }

    #[inline]
    fn is_enabled(&self, node_id: NodeId) -> bool {
        self.enabled.get(node_id.index()).copied().unwrap_or(false)
    }

    fn map_iommu_map(&self, map: IdMap) -> DtbIommuMap {
        DtbIommuMap {
            mask: map.mask,
            entries: map
                .entries
                .into_iter()
                .map(|entry| DtbIommuMapEntry {
                    input_base: entry.input_base,
                    provider: entry.provider,
                    provider_path: self.path(entry.provider).into_boxed_str(),
                    provider_phandle: entry.provider_phandle,
                    output_base: entry.output_base.into_boxed_slice(),
                    length: entry.length,
                    provider_available: self.is_enabled(entry.provider),
                })
                .collect(),
        }
    }

    fn read_cpu_reg(&self, node_id: NodeId) -> Result<u64, DtbFirmwareError> {
        let reg = self
            .tree
            .reg(node_id)
            .map_err(DtbFirmwareError::InvalidAddress)?;
        let [reg] = reg.as_slice() else {
            return Err(DtbFirmwareError::InvalidValue {
                node: node_id,
                property: "reg",
            });
        };
        u64::try_from(reg.address).map_err(|_| DtbFirmwareError::InvalidValue {
            node: node_id,
            property: "reg",
        })
    }
}

fn append_named_references(
    target: &mut Vec<DtbProviderReference>,
    property: &str,
    entries: Option<Vec<NamedPhandleArgs>>,
) {
    let Some(entries) = entries else {
        return;
    };
    target.extend(entries.into_iter().map(|entry| {
        provider_reference(
            property,
            entry.name.map(String::into_boxed_str),
            entry.specifier,
        )
    }));
}

fn pinctrl_state_index(property: &str) -> Option<usize> {
    let suffix = property.strip_prefix("pinctrl-")?;
    (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| suffix.parse().ok())?
}

fn append_references(
    target: &mut Vec<DtbProviderReference>,
    property: &str,
    entries: Option<Vec<PhandleArgs>>,
) {
    let Some(entries) = entries else {
        return;
    };
    target.extend(
        entries
            .into_iter()
            .map(|entry| provider_reference(property, None, entry)),
    );
}

fn provider_reference(
    property: &str,
    name: Option<Box<str>>,
    specifier: PhandleArgs,
) -> DtbProviderReference {
    DtbProviderReference {
        property: property.into(),
        name,
        provider: specifier.provider,
        provider_path: None,
        provider_available: None,
        phandle: specifier.phandle,
        args: specifier.args.into_boxed_slice(),
    }
}

fn map_cell_value(value: fdt::CellValue) -> DtbCellValue {
    DtbCellValue {
        cells: value.cells().into(),
    }
}

fn map_dma_range(range: CellRangeMapping) -> DtbDmaRangeMapping {
    DtbDmaRangeMapping {
        child_address: map_cell_value(range.child_address),
        parent_address: map_cell_value(range.parent_address),
        size: range.size.map(map_cell_value),
    }
}

fn map_graph_endpoint(tree: &Tree<'_>, endpoint: GraphEndpoint) -> DtbGraphEndpoint {
    DtbGraphEndpoint {
        node: endpoint.node,
        node_path: tree
            .path(endpoint.node)
            .expect("graph endpoint NodeId must remain valid")
            .into_boxed_str(),
        port: endpoint.port,
        port_path: tree
            .path(endpoint.port)
            .expect("graph port NodeId must remain valid")
            .into_boxed_str(),
        port_id: endpoint.port_id,
        endpoint_id: endpoint.endpoint_id,
        phandle: endpoint.phandle,
        remote: endpoint.remote,
        remote_path: endpoint.remote.map(|remote| {
            tree.path(remote)
                .expect("graph remote NodeId must remain valid")
                .into_boxed_str()
        }),
        remote_phandle: endpoint.remote_phandle,
    }
}

fn map_fdt_address_error(error: FdtAddressError) -> DtbAddressError {
    match error {
        FdtAddressError::UnsupportedCellCount { .. } => DtbAddressError::UnsupportedCells,
        FdtAddressError::MissingRanges(_) => DtbAddressError::UnsupportedBus,
        FdtAddressError::UnmappedAddress { .. } => DtbAddressError::UnmatchedRange,
        FdtAddressError::Overflow => DtbAddressError::Overflow,
        _ => DtbAddressError::InvalidReg,
    }
}

fn compatible_contains(node: Node<'_>, compatible: &[u8]) -> bool {
    let Some(prop) = node.find_property("compatible") else {
        return false;
    };
    prop.value()
        .split(|&byte| byte == 0)
        .any(|entry| entry == compatible)
}

fn is_default_platform_bus(compatible: &[Box<str>]) -> bool {
    compatible.iter().any(|value| {
        let value = value.as_bytes();
        value == COMPAT_SIMPLE_BUS
            || value == COMPAT_SIMPLE_MFD
            || value == COMPAT_SIMPLE_PM_BUS
            || value == COMPAT_QEMU_PLATFORM
            || value == COMPAT_ARM_AMBA_BUS
    })
}

fn compatible_strings(node: Node<'_>) -> Vec<Box<str>> {
    let Some(prop) = node.find_property("compatible") else {
        return Vec::new();
    };
    prop.value()
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| str::from_utf8(entry).ok().map(Into::into))
        .collect()
}

fn property_first_string_eq(node: Node<'_>, name: &str, expected: &str) -> bool {
    node.find_property(name)
        .and_then(|prop| parse_dtb_string(prop.value()))
        == Some(expected)
}

fn parse_dtb_string(value: &[u8]) -> Option<&str> {
    let end = value
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(value.len());
    str::from_utf8(value.get(..end)?).ok()
}

fn read_clock_hz(node: Node<'_>) -> Option<u32> {
    read_be_u32_prop(node.find_property("clock-frequency")?.value())
}

fn read_current_speed(node: Node<'_>) -> Option<u32> {
    read_be_u32_prop(node.find_property("current-speed")?.value())
}

fn raw_properties(node: Node<'_>) -> Vec<DtbDeviceProperty> {
    node.properties()
        .map(|property| DtbDeviceProperty {
            name: property.name().into(),
            value: property.value().to_vec().into_boxed_slice(),
        })
        .collect()
}

fn indexed_name_suffix(name: &str, prefix: &str) -> Option<u32> {
    let suffix = name.strip_prefix(prefix)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn cpu_map_node_name(name: &str) -> Option<(DtbCpuMapNodeKind, u32)> {
    [
        ("socket", DtbCpuMapNodeKind::Socket),
        ("cluster", DtbCpuMapNodeKind::Cluster),
        ("core", DtbCpuMapNodeKind::Core),
        ("thread", DtbCpuMapNodeKind::Thread),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| indexed_name_suffix(name, prefix).map(|id| (kind, id)))
}

fn read_be_u32_prop(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.try_into().ok()?))
}

fn read_usize_scalar(value: &[u8]) -> Option<usize> {
    let raw = match value.len() {
        4 => u64::from(read_be_u32_prop(value)?),
        8 => u64::from_be_bytes(value.try_into().ok()?),
        _ => return None,
    };
    (raw <= usize::MAX as u64).then_some(raw as usize)
}

fn read_u64_scalar(value: &[u8]) -> Option<u64> {
    match value.len() {
        4 => read_be_u32_prop(value).map(u64::from),
        8 => Some(u64::from_be_bytes(value.try_into().ok()?)),
        _ => None,
    }
}

fn u128_to_usize(value: u128) -> Result<usize, DtbAddressError> {
    if value <= usize::MAX as u128 {
        Ok(value as usize)
    } else {
        Err(DtbAddressError::Overflow)
    }
}

fn pci_overflow(node: NodeId, property: &'static str, entry: usize) -> DtbFirmwareError {
    DtbFirmwareError::InvalidPci(fdt::PciError::Overflow {
        node,
        property,
        entry,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;
    use allocator::MemorySegment;

    use super::*;

    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 9;
    static NODE_GRAPH_TEST_LOCK: SpinMutex<()> = SpinMutex::new(());

    struct NodeGraphStateReset(DtbNodeGraphState);

    impl NodeGraphStateReset {
        fn take() -> Self {
            let mut state = NODE_GRAPH_SNAPSHOT.lock();
            Self(core::mem::replace(
                &mut *state,
                DtbNodeGraphState {
                    generation: 0,
                    update_generation: None,
                    nodes: None,
                },
            ))
        }
    }

    impl Drop for NodeGraphStateReset {
        fn drop(&mut self) {
            let mut state = NODE_GRAPH_SNAPSHOT.lock();
            core::mem::swap(&mut *state, &mut self.0);
        }
    }

    #[test]
    fn live_node_graph_update_checks_generation_and_aborts_without_publication() {
        let _lock = NODE_GRAPH_TEST_LOCK.lock();
        let _reset = NodeGraphStateReset::take();
        let initial = parse_test_firmware().nodes;
        install_node_graph(initial.clone()).unwrap();
        let generation = node_graph_generation().unwrap();

        let update = begin_node_graph_update(generation).unwrap();
        assert!(matches!(
            begin_node_graph_update(generation),
            Err(DtbNodeGraphUpdateError::Busy)
        ));
        drop(update);
        assert_eq!(node_graph_snapshot(), initial);

        let mut candidate = initial.clone();
        candidate[0].enabled = !candidate[0].enabled;
        let committed = begin_node_graph_update(generation)
            .unwrap()
            .commit(candidate.clone().into_boxed_slice());
        assert_eq!(committed.previous.as_ref(), initial.as_slice());
        assert_ne!(committed.generation, generation);
        assert_eq!(node_graph_snapshot(), candidate);
        assert!(matches!(
            begin_node_graph_update(generation),
            Err(DtbNodeGraphUpdateError::BaselineMismatch)
        ));
    }

    struct DtbBuilder {
        structure: Vec<u8>,
        strings: Vec<u8>,
    }

    impl DtbBuilder {
        fn new() -> Self {
            Self {
                structure: Vec::new(),
                strings: Vec::new(),
            }
        }

        fn begin_node(&mut self, name: &str) {
            push_u32(&mut self.structure, FDT_BEGIN_NODE);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            pad_to(&mut self.structure, 4);
        }

        fn property(&mut self, name: &str, value: &[u8]) {
            let name_offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);

            push_u32(&mut self.structure, FDT_PROP);
            push_u32(&mut self.structure, value.len() as u32);
            push_u32(&mut self.structure, name_offset);
            self.structure.extend_from_slice(value);
            pad_to(&mut self.structure, 4);
        }

        fn end_node(&mut self) {
            push_u32(&mut self.structure, FDT_END_NODE);
        }

        fn finish(mut self) -> Vec<u8> {
            push_u32(&mut self.structure, FDT_END);

            let mut blob = vec![0; 40];
            pad_to(&mut blob, 8);
            let reservations_offset = blob.len();
            blob.extend_from_slice(&[0; 16]);
            let structure_offset = blob.len();
            blob.extend_from_slice(&self.structure);
            let strings_offset = blob.len();
            blob.extend_from_slice(&self.strings);
            let total_size = blob.len();

            set_u32(&mut blob, 0, fdt::DTB_MAGIC);
            set_u32(&mut blob, 4, total_size as u32);
            set_u32(&mut blob, 8, structure_offset as u32);
            set_u32(&mut blob, 12, strings_offset as u32);
            set_u32(&mut blob, 16, reservations_offset as u32);
            set_u32(&mut blob, 20, 17);
            set_u32(&mut blob, 24, 16);
            set_u32(&mut blob, 28, 0);
            set_u32(&mut blob, 32, self.strings.len() as u32);
            set_u32(&mut blob, 36, self.structure.len() as u32);
            blob
        }
    }

    #[test]
    fn ordinary_ranges_are_translated_by_the_shared_tree() {
        let firmware = parse_test_firmware();
        let serial = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/serial@1000")
            .expect("serial platform device must be present");
        assert_eq!(
            serial.reg_ranges,
            vec![DtbMmioRangeInfo {
                phys_addr: 0x4000_1000,
                size: 0x100,
            }]
        );
    }

    #[test]
    fn cpu_binding_reg_is_not_exposed_as_platform_mmio() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[2]));

        builder.begin_node("cpus");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[0]));

        builder.begin_node("cpu@0");
        builder.property("compatible", b"riscv\0");
        builder.property("device_type", b"cpu\0");
        builder.property("reg", &cells(&[0]));

        builder.begin_node("interrupt-controller");
        builder.property("compatible", b"riscv,cpu-intc\0");
        builder.property("interrupt-controller", &[]);
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("phandle", &cells(&[1]));
        builder.end_node();

        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.cpus.len(), 1);
        assert_eq!(firmware.cpus[0].reg, 0);
        assert!(
            firmware
                .platform_devices
                .iter()
                .all(|device| !device.path.starts_with("/cpus/"))
        );
    }

    #[test]
    fn cpu_reg_is_required_single_and_representable() {
        let mut missing = cpu_tree_builder();
        missing.begin_node("cpu@0");
        missing.property("device_type", b"cpu\0");
        missing.property("phandle", &cells(&[10]));
        missing.end_node();
        finish_cpu_tree(&mut missing);
        assert!(matches!(
            parse_test_firmware_result(missing),
            Err(DtbFirmwareError::InvalidValue {
                property: "reg",
                ..
            })
        ));

        let mut multiple = cpu_tree_builder();
        multiple.begin_node("cpu@0");
        multiple.property("device_type", b"cpu\0");
        multiple.property("reg", &cells(&[0, 1]));
        multiple.property("phandle", &cells(&[10]));
        multiple.end_node();
        finish_cpu_tree(&mut multiple);
        assert!(matches!(
            parse_test_firmware_result(multiple),
            Err(DtbFirmwareError::InvalidValue {
                property: "reg",
                ..
            })
        ));

        let mut wide = DtbBuilder::new();
        wide.begin_node("");
        wide.property("#address-cells", &cells(&[2]));
        wide.property("#size-cells", &cells(&[2]));
        wide.begin_node("cpus");
        wide.property("#address-cells", &cells(&[3]));
        wide.property("#size-cells", &cells(&[0]));
        wide.begin_node("cpu@10000000000000000");
        wide.property("device_type", b"cpu\0");
        wide.property("reg", &cells(&[1, 0, 0]));
        wide.end_node();
        finish_cpu_tree(&mut wide);
        assert!(matches!(
            parse_test_firmware_result(wide),
            Err(DtbFirmwareError::InvalidValue {
                property: "reg",
                ..
            })
        ));
    }

    #[test]
    fn cpu_map_preserves_scoped_nested_clusters_capacity_and_cpu_intc() {
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu-map");

        builder.begin_node("cluster0");
        builder.begin_node("cluster0");
        builder.begin_node("core0");
        builder.begin_node("thread0");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        builder.begin_node("thread1");
        builder.property("cpu", &cells(&[11]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        builder.end_node();

        builder.begin_node("cluster1");
        builder.begin_node("cluster0");
        builder.begin_node("core0");
        builder.property("cpu", &cells(&[12]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        builder.end_node();
        add_test_cpu(&mut builder, 0, 10, Some(512), &[100]);
        add_test_cpu(&mut builder, 1, 11, Some(512), &[101, 102]);
        add_test_cpu(&mut builder, 2, 12, Some(1024), &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.cpus.len(), 3);
        assert_eq!(firmware.cpus[0].cluster_path.as_ref(), &[0, 0]);
        assert_eq!(firmware.cpus[1].cluster_path.as_ref(), &[0, 0]);
        assert_eq!(firmware.cpus[2].cluster_path.as_ref(), &[1, 0]);
        assert_eq!(firmware.cpus[0].core_id, Some(0));
        assert_eq!(firmware.cpus[2].core_id, Some(0));
        assert_eq!(firmware.cpus[0].thread_id, Some(0));
        assert_eq!(firmware.cpus[1].thread_id, Some(1));
        assert_eq!(firmware.cpus[2].thread_id, None);
        assert_eq!(firmware.cpus[0].capacity_dmips_mhz, Some(512));
        assert_eq!(firmware.cpus[2].capacity_dmips_mhz, Some(1024));
        assert_eq!(
            firmware.cpus[0].interrupt_controller_phandles.as_ref(),
            &[100]
        );
        assert_eq!(
            firmware.cpus[1].interrupt_controller_phandles.as_ref(),
            &[101, 102]
        );
    }

    #[test]
    fn cpu_map_supports_arbitrary_cluster_depth() {
        const DEPTH: usize = 32;
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu-map");
        for _ in 0..DEPTH {
            builder.begin_node("cluster0");
        }
        builder.begin_node("core0");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        for _ in 0..DEPTH {
            builder.end_node();
        }
        builder.end_node();
        add_test_cpu(&mut builder, 0, 10, None, &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.cpus[0].cluster_path.len(), DEPTH);
        assert!(
            firmware.cpus[0]
                .cluster_path
                .iter()
                .all(|cluster| *cluster == 0)
        );
    }

    #[test]
    fn cpu_map_accepts_socket_with_direct_cores() {
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu-map");
        builder.begin_node("socket0");
        builder.begin_node("core0");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        add_test_cpu(&mut builder, 0, 10, None, &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.cpus[0].socket_id, Some(0));
        assert!(firmware.cpus[0].cluster_path.is_empty());
        assert_eq!(firmware.cpus[0].core_id, Some(0));
    }

    #[test]
    fn malformed_cpu_reference_does_not_become_an_absent_property() {
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu-map");
        builder.begin_node("cluster0");
        builder.begin_node("core0");
        builder.property("cpu", &[0, 10]);
        builder.begin_node("thread0");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        builder.end_node();
        add_test_cpu(&mut builder, 0, 10, None, &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert!(firmware.cpus[0].cluster_path.is_empty());
        assert!(firmware.cpus[0].core_id.is_none());
        assert!(firmware.cpus[0].thread_id.is_none());
    }

    #[test]
    fn invalid_cpu_map_is_ignored_atomically() {
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu-map");
        builder.begin_node("cluster0");
        builder.begin_node("core0");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        builder.end_node();
        // sibling 编号不连续；即使所有 CPU 都被引用，也必须丢弃整张 map。
        builder.begin_node("cluster2");
        builder.begin_node("core0");
        builder.property("cpu", &cells(&[11]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        add_test_cpu(&mut builder, 0, 10, None, &[]);
        add_test_cpu(&mut builder, 1, 11, None, &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert!(firmware.cpus.iter().all(|cpu| {
            cpu.socket_id.is_none()
                && cpu.cluster_path.is_empty()
                && cpu.core_id.is_none()
                && cpu.thread_id.is_none()
        }));
    }

    #[test]
    fn duplicate_cpu_map_reference_is_ignored_atomically() {
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu-map");
        builder.begin_node("cluster0");
        builder.begin_node("core0");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        builder.begin_node("core1");
        builder.property("cpu", &cells(&[10]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        add_test_cpu(&mut builder, 0, 10, None, &[]);
        add_test_cpu(&mut builder, 1, 11, None, &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert!(firmware.cpus.iter().all(|cpu| cpu.core_id.is_none()));
    }

    #[test]
    fn partial_cpu_capacity_falls_back_for_every_cpu() {
        let mut builder = cpu_tree_builder();
        add_test_cpu(&mut builder, 0, 10, Some(512), &[]);
        add_test_cpu(&mut builder, 1, 11, None, &[]);
        finish_cpu_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        assert!(
            firmware
                .cpus
                .iter()
                .all(|cpu| cpu.capacity_dmips_mhz.is_none())
        );
    }

    #[test]
    fn cpu_interrupt_controller_requires_a_phandle() {
        let mut builder = cpu_tree_builder();
        builder.begin_node("cpu@0");
        builder.property("device_type", b"cpu\0");
        builder.property("reg", &cells(&[0]));
        builder.property("phandle", &cells(&[10]));
        builder.begin_node("interrupt-controller");
        builder.property("interrupt-controller", &[]);
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.end_node();
        builder.end_node();
        finish_cpu_tree(&mut builder);

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::InvalidValue {
                property: "phandle",
                ..
            })
        ));
    }

    #[test]
    fn binding_specific_child_bus_addresses_are_not_platform_mmio() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);

        builder.begin_node("i2c@1000");
        builder.property("compatible", b"vendor,i2c\0");
        builder.property("reg", &cells(&[0x1000, 0x100]));
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[0]));

        builder.begin_node("sensor@50");
        builder.property("compatible", b"vendor,sensor\0");
        builder.property("reg", &cells(&[0x50]));
        builder.end_node();

        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let controller = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/i2c@1000")
            .expect("I2C controller must remain a platform device");
        assert_eq!(controller.reg_ranges[0].phys_addr, 0x1000);
        assert!(
            firmware
                .platform_devices
                .iter()
                .all(|device| device.path.as_ref() != "/soc/i2c@1000/sensor@50")
        );
        let sensor = firmware
            .nodes
            .iter()
            .find(|node| node.path.as_ref() == "/soc/i2c@1000/sensor@50")
            .expect("specialized bus child must remain in the complete node graph");
        assert_eq!(sensor.compatible.len(), 1);
        assert_eq!(sensor.compatible[0].as_ref(), "vendor,sensor");
        assert_eq!(sensor.reg_entries[0].address.cells(), &[0x50]);
        assert_eq!(sensor.reg_entries[0].size, None);
        assert_eq!(sensor.reg_entries[0].mmio, None);
    }

    #[test]
    fn platform_bus_without_ranges_is_not_assumed_to_be_identity_mapped() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("device@1000");
        builder.property("compatible", b"vendor,device\0");
        builder.property("reg", &cells(&[0x1000, 0x100]));
        builder.end_node();

        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let device = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/device@1000")
            .expect("untranslated child must remain visible to the firmware graph");
        assert!(device.reg_ranges.is_empty());
        assert_eq!(device.reg_entries.len(), 1);
        assert_eq!(device.reg_entries[0].address.cells(), &[0x1000]);
        assert_eq!(
            device.reg_entries[0].size.as_ref().map(DtbCellValue::cells),
            Some([0x100].as_slice())
        );
        assert_eq!(device.reg_entries[0].translated_address, None);
        assert_eq!(device.reg_entries[0].mmio, None);
    }

    #[test]
    fn chosen_legacy_path_resolves_alias_and_serial_options() {
        let firmware = parse_test_firmware();
        let stdout = firmware
            .stdout_serial
            .expect("chosen alias must resolve to the serial node");
        assert_eq!(stdout.name.as_ref(), "serial@1000");
        assert_eq!(stdout.phys_addr, 0x4000_1000);
        assert_eq!(stdout.reg_size, Some(0x100));
        assert_eq!(stdout.baud, Some(115_200));
    }

    #[test]
    fn only_root_memory_nodes_supply_usable_ram() {
        let firmware = parse_test_firmware();
        assert_eq!(
            described_memory_segments(&firmware.memory).unwrap(),
            vec![MemorySegment {
                start: 0x8000_0000,
                size: 0x0100_0000,
            }]
        );
    }

    #[test]
    fn firmware_without_memory_nodes_remains_parseable_for_uefi_boot() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("compatible", b"test,uefi-board\0");
        builder.end_node();
        let bytes: &'static [u8] = Box::leak(builder.finish().into_boxed_slice());

        let firmware = parse(Fdt::parse(bytes).unwrap()).unwrap();
        assert!(firmware.memory.memory_banks.is_empty());
    }

    #[test]
    fn numa_topology_survives_normalization_and_splits_allocator_segments() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("distance-map");
        builder.property("compatible", b"numa-distance-map-v1\0");
        builder.property(
            "distance-matrix",
            &cells(&[0, 0, 10, 0, 1, 20, 1, 0, 20, 1, 1, 10]),
        );
        builder.end_node();
        for (start, node_id) in [(0x1000, 0), (0x3000, 1)] {
            builder.begin_node(if node_id == 0 {
                "memory@1000"
            } else {
                "memory@3000"
            });
            builder.property("device_type", b"memory\0");
            builder.property("reg", &cells(&[start, 0x2000]));
            builder.property("numa-node-id", &cells(&[node_id]));
            builder.end_node();
        }
        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("ranges", &[]);
        builder.property("numa-node-id", &cells(&[1]));
        builder.begin_node("device");
        builder.property("compatible", b"test,device\0");
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.numa.distance(1, 0), Some(20));
        assert_eq!(firmware.numa.node_id_for_path("/memory@3000"), Some(1));
        assert_eq!(
            firmware
                .platform_devices
                .iter()
                .find(|device| device.path.as_ref() == "/soc/device")
                .and_then(|device| device.numa_node_id),
            Some(1)
        );

        let merged = described_memory_segments(&firmware.memory).unwrap();
        assert_eq!(merged.len(), 1);
        let layout = split_numa_memory_segments(&firmware.memory, merged).unwrap();
        assert_eq!(
            layout.usable_segments,
            vec![
                MemorySegment {
                    start: 0x1000,
                    size: 0x2000,
                },
                MemorySegment {
                    start: 0x3000,
                    size: 0x2000,
                },
            ]
        );
        assert_eq!(layout.numa_ranges.len(), 2);
        assert_eq!(layout.numa_ranges[0].node_id, 0);
        assert_eq!(layout.numa_ranges[1].node_id, 1);
    }

    #[test]
    fn conflicting_memory_numa_nodes_fail_before_allocator_init() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        for (name, start, node_id) in [("memory@1000", 0x1000, 0), ("memory@2000", 0x2000, 1)] {
            builder.begin_node(name);
            builder.property("device_type", b"memory\0");
            builder.property("reg", &cells(&[start, 0x2000]));
            builder.property("numa-node-id", &cells(&[node_id]));
            builder.end_node();
        }
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let memory = described_memory_segments(&firmware.memory).unwrap();
        assert!(matches!(
            split_numa_memory_segments(&firmware.memory, memory),
            Err(DtbMemoryLayoutError::ConflictingNumaRange {
                first: 0,
                second: 1,
                ..
            })
        ));
    }

    #[test]
    fn platform_properties_preserve_the_complete_raw_value() {
        let firmware = parse_test_firmware();
        let serial = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/serial@1000")
            .expect("serial platform device must be present");
        let raw = |name: &str| {
            serial
                .properties
                .iter()
                .find(|property| property.name.as_ref() == name)
                .map(|property| property.value.as_ref())
        };

        assert_eq!(
            raw("current-speed"),
            Some(115_200u32.to_be_bytes().as_slice())
        );
        assert_eq!(raw("label"), Some(b"console\0".as_slice()));
        assert_eq!(raw("opaque"), Some([0xaa, 0xbb, 0xcc].as_slice()));
        assert_eq!(raw("flag"), Some([].as_slice()));
    }

    #[test]
    fn dynamic_reserved_memory_is_allocated_after_static_and_boot_ranges() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("memory@80000000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x8000_0000, 0x0010_0000]));
        builder.end_node();

        builder.begin_node("reserved-memory");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);

        // 动态节点先出现，静态节点后出现；解析器仍须先处理所有静态区域。
        builder.begin_node("dma-pool");
        builder.property("size", &cells(&[0x5000]));
        builder.property("alignment", &cells(&[0x1000]));
        builder.property("alloc-ranges", &cells(&[0x8000_0000, 0x10000]));
        builder.property("no-map", &[]);
        builder.end_node();

        builder.begin_node("framebuffer@80004000");
        builder.property("reg", &cells(&[0x8000_4000, 0x1000]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        let bytes: &'static [u8] = Box::leak(builder.finish().into_boxed_slice());

        let firmware = parse(Fdt::parse(bytes).unwrap()).unwrap();
        let base = described_memory_segments(&firmware.memory).unwrap();
        let layout = resolve_memory_layout(
            &firmware.memory,
            base,
            &[MemorySegment {
                start: 0x8000_0000,
                size: 0x1000,
            }],
        )
        .unwrap();

        let efi_map = [crate::StartMemoryRegion::new(
            crate::StartPhysRange::new(0x8000_4000, 0x8000_5000),
            crate::StartMemoryRegionKind::FirmwareReclaimable,
            0,
        )
        .with_source_type(4)];
        validate_uefi_reserved_memory(&layout.reserved_memory, &efi_map).unwrap();
        let wrong_efi_map = [crate::StartMemoryRegion::new(
            crate::StartPhysRange::new(0x8000_4000, 0x8000_5000),
            crate::StartMemoryRegionKind::UsableRam,
            0,
        )
        .with_source_type(7)];
        assert_eq!(
            validate_uefi_reserved_memory(&layout.reserved_memory, &wrong_efi_map),
            Err(DtbUefiReservationError {
                node: layout.reserved_memory[1].request.node,
                range: MemorySegment {
                    start: 0x8000_4000,
                    size: 0x1000,
                },
                expected_efi_type: 4,
            })
        );

        assert_eq!(
            layout.reserved_memory[0].ranges,
            vec![MemorySegment {
                start: 0x8000_5000,
                size: 0x5000,
            }]
        );
        assert_eq!(layout.reserved_memory[1].ranges[0].start, 0x8000_4000);
        assert_eq!(layout.no_map_segments, layout.reserved_memory[0].ranges);
        assert!(
            layout
                .usable_segments
                .iter()
                .all(|segment| segment.start != 0x8000_5000)
        );
    }

    #[test]
    fn chosen_usable_ranges_are_applied_to_uefi_memory_segments() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        // UEFI 的 GetMemoryMap 是 RAM 的权威来源；DT 中的 /memory 仅用于
        // 覆盖直接 DT 启动路径，chosen 限制则必须应用到两种来源。
        builder.begin_node("memory@0");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0, 0x20_000]));
        builder.end_node();

        builder.begin_node("chosen");
        builder.property(
            "linux,usable-memory-range",
            &cells(&[0x2000, 0x2000, 0xc000, 0x1000]),
        );
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let uefi_segments = vec![
            MemorySegment {
                start: 0x1000,
                size: 0x8000,
            },
            MemorySegment {
                start: 0xc000,
                size: 0x2000,
            },
        ];

        assert_eq!(
            apply_chosen_usable_ranges(uefi_segments, &firmware.memory).unwrap(),
            vec![
                MemorySegment {
                    start: 0x2000,
                    size: 0x2000,
                },
                MemorySegment {
                    start: 0xc000,
                    size: 0x1000,
                },
            ]
        );
    }

    #[test]
    fn uefi_no_map_static_reservation_requires_reserved_memory_type() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("memory@0");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0, 0x10_000]));
        builder.end_node();

        begin_reserved_memory(&mut builder);
        builder.begin_node("secure-buffer@2000");
        builder.property("reg", &cells(&[0x2000, 0x1000]));
        builder.property("no-map", &[]);
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let layout = resolve_memory_layout(
            &firmware.memory,
            described_memory_segments(&firmware.memory).unwrap(),
            &[],
        )
        .unwrap();
        let reserved = layout.reserved_memory[0].ranges[0];
        assert_eq!(layout.no_map_segments, vec![reserved]);

        let efi_reserved = [efi_region(reserved, 0)];
        assert!(validate_uefi_reserved_memory(&layout.reserved_memory, &efi_reserved).is_ok());

        let efi_boot_services = [efi_region(reserved, 4)];
        assert_eq!(
            validate_uefi_reserved_memory(&layout.reserved_memory, &efi_boot_services),
            Err(DtbUefiReservationError {
                node: layout.reserved_memory[0].request.node,
                range: reserved,
                expected_efi_type: 0,
            })
        );
    }

    #[test]
    fn uefi_static_reservation_requires_complete_expected_type_coverage() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("memory@0");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0, 0x10_000]));
        builder.end_node();

        begin_reserved_memory(&mut builder);
        builder.begin_node("framebuffer@3000");
        builder.property("reg", &cells(&[0x3000, 0x2000]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let layout = resolve_memory_layout(
            &firmware.memory,
            described_memory_segments(&firmware.memory).unwrap(),
            &[],
        )
        .unwrap();
        let reserved = layout.reserved_memory[0].ranges[0];

        // EFI 可以用多个相邻 descriptor 覆盖同一个 DT 范围，但每段都必须是类型 4。
        let split_valid = [
            efi_region(
                MemorySegment {
                    start: 0x3000,
                    size: 0x1000,
                },
                4,
            ),
            efi_region(
                MemorySegment {
                    start: 0x4000,
                    size: 0x1000,
                },
                4,
            ),
        ];
        assert!(validate_uefi_reserved_memory(&layout.reserved_memory, &split_valid).is_ok());

        // 即使只有后半段类型错误，也必须拒绝整个静态保留区，
        // 不能只检查首条 descriptor。
        let split_mismatch = [
            efi_region(
                MemorySegment {
                    start: 0x3000,
                    size: 0x1000,
                },
                4,
            ),
            efi_region(
                MemorySegment {
                    start: 0x4000,
                    size: 0x1000,
                },
                7,
            ),
        ];
        assert_eq!(
            validate_uefi_reserved_memory(&layout.reserved_memory, &split_mismatch),
            Err(DtbUefiReservationError {
                node: layout.reserved_memory[0].request.node,
                range: reserved,
                expected_efi_type: 4,
            })
        );
    }

    #[test]
    fn multiple_dynamic_reservations_are_allocated_in_stable_order() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("memory@1000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x1000, 0x6000]));
        builder.end_node();

        begin_reserved_memory(&mut builder);
        builder.begin_node("first-pool");
        builder.property("size", &cells(&[0x2000]));
        builder.property("alignment", &cells(&[0x1000]));
        builder.property("alloc-ranges", &cells(&[0x1000, 0x5000]));
        builder.end_node();
        builder.begin_node("second-pool");
        builder.property("size", &cells(&[0x2000]));
        builder.property("alignment", &cells(&[0x1000]));
        builder.property("alloc-ranges", &cells(&[0x1000, 0x5000]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let layout = resolve_memory_layout(
            &firmware.memory,
            described_memory_segments(&firmware.memory).unwrap(),
            &[],
        )
        .unwrap();

        assert_eq!(layout.reserved_memory.len(), 2);
        assert_eq!(
            layout.reserved_memory[0].ranges,
            vec![MemorySegment {
                start: 0x1000,
                size: 0x2000,
            }]
        );
        assert_eq!(
            layout.reserved_memory[1].ranges,
            vec![MemorySegment {
                start: 0x3000,
                size: 0x2000,
            }]
        );
        assert_eq!(
            layout.usable_segments,
            vec![MemorySegment {
                start: 0x5000,
                size: 0x2000,
            }]
        );
    }

    #[test]
    fn dynamic_reservation_reports_allocation_failure() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("memory@1000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x1000, 0x3000]));
        builder.end_node();

        begin_reserved_memory(&mut builder);
        builder.begin_node("first-pool");
        builder.property("size", &cells(&[0x2000]));
        builder.property("alignment", &cells(&[0x1000]));
        builder.property("alloc-ranges", &cells(&[0x1000, 0x3000]));
        builder.end_node();
        builder.begin_node("second-pool");
        builder.property("size", &cells(&[0x2000]));
        builder.property("alignment", &cells(&[0x1000]));
        builder.property("alloc-ranges", &cells(&[0x1000, 0x3000]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let memory = described_memory_segments(&firmware.memory).unwrap();
        let second_node = firmware.memory.reserved_memory[1].node;
        assert_eq!(
            resolve_memory_layout(&firmware.memory, memory, &[]),
            Err(DtbMemoryLayoutError::DynamicAllocationFailed {
                node: second_node,
                size: 0x2000,
            })
        );
    }

    #[test]
    fn reserved_memory_registry_lookup_uses_the_normalized_phandle() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("memory@1000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x1000, 0x5000]));
        builder.end_node();
        begin_reserved_memory(&mut builder);
        builder.begin_node("pool@2000");
        builder.property("phandle", &cells(&[0x44]));
        builder.property("reg", &cells(&[0x2000, 0x1000]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let layout = resolve_memory_layout(
            &firmware.memory,
            described_memory_segments(&firmware.memory).unwrap(),
            &[],
        )
        .unwrap();
        let found = find_reserved_memory_by_phandle(&layout.reserved_memory, 0x44)
            .expect("normalized runtime registry must retain the reserved-memory phandle");
        assert_eq!(found.request.path, "/reserved-memory/pool@2000");
        assert_eq!(found.ranges[0].start, 0x2000);
        assert!(find_reserved_memory_by_phandle(&layout.reserved_memory, 0x45).is_none());
    }

    #[test]
    fn no_map_granule_expands_allocator_holes_and_rejects_boot_objects() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("memory@1000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x1000, 0x4000]));
        builder.end_node();
        begin_reserved_memory(&mut builder);
        builder.begin_node("secret@1800");
        builder.property("reg", &cells(&[0x1800, 0x100]));
        builder.property("no-map", &[]);
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let base = described_memory_segments(&firmware.memory).unwrap();
        let mut layout = resolve_memory_layout(&firmware.memory, base.clone(), &[]).unwrap();
        let protected = MemorySegment {
            start: 0x1000,
            size: 0x800,
        };
        assert_eq!(
            apply_no_map_granule(&mut layout, 0x1000, &[protected]),
            Err(DtbMemoryLayoutError::NoMapOverlapsProtected {
                range: MemorySegment {
                    start: 0x1000,
                    size: 0x1000,
                },
                protected,
            })
        );

        let mut layout = resolve_memory_layout(&firmware.memory, base, &[]).unwrap();
        apply_no_map_granule(&mut layout, 0x1000, &[]).unwrap();
        assert_eq!(
            layout.no_map_segments,
            vec![MemorySegment {
                start: 0x1000,
                size: 0x1000,
            }]
        );
        assert!(
            layout
                .usable_segments
                .iter()
                .all(|segment| segment.start >= 0x2000)
        );
    }

    #[test]
    fn pci_host_binding_is_strict_and_preserves_range_metadata() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[2]));
        builder.begin_node("msi-zero");
        builder.property("phandle", &cells(&[0x44]));
        builder.property("msi-controller", &[]);
        builder.end_node();
        builder.begin_node("msi-one");
        builder.property("phandle", &cells(&[0x45]));
        builder.property("msi-controller", &[]);
        builder.property("#msi-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("iommu");
        builder.property("phandle", &cells(&[0x46]));
        builder.property("#iommu-cells", &cells(&[2]));
        builder.end_node();
        builder.begin_node("interrupt-controller");
        builder.property("phandle", &cells(&[0x47]));
        builder.property("interrupt-controller", &[]);
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("pcie@30000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("bus-range", &cells(&[0, 0xff]));
        builder.property("reg", &cells(&[0, 0x3000_0000, 0, 0x1000_0000]));
        builder.property("msi-parent", &cells(&[0x44, 0x45, 0x123]));
        builder.property(
            "dma-ranges",
            &cells(&[0x0200_0000, 0, 0, 0, 0x8000_0000, 0, 0x1000]),
        );
        builder.property("iommu-map-mask", &cells(&[0xffff]));
        builder.property("iommu-map", &cells(&[0x100, 0x46, 7, 8, 0x20]));
        builder.property("interrupt-map-mask", &cells(&[0, 0, 0, 7]));
        builder.property("interrupt-map-pass-thru", &cells(&[0, 0, 0, 0]));
        builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 0x47, 9]));
        builder.property(
            "ranges",
            &cells(&[0x4300_0000, 0, 0x4000_0000, 0, 0x4000_0000, 0, 0x1000_0000]),
        );
        builder.begin_node("ethernet@0,2");
        builder.property("reg", &cells(&[0x0000_1100, 0, 0, 0, 0]));
        builder.property("iommus", &cells(&[0x46, 0xaa, 0xbb]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.pcie_hosts.len(), 1);
        let host = &firmware.pcie_hosts[0];
        assert_eq!((host.bus_start, host.bus_end), (0, 0xff));
        assert_eq!(host.ecam_size, 0x1000_0000);
        assert_eq!(host.config_space, DtbPciConfigSpace::Ecam);
        assert_eq!(host.ranges.len(), 1);
        assert_eq!(host.ranges[0].phys_hi, 0x4300_0000);
        assert!(host.ranges[0].memory_64);
        assert_eq!(host.ranges[0].space, DtbPciAddressSpace::PrefetchableMemory);
        assert_eq!(
            host.interrupt_map_pass_thru.as_deref(),
            Some([0, 0, 0, 0].as_slice())
        );
        assert!(host.interrupt_resolver.is_some());
        assert_eq!(host.interrupt_map.len(), 1);
        assert_eq!(host.interrupt_map[0].parent, 0x47);
        assert_eq!(
            host.interrupt_map[0]
                .resolved
                .as_ref()
                .map(|route| (route.parent, route.parent_specifier.as_ref())),
            Some((0x47, [9].as_slice()))
        );
        let route =
            resolve_pci_interrupt(host.interrupt_resolver.as_ref().unwrap(), &[0, 0, 0], &[1])
                .unwrap()
                .unwrap();
        assert_eq!(
            (route.parent, route.parent_specifier.as_ref()),
            (0x47, [9].as_slice())
        );
        assert!(!host.msi_map_present);
        assert_eq!(host.msi_parents.len(), 2);
        assert_eq!(host.msi_parents[0].controller, 0x44);
        assert!(host.msi_parents[0].msi_specifier.is_empty());
        assert_eq!(host.msi_parents[1].controller, 0x45);
        assert_eq!(host.msi_parents[1].msi_specifier.as_ref(), &[0x123]);
        let dma_ranges = host
            .dma_ranges
            .as_ref()
            .expect("PCI dma-ranges must decode");
        assert_eq!(dma_ranges[0].child_address.cells(), &[0x0200_0000, 0, 0]);
        assert_eq!(dma_ranges[0].parent_address.cells(), &[0, 0x8000_0000]);
        assert_eq!(
            dma_ranges[0].size.as_ref().map(DtbCellValue::cells),
            Some([0, 0x1000].as_slice())
        );
        let iommu_map = host.iommu_map.as_ref().expect("PCI iommu-map must decode");
        assert_eq!(iommu_map.mask, 0xffff);
        assert_eq!(iommu_map.entries[0].provider_phandle, 0x46);
        assert_eq!(iommu_map.entries[0].output_base.as_ref(), &[7, 8]);
        assert_eq!(iommu_map.match_id(0x110).unwrap().offset, 0x10);
        assert!(matches!(
            iommu_map.map_id(0x110),
            Err(DtbIommuMapTranslationError::AmbiguousMultiCellRange { .. })
        ));
        assert!(host.graph_endpoints.is_empty());
        assert_eq!(host.children.len(), 1);
        let child = &host.children[0];
        assert_eq!((child.bus, child.device, child.function), (0, 2, 1));
        assert_eq!(child.path.as_ref(), "/pcie@30000000/ethernet@0,2");
        assert!(child.bindings.effective_dma.iommu_required);
        assert_eq!(child.bindings.references[0].property.as_ref(), "iommus");
        assert_eq!(
            child.bindings.effective_dma.windows.as_deref(),
            Some(
                [DtbDmaWindow {
                    cpu_start: 0x8000_0000,
                    dma_start: 0,
                    size: 0x1000,
                }]
                .as_slice()
            )
        );
    }

    #[test]
    fn pci_interrupt_resolver_applies_actual_pass_thru_bits_and_scrubs_boot_secrets() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[2]));
        builder.begin_node("chosen");
        builder.property("rng-seed", &[1, 2, 3, 4]);
        builder.property("kaslr-seed", &[5, 6, 7, 8]);
        builder.end_node();
        builder.begin_node("intc");
        builder.property("phandle", &cells(&[1]));
        builder.property("#address-cells", &cells(&[0]));
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("interrupt-controller", &[]);
        builder.end_node();
        builder.begin_node("pci-intc-nexus");
        builder.property("phandle", &cells(&[3]));
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("interrupt-map-mask", &cells(&[0, 0, 0, 7]));
        builder.property(
            "interrupt-map",
            &cells(&[0, 0, 0, 1, 1, 0x31, 0, 0, 0, 2, 1, 0x32]),
        );
        builder.end_node();
        builder.begin_node("pcie@30000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("bus-range", &cells(&[0, 0]));
        builder.property("reg", &cells(&[0, 0x3000_0000, 0, 0x10_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0, 0, 0x4000_0000, 0, 0x1000]),
        );
        builder.property("interrupt-map-mask", &cells(&[0, 0, 0, 0]));
        builder.property("interrupt-map-pass-thru", &cells(&[0, 0, 0, 7]));
        builder.property("interrupt-map", &cells(&[0, 0, 0, 0, 3, 0, 0, 0, 0]));
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.rng_seed.as_deref(), Some([1, 2, 3, 4].as_slice()));
        let host = &firmware.pcie_hosts[0];
        let resolver = host.interrupt_resolver.as_ref().unwrap();
        assert_eq!(
            host.interrupt_map_pass_thru.as_deref(),
            Some([0, 0, 0, 7].as_slice())
        );
        assert!(host.interrupt_map[0].resolved.is_none());

        let first = resolve_pci_interrupt(resolver, &[0x0000_1100, 0, 0], &[1])
            .unwrap()
            .unwrap();
        let second = resolve_pci_interrupt(resolver, &[0x0000_1100, 0, 0], &[2])
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.parent, first.parent_specifier.as_ref()),
            (1, [0x31].as_slice())
        );
        assert_eq!(
            (second.parent, second.parent_specifier.as_ref()),
            (1, [0x32].as_slice())
        );

        let snapshot = Fdt::parse(resolver.inner.blob.as_ref()).unwrap();
        let chosen = snapshot
            .root()
            .children()
            .find(|node| node.name() == "chosen")
            .unwrap();
        assert!(chosen.find_property("rng-seed").is_none());
        assert!(chosen.find_property("kaslr-seed").is_none());
        assert!(matches!(
            resolve_pci_interrupt(resolver, &[0, 0], &[1]),
            Err(DtbPciInterruptResolveError::InvalidKey { .. })
        ));
        assert_eq!(
            resolve_pci_interrupt(resolver, &[0, 0, 0], &[3]),
            Err(DtbPciInterruptResolveError::InvalidMap)
        );

        let mut malformed = resolver.clone();
        Arc::make_mut(&mut malformed.inner).blob = Arc::from(vec![0u8; 4].into_boxed_slice());
        assert_eq!(
            resolve_pci_interrupt(&malformed, &[0, 0, 0], &[1]),
            Err(DtbPciInterruptResolveError::InvalidSnapshot)
        );
        let mut missing = resolver.clone();
        Arc::make_mut(&mut missing.inner).host_path = "/missing-pcie".into();
        assert_eq!(
            resolve_pci_interrupt(&missing, &[0, 0, 0], &[1]),
            Err(DtbPciInterruptResolveError::MissingHost)
        );
    }

    #[test]
    fn generic_pci_cam_uses_sixty_four_kib_per_bus() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[2]));
        builder.begin_node("pci@30000000");
        builder.property("compatible", b"pci-host-cam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("bus-range", &cells(&[0, 1]));
        builder.property("reg", &cells(&[0, 0x3000_0000, 0, 0x0002_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0x4000_0000, 0, 0x4000_0000, 0, 0x1000_0000]),
        );
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        assert_eq!(firmware.pcie_hosts.len(), 1);
        let host = &firmware.pcie_hosts[0];
        assert_eq!(host.config_space, DtbPciConfigSpace::Cam);
        assert_eq!(host.bus_start, 0);
        assert_eq!(host.bus_end, 1);
        assert_eq!(host.ecam_size, 0x0002_0000);
    }

    #[test]
    fn qemu_loongarch_legacy_pci_interrupt_map_remains_bootable() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[2]));
        builder.begin_node("pch-pic@10000000");
        builder.property("compatible", b"loongson,pch-pic-1.0\0");
        builder.property("phandle", &cells(&[0x8003]));
        builder.property("interrupt-controller", &[]);
        builder.property("#interrupt-cells", &cells(&[2]));
        builder.property("reg", &cells(&[0, 0x1000_0000, 0, 0x400]));
        builder.end_node();
        builder.begin_node("msi@2ff00000");
        builder.property("compatible", b"loongson,pch-msi-1.0\0");
        builder.property("phandle", &cells(&[0x8004]));
        builder.property("interrupt-controller", &[]);
        builder.property("reg", &cells(&[0, 0x2ff0_0000, 0, 8]));
        builder.end_node();
        builder.begin_node("pcie@20000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("device_type", b"pci\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("bus-range", &cells(&[0, 0x7f]));
        builder.property("reg", &cells(&[0, 0x2000_0000, 0, 0x0800_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0x4000_0000, 0, 0x4000_0000, 0, 0x4000_0000]),
        );
        builder.property("interrupt-map-mask", &cells(&[0x1800, 0, 0, 7]));
        builder.property(
            "interrupt-map",
            &cells(&[0, 0, 0, 1, 0x8003, 0x10, 0x800, 0, 0, 2, 0x8003, 0x11]),
        );
        builder.property("msi-map", &cells(&[0, 0x8004, 0, 0x10000]));
        builder.property("dma-coherent", &[]);
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let host = &firmware.pcie_hosts[0];
        assert_eq!(host.interrupt_cells, 1);
        assert_eq!(host.interrupt_map.len(), 2);
        assert_eq!(host.interrupt_map[0].parent, 0x8003);
        assert_eq!(
            host.interrupt_map[0]
                .resolved
                .as_ref()
                .map(|route| route.parent_specifier.as_ref()),
            Some([0x10].as_slice())
        );
        assert_eq!(host.msi_map.len(), 1);
        assert_eq!(host.msi_map[0].msi_specifier.as_ref(), &[0]);
    }

    #[test]
    fn malformed_pci_msi_parent_is_not_partially_exposed() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("msi");
        builder.property("phandle", &cells(&[0x44]));
        builder.property("#msi-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("pcie@30000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("bus-range", &cells(&[0, 0]));
        builder.property("reg", &cells(&[0x3000_0000, 0x10_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0x4000_0000, 0x4000_0000, 0, 0x1000]),
        );
        builder.property("msi-parent", &cells(&[0x44]));
        builder.end_node();
        builder.end_node();

        let bytes: &'static [u8] = Box::leak(builder.finish().into_boxed_slice());
        assert!(matches!(
            parse(Fdt::parse(bytes).unwrap()),
            Err(DtbFirmwareError::InvalidMsi(
                fdt::MsiError::IncompleteEntry {
                    entry: 0,
                    remaining_cells: 0,
                    required_cells: 1,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn pci_host_distinguishes_empty_dma_ranges_from_absence() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("pcie@30000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("bus-range", &cells(&[0, 0]));
        builder.property("reg", &cells(&[0x3000_0000, 0x10_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0x4000_0000, 0x4000_0000, 0, 0x1000]),
        );
        builder.property("dma-ranges", &[]);
        builder.end_node();

        builder.begin_node("pcie@31000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("bus-range", &cells(&[1, 1]));
        builder.property("reg", &cells(&[0x3100_0000, 0x10_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0x5000_0000, 0x5000_0000, 0, 0x1000]),
        );
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let explicit = firmware
            .pcie_hosts
            .iter()
            .find(|host| host.path.as_ref() == "/pcie@30000000")
            .unwrap();
        assert_eq!(explicit.dma_ranges, Some(Vec::new()));
        assert_eq!(explicit.iommu_map, None);
        assert_eq!(explicit.interrupt_map_pass_thru, None);
        assert!(explicit.interrupt_resolver.is_none());
        let absent = firmware
            .pcie_hosts
            .iter()
            .find(|host| host.path.as_ref() == "/pcie@31000000")
            .unwrap();
        assert_eq!(absent.dma_ranges, None);
        assert_eq!(absent.iommu_map, None);
        assert_eq!(absent.interrupt_map_pass_thru, None);
        assert!(absent.interrupt_resolver.is_none());
    }

    #[test]
    fn malformed_pci_suffix_and_boolean_are_not_silently_accepted() {
        let build = |bad_boolean: bool| {
            let mut builder = DtbBuilder::new();
            builder.begin_node("");
            builder.property("#address-cells", &cells(&[1]));
            builder.property("#size-cells", &cells(&[1]));
            builder.begin_node("pcie@30000000");
            builder.property("compatible", b"pci-host-ecam-generic\0");
            builder.property("#address-cells", &cells(&[3]));
            builder.property("#size-cells", &cells(&[2]));
            builder.property("bus-range", &cells(&[0, 0]));
            builder.property("reg", &cells(&[0x3000_0000, 0x10_0000]));
            if bad_boolean {
                builder.property("dma-coherent", &[1]);
                builder.property(
                    "ranges",
                    &cells(&[0x0200_0000, 0, 0x4000_0000, 0x4000_0000, 0, 0x1000]),
                );
            } else {
                builder.property(
                    "ranges",
                    &cells(&[
                        0x0200_0000,
                        0,
                        0x4000_0000,
                        0x4000_0000,
                        0,
                        0x1000,
                        0x0200_0000,
                    ]),
                );
            }
            builder.end_node();
            builder.end_node();
            let bytes: &'static [u8] = Box::leak(builder.finish().into_boxed_slice());
            parse(Fdt::parse(bytes).unwrap())
        };

        assert!(matches!(
            build(false),
            Err(DtbFirmwareError::InvalidPci(
                fdt::PciError::IncompleteEntry { .. }
            ))
        ));
        assert!(matches!(
            build(true),
            Err(DtbFirmwareError::InvalidProperty {
                property: "dma-coherent",
                ..
            })
        ));
    }

    #[test]
    fn platform_bindings_preserve_all_standard_provider_relations() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder.property("dma-coherent", &[]);
        builder.property("dma-ranges", &cells(&[0, 0x8000_0000, 0x1000]));

        builder.begin_node("provider");
        builder.property("phandle", &cells(&[0x10]));
        builder.property("#clock-cells", &cells(&[1]));
        builder.property("#reset-cells", &cells(&[1]));
        builder.property("#dma-cells", &cells(&[1]));
        builder.property("#iommu-cells", &cells(&[2]));
        builder.property("#gpio-cells", &cells(&[2]));
        builder.property("#phy-cells", &cells(&[1]));
        builder.property("#power-domain-cells", &cells(&[1]));
        builder.property("#interconnect-cells", &cells(&[1]));
        builder.property("#pwm-cells", &cells(&[1]));
        builder.property("#mbox-cells", &cells(&[1]));
        builder.property("#io-channel-cells", &cells(&[1]));
        builder.property("#thermal-sensor-cells", &cells(&[1]));
        builder.property("#sound-dai-cells", &cells(&[1]));
        builder.property("interrupt-controller", &[]);
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.end_node();

        builder.begin_node("remote-device");
        builder.begin_node("port");
        builder.begin_node("endpoint");
        builder.property("phandle", &cells(&[0x31]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        builder.begin_node("consumer");
        builder.property("compatible", b"test,consumer\0");
        builder.property("#address-cells", &cells(&[5]));
        builder.property("#size-cells", &cells(&[3]));
        builder.property("clocks", &cells(&[0x10, 1]));
        builder.property("clock-names", b"core\0");
        builder.property("assigned-clocks", &cells(&[0x10, 16]));
        builder.property("assigned-clock-parents", &cells(&[0x10, 17]));
        builder.property("resets", &cells(&[0x10, 2]));
        builder.property("reset-names", b"bus\0");
        builder.property("dmas", &cells(&[0x10, 3]));
        builder.property("dma-names", b"rx\0");
        builder.property("iommus", &cells(&[0x10, 4, 5]));
        builder.property("reset-gpios", &cells(&[0x10, 6, 7]));
        builder.property("phys", &cells(&[0x10, 8]));
        builder.property("phy-names", b"usb\0");
        builder.property("power-domains", &cells(&[0x10, 9]));
        builder.property("power-domain-names", b"main\0");
        builder.property("interconnects", &cells(&[0x10, 10]));
        builder.property("interconnect-names", b"memory\0");
        builder.property("pwms", &cells(&[0x10, 11]));
        builder.property("pwm-names", b"fan\0");
        builder.property("mboxes", &cells(&[0x10, 12]));
        builder.property("mbox-names", b"tx\0");
        builder.property("io-channels", &cells(&[0x10, 13]));
        builder.property("io-channel-names", b"voltage\0");
        builder.property("thermal-sensors", &cells(&[0x10, 14]));
        builder.property("sound-dai", &cells(&[0x10, 15]));
        builder.property("memory-region", &cells(&[0x10]));
        builder.property("memory-region-names", b"scratch\0");
        builder.property("nvmem-cells", &cells(&[0x10]));
        builder.property("nvmem-cell-names", b"calibration\0");
        builder.property("operating-points-v2", &cells(&[0x10]));
        builder.property("interrupt-affinity", &cells(&[0x10]));
        builder.property("wakeup-parent", &cells(&[0x10]));
        builder.property("msi-parent", &cells(&[0x10]));
        builder.property("vdd-supply", &cells(&[0x10]));
        builder.property("pinctrl-names", b"default\0sleep\0");
        builder.property("pinctrl-0", &cells(&[0x10]));
        builder.property("pinctrl-1", &cells(&[0x10]));
        builder.property("interrupt-parent", &cells(&[0x10]));
        builder.property("interrupts", &cells(&[0x55]));
        builder.property("interrupt-names", b"event\0");
        builder.property(
            "dma-ranges",
            &cells(&[0, 1, 2, 3, 4, 0x8000_0000, 0, 0, 0x1000]),
        );
        builder.property("iommu-map-mask", &cells(&[0xff]));
        builder.property("iommu-map", &cells(&[0x20, 0x10, 0x30, 0x40, 0x10]));
        builder.begin_node("ports");
        builder.begin_node("port@0");
        builder.property("reg", &cells(&[0]));
        builder.begin_node("endpoint@1");
        builder.property("reg", &cells(&[1]));
        builder.property("phandle", &cells(&[0x30]));
        builder.property("remote-endpoint", &cells(&[0x31]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        builder.end_node();

        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let device = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/consumer")
            .expect("consumer must be populated as a platform device");
        let bindings = &device.bindings;
        assert_eq!(bindings.references.len(), 24);

        let reference = |property: &str| {
            bindings
                .references
                .iter()
                .find(|reference| reference.property.as_ref() == property)
                .unwrap_or_else(|| panic!("missing normalized {property} reference"))
        };
        assert_eq!(reference("clocks").name.as_deref(), Some("core"));
        assert_eq!(reference("clocks").phandle, 0x10);
        assert!(reference("clocks").provider.is_some());
        assert_eq!(
            reference("clocks").provider_path.as_deref(),
            Some("/soc/provider")
        );
        assert_eq!(reference("clocks").args.as_ref(), &[1]);
        assert_eq!(reference("assigned-clocks").args.as_ref(), &[16]);
        assert_eq!(reference("assigned-clock-parents").args.as_ref(), &[17]);
        assert_eq!(reference("iommus").args.as_ref(), &[4, 5]);
        assert_eq!(reference("reset-gpios").args.as_ref(), &[6, 7]);
        assert_eq!(reference("power-domains").name.as_deref(), Some("main"));
        assert_eq!(reference("interconnects").name.as_deref(), Some("memory"));
        assert_eq!(reference("pwms").name.as_deref(), Some("fan"));
        assert_eq!(reference("pinctrl-0").name.as_deref(), Some("default"));
        assert_eq!(reference("pinctrl-1").name.as_deref(), Some("sleep"));
        assert_eq!(reference("vdd-supply").phandle, 0x10);
        assert_eq!(reference("msi-parent").phandle, 0x10);
        assert_eq!(reference("wakeup-parent").phandle, 0x10);
        assert_eq!(reference("memory-region").name.as_deref(), Some("scratch"));
        assert_eq!(
            reference("nvmem-cells").name.as_deref(),
            Some("calibration")
        );

        let dma_ranges = bindings
            .dma_ranges
            .as_ref()
            .expect("present dma-ranges must remain distinguishable from absence");
        assert_eq!(dma_ranges.len(), 1);
        assert_eq!(dma_ranges[0].child_address.cells(), &[0, 1, 2, 3, 4]);
        assert_eq!(dma_ranges[0].parent_address.cells(), &[0x8000_0000]);
        assert_eq!(
            dma_ranges[0].size.as_ref().map(DtbCellValue::cells),
            Some([0, 0, 0x1000].as_slice())
        );
        assert!(bindings.effective_dma.coherent);
        assert_eq!(
            bindings.effective_dma.windows.as_deref(),
            Some(
                [DtbDmaWindow {
                    cpu_start: 0x8000_0000,
                    dma_start: 0,
                    size: 0x1000,
                }]
                .as_slice()
            )
        );

        let iommu_map = bindings.iommu_map.as_ref().expect("iommu-map must decode");
        assert_eq!(iommu_map.mask, 0xff);
        assert_eq!(iommu_map.entries.len(), 1);
        assert_eq!(iommu_map.entries[0].input_base, 0x20);
        assert_eq!(iommu_map.entries[0].provider_phandle, 0x10);
        assert_eq!(iommu_map.entries[0].provider_path.as_ref(), "/soc/provider");
        assert_eq!(iommu_map.entries[0].output_base.as_ref(), &[0x30, 0x40]);
        assert_eq!(iommu_map.entries[0].length, 0x10);

        assert_eq!(bindings.graph_endpoints.len(), 1);
        let endpoint = &bindings.graph_endpoints[0];
        assert_eq!((endpoint.port_id, endpoint.endpoint_id), (Some(0), Some(1)));
        assert_eq!(endpoint.phandle, Some(0x30));
        assert_eq!(endpoint.remote_phandle, Some(0x31));
        assert!(endpoint.remote.is_some());
        assert_eq!(
            endpoint.node_path.as_ref(),
            "/soc/consumer/ports/port@0/endpoint@1"
        );
        assert_eq!(endpoint.port_path.as_ref(), "/soc/consumer/ports/port@0");
        assert_eq!(
            endpoint.remote_path.as_deref(),
            Some("/soc/remote-device/port/endpoint")
        );
        assert_eq!(device.interrupts.len(), 1);
        assert_eq!(device.interrupts[0].name.as_deref(), Some("event"));
        assert_eq!(device.interrupts[0].provider_path.as_ref(), "/soc/provider");
    }

    #[test]
    fn interrupt_names_are_validated_atomically() {
        let build = |names: &[u8]| {
            let mut builder = DtbBuilder::new();
            builder.begin_node("");
            builder.property("#address-cells", &cells(&[1]));
            builder.property("#size-cells", &cells(&[1]));
            builder.begin_node("soc");
            builder.property("compatible", b"simple-bus\0");
            builder.property("#address-cells", &cells(&[1]));
            builder.property("#size-cells", &cells(&[1]));
            builder.property("ranges", &[]);
            builder.begin_node("intc");
            builder.property("phandle", &cells(&[1]));
            builder.property("interrupt-controller", &[]);
            builder.property("#interrupt-cells", &cells(&[1]));
            builder.end_node();
            builder.begin_node("device");
            builder.property("compatible", b"test,named-irqs\0");
            builder.property("interrupt-parent", &cells(&[1]));
            builder.property("interrupts", &cells(&[3, 4]));
            builder.property("interrupt-names", names);
            builder.end_node();
            builder.end_node();
            builder.end_node();
            parse_test_firmware_result(builder)
        };

        let firmware = build(b"rx\0tx\0").expect("matching names must parse");
        let device = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/device")
            .unwrap();
        assert_eq!(device.interrupts[0].name.as_deref(), Some("rx"));
        assert_eq!(device.interrupts[1].name.as_deref(), Some("tx"));

        assert!(matches!(
            build(b"only-one\0"),
            Err(DtbFirmwareError::InvalidValue {
                property: "interrupt-names",
                ..
            })
        ));
        assert!(matches!(
            build(b"unterminated"),
            Err(DtbFirmwareError::InvalidProperty {
                property: "interrupt-names",
                ..
            })
        ));
    }

    #[test]
    fn effective_dma_inherits_window_across_bus_without_dma_ranges() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder.property("dma-ranges", &cells(&[0, 0x8000_0000, 0x1000]));

        builder.begin_node("bridge");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);

        builder.begin_node("device");
        builder.property("compatible", b"test,inherited-dma-window\0");
        builder.end_node();
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let device = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/bridge/device")
            .expect("nested DMA consumer must be populated");
        assert_eq!(
            device.bindings.effective_dma.windows.as_deref(),
            Some(
                [DtbDmaWindow {
                    cpu_start: 0x8000_0000,
                    dma_start: 0,
                    size: 0x1000,
                }]
                .as_slice()
            )
        );
        assert!(!device.bindings.effective_dma.unsupported);
    }

    #[test]
    fn wide_or_sizeless_reg_is_preserved_without_native_mmio_projection() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("wide-bus");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[5]));
        builder.property("#size-cells", &cells(&[0]));
        builder.begin_node("device");
        builder.property("compatible", b"test,wide-register\0");
        builder.property("reg", &cells(&[1, 2, 3, 4, 5]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let device = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/wide-bus/device")
            .expect("wide register device must remain discoverable");
        assert!(device.reg_ranges.is_empty());
        assert_eq!(device.reg_entries.len(), 1);
        assert_eq!(device.reg_entries[0].address.cells(), &[1, 2, 3, 4, 5]);
        assert_eq!(device.reg_entries[0].size, None);
        assert_eq!(device.reg_entries[0].translated_address, None);
        assert_eq!(device.reg_entries[0].mmio, None);
    }

    #[test]
    fn unavailable_direct_iommu_provider_blocks_dma_but_empty_slot_does_not() {
        let mut builder = basic_platform_tree();
        builder.begin_node("iommu");
        builder.property("phandle", &cells(&[1]));
        builder.property("#iommu-cells", &cells(&[1]));
        builder.property("status", b"disabled\0");
        builder.end_node();
        builder.begin_node("blocked-device");
        builder.property("compatible", b"test,blocked-iommu\0");
        builder.property("iommus", &cells(&[1, 7]));
        builder.end_node();
        builder.begin_node("empty-slot-device");
        builder.property("compatible", b"test,empty-iommu-slot\0");
        builder.property("iommus", &cells(&[0]));
        builder.end_node();
        finish_basic_platform_tree(&mut builder);

        let firmware = parse_test_firmware_from(builder);
        let blocked = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/blocked-device")
            .expect("disabled-IOMMU consumer must be populated");
        let reference = blocked
            .bindings
            .references
            .iter()
            .find(|reference| reference.property.as_ref() == "iommus")
            .expect("direct IOMMU reference must be preserved");
        assert_eq!(reference.provider_available, Some(false));
        assert!(blocked.bindings.effective_dma.iommu_required);
        assert!(blocked.bindings.effective_dma.unsupported);

        let empty = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/soc/empty-slot-device")
            .expect("empty-slot consumer must be populated");
        assert_eq!(
            empty.bindings.references[0].provider_available, None,
            "phandle 0 must remain an empty slot"
        );
        assert!(!empty.bindings.effective_dma.iommu_required);
        assert!(!empty.bindings.effective_dma.unsupported);
    }

    #[test]
    fn generic_dma_parent_iommu_map_blocks_only_available_providers() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("iommu-enabled");
        builder.property("phandle", &cells(&[1]));
        builder.property("#iommu-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("iommu-disabled");
        builder.property("phandle", &cells(&[2]));
        builder.property("#iommu-cells", &cells(&[1]));
        builder.property("status", b"disabled\0");
        builder.end_node();

        builder.begin_node("active-bus");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder.property("dma-ranges", &cells(&[0, 0x8000_0000, 0x1000]));
        builder.property("iommu-map", &cells(&[0, 1, 7, 1]));
        builder.begin_node("device");
        builder.property("compatible", b"test,active-iommu-map\0");
        builder.end_node();
        builder.end_node();

        builder.begin_node("fallback-bus");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder.property("dma-ranges", &cells(&[0, 0x9000_0000, 0x2000]));
        builder.property("iommu-map", &cells(&[0, 2, 9, 1]));
        builder.begin_node("device");
        builder.property("compatible", b"test,disabled-iommu-map\0");
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let active = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/active-bus/device")
            .expect("active-map consumer must be populated");
        assert!(active.bindings.effective_dma.iommu_required);
        assert!(active.bindings.effective_dma.unsupported);
        assert_eq!(
            active.bindings.effective_dma.windows.as_deref(),
            Some(
                [DtbDmaWindow {
                    cpu_start: 0x8000_0000,
                    dma_start: 0,
                    size: 0x1000,
                }]
                .as_slice()
            )
        );

        let fallback = firmware
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == "/fallback-bus/device")
            .expect("disabled-map consumer must be populated");
        assert!(!fallback.bindings.effective_dma.iommu_required);
        assert!(!fallback.bindings.effective_dma.unsupported);
        assert_eq!(
            fallback.bindings.effective_dma.windows.as_deref(),
            Some(
                [DtbDmaWindow {
                    cpu_start: 0x9000_0000,
                    dma_start: 0,
                    size: 0x2000,
                }]
                .as_slice()
            )
        );
    }

    #[test]
    fn generic_dma_parent_chain_validates_every_iommu_map() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("iommu");
        builder.property("phandle", &cells(&[1]));
        builder.property("#iommu-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("dma-nexus-b");
        builder.property("phandle", &cells(&[0x21]));
        builder.property("#interconnect-cells", &cells(&[0]));
        builder.property("iommu-map", &cells(&[0, 1, 8]));
        builder.end_node();
        builder.begin_node("dma-nexus-a");
        builder.property("phandle", &cells(&[0x20]));
        builder.property("#interconnect-cells", &cells(&[0]));
        builder.property("interconnects", &cells(&[0x21]));
        builder.property("interconnect-names", b"dma-mem\0");
        builder.property("iommu-map", &cells(&[0, 1, 7, 1]));
        builder.end_node();

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder.begin_node("device");
        builder.property("compatible", b"test,dma-map-validation\0");
        builder.property("dma-coherent", &[]);
        builder.property("interconnects", &cells(&[0x20]));
        builder.property("interconnect-names", b"dma-mem\0");
        builder.end_node();
        builder.end_node();
        builder.end_node();

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::InvalidIdMap(
                fdt::IdMapError::IncompleteEntry { .. }
            ))
        ));
    }

    #[test]
    fn generic_dma_parent_chain_cycle_is_rejected_after_coherency_is_known() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("dma-nexus-a");
        builder.property("phandle", &cells(&[0x20]));
        builder.property("#interconnect-cells", &cells(&[0]));
        builder.property("interconnects", &cells(&[0x21]));
        builder.property("interconnect-names", b"dma-mem\0");
        builder.end_node();
        builder.begin_node("dma-nexus-b");
        builder.property("phandle", &cells(&[0x21]));
        builder.property("#interconnect-cells", &cells(&[0]));
        builder.property("interconnects", &cells(&[0x20]));
        builder.property("interconnect-names", b"dma-mem\0");
        builder.end_node();

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder.begin_node("device");
        builder.property("compatible", b"test,dma-parent-cycle\0");
        builder.property("dma-coherent", &[]);
        builder.property("interconnects", &cells(&[0x20]));
        builder.property("interconnect-names", b"dma-mem\0");
        builder.end_node();
        builder.end_node();
        builder.end_node();

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::DmaParentCycle(_))
        ));
    }

    #[test]
    fn pci_iommu_map_remains_a_per_bdf_policy() {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("iommu");
        builder.property("phandle", &cells(&[0x46]));
        builder.property("#iommu-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("pcie@30000000");
        builder.property("compatible", b"pci-host-ecam-generic\0");
        builder.property("#address-cells", &cells(&[3]));
        builder.property("#size-cells", &cells(&[2]));
        builder.property("bus-range", &cells(&[0, 0]));
        builder.property("reg", &cells(&[0x3000_0000, 0x10_0000]));
        builder.property(
            "ranges",
            &cells(&[0x0200_0000, 0, 0x4000_0000, 0x4000_0000, 0, 0x1000]),
        );
        builder.property("iommu-map", &cells(&[0x11, 0x46, 7, 1]));
        builder.begin_node("ethernet@0,2");
        builder.property("reg", &cells(&[0x0000_1100, 0, 0, 0, 0]));
        builder.end_node();
        builder.end_node();
        builder.end_node();

        let firmware = parse_test_firmware_from(builder);
        let host = &firmware.pcie_hosts[0];
        assert!(
            host.iommu_map
                .as_ref()
                .and_then(|map| map.match_id(0x11))
                .is_some()
        );
        assert!(!host.effective_dma.iommu_required);
        assert!(!host.effective_dma.unsupported);
        assert_eq!(host.children.len(), 1);
        assert!(!host.children[0].bindings.effective_dma.iommu_required);
        assert!(!host.children[0].bindings.effective_dma.unsupported);
    }

    #[test]
    fn malformed_platform_specifier_fails_the_complete_firmware_parse() {
        let mut builder = basic_platform_tree();
        builder.begin_node("provider");
        builder.property("phandle", &cells(&[1]));
        builder.property("#clock-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("device");
        builder.property("compatible", b"test,device\0");
        builder.property("clocks", &cells(&[1]));
        builder.end_node();
        finish_basic_platform_tree(&mut builder);

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::InvalidSpecifier(
                fdt::SpecifierError::IncompleteEntry { .. }
            ))
        ));
    }

    #[test]
    fn malformed_platform_iommu_map_fails_the_complete_firmware_parse() {
        let mut builder = basic_platform_tree();
        builder.begin_node("iommu");
        builder.property("phandle", &cells(&[1]));
        builder.property("#iommu-cells", &cells(&[1]));
        builder.end_node();
        builder.begin_node("device");
        builder.property("compatible", b"test,device\0");
        builder.property("iommu-map", &cells(&[0, 1, 7]));
        builder.end_node();
        finish_basic_platform_tree(&mut builder);

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::InvalidIdMap(
                fdt::IdMapError::IncompleteEntry { .. }
            ))
        ));
    }

    #[test]
    fn malformed_platform_dma_ranges_fail_the_complete_firmware_parse() {
        let mut builder = basic_platform_tree();
        builder.begin_node("device");
        builder.property("compatible", b"test,device\0");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("dma-ranges", &cells(&[0, 1, 0x8000_0000]));
        builder.end_node();
        finish_basic_platform_tree(&mut builder);

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::InvalidCellAddress(
                fdt::CellAddressError::IncompleteEntry { .. }
            ))
        ));
    }

    #[test]
    fn malformed_platform_graph_fails_the_complete_firmware_parse() {
        let mut builder = basic_platform_tree();
        builder.begin_node("device");
        builder.property("compatible", b"test,device\0");
        builder.begin_node("port");
        builder.begin_node("endpoint");
        builder.property("remote-endpoint", &cells(&[0xdead]));
        builder.end_node();
        builder.end_node();
        builder.end_node();
        finish_basic_platform_tree(&mut builder);

        assert!(matches!(
            parse_test_firmware_result(builder),
            Err(DtbFirmwareError::InvalidGraph(
                fdt::GraphError::UnknownRemote {
                    phandle: 0xdead,
                    ..
                }
            ))
        ));
    }

    fn parse_test_firmware() -> DtbFirmwareInfo {
        let bytes = semantic_blob();
        parse(Fdt::parse(&bytes).expect("test DTB must be valid"))
            .expect("test DTB must produce firmware information")
    }

    fn parse_test_firmware_from(builder: DtbBuilder) -> DtbFirmwareInfo {
        let bytes = builder.finish();
        parse(Fdt::parse(&bytes).expect("test DTB must be valid"))
            .expect("test DTB must produce firmware information")
    }

    fn parse_test_firmware_result(
        builder: DtbBuilder,
    ) -> Result<DtbFirmwareInfo, DtbFirmwareError> {
        let bytes = builder.finish();
        parse(Fdt::parse(&bytes).expect("test DTB must be valid"))
    }

    fn cpu_tree_builder() -> DtbBuilder {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[2]));
        builder.property("#size-cells", &cells(&[2]));
        builder.begin_node("cpus");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[0]));
        builder
    }

    fn add_test_cpu(
        builder: &mut DtbBuilder,
        reg: u32,
        phandle: u32,
        capacity: Option<u32>,
        interrupt_controllers: &[u32],
    ) {
        builder.begin_node(&alloc::format!("cpu@{reg:x}"));
        builder.property("device_type", b"cpu\0");
        builder.property("compatible", b"test,cpu\0");
        builder.property("reg", &cells(&[reg]));
        builder.property("phandle", &cells(&[phandle]));
        if let Some(capacity) = capacity {
            builder.property("capacity-dmips-mhz", &cells(&[capacity]));
        }
        for (index, phandle) in interrupt_controllers.iter().copied().enumerate() {
            builder.begin_node(&alloc::format!("interrupt-controller{index}"));
            builder.property("interrupt-controller", &[]);
            builder.property("#interrupt-cells", &cells(&[1]));
            builder.property("phandle", &cells(&[phandle]));
            builder.end_node();
        }
        builder.end_node();
    }

    fn finish_cpu_tree(builder: &mut DtbBuilder) {
        builder.end_node();
        builder.end_node();
    }

    fn basic_platform_tree() -> DtbBuilder {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
        builder
    }

    fn finish_basic_platform_tree(builder: &mut DtbBuilder) {
        builder.end_node();
        builder.end_node();
    }

    fn begin_reserved_memory(builder: &mut DtbBuilder) {
        builder.begin_node("reserved-memory");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);
    }

    fn efi_region(segment: MemorySegment, source_type: u32) -> crate::StartMemoryRegion {
        let kind = match source_type {
            0 => crate::StartMemoryRegionKind::Reserved,
            4 => crate::StartMemoryRegionKind::FirmwareReclaimable,
            7 => crate::StartMemoryRegionKind::UsableRam,
            _ => crate::StartMemoryRegionKind::Reserved,
        };
        crate::StartMemoryRegion::new(
            crate::StartPhysRange::new(segment.start, segment.end()),
            kind,
            0,
        )
        .with_source_type(source_type)
    }

    fn semantic_blob() -> Vec<u8> {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("compatible", b"test,board\0");

        builder.begin_node("aliases");
        builder.property("serial0", b"/soc/serial@1000\0");
        builder.end_node();

        builder.begin_node("chosen@0");
        builder.property("linux,stdout-path", b"serial0:115200n8\0");
        builder.end_node();

        builder.begin_node("memory@80000000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x8000_0000, 0x0100_0000]));
        builder.end_node();

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &cells(&[0, 0x4000_0000, 0x0001_0000]));

        builder.begin_node("memory@2000");
        builder.property("device_type", b"memory\0");
        builder.property("reg", &cells(&[0x2000, 0x1000]));
        builder.end_node();

        builder.begin_node("serial@1000");
        builder.property("compatible", b"ns16550a\0");
        builder.property("reg", &cells(&[0x1000, 0x100]));
        builder.property("clock-frequency", &cells(&[24_000_000]));
        builder.property("current-speed", &cells(&[115_200]));
        builder.property("label", b"console\0");
        builder.property("opaque", &[0xaa, 0xbb, 0xcc]);
        builder.property("flag", &[]);
        builder.end_node();

        builder.end_node();
        builder.end_node();
        builder.finish()
    }

    fn cells(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn pad_to(bytes: &mut Vec<u8>, alignment: usize) {
        while !bytes.len().is_multiple_of(alignment) {
            bytes.push(0);
        }
    }
}
