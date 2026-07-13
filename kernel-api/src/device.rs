//! ELM 设备对象、驱动、发现源和硬件资源的稳定函数表。
//!
//! `kernel.device@1` 直接投影内核现有的 PnP 设备图和 function 模型。它不定义
//! Unix 设备号、inode、`file_operations` 或固定的字符/块设备联合体；VFS 只是
//! function 的可选观察与投影层。

use crate::{ApiGrantTokenV1, ApiTableHeaderV1, KernelApiTable};

/// 设备 API 的规范命名空间 identifier。
pub const KERNEL_DEVICE_API_IDENTIFIER: &str = "kernel.device";
/// 设备 API 当前唯一版本。
pub const KERNEL_DEVICE_API_VERSION: u16 = 1;

/// 允许观察设备、function 和资源描述。
pub const KERNEL_DEVICE_CAP_OBSERVE: u64 = 1 << 0;
/// 允许注册和注销 PnP 驱动。
pub const KERNEL_DEVICE_CAP_DRIVER: u64 = 1 << 1;
/// 允许注册发现源并发布、撤销设备。
pub const KERNEL_DEVICE_CAP_DISCOVERY: u64 = 1 << 2;
/// 允许注册 function class 和 function 实例。
pub const KERNEL_DEVICE_CAP_FUNCTION: u64 = 1 << 3;
/// 允许映射和访问设备 MMIO 窗口。
pub const KERNEL_DEVICE_CAP_MMIO: u64 = 1 << 4;
/// 允许申请和释放设备 IRQ。
pub const KERNEL_DEVICE_CAP_IRQ: u64 = 1 << 5;
/// 允许申请和释放 MSI vector。
pub const KERNEL_DEVICE_CAP_MSI: u64 = 1 << 6;
/// 允许按设备约束分配和同步 DMA 缓冲区。
pub const KERNEL_DEVICE_CAP_DMA: u64 = 1 << 7;
/// 允许调用设备 function 公布的操作契约。
pub const KERNEL_DEVICE_CAP_INVOKE: u64 = 1 << 8;
/// 设备 API 定义的全部能力位。
pub const KERNEL_DEVICE_CAPABILITIES: u64 = KERNEL_DEVICE_CAP_OBSERVE
    | KERNEL_DEVICE_CAP_DRIVER
    | KERNEL_DEVICE_CAP_DISCOVERY
    | KERNEL_DEVICE_CAP_FUNCTION
    | KERNEL_DEVICE_CAP_MMIO
    | KERNEL_DEVICE_CAP_IRQ
    | KERNEL_DEVICE_CAP_MSI
    | KERNEL_DEVICE_CAP_DMA
    | KERNEL_DEVICE_CAP_INVOKE;

/// 调用成功。
pub const KERNEL_DEVICE_STATUS_OK: i32 = 0;
/// 请求字段、指针范围或结构版本无效。
pub const KERNEL_DEVICE_STATUS_INVALID: i32 = -1;
/// grant、generation、能力或对象所有权校验失败。
pub const KERNEL_DEVICE_STATUS_PERMISSION: i32 = -2;
/// 请求对象不存在或已经被撤销。
pub const KERNEL_DEVICE_STATUS_NOT_FOUND: i32 = -3;
/// 对象仍有调用、子对象或资源，当前操作不能完成。
pub const KERNEL_DEVICE_STATUS_BUSY: i32 = -4;
/// 名称、身份或注册键已存在。
pub const KERNEL_DEVICE_STATUS_EXISTS: i32 = -5;
/// 内核无法为请求分配必要资源。
pub const KERNEL_DEVICE_STATUS_NO_MEMORY: i32 = -6;
/// 设备、总线或平台不支持请求的能力。
pub const KERNEL_DEVICE_STATUS_UNSUPPORTED: i32 = -7;
/// 驱动要求的依赖尚未就绪，应保留设备等待重试。
pub const KERNEL_DEVICE_STATUS_DEFERRED: i32 = -8;
/// 硬件访问或 ELM 回调发生故障。
pub const KERNEL_DEVICE_STATUS_FAULT: i32 = -9;
/// 没有匹配驱动；设备仍然有效且保持已发现状态。
pub const KERNEL_DEVICE_STATUS_NO_DRIVER: i32 = -10;

/// identifier 固定字段的最大字节数。
pub const KERNEL_DEVICE_IDENTIFIER_LEN: usize = 64;
/// 设备和驱动名称固定字段的最大字节数。
pub const KERNEL_DEVICE_NAME_LEN: usize = 64;
/// 动态设备身份的最大字节数。
pub const KERNEL_DEVICE_IDENTITY_LEN: usize = 128;
/// 单个设备属性值的最大字节数。
pub const KERNEL_DEVICE_PROPERTY_VALUE_LEN: usize = 64;
/// 单条不透明资源载荷的最大字节数。
pub const KERNEL_DEVICE_RESOURCE_PAYLOAD_LEN: usize = 64;
/// 单个发布请求允许携带的最大资源数。
pub const KERNEL_DEVICE_MAX_RESOURCES: usize = 8;
/// 单个发布请求允许携带的最大属性数。
pub const KERNEL_DEVICE_MAX_PROPERTIES: usize = 16;
/// function 调用帧的固定载荷长度。
pub const KERNEL_DEVICE_IO_PAYLOAD_LEN: usize = 256;

/// 请求不带额外标志。
pub const KERNEL_DEVICE_FLAG_NONE: u32 = 0;
/// 驱动可以作为同总线精确驱动之外的通用匹配器。
pub const KERNEL_DEVICE_DRIVER_FLAG_GENERIC: u32 = 1 << 0;
/// function 回调允许阻塞。
pub const KERNEL_DEVICE_FUNCTION_FLAG_MAY_BLOCK: u32 = 1 << 0;
/// IRQ 处理器在硬中断 top-half 中执行。
pub const KERNEL_DEVICE_IRQ_MODE_TOP_HALF: u32 = 1;
/// top-half 单次回调的运行时硬上限；超时会触发受控故障退出。
pub const KERNEL_DEVICE_IRQ_TOP_HALF_BUDGET_NS: u64 = 1_000_000;
/// IRQ 处理器由内核投递到可调度上下文后执行。
pub const KERNEL_DEVICE_IRQ_MODE_DEFERRED: u32 = 2;
/// IRQ 来源是设备声明的 IRQ 资源。
pub const KERNEL_DEVICE_IRQ_SOURCE_RESOURCE: u32 = 1;
/// IRQ 来源是先前由同一 cell 分配的 MSI vector。
pub const KERNEL_DEVICE_IRQ_SOURCE_MSI: u32 = 2;

/// MMIO 资源类别。
pub const KERNEL_DEVICE_RESOURCE_MMIO: u32 = 1;
/// 固件 IRQ 描述资源类别。
pub const KERNEL_DEVICE_RESOURCE_IRQ: u32 = 2;
/// DMA 约束资源类别。
pub const KERNEL_DEVICE_RESOURCE_DMA: u32 = 3;
/// MSI 能力资源类别。
pub const KERNEL_DEVICE_RESOURCE_MSI: u32 = 4;
/// 总线自行解释的不透明资源类别起始值。
pub const KERNEL_DEVICE_RESOURCE_CUSTOM_BASE: u32 = 0x1000;

/// DMA 从 CPU 内存传向设备。
pub const KERNEL_DEVICE_DMA_TO_DEVICE: u32 = 1;
/// DMA 从设备传向 CPU 内存。
pub const KERNEL_DEVICE_DMA_FROM_DEVICE: u32 = 2;
/// DMA 双向传输。
pub const KERNEL_DEVICE_DMA_BIDIRECTIONAL: u32 = 3;
/// 把 CPU 修改同步给设备。
pub const KERNEL_DEVICE_DMA_SYNC_FOR_DEVICE: u32 = 1;
/// 把设备修改同步给 CPU。
pub const KERNEL_DEVICE_DMA_SYNC_FOR_CPU: u32 = 2;

/// IRQ 未由当前处理器消费。
pub const KERNEL_DEVICE_IRQ_UNHANDLED: i32 = 0;
/// IRQ 已由当前处理器消费。
pub const KERNEL_DEVICE_IRQ_HANDLED: i32 = 1;
/// 动态 IRQ 资源的 `flags[7:0]` 保存规范 line 类别。
pub const KERNEL_DEVICE_IRQ_RESOURCE_LINE_KIND_MASK: u64 = 0xff;
/// 核间中断 line 类别。
pub const KERNEL_DEVICE_IRQ_LINE_KIND_IPI: u32 = 1;
/// 架构硬件中断 line 类别。
pub const KERNEL_DEVICE_IRQ_LINE_KIND_HARDWARE: u32 = 2;
/// 由显式 controller/domain 解释的中断 line 类别。
pub const KERNEL_DEVICE_IRQ_LINE_KIND_CONTROLLER: u32 = 3;
/// 其它由平台约定的中断 line 类别。
pub const KERNEL_DEVICE_IRQ_LINE_KIND_OTHER: u32 = 4;
/// 动态 IRQ 资源的 `flags[63:32]` 保存 controller/domain。
pub const KERNEL_DEVICE_IRQ_RESOURCE_DOMAIN_SHIFT: u32 = 32;
/// 动态 IRQ 资源的 controller/domain 字段掩码。
pub const KERNEL_DEVICE_IRQ_RESOURCE_DOMAIN_MASK: u64 = (u32::MAX as u64) << 32;
/// 动态 DMA 资源声明 cache coherent。
pub const KERNEL_DEVICE_DMA_RESOURCE_COHERENT: u64 = 1 << 0;
/// 动态 DMA 资源支持 scatter-gather。
pub const KERNEL_DEVICE_DMA_RESOURCE_SCATTER_GATHER: u64 = 1 << 1;
/// 动态 DMA 资源允许 bounce buffer。
pub const KERNEL_DEVICE_DMA_RESOURCE_ALLOW_BOUNCE: u64 = 1 << 2;
/// 动态 DMA 资源的 `flags[31:16]` 保存最大 segment 数。
pub const KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_SHIFT: u32 = 16;
/// 动态 DMA 资源最大 segment 数字段掩码。
pub const KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_MASK: u64 =
    0xffff << KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_SHIFT;

/// 校验动态 IRQ 资源中的规范 line 编码。
///
/// `number` 对应资源的 `start`，`flags` 同时保存 line 类别和可选 controller。
/// 固定总线从自身资源模型解析 IRQ，不使用本编码。
pub const fn valid_dynamic_irq_resource_encoding(number: u64, flags: u64) -> bool {
    let kind = (flags & KERNEL_DEVICE_IRQ_RESOURCE_LINE_KIND_MASK) as u32;
    let domain = (flags >> KERNEL_DEVICE_IRQ_RESOURCE_DOMAIN_SHIFT) as u32;
    let reserved_mask =
        !(KERNEL_DEVICE_IRQ_RESOURCE_LINE_KIND_MASK | KERNEL_DEVICE_IRQ_RESOURCE_DOMAIN_MASK);
    if flags & reserved_mask != 0 {
        return false;
    }
    match kind {
        KERNEL_DEVICE_IRQ_LINE_KIND_IPI => domain == 0 && number == 0,
        KERNEL_DEVICE_IRQ_LINE_KIND_HARDWARE | KERNEL_DEVICE_IRQ_LINE_KIND_OTHER => {
            domain == 0 && number <= usize::MAX as u64
        }
        KERNEL_DEVICE_IRQ_LINE_KIND_CONTROLLER => number <= u32::MAX as u64,
        _ => false,
    }
}

/// `kernel.device@1` 的完整规范布局字符串。
///
/// schema 同时覆盖所有间接参数结构和函数表，不能只在函数指针顺序变化时更新。任何字段
/// 增删、重排、类型或固定数组长度变化都必须同步修改本字符串并重算布局摘要。
#[rustfmt::skip]
pub const KERNEL_DEVICE_LAYOUT_SCHEMA_V1: &str = "kernel.device@1|ApiTableHeaderV1{struct_size:u32,abi_version:u16,reserved0:u16,capabilities:u64}|ApiGrantTokenV1{grant_id:u64,generation:u64}|KernelDeviceIdentifierV1{len:u16,reserved0:u16,reserved1:u32,bytes:[u8;64]}|KernelDeviceNameV1{len:u16,reserved0:u16,reserved1:u32,bytes:[u8;64]}|KernelDeviceHandleV1{id:u64,generation:u64}|KernelDeviceBusRequestV1{struct_size:u32,flags:u32,identifier:KernelDeviceIdentifierV1,device_contract:KernelDeviceIdentifierV1}|KernelDeviceDriverRequestV1{struct_size:u32,flags:u32,name:KernelDeviceNameV1,bus:KernelDeviceIdentifierV1,priority:i16,reserved0:u16,reserved1:u32,match_callback:u64,probe_callback:u64,remove_callback:u64}|KernelDeviceResourceV1{struct_size:u32,kind:u32,index:u32,reserved0:u32,start:u64,length:u64,flags:u64,payload_len:u32,reserved1:u32,payload:[u8;64]}|KernelDevicePropertyV1{struct_size:u32,flags:u32,name:KernelDeviceIdentifierV1,value_len:u32,reserved0:u32,value:[u8;64]}|KernelDevicePublishRequestV1{struct_size:u32,flags:u32,bus:KernelDeviceHandleV1,parent:KernelDeviceHandleV1,name:KernelDeviceNameV1,identity_contract:KernelDeviceIdentifierV1,identity_len:u32,resource_count:u32,property_count:u32,identity:[u8;128],resources:[KernelDeviceResourceV1;8],properties:[KernelDevicePropertyV1;16]}|KernelDeviceSnapshotV1{struct_size:u32,state:u32,handle:KernelDeviceHandleV1,parent:KernelDeviceHandleV1,bus:KernelDeviceIdentifierV1,name:KernelDeviceNameV1,identity_contract:KernelDeviceIdentifierV1,resource_count:u32,function_count:u32,identity_len:u32,identity:[u8;128],bound:u32,property_count:u32,reserved0:u32}|KernelDeviceFunctionSnapshotV1{struct_size:u32,flags:u32,handle:KernelDeviceHandleV1,device:KernelDeviceHandleV1,class:KernelDeviceIdentifierV1,name:KernelDeviceNameV1,operation_contract:KernelDeviceIdentifierV1,active:u32,reserved0:u32}|KernelDeviceMatchFrameV1{struct_size:u32,flags:u32,cell_id:u64,generation:u64,device:KernelDeviceSnapshotV1,matched:u32,reserved0:u32}|KernelDeviceProbeFrameV1{struct_size:u32,flags:u32,cell_id:u64,generation:u64,device:KernelDeviceSnapshotV1,status:i32,reserved0:u32}|KernelDeviceRemoveFrameV1{struct_size:u32,flags:u32,cell_id:u64,generation:u64,device:KernelDeviceSnapshotV1,status:i32,reserved0:u32}|KernelDeviceFunctionClassRequestV1{struct_size:u32,flags:u32,identifier:KernelDeviceIdentifierV1,operation_contract:KernelDeviceIdentifierV1}|KernelDeviceFunctionRequestV1{struct_size:u32,flags:u32,device:KernelDeviceHandleV1,class:KernelDeviceHandleV1,name:KernelDeviceNameV1,invoke_callback:u64,quiesce_callback:u64,drain_callback:u64}|KernelDeviceIoFrameV1{struct_size:u32,flags:u32,function:KernelDeviceHandleV1,opcode:u32,input_len:u32,output_capacity:u32,output_len:u32,payload:[u8;256],status:i32,reserved0:u32}|KernelDeviceMmioMappingV1{struct_size:u32,flags:u32,handle:KernelDeviceHandleV1,physical_address:u64,virtual_address:u64,length:u64}|KernelDeviceIrqRequestV1{struct_size:u32,flags:u32,device:KernelDeviceHandleV1,mode:u32,source_kind:u32,resource_index:u32,shared:u32,msi:KernelDeviceHandleV1,callback:u64}|KernelDeviceIrqFrameV1{struct_size:u32,flags:u32,irq:KernelDeviceHandleV1,line_kind:u32,line_domain:u32,line_number:u64,result:i32,reserved0:u32}|KernelDeviceMsiRequestV1{struct_size:u32,flags:u32,device:KernelDeviceHandleV1,controller:u32,requester:u32}|KernelDeviceMsiAllocationV1{struct_size:u32,flags:u32,handle:KernelDeviceHandleV1,message_address:u64,message_data:u32,line_kind:u32,line_domain:u32,reserved0:u32,line_number:u64}|KernelDeviceDmaRequestV1{struct_size:u32,flags:u32,device:KernelDeviceHandleV1,length:u64,align:u64,direction:u32,resource_index:u32}|KernelDeviceDmaBufferV1{struct_size:u32,flags:u32,handle:KernelDeviceHandleV1,virtual_address:u64,dma_address:u64,length:u64,direction:u32,reserved0:u32}|KernelDeviceApiV1{header:ApiTableHeaderV1,enumerate:fn(ApiGrantTokenV1,u64,*mut KernelDeviceSnapshotV1,*mut u64)->i32,query_device:fn(ApiGrantTokenV1,KernelDeviceHandleV1,*mut KernelDeviceSnapshotV1)->i32,query_resource:fn(ApiGrantTokenV1,KernelDeviceHandleV1,u32,*mut KernelDeviceResourceV1)->i32,query_property:fn(ApiGrantTokenV1,KernelDeviceHandleV1,u32,*mut KernelDevicePropertyV1)->i32,enumerate_function:fn(ApiGrantTokenV1,KernelDeviceHandleV1,u64,*mut KernelDeviceFunctionSnapshotV1,*mut u64)->i32,query_function:fn(ApiGrantTokenV1,KernelDeviceFunctionHandleV1,*mut KernelDeviceFunctionSnapshotV1)->i32,invoke_function:fn(ApiGrantTokenV1,*mut KernelDeviceIoFrameV1)->i32,register_bus:fn(ApiGrantTokenV1,*const KernelDeviceBusRequestV1,*mut KernelDeviceBusHandleV1)->i32,unregister_bus:fn(ApiGrantTokenV1,KernelDeviceBusHandleV1)->i32,register_driver:fn(ApiGrantTokenV1,*const KernelDeviceDriverRequestV1,*mut KernelDeviceDriverHandleV1)->i32,unregister_driver:fn(ApiGrantTokenV1,KernelDeviceDriverHandleV1)->i32,publish_device:fn(ApiGrantTokenV1,*const KernelDevicePublishRequestV1,*mut KernelDeviceHandleV1)->i32,remove_device:fn(ApiGrantTokenV1,KernelDeviceHandleV1)->i32,register_function_class:fn(ApiGrantTokenV1,*const KernelDeviceFunctionClassRequestV1,*mut KernelDeviceFunctionClassHandleV1)->i32,unregister_function_class:fn(ApiGrantTokenV1,KernelDeviceFunctionClassHandleV1)->i32,register_function:fn(ApiGrantTokenV1,*const KernelDeviceFunctionRequestV1,*mut KernelDeviceFunctionHandleV1)->i32,unregister_function:fn(ApiGrantTokenV1,KernelDeviceFunctionHandleV1)->i32,map_mmio:fn(ApiGrantTokenV1,KernelDeviceHandleV1,u32,*mut KernelDeviceMmioMappingV1)->i32,unmap_mmio:fn(ApiGrantTokenV1,KernelDeviceMmioHandleV1)->i32,mmio_read:fn(ApiGrantTokenV1,KernelDeviceMmioHandleV1,u64,u32,*mut u64)->i32,mmio_write:fn(ApiGrantTokenV1,KernelDeviceMmioHandleV1,u64,u32,u64)->i32,request_irq:fn(ApiGrantTokenV1,*const KernelDeviceIrqRequestV1,*mut KernelDeviceIrqHandleV1)->i32,release_irq:fn(ApiGrantTokenV1,KernelDeviceIrqHandleV1)->i32,allocate_msi:fn(ApiGrantTokenV1,*const KernelDeviceMsiRequestV1,*mut KernelDeviceMsiAllocationV1)->i32,release_msi:fn(ApiGrantTokenV1,KernelDeviceMsiHandleV1)->i32,allocate_dma:fn(ApiGrantTokenV1,*const KernelDeviceDmaRequestV1,*mut KernelDeviceDmaBufferV1)->i32,sync_dma:fn(ApiGrantTokenV1,KernelDeviceDmaHandleV1,u32)->i32,release_dma:fn(ApiGrantTokenV1,KernelDeviceDmaHandleV1)->i32}";

/// [`KERNEL_DEVICE_LAYOUT_SCHEMA_V1`] 的 SHA-256。
pub const KERNEL_DEVICE_LAYOUT_HASH_V1: [u8; 32] = [
    0x10, 0x35, 0x7e, 0x37, 0xf8, 0x90, 0xe1, 0xf1, 0x7b, 0x65, 0xbb, 0x02, 0x40, 0xd1, 0xbe, 0xe5,
    0x9b, 0x2a, 0x63, 0xe1, 0x7b, 0xe8, 0x91, 0xba, 0x6b, 0xf1, 0x75, 0x31, 0x41, 0x19, 0x1d, 0x2e,
];

/// 带长度的固定 identifier。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDeviceIdentifierV1 {
    /// 有效 UTF-8 字节数。
    pub len: u16,
    /// v1 必须为零。
    pub reserved0: u16,
    /// v1 必须为零。
    pub reserved1: u32,
    /// identifier 字节；尾部必须清零。
    pub bytes: [u8; KERNEL_DEVICE_IDENTIFIER_LEN],
}

impl KernelDeviceIdentifierV1 {
    /// 构造 identifier；过长、空字符串、非 ASCII 或包含 NUL 时返回 `None`。
    pub fn new(value: &str) -> Option<Self> {
        if value.is_empty()
            || value.len() > KERNEL_DEVICE_IDENTIFIER_LEN
            || !value.is_ascii()
            || value.as_bytes().contains(&0)
        {
            return None;
        }
        let mut out = Self::empty();
        out.len = value.len() as u16;
        out.bytes[..value.len()].copy_from_slice(value.as_bytes());
        Some(out)
    }

    /// 返回全零空值，仅用于初始化输出槽。
    pub const fn empty() -> Self {
        Self {
            len: 0,
            reserved0: 0,
            reserved1: 0,
            bytes: [0; KERNEL_DEVICE_IDENTIFIER_LEN],
        }
    }

    /// 验证并返回 identifier。
    pub fn as_str(&self) -> Option<&str> {
        let len = usize::from(self.len);
        if len == 0
            || len > self.bytes.len()
            || self.reserved0 != 0
            || self.reserved1 != 0
            || self.bytes[len..].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let value = core::str::from_utf8(&self.bytes[..len]).ok()?;
        if !value.is_ascii() || value.as_bytes().contains(&0) {
            return None;
        }
        Some(value)
    }
}

impl Default for KernelDeviceIdentifierV1 {
    fn default() -> Self {
        Self::empty()
    }
}

/// 带长度的固定名称。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDeviceNameV1 {
    /// 有效 UTF-8 字节数。
    pub len: u16,
    /// v1 必须为零。
    pub reserved0: u16,
    /// v1 必须为零。
    pub reserved1: u32,
    /// 名称字节；尾部必须清零。
    pub bytes: [u8; KERNEL_DEVICE_NAME_LEN],
}

impl KernelDeviceNameV1 {
    /// 构造非空名称。
    pub fn new(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > KERNEL_DEVICE_NAME_LEN || value.as_bytes().contains(&0)
        {
            return None;
        }
        let mut out = Self::empty();
        out.len = value.len() as u16;
        out.bytes[..value.len()].copy_from_slice(value.as_bytes());
        core::str::from_utf8(&out.bytes[..value.len()]).ok()?;
        Some(out)
    }

    /// 返回全零空值，仅用于初始化输出槽。
    pub const fn empty() -> Self {
        Self {
            len: 0,
            reserved0: 0,
            reserved1: 0,
            bytes: [0; KERNEL_DEVICE_NAME_LEN],
        }
    }

    /// 验证并返回名称。
    pub fn as_str(&self) -> Option<&str> {
        let len = usize::from(self.len);
        if len == 0
            || len > self.bytes.len()
            || self.reserved0 != 0
            || self.reserved1 != 0
            || self.bytes[len..].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        core::str::from_utf8(&self.bytes[..len]).ok()
    }
}

impl Default for KernelDeviceNameV1 {
    fn default() -> Self {
        Self::empty()
    }
}

/// 绑定 cell generation 的设备对象句柄。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KernelDeviceHandleV1 {
    /// 内核运行期对象编号。
    pub id: u64,
    /// 创建或取得句柄时的 owner generation。
    pub generation: u64,
}

impl KernelDeviceHandleV1 {
    /// 返回句柄是否具备基本字段。
    pub const fn is_well_formed(self) -> bool {
        self.id != 0 && self.generation != 0
    }
}

impl KernelDeviceSnapshotV1 {
    /// 验证由内核写出的设备快照固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.handle.is_well_formed()
            && (self.parent == KernelDeviceHandleV1::default() || self.parent.is_well_formed())
            && self.bus.as_str().is_some()
            && self.name.as_str().is_some()
            && self.identity_contract.as_str().is_some()
            && (self.identity_len as usize) <= self.identity.len()
            && self.identity[self.identity_len as usize..]
                .iter()
                .all(|byte| *byte == 0)
            && self.resource_count <= KERNEL_DEVICE_MAX_RESOURCES as u32
            && self.property_count <= KERNEL_DEVICE_MAX_PROPERTIES as u32
            && self.reserved0 == 0
            && self.bound <= 1
    }
}

/// 动态总线注册句柄。
pub type KernelDeviceBusHandleV1 = KernelDeviceHandleV1;
/// 驱动注册句柄。
pub type KernelDeviceDriverHandleV1 = KernelDeviceHandleV1;
/// function class 注册句柄。
pub type KernelDeviceFunctionClassHandleV1 = KernelDeviceHandleV1;
/// function 实例句柄。
pub type KernelDeviceFunctionHandleV1 = KernelDeviceHandleV1;
/// MMIO 映射句柄。
pub type KernelDeviceMmioHandleV1 = KernelDeviceHandleV1;
/// IRQ 注册句柄。
pub type KernelDeviceIrqHandleV1 = KernelDeviceHandleV1;
/// MSI 分配句柄。
pub type KernelDeviceMsiHandleV1 = KernelDeviceHandleV1;
/// DMA 缓冲区句柄。
pub type KernelDeviceDmaHandleV1 = KernelDeviceHandleV1;

/// 注册动态总线的请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceBusRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 总线 identifier。
    pub identifier: KernelDeviceIdentifierV1,
    /// 总线设备描述契约。
    pub device_contract: KernelDeviceIdentifierV1,
}

impl KernelDeviceBusRequestV1 {
    /// 构造动态总线注册请求。
    pub fn new(identifier: &str, device_contract: &str) -> Option<Self> {
        Some(Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            identifier: KernelDeviceIdentifierV1::new(identifier)?,
            device_contract: KernelDeviceIdentifierV1::new(device_contract)?,
        })
    }

    /// 验证固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.identifier.as_str().is_some()
            && self.device_contract.as_str().is_some()
    }
}

/// ELM 驱动注册请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceDriverRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// [`KERNEL_DEVICE_DRIVER_FLAG_GENERIC`] 等标志。
    pub flags: u32,
    /// 驱动名称。
    pub name: KernelDeviceNameV1,
    /// 精确匹配的总线 identifier；通用兜底驱动必须写入 `generic`。
    pub bus: KernelDeviceIdentifierV1,
    /// 同总线匹配优先级。
    pub priority: i16,
    /// v1 必须为零。
    pub reserved0: u16,
    /// v1 必须为零。
    pub reserved1: u32,
    /// `extern "C" fn(*mut KernelDeviceMatchFrameV1) -> i32` 地址。
    pub match_callback: u64,
    /// `extern "C" fn(*mut KernelDeviceProbeFrameV1) -> i32` 地址。
    pub probe_callback: u64,
    /// `extern "C" fn(*mut KernelDeviceRemoveFrameV1) -> i32` 地址。
    pub remove_callback: u64,
}

impl KernelDeviceDriverRequestV1 {
    /// 验证不依赖当前镜像的固定字段。
    pub fn is_well_formed(&self) -> bool {
        let bus = self.bus.as_str();
        let generic = self.flags & KERNEL_DEVICE_DRIVER_FLAG_GENERIC != 0;
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags & !KERNEL_DEVICE_DRIVER_FLAG_GENERIC == 0
            && self.name.as_str().is_some()
            && bus.is_some()
            && (generic && bus == Some("generic") || !generic && bus != Some("generic"))
            && self.reserved0 == 0
            && self.reserved1 == 0
            && self.match_callback != 0
            && self.probe_callback != 0
            && self.remove_callback != 0
    }
}

/// 动态设备资源描述。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDeviceResourceV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// [`KERNEL_DEVICE_RESOURCE_MMIO`] 等类别。
    pub kind: u32,
    /// 同类别资源索引。
    pub index: u32,
    /// v1 必须为零。
    pub reserved0: u32,
    /// MMIO 物理起点、IRQ line、DMA 地址掩码或 MSI controller。
    pub start: u64,
    /// MMIO 窗口长度、DMA 最大 segment 长度或 MSI requester；IRQ 必须为零。
    pub length: u64,
    /// 类别定义的属性位；IRQ 和 DMA 使用本模块公布的固定编码。
    pub flags: u64,
    /// 不透明载荷有效长度。
    pub payload_len: u32,
    /// v1 必须为零。
    pub reserved1: u32,
    /// 总线或资源契约解释的不透明数据。
    pub payload: [u8; KERNEL_DEVICE_RESOURCE_PAYLOAD_LEN],
}

/// 动态设备属性描述。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDevicePropertyV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 属性 identifier。
    pub name: KernelDeviceIdentifierV1,
    /// 属性值有效字节数。
    pub value_len: u32,
    /// v1 必须为零。
    pub reserved0: u32,
    /// 不透明但有长度边界的属性值。
    pub value: [u8; KERNEL_DEVICE_PROPERTY_VALUE_LEN],
}

impl KernelDevicePropertyV1 {
    /// 构造一个属性记录。
    pub fn new(name: &str, value: &[u8]) -> Option<Self> {
        if value.len() > KERNEL_DEVICE_PROPERTY_VALUE_LEN {
            return None;
        }
        let mut property = Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            name: KernelDeviceIdentifierV1::new(name)?,
            value_len: value.len() as u32,
            reserved0: 0,
            value: [0; KERNEL_DEVICE_PROPERTY_VALUE_LEN],
        };
        property.value[..value.len()].copy_from_slice(value);
        Some(property)
    }

    /// 验证属性记录及其尾部清零约束。
    pub fn is_well_formed(&self) -> bool {
        let length = self.value_len as usize;
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.name.as_str().is_some()
            && length <= self.value.len()
            && self.reserved0 == 0
            && self.value[length..].iter().all(|byte| *byte == 0)
    }
}

impl Default for KernelDevicePropertyV1 {
    fn default() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            name: KernelDeviceIdentifierV1::empty(),
            value_len: 0,
            reserved0: 0,
            value: [0; KERNEL_DEVICE_PROPERTY_VALUE_LEN],
        }
    }
}

impl KernelDeviceResourceV1 {
    /// 返回全零输出槽。
    pub const fn empty() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            kind: 0,
            index: 0,
            reserved0: 0,
            start: 0,
            length: 0,
            flags: 0,
            payload_len: 0,
            reserved1: 0,
            payload: [0; KERNEL_DEVICE_RESOURCE_PAYLOAD_LEN],
        }
    }

    /// 验证结构和载荷边界。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && (matches!(
                self.kind,
                KERNEL_DEVICE_RESOURCE_MMIO
                    | KERNEL_DEVICE_RESOURCE_IRQ
                    | KERNEL_DEVICE_RESOURCE_DMA
                    | KERNEL_DEVICE_RESOURCE_MSI
            ) || self.kind >= KERNEL_DEVICE_RESOURCE_CUSTOM_BASE)
            && self.reserved0 == 0
            && self.reserved1 == 0
            && self.payload_len as usize <= self.payload.len()
            && self.payload[self.payload_len as usize..]
                .iter()
                .all(|byte| *byte == 0)
    }

    /// 校验由动态发现源发布的标准资源语义。
    ///
    /// 自定义资源只受结构和载荷边界约束；MMIO、IRQ、DMA 与 MSI 必须使用本模块
    /// 固定的字段编码。固定 PCI、USB 或 platform 资源由各自总线模型生成，不调用
    /// 该方法。
    pub fn has_valid_dynamic_encoding(&self) -> bool {
        if !self.is_well_formed() {
            return false;
        }
        match self.kind {
            KERNEL_DEVICE_RESOURCE_MMIO => {
                self.length != 0
                    && self.start.checked_add(self.length).is_some()
                    && self.payload_len == 0
            }
            KERNEL_DEVICE_RESOURCE_IRQ => {
                self.length == 0
                    && self.payload_len == 0
                    && valid_dynamic_irq_resource_encoding(self.start, self.flags)
            }
            KERNEL_DEVICE_RESOURCE_MSI => {
                self.start <= u32::MAX as u64
                    && self.length <= u32::MAX as u64
                    && self.flags == 0
                    && self.payload_len == 0
            }
            KERNEL_DEVICE_RESOURCE_DMA => {
                let allowed_flags = KERNEL_DEVICE_DMA_RESOURCE_COHERENT
                    | KERNEL_DEVICE_DMA_RESOURCE_SCATTER_GATHER
                    | KERNEL_DEVICE_DMA_RESOURCE_ALLOW_BOUNCE
                    | KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_MASK;
                self.start <= usize::MAX as u64
                    && self.length != 0
                    && self.length <= usize::MAX as u64
                    && self.flags & !allowed_flags == 0
                    && self.flags & KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_MASK != 0
                    && self.payload_len == 0
            }
            _ => self.kind >= KERNEL_DEVICE_RESOURCE_CUSTOM_BASE,
        }
    }
}

impl Default for KernelDeviceResourceV1 {
    fn default() -> Self {
        Self::empty()
    }
}

/// 发布动态设备的请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDevicePublishRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 已注册动态总线。
    pub bus: KernelDeviceBusHandleV1,
    /// 可选父设备；全零表示根设备。
    pub parent: KernelDeviceHandleV1,
    /// 设备内部名称。
    pub name: KernelDeviceNameV1,
    /// 身份契约，必须与总线注册声明兼容。
    pub identity_contract: KernelDeviceIdentifierV1,
    /// 规范化身份字节数。
    pub identity_len: u32,
    /// 资源记录数。
    pub resource_count: u32,
    /// 属性记录数。
    pub property_count: u32,
    /// 规范化身份字节串。
    pub identity: [u8; KERNEL_DEVICE_IDENTITY_LEN],
    /// 设备资源记录。
    pub resources: [KernelDeviceResourceV1; KERNEL_DEVICE_MAX_RESOURCES],
    /// 设备属性记录。
    pub properties: [KernelDevicePropertyV1; KERNEL_DEVICE_MAX_PROPERTIES],
}

impl KernelDevicePublishRequestV1 {
    /// 验证固定字段、身份和资源数组边界。
    pub fn is_well_formed(&self) -> bool {
        let identity_len = self.identity_len as usize;
        let resource_count = self.resource_count as usize;
        let property_count = self.property_count as usize;
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.bus.is_well_formed()
            && (self.parent == KernelDeviceHandleV1::default() || self.parent.is_well_formed())
            && self.name.as_str().is_some()
            && self.identity_contract.as_str().is_some()
            && identity_len != 0
            && identity_len <= self.identity.len()
            && self.identity[identity_len..].iter().all(|byte| *byte == 0)
            && resource_count <= self.resources.len()
            && property_count <= self.properties.len()
            && self.resources[..resource_count]
                .iter()
                .all(KernelDeviceResourceV1::is_well_formed)
            && self.resources[..resource_count]
                .iter()
                .enumerate()
                .all(|(index, resource)| {
                    self.resources[..index]
                        .iter()
                        .all(|other| other.kind != resource.kind || other.index != resource.index)
                })
            && self.resources[resource_count..]
                .iter()
                .all(|resource| *resource == KernelDeviceResourceV1::empty())
            && self.properties[..property_count]
                .iter()
                .all(KernelDevicePropertyV1::is_well_formed)
            && self.properties[..property_count]
                .iter()
                .enumerate()
                .all(|(index, property)| {
                    self.properties[..index]
                        .iter()
                        .all(|other| other.name != property.name)
                })
            && self.properties[property_count..]
                .iter()
                .all(|property| *property == KernelDevicePropertyV1::default())
    }
}

/// 设备只读快照。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceSnapshotV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// 当前 PnP 状态编码。
    pub state: u32,
    /// 设备句柄。
    pub handle: KernelDeviceHandleV1,
    /// 父设备句柄；全零表示根设备。
    pub parent: KernelDeviceHandleV1,
    /// 总线 identifier。
    pub bus: KernelDeviceIdentifierV1,
    /// 设备名称。
    pub name: KernelDeviceNameV1,
    /// 身份契约；固定总线可以使用其规范契约。
    pub identity_contract: KernelDeviceIdentifierV1,
    /// 当前资源数。
    pub resource_count: u32,
    /// 当前 function 数。
    pub function_count: u32,
    /// 规范化身份字节数。
    pub identity_len: u32,
    /// 固定总线或动态总线的规范化身份。
    pub identity: [u8; KERNEL_DEVICE_IDENTITY_LEN],
    /// 是否已经绑定驱动。
    pub bound: u32,
    /// 当前属性数。
    pub property_count: u32,
    /// v1 必须为零。
    pub reserved0: u32,
}

impl Default for KernelDeviceSnapshotV1 {
    fn default() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            state: 0,
            handle: KernelDeviceHandleV1::default(),
            parent: KernelDeviceHandleV1::default(),
            bus: KernelDeviceIdentifierV1::empty(),
            name: KernelDeviceNameV1::empty(),
            identity_contract: KernelDeviceIdentifierV1::empty(),
            resource_count: 0,
            function_count: 0,
            identity_len: 0,
            identity: [0; KERNEL_DEVICE_IDENTITY_LEN],
            bound: 0,
            property_count: 0,
            reserved0: 0,
        }
    }
}

/// 设备 function 的只读快照。
///
/// function 是设备向其它内核组成部分开放的契约化能力，不等同于 Unix 字符设备、
/// 块设备或文件节点。`operation_contract` 决定可使用的 opcode 和载荷格式。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelDeviceFunctionSnapshotV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 当前 cell generation 内有效的 function 句柄。
    pub handle: KernelDeviceFunctionHandleV1,
    /// function 所属设备。
    pub device: KernelDeviceHandleV1,
    /// function class identifier。
    pub class: KernelDeviceIdentifierV1,
    /// function 实例名称。
    pub name: KernelDeviceNameV1,
    /// opcode 和载荷语义所属的契约 identifier；空值表示没有通用调用面。
    pub operation_contract: KernelDeviceIdentifierV1,
    /// function 当前是否接受新调用。
    pub active: u32,
    /// v1 必须为零。
    pub reserved0: u32,
}

impl KernelDeviceFunctionSnapshotV1 {
    /// 验证由内核写出的 function 快照固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.handle.is_well_formed()
            && self.device.is_well_formed()
            && self.class.as_str().is_some()
            && self.name.as_str().is_some()
            && (self.operation_contract == KernelDeviceIdentifierV1::empty()
                || self.operation_contract.as_str().is_some())
            && self.active <= 1
            && self.reserved0 == 0
    }
}

/// 驱动匹配回调帧。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceMatchFrameV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 调用目标 cell。
    pub cell_id: u64,
    /// 调用目标 generation。
    pub generation: u64,
    /// 待匹配设备快照。
    pub device: KernelDeviceSnapshotV1,
    /// 回调写入 0 或 1。
    pub matched: u32,
    /// v1 必须为零。
    pub reserved0: u32,
}

/// 驱动 probe 回调帧。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceProbeFrameV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 调用目标 cell。
    pub cell_id: u64,
    /// 调用目标 generation。
    pub generation: u64,
    /// 正在 probe 的设备。
    pub device: KernelDeviceSnapshotV1,
    /// 回调写入 [`KERNEL_DEVICE_STATUS_OK`] 或精确失败状态。
    pub status: i32,
    /// v1 必须为零。
    pub reserved0: u32,
}

/// 驱动 remove 回调帧。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceRemoveFrameV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 调用目标 cell。
    pub cell_id: u64,
    /// 调用目标 generation。
    pub generation: u64,
    /// 正在移除的设备。
    pub device: KernelDeviceSnapshotV1,
    /// 回调写入状态；失败会进入诊断，但不能中止硬件移除。
    pub status: i32,
    /// v1 必须为零。
    pub reserved0: u32,
}

/// 注册动态 function class 的请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceFunctionClassRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 类别 identifier。
    pub identifier: KernelDeviceIdentifierV1,
    /// function 操作契约。
    pub operation_contract: KernelDeviceIdentifierV1,
}

impl KernelDeviceFunctionClassRequestV1 {
    /// 验证固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.identifier.as_str().is_some()
            && self.operation_contract.as_str().is_some()
    }
}

/// 注册动态 function 实例的请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceFunctionRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// [`KERNEL_DEVICE_FUNCTION_FLAG_MAY_BLOCK`] 等标志。
    pub flags: u32,
    /// function 所属 PnP 设备。
    pub device: KernelDeviceHandleV1,
    /// 已注册动态类别。
    pub class: KernelDeviceFunctionClassHandleV1,
    /// function registry 名称。
    pub name: KernelDeviceNameV1,
    /// `extern "C" fn(*mut KernelDeviceIoFrameV1) -> i32` 地址。
    pub invoke_callback: u64,
    /// 可选停止新 I/O 回调地址。
    pub quiesce_callback: u64,
    /// 可选排空 I/O 回调地址。
    pub drain_callback: u64,
}

impl KernelDeviceFunctionRequestV1 {
    /// 验证固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags & !KERNEL_DEVICE_FUNCTION_FLAG_MAY_BLOCK == 0
            && self.device.is_well_formed()
            && self.class.is_well_formed()
            && self.name.as_str().is_some()
            && self.invoke_callback != 0
    }
}

/// 动态 function 操作帧。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceIoFrameV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// function 句柄。
    pub function: KernelDeviceFunctionHandleV1,
    /// 操作编号，由 function 契约定义。
    pub opcode: u32,
    /// 输入有效字节数。
    pub input_len: u32,
    /// 输出缓冲容量。
    pub output_capacity: u32,
    /// 回调写入的输出有效字节数。
    pub output_len: u32,
    /// 输入和输出共用的固定载荷。
    pub payload: [u8; KERNEL_DEVICE_IO_PAYLOAD_LEN],
    /// 回调写入的状态。
    pub status: i32,
    /// v1 必须为零。
    pub reserved0: u32,
}

impl KernelDeviceIoFrameV1 {
    /// 构造一次 function 契约调用。
    ///
    /// 输入和输出共用固定载荷；`output_capacity` 不能超过载荷上限。调用成功后应读取
    /// `status`、`output_len` 和 `payload[..output_len]`。
    pub fn new(
        function: KernelDeviceFunctionHandleV1,
        opcode: u32,
        input: &[u8],
        output_capacity: usize,
    ) -> Option<Self> {
        if !function.is_well_formed()
            || input.len() > KERNEL_DEVICE_IO_PAYLOAD_LEN
            || output_capacity > KERNEL_DEVICE_IO_PAYLOAD_LEN
        {
            return None;
        }
        let mut frame = Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            function,
            opcode,
            input_len: input.len() as u32,
            output_capacity: output_capacity as u32,
            output_len: 0,
            payload: [0; KERNEL_DEVICE_IO_PAYLOAD_LEN],
            status: KERNEL_DEVICE_STATUS_FAULT,
            reserved0: 0,
        };
        frame.payload[..input.len()].copy_from_slice(input);
        Some(frame)
    }

    /// 检查调用前的固定字段和载荷边界。
    pub fn is_well_formed_request(&self) -> bool {
        let input_len = self.input_len as usize;
        let output_capacity = self.output_capacity as usize;
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.function.is_well_formed()
            && input_len <= self.payload.len()
            && output_capacity <= self.payload.len()
            && self.output_len == 0
            && self.reserved0 == 0
    }

    /// 返回成功调用写入的输出切片。
    pub fn output(&self) -> Option<&[u8]> {
        let output_len = self.output_len as usize;
        if output_len > self.output_capacity as usize || output_len > self.payload.len() {
            return None;
        }
        Some(&self.payload[..output_len])
    }
}

/// MMIO 映射结果。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelDeviceMmioMappingV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 映射句柄。
    pub handle: KernelDeviceMmioHandleV1,
    /// MMIO 物理地址，仅用于诊断。
    pub physical_address: u64,
    /// 内核虚拟地址；直接访问需要显式 unsafe 契约。
    pub virtual_address: u64,
    /// 窗口长度。
    pub length: u64,
}

/// IRQ 注册请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceIrqRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// IRQ 所属设备。
    pub device: KernelDeviceHandleV1,
    /// [`KERNEL_DEVICE_IRQ_MODE_TOP_HALF`] 或 deferred。top-half 不能进入 Kernel API，
    /// 且受 [`KERNEL_DEVICE_IRQ_TOP_HALF_BUDGET_NS`] 硬截止时间限制。
    pub mode: u32,
    /// [`KERNEL_DEVICE_IRQ_SOURCE_RESOURCE`] 或 MSI。
    pub source_kind: u32,
    /// 设备 IRQ 资源索引；MSI 来源必须为零。
    pub resource_index: u32,
    /// 是否允许共享。
    pub shared: u32,
    /// MSI 来源句柄；设备资源来源必须为全零。
    pub msi: KernelDeviceMsiHandleV1,
    /// `extern "C" fn(*mut KernelDeviceIrqFrameV1) -> i32` 地址。
    pub callback: u64,
}

impl KernelDeviceIrqRequestV1 {
    /// 验证固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.device.is_well_formed()
            && matches!(
                self.mode,
                KERNEL_DEVICE_IRQ_MODE_TOP_HALF | KERNEL_DEVICE_IRQ_MODE_DEFERRED
            )
            && self.shared <= 1
            && self.callback != 0
            && match self.source_kind {
                KERNEL_DEVICE_IRQ_SOURCE_RESOURCE => self.msi == KernelDeviceMsiHandleV1::default(),
                KERNEL_DEVICE_IRQ_SOURCE_MSI => {
                    self.resource_index == 0
                        && self.shared == 0
                        && self.msi.is_well_formed()
                        && self.msi.generation == self.device.generation
                }
                _ => false,
            }
    }
}

/// IRQ 回调帧。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceIrqFrameV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// IRQ 注册句柄。
    pub irq: KernelDeviceIrqHandleV1,
    /// 规范化 IRQ line 类别。
    pub line_kind: u32,
    /// 控制器或 line 的高位数据。
    pub line_domain: u32,
    /// line 编号。
    pub line_number: u64,
    /// 回调写入 handled/unhandled。
    pub result: i32,
    /// v1 必须为零。
    pub reserved0: u32,
}

/// MSI 申请请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceMsiRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// MSI 所属设备。
    pub device: KernelDeviceHandleV1,
    /// 动态设备声明的 MSI controller identifier；PCI 设备由总线自行解析。
    pub controller: u32,
    /// 动态设备声明的 requester identifier；PCI 设备由 BDF 和固件路由自行解析。
    pub requester: u32,
}

impl KernelDeviceMsiRequestV1 {
    /// 验证 MSI 请求的固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.device.is_well_formed()
    }
}

/// MSI 分配结果。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelDeviceMsiAllocationV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// MSI 句柄。
    pub handle: KernelDeviceMsiHandleV1,
    /// message address。
    pub message_address: u64,
    /// message data。
    pub message_data: u32,
    /// 规范化 line 类别。
    pub line_kind: u32,
    /// line domain。
    pub line_domain: u32,
    /// v1 必须为零。
    pub reserved0: u32,
    /// line 编号。
    pub line_number: u64,
}

/// DMA 缓冲区申请请求。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelDeviceDmaRequestV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// DMA 所属设备。
    pub device: KernelDeviceHandleV1,
    /// 逻辑字节数。
    pub length: u64,
    /// 对齐。
    pub align: u64,
    /// DMA direction。
    pub direction: u32,
    /// 动态设备使用的 DMA 资源索引；固定总线必须传零。
    pub resource_index: u32,
}

impl KernelDeviceDmaRequestV1 {
    /// 验证固定字段。
    pub fn is_well_formed(&self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags == 0
            && self.device.is_well_formed()
            && self.length != 0
            && self.align != 0
            && self.align.is_power_of_two()
            && matches!(
                self.direction,
                KERNEL_DEVICE_DMA_TO_DEVICE
                    | KERNEL_DEVICE_DMA_FROM_DEVICE
                    | KERNEL_DEVICE_DMA_BIDIRECTIONAL
            )
    }
}

/// DMA 缓冲区结果。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelDeviceDmaBufferV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// DMA 句柄。
    pub handle: KernelDeviceDmaHandleV1,
    /// CPU 可访问虚拟地址。
    pub virtual_address: u64,
    /// 设备描述符使用的 DMA 地址。
    pub dma_address: u64,
    /// 缓冲区长度。
    pub length: u64,
    /// DMA direction。
    pub direction: u32,
    /// v1 必须为零。
    pub reserved0: u32,
}

/// `kernel.device@1` 固定布局函数表。
#[repr(C)]
pub struct KernelDeviceApiV1 {
    /// 所有 Kernel API 表共享的稳定头部。
    pub header: ApiTableHeaderV1,
    /// 按游标枚举一个设备；成功时写入快照和下一游标，游标 0 表示开始。
    pub enumerate:
        extern "C" fn(ApiGrantTokenV1, u64, *mut KernelDeviceSnapshotV1, *mut u64) -> i32,
    /// 查询一个设备快照。
    pub query_device:
        extern "C" fn(ApiGrantTokenV1, KernelDeviceHandleV1, *mut KernelDeviceSnapshotV1) -> i32,
    /// 查询设备的第 N 条资源。
    pub query_resource: extern "C" fn(
        ApiGrantTokenV1,
        KernelDeviceHandleV1,
        u32,
        *mut KernelDeviceResourceV1,
    ) -> i32,
    /// 查询设备的第 N 条属性。
    pub query_property: extern "C" fn(
        ApiGrantTokenV1,
        KernelDeviceHandleV1,
        u32,
        *mut KernelDevicePropertyV1,
    ) -> i32,
    /// 按句柄游标枚举指定设备当前开放的 function。
    pub enumerate_function: extern "C" fn(
        ApiGrantTokenV1,
        KernelDeviceHandleV1,
        u64,
        *mut KernelDeviceFunctionSnapshotV1,
        *mut u64,
    ) -> i32,
    /// 查询一个 function 的当前快照。
    pub query_function: extern "C" fn(
        ApiGrantTokenV1,
        KernelDeviceFunctionHandleV1,
        *mut KernelDeviceFunctionSnapshotV1,
    ) -> i32,
    /// 调用 function 的操作契约；传入帧同时作为输出槽。
    pub invoke_function: extern "C" fn(ApiGrantTokenV1, *mut KernelDeviceIoFrameV1) -> i32,
    /// 注册动态总线。
    pub register_bus: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceBusRequestV1,
        *mut KernelDeviceBusHandleV1,
    ) -> i32,
    /// 注销动态总线。
    pub unregister_bus: extern "C" fn(ApiGrantTokenV1, KernelDeviceBusHandleV1) -> i32,
    /// 注册 ELM PnP 驱动。
    pub register_driver: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceDriverRequestV1,
        *mut KernelDeviceDriverHandleV1,
    ) -> i32,
    /// 注销 ELM PnP 驱动并解绑设备。
    pub unregister_driver: extern "C" fn(ApiGrantTokenV1, KernelDeviceDriverHandleV1) -> i32,
    /// 发布动态设备并立即进入现有 PnP probe 路径。
    pub publish_device: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDevicePublishRequestV1,
        *mut KernelDeviceHandleV1,
    ) -> i32,
    /// 热拔并撤销一个由当前 ELM 发布的设备。
    pub remove_device: extern "C" fn(ApiGrantTokenV1, KernelDeviceHandleV1) -> i32,
    /// 注册动态 function class。
    pub register_function_class: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceFunctionClassRequestV1,
        *mut KernelDeviceFunctionClassHandleV1,
    ) -> i32,
    /// 注销空闲的动态 function class。
    pub unregister_function_class:
        extern "C" fn(ApiGrantTokenV1, KernelDeviceFunctionClassHandleV1) -> i32,
    /// 在当前 probe 的设备上注册 function。
    pub register_function: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceFunctionRequestV1,
        *mut KernelDeviceFunctionHandleV1,
    ) -> i32,
    /// 注销 function；通常由 PnP remove 自动完成。
    pub unregister_function: extern "C" fn(ApiGrantTokenV1, KernelDeviceFunctionHandleV1) -> i32,
    /// 取得设备 MMIO 资源的受控映射。
    pub map_mmio: extern "C" fn(
        ApiGrantTokenV1,
        KernelDeviceHandleV1,
        u32,
        *mut KernelDeviceMmioMappingV1,
    ) -> i32,
    /// 释放 MMIO 映射。
    pub unmap_mmio: extern "C" fn(ApiGrantTokenV1, KernelDeviceMmioHandleV1) -> i32,
    /// 从映射窗口读取 1、2、4 或 8 字节。
    pub mmio_read:
        extern "C" fn(ApiGrantTokenV1, KernelDeviceMmioHandleV1, u64, u32, *mut u64) -> i32,
    /// 向映射窗口写入 1、2、4 或 8 字节。
    pub mmio_write: extern "C" fn(ApiGrantTokenV1, KernelDeviceMmioHandleV1, u64, u32, u64) -> i32,
    /// 注册 IRQ handler。
    pub request_irq: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceIrqRequestV1,
        *mut KernelDeviceIrqHandleV1,
    ) -> i32,
    /// 释放 IRQ handler。
    pub release_irq: extern "C" fn(ApiGrantTokenV1, KernelDeviceIrqHandleV1) -> i32,
    /// 分配 MSI vector。
    pub allocate_msi: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceMsiRequestV1,
        *mut KernelDeviceMsiAllocationV1,
    ) -> i32,
    /// 释放 MSI vector。
    pub release_msi: extern "C" fn(ApiGrantTokenV1, KernelDeviceMsiHandleV1) -> i32,
    /// 分配 DMA 缓冲区。
    pub allocate_dma: extern "C" fn(
        ApiGrantTokenV1,
        *const KernelDeviceDmaRequestV1,
        *mut KernelDeviceDmaBufferV1,
    ) -> i32,
    /// 同步 DMA 缓冲区。
    pub sync_dma: extern "C" fn(ApiGrantTokenV1, KernelDeviceDmaHandleV1, u32) -> i32,
    /// 释放 DMA 缓冲区。
    pub release_dma: extern "C" fn(ApiGrantTokenV1, KernelDeviceDmaHandleV1) -> i32,
}

impl KernelDeviceApiV1 {
    /// 按游标取得下一台设备。返回 `NOT_FOUND` 表示枚举结束。
    pub fn next_device(
        &self,
        token: ApiGrantTokenV1,
        cursor: u64,
    ) -> Result<(KernelDeviceSnapshotV1, u64), i32> {
        let mut snapshot = KernelDeviceSnapshotV1::default();
        let mut next_cursor = 0;
        status_value(
            (self.enumerate)(token, cursor, &mut snapshot, &mut next_cursor),
            (snapshot, next_cursor),
        )
    }

    /// 查询设备当前状态和拓扑摘要。
    pub fn device(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceHandleV1,
    ) -> Result<KernelDeviceSnapshotV1, i32> {
        let mut snapshot = KernelDeviceSnapshotV1::default();
        status_value((self.query_device)(token, handle, &mut snapshot), snapshot)
    }

    /// 查询设备的第 `ordinal` 条资源描述。
    pub fn resource(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceHandleV1,
        ordinal: u32,
    ) -> Result<KernelDeviceResourceV1, i32> {
        let mut resource = KernelDeviceResourceV1::default();
        status_value(
            (self.query_resource)(token, handle, ordinal, &mut resource),
            resource,
        )
    }

    /// 查询设备的第 `ordinal` 条属性。
    pub fn property(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceHandleV1,
        ordinal: u32,
    ) -> Result<KernelDevicePropertyV1, i32> {
        let mut property = KernelDevicePropertyV1::default();
        status_value(
            (self.query_property)(token, handle, ordinal, &mut property),
            property,
        )
    }

    /// 按游标取得设备开放的下一个 function。
    pub fn next_function(
        &self,
        token: ApiGrantTokenV1,
        device: KernelDeviceHandleV1,
        cursor: u64,
    ) -> Result<(KernelDeviceFunctionSnapshotV1, u64), i32> {
        let mut snapshot = KernelDeviceFunctionSnapshotV1::default();
        let mut next_cursor = 0;
        status_value(
            (self.enumerate_function)(token, device, cursor, &mut snapshot, &mut next_cursor),
            (snapshot, next_cursor),
        )
    }

    /// 查询 function 的类别、名称和操作契约。
    pub fn function(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceFunctionHandleV1,
    ) -> Result<KernelDeviceFunctionSnapshotV1, i32> {
        let mut snapshot = KernelDeviceFunctionSnapshotV1::default();
        status_value(
            (self.query_function)(token, handle, &mut snapshot),
            snapshot,
        )
    }

    /// 调用 function 的契约操作。
    ///
    /// 返回的外层错误表示内核没有执行该调用；调用已经到达 function 时，应继续检查
    /// `frame.status` 以取得契约定义的结果。
    pub fn invoke(
        &self,
        token: ApiGrantTokenV1,
        mut frame: KernelDeviceIoFrameV1,
    ) -> Result<KernelDeviceIoFrameV1, i32> {
        status_value((self.invoke_function)(token, &mut frame), frame)
    }

    /// 注册一个动态设备总线。
    pub fn register_device_bus(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceBusRequestV1,
    ) -> Result<KernelDeviceBusHandleV1, i32> {
        let mut handle = KernelDeviceBusHandleV1::default();
        status_value((self.register_bus)(token, request, &mut handle), handle)
    }

    /// 注销一个不再拥有驱动和设备的动态总线。
    pub fn unregister_device_bus(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceBusHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.unregister_bus)(token, handle))
    }

    /// 注册包含原生回调地址的 PnP 驱动。
    ///
    /// # Safety
    ///
    /// 请求中的三个回调地址必须由当前 ELM 镜像中签名完全匹配的函数产生，并在驱动
    /// 注销及其全部设备回调排空前保持有效。不能把数据地址或其它 ABI 函数伪装成回调。
    pub unsafe fn register_device_driver(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceDriverRequestV1,
    ) -> Result<KernelDeviceDriverHandleV1, i32> {
        let mut handle = KernelDeviceDriverHandleV1::default();
        status_value((self.register_driver)(token, request, &mut handle), handle)
    }

    /// 注销驱动并排空其设备回调。
    pub fn unregister_device_driver(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceDriverHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.unregister_driver)(token, handle))
    }

    /// 向动态总线发布一个设备。
    pub fn publish(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDevicePublishRequestV1,
    ) -> Result<KernelDeviceHandleV1, i32> {
        let mut handle = KernelDeviceHandleV1::default();
        status_value((self.publish_device)(token, request, &mut handle), handle)
    }

    /// 热拔一个由当前 ELM 发布的设备。
    pub fn remove(&self, token: ApiGrantTokenV1, handle: KernelDeviceHandleV1) -> Result<(), i32> {
        status_unit((self.remove_device)(token, handle))
    }

    /// 注册 function class 及其操作契约。
    pub fn register_class(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceFunctionClassRequestV1,
    ) -> Result<KernelDeviceFunctionClassHandleV1, i32> {
        let mut handle = KernelDeviceFunctionClassHandleV1::default();
        status_value(
            (self.register_function_class)(token, request, &mut handle),
            handle,
        )
    }

    /// 注销一个不再包含 function 的动态类别。
    pub fn unregister_class(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceFunctionClassHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.unregister_function_class)(token, handle))
    }

    /// 注册包含原生调用回调的 function。
    ///
    /// # Safety
    ///
    /// 请求中的非零回调地址必须位于当前 ELM 镜像内、签名与对应回调帧完全一致，并在
    /// function 注销和全部在途调用排空前保持有效。
    pub unsafe fn register_device_function(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceFunctionRequestV1,
    ) -> Result<KernelDeviceFunctionHandleV1, i32> {
        let mut handle = KernelDeviceFunctionHandleV1::default();
        status_value(
            (self.register_function)(token, request, &mut handle),
            handle,
        )
    }

    /// 注销并排空由当前 ELM 实现的 function。
    pub fn unregister_device_function(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceFunctionHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.unregister_function)(token, handle))
    }

    /// 取得设备 MMIO 资源的受控映射记录。
    pub fn map_device_mmio(
        &self,
        token: ApiGrantTokenV1,
        device: KernelDeviceHandleV1,
        resource_ordinal: u32,
    ) -> Result<KernelDeviceMmioMappingV1, i32> {
        let mut mapping = KernelDeviceMmioMappingV1::default();
        status_value(
            (self.map_mmio)(token, device, resource_ordinal, &mut mapping),
            mapping,
        )
    }

    /// 释放 MMIO 映射句柄。
    pub fn unmap_device_mmio(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceMmioHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.unmap_mmio)(token, handle))
    }

    /// 通过受控入口读取 MMIO。
    pub fn read_mmio(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceMmioHandleV1,
        offset: u64,
        width: u32,
    ) -> Result<u64, i32> {
        let mut value = 0;
        status_value(
            (self.mmio_read)(token, handle, offset, width, &mut value),
            value,
        )
    }

    /// 通过受控入口写入 MMIO。
    pub fn write_mmio(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceMmioHandleV1,
        offset: u64,
        width: u32,
        value: u64,
    ) -> Result<(), i32> {
        status_unit((self.mmio_write)(token, handle, offset, width, value))
    }

    /// 注册包含原生回调地址的 IRQ handler。
    ///
    /// # Safety
    ///
    /// `request.callback` 必须位于当前镜像中且签名匹配 IRQ 回调帧。top-half 回调还必须
    /// 满足硬中断上下文约束，不能阻塞、分配或调用仅允许任务上下文使用的服务。
    pub unsafe fn request_device_irq(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceIrqRequestV1,
    ) -> Result<KernelDeviceIrqHandleV1, i32> {
        let mut handle = KernelDeviceIrqHandleV1::default();
        status_value((self.request_irq)(token, request, &mut handle), handle)
    }

    /// 注销并排空 IRQ handler。
    pub fn release_device_irq(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceIrqHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.release_irq)(token, handle))
    }

    /// 为设备分配 MSI vector。
    pub fn allocate_device_msi(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceMsiRequestV1,
    ) -> Result<KernelDeviceMsiAllocationV1, i32> {
        let mut allocation = KernelDeviceMsiAllocationV1::default();
        status_value(
            (self.allocate_msi)(token, request, &mut allocation),
            allocation,
        )
    }

    /// 释放 MSI vector。
    pub fn release_device_msi(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceMsiHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.release_msi)(token, handle))
    }

    /// 按设备约束分配 DMA 缓冲区。
    pub fn allocate_device_dma(
        &self,
        token: ApiGrantTokenV1,
        request: &KernelDeviceDmaRequestV1,
    ) -> Result<KernelDeviceDmaBufferV1, i32> {
        let mut buffer = KernelDeviceDmaBufferV1::default();
        status_value((self.allocate_dma)(token, request, &mut buffer), buffer)
    }

    /// 在 CPU 与设备之间同步 DMA 缓冲区所有权。
    pub fn sync_device_dma(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceDmaHandleV1,
        operation: u32,
    ) -> Result<(), i32> {
        status_unit((self.sync_dma)(token, handle, operation))
    }

    /// 释放 DMA 缓冲区。
    pub fn release_device_dma(
        &self,
        token: ApiGrantTokenV1,
        handle: KernelDeviceDmaHandleV1,
    ) -> Result<(), i32> {
        status_unit((self.release_dma)(token, handle))
    }
}

fn status_unit(status: i32) -> Result<(), i32> {
    if status == KERNEL_DEVICE_STATUS_OK {
        Ok(())
    } else {
        Err(status)
    }
}

fn status_value<T>(status: i32, value: T) -> Result<T, i32> {
    if status == KERNEL_DEVICE_STATUS_OK {
        Ok(value)
    } else {
        Err(status)
    }
}

/// 设备属性宏使用的固定 ABI 回调入口。
#[doc(hidden)]
pub mod __private {
    use super::*;

    fn valid_frame_pointer<T>(pointer: *mut T) -> bool {
        !pointer.is_null() && (pointer as usize) % core::mem::align_of::<T>() == 0
    }

    /// 执行设备匹配回调并把布尔结果写回匹配帧。
    ///
    /// # Safety
    ///
    /// raw 必须是内核按照 KernelDeviceMatchFrameV1 提供的可读写帧地址；回调不得
    /// 保存其中的借用或跨返回点访问该地址。
    pub unsafe fn device_match_trampoline<F>(raw: *mut KernelDeviceMatchFrameV1, callback: F) -> i32
    where
        F: FnOnce(&KernelDeviceSnapshotV1) -> bool,
    {
        if !valid_frame_pointer(raw) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        // Safety: 调用方契约保证 raw 指向当前回调帧，且上面的检查排除了空指针和未对齐地址。
        let mut frame = unsafe { raw.read() };
        if frame.struct_size != core::mem::size_of::<KernelDeviceMatchFrameV1>() as u32
            || frame.flags != 0
            || !frame.device.is_well_formed()
            || frame.matched != 0
            || frame.reserved0 != 0
        {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        frame.matched = u32::from(callback(&frame.device));
        // Safety: raw 仍是本次回调的有效可写帧地址，且 frame 保持固定 repr(C) 布局。
        unsafe { raw.write(frame) };
        KERNEL_DEVICE_STATUS_OK
    }

    /// 执行设备 probe 回调并把结果写回 probe 帧。
    ///
    /// # Safety
    ///
    /// raw 必须是内核提供的有效可读写 probe 帧，回调不得保存帧内引用。
    pub unsafe fn device_probe_trampoline<F>(raw: *mut KernelDeviceProbeFrameV1, callback: F) -> i32
    where
        F: FnOnce(&KernelDeviceSnapshotV1) -> Result<(), i32>,
    {
        if !valid_frame_pointer(raw) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        // Safety: raw 由内核回调门提供，并已通过空指针和对齐检查。
        let mut frame = unsafe { raw.read() };
        if frame.struct_size != core::mem::size_of::<KernelDeviceProbeFrameV1>() as u32
            || frame.flags != 0
            || !frame.device.is_well_formed()
            || frame.reserved0 != 0
        {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        frame.status = callback(&frame.device)
            .err()
            .unwrap_or(KERNEL_DEVICE_STATUS_OK);
        // Safety: raw 仍指向内核分配的可写固定帧。
        unsafe { raw.write(frame) };
        KERNEL_DEVICE_STATUS_OK
    }

    /// 执行设备 remove 回调并把结果写回 remove 帧。
    ///
    /// # Safety
    ///
    /// raw 必须是内核提供的有效可读写 remove 帧，回调不得保存帧内引用。
    pub unsafe fn device_remove_trampoline<F>(
        raw: *mut KernelDeviceRemoveFrameV1,
        callback: F,
    ) -> i32
    where
        F: FnOnce(&KernelDeviceSnapshotV1) -> Result<(), i32>,
    {
        if !valid_frame_pointer(raw) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        // Safety: raw 由内核回调门提供，并已通过空指针和对齐检查。
        let mut frame = unsafe { raw.read() };
        if frame.struct_size != core::mem::size_of::<KernelDeviceRemoveFrameV1>() as u32
            || frame.flags != 0
            || !frame.device.is_well_formed()
            || frame.reserved0 != 0
        {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        frame.status = callback(&frame.device)
            .err()
            .unwrap_or(KERNEL_DEVICE_STATUS_OK);
        // Safety: raw 仍指向内核分配的可写固定帧。
        unsafe { raw.write(frame) };
        KERNEL_DEVICE_STATUS_OK
    }

    /// 执行通用 function 操作回调。
    ///
    /// # Safety
    ///
    /// raw 必须是内核提供的有效可读写 IO 帧，回调不得保存帧内引用或把 payload
    /// 解释为未声明的外部对象。
    pub unsafe fn device_function_trampoline<F>(raw: *mut KernelDeviceIoFrameV1, callback: F) -> i32
    where
        F: FnOnce(&mut KernelDeviceIoFrameV1) -> Result<(), i32>,
    {
        if !valid_frame_pointer(raw) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        // Safety: raw 由内核回调门提供，并已通过空指针和对齐检查。
        let mut frame = unsafe { raw.read() };
        if !frame.is_well_formed_request() {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        frame.status = match callback(&mut frame) {
            Ok(()) => KERNEL_DEVICE_STATUS_OK,
            Err(status) => status,
        };
        if frame.output_len > frame.output_capacity {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        // Safety: raw 仍指向内核分配的可写固定帧。
        unsafe { raw.write(frame) };
        KERNEL_DEVICE_STATUS_OK
    }

    /// 执行 IRQ 回调并把 handled 结果写回 IRQ 帧。
    ///
    /// # Safety
    ///
    /// raw 必须是内核提供的有效可读写 IRQ 帧；top-half 调用方还必须保证回调不阻塞、
    /// 不分配并只使用 IRQ 上下文允许的 API。
    pub unsafe fn device_irq_trampoline<F>(raw: *mut KernelDeviceIrqFrameV1, callback: F) -> i32
    where
        F: FnOnce(&KernelDeviceIrqFrameV1) -> Result<bool, i32>,
    {
        if !valid_frame_pointer(raw) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        // Safety: raw 由内核回调门提供，并已通过空指针和对齐检查。
        let mut frame = unsafe { raw.read() };
        if frame.struct_size != core::mem::size_of::<KernelDeviceIrqFrameV1>() as u32
            || frame.flags != 0
            || !frame.irq.is_well_formed()
            || frame.reserved0 != 0
        {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        frame.result = if callback(&frame).unwrap_or(false) {
            KERNEL_DEVICE_IRQ_HANDLED
        } else {
            KERNEL_DEVICE_IRQ_UNHANDLED
        };
        // Safety: raw 仍指向内核分配的可写固定帧。
        unsafe { raw.write(frame) };
        KERNEL_DEVICE_STATUS_OK
    }
}

impl crate::table::sealed::Sealed for KernelDeviceApiV1 {}

// Safety: 该类型使用 repr(C)，首字段为规范表头，所有入口均使用声明中的固定 C ABI。
unsafe impl KernelApiTable for KernelDeviceApiV1 {
    const IDENTIFIER: &'static str = KERNEL_DEVICE_API_IDENTIFIER;
    const VERSION: u16 = KERNEL_DEVICE_API_VERSION;
    const CAPABILITIES: u64 = KERNEL_DEVICE_CAPABILITIES;
    const LAYOUT_HASH: [u8; 32] = KERNEL_DEVICE_LAYOUT_HASH_V1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_noncanonical_storage() {
        let identifier = KernelDeviceIdentifierV1::new("display.surface").unwrap();
        assert_eq!(identifier.as_str(), Some("display.surface"));
        let mut invalid = identifier;
        invalid.bytes[KERNEL_DEVICE_IDENTIFIER_LEN - 1] = 1;
        assert!(invalid.as_str().is_none());
    }

    #[test]
    fn generic_driver_flag_requires_generic_bus() {
        let mut request = KernelDeviceDriverRequestV1 {
            struct_size: core::mem::size_of::<KernelDeviceDriverRequestV1>() as u32,
            flags: KERNEL_DEVICE_DRIVER_FLAG_GENERIC,
            name: KernelDeviceNameV1::new("fallback-driver").unwrap(),
            bus: KernelDeviceIdentifierV1::new("generic").unwrap(),
            priority: 0,
            reserved0: 0,
            reserved1: 0,
            match_callback: 1,
            probe_callback: 2,
            remove_callback: 3,
        };
        assert!(request.is_well_formed());

        request.bus = KernelDeviceIdentifierV1::new("pci").unwrap();
        assert!(!request.is_well_formed());

        request.flags = 0;
        assert!(request.is_well_formed());

        request.bus = KernelDeviceIdentifierV1::new("generic").unwrap();
        assert!(!request.is_well_formed());
    }

    #[test]
    fn publish_request_checks_unused_records() {
        let mut request = KernelDevicePublishRequestV1 {
            struct_size: core::mem::size_of::<KernelDevicePublishRequestV1>() as u32,
            flags: 0,
            bus: KernelDeviceHandleV1 {
                id: 1,
                generation: 1,
            },
            parent: KernelDeviceHandleV1::default(),
            name: KernelDeviceNameV1::new("surface0").unwrap(),
            identity_contract: KernelDeviceIdentifierV1::new("display.identity").unwrap(),
            identity_len: 1,
            resource_count: 0,
            identity: [0; KERNEL_DEVICE_IDENTITY_LEN],
            resources: [KernelDeviceResourceV1::empty(); KERNEL_DEVICE_MAX_RESOURCES],
            property_count: 0,
            properties: [KernelDevicePropertyV1::default(); KERNEL_DEVICE_MAX_PROPERTIES],
        };
        request.identity[0] = 1;
        assert!(request.is_well_formed());
        request.resources[1].kind = KERNEL_DEVICE_RESOURCE_MMIO;
        assert!(!request.is_well_formed());

        request.resources[1] = KernelDeviceResourceV1::empty();
        request.resource_count = 2;
        for resource in &mut request.resources[..2] {
            resource.kind = KERNEL_DEVICE_RESOURCE_MMIO;
            resource.start = 0x1000;
            resource.length = 0x100;
        }
        assert!(!request.is_well_formed());

        request.resource_count = 0;
        request.resources[..2].fill(KernelDeviceResourceV1::empty());
        request.property_count = 2;
        let property = KernelDevicePropertyV1::new("device.mode", b"test").unwrap();
        request.properties[0] = property;
        request.properties[1] = property;
        assert!(!request.is_well_formed());
    }

    #[test]
    fn dynamic_standard_resources_have_canonical_encodings() {
        let mut irq = KernelDeviceResourceV1::empty();
        irq.kind = KERNEL_DEVICE_RESOURCE_IRQ;
        irq.index = 3;
        irq.start = 41;
        irq.flags = u64::from(KERNEL_DEVICE_IRQ_LINE_KIND_CONTROLLER)
            | (7u64 << KERNEL_DEVICE_IRQ_RESOURCE_DOMAIN_SHIFT);
        assert!(irq.has_valid_dynamic_encoding());
        irq.length = 1;
        assert!(!irq.has_valid_dynamic_encoding());

        let mut msi = KernelDeviceResourceV1::empty();
        msi.kind = KERNEL_DEVICE_RESOURCE_MSI;
        msi.start = 7;
        msi.length = 0x102;
        assert!(msi.has_valid_dynamic_encoding());
        msi.flags = 1;
        assert!(!msi.has_valid_dynamic_encoding());

        let mut dma = KernelDeviceResourceV1::empty();
        dma.kind = KERNEL_DEVICE_RESOURCE_DMA;
        dma.index = 9;
        dma.start = u32::MAX as u64;
        dma.length = 64 * 1024;
        dma.flags = KERNEL_DEVICE_DMA_RESOURCE_COHERENT
            | (4u64 << KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_SHIFT);
        assert!(dma.has_valid_dynamic_encoding());
        dma.flags &= !KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_MASK;
        assert!(!dma.has_valid_dynamic_encoding());
    }

    #[test]
    fn dma_request_selects_a_dynamic_resource() {
        let request = KernelDeviceDmaRequestV1 {
            struct_size: core::mem::size_of::<KernelDeviceDmaRequestV1>() as u32,
            flags: 0,
            device: KernelDeviceHandleV1 {
                id: 9,
                generation: 2,
            },
            length: 4096,
            align: 4096,
            direction: KERNEL_DEVICE_DMA_BIDIRECTIONAL,
            resource_index: 7,
        };
        assert!(request.is_well_formed());
    }

    #[test]
    fn irq_request_distinguishes_resource_and_msi_sources() {
        let device = KernelDeviceHandleV1 {
            id: 7,
            generation: 3,
        };
        let mut request = KernelDeviceIrqRequestV1 {
            struct_size: core::mem::size_of::<KernelDeviceIrqRequestV1>() as u32,
            flags: 0,
            device,
            mode: KERNEL_DEVICE_IRQ_MODE_DEFERRED,
            source_kind: KERNEL_DEVICE_IRQ_SOURCE_RESOURCE,
            resource_index: 2,
            shared: 1,
            msi: KernelDeviceMsiHandleV1::default(),
            callback: 0x1000,
        };
        assert!(request.is_well_formed());

        request.source_kind = KERNEL_DEVICE_IRQ_SOURCE_MSI;
        request.resource_index = 0;
        request.shared = 0;
        request.msi = KernelDeviceMsiHandleV1 {
            id: 8,
            generation: device.generation,
        };
        assert!(request.is_well_formed());

        request.msi.generation += 1;
        assert!(!request.is_well_formed());
        request.msi.generation = device.generation;
        request.resource_index = 1;
        assert!(!request.is_well_formed());
    }

    #[test]
    fn device_table_layout_is_stable() {
        assert_eq!(core::mem::offset_of!(KernelDeviceApiV1, enumerate), 16);
        assert_eq!(
            core::mem::offset_of!(KernelDeviceApiV1, enumerate_function),
            48
        );
        assert_eq!(core::mem::offset_of!(KernelDeviceApiV1, register_bus), 72);
        assert_eq!(core::mem::offset_of!(KernelDeviceApiV1, release_dma), 232);
        assert_eq!(core::mem::size_of::<KernelDeviceApiV1>(), 240);
        assert_eq!(core::mem::size_of::<KernelDeviceIdentifierV1>(), 72);
        assert_eq!(core::mem::size_of::<KernelDeviceNameV1>(), 72);
        assert_eq!(core::mem::size_of::<KernelDeviceHandleV1>(), 16);
        assert_eq!(core::mem::size_of::<KernelDeviceBusRequestV1>(), 152);
        assert_eq!(core::mem::size_of::<KernelDeviceDriverRequestV1>(), 184);
        assert_eq!(core::mem::size_of::<KernelDeviceResourceV1>(), 112);
        assert_eq!(core::mem::size_of::<KernelDevicePropertyV1>(), 152);
        assert_eq!(core::mem::size_of::<KernelDevicePublishRequestV1>(), 3656);
        assert_eq!(core::mem::size_of::<KernelDeviceSnapshotV1>(), 408);
        assert_eq!(core::mem::size_of::<KernelDeviceFunctionSnapshotV1>(), 264);
        assert_eq!(core::mem::size_of::<KernelDeviceMatchFrameV1>(), 440);
        assert_eq!(core::mem::size_of::<KernelDeviceProbeFrameV1>(), 440);
        assert_eq!(core::mem::size_of::<KernelDeviceRemoveFrameV1>(), 440);
        assert_eq!(
            core::mem::size_of::<KernelDeviceFunctionClassRequestV1>(),
            152
        );
        assert_eq!(core::mem::size_of::<KernelDeviceFunctionRequestV1>(), 136);
        assert_eq!(core::mem::size_of::<KernelDeviceIoFrameV1>(), 304);
        assert_eq!(core::mem::size_of::<KernelDeviceMmioMappingV1>(), 48);
        assert_eq!(core::mem::size_of::<KernelDeviceIrqRequestV1>(), 64);
        assert_eq!(core::mem::size_of::<KernelDeviceIrqFrameV1>(), 48);
        assert_eq!(core::mem::size_of::<KernelDeviceMsiRequestV1>(), 32);
        assert_eq!(core::mem::size_of::<KernelDeviceMsiAllocationV1>(), 56);
        assert_eq!(core::mem::size_of::<KernelDeviceDmaRequestV1>(), 48);
        assert_eq!(core::mem::size_of::<KernelDeviceDmaBufferV1>(), 56);
        assert_eq!(
            crate::KernelApiLayoutV1::of::<KernelDeviceApiV1>().layout_hash,
            KERNEL_DEVICE_LAYOUT_HASH_V1
        );
    }
}
