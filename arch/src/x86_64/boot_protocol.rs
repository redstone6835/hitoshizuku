//! x86_64 启动协议解析与规范化。
//!
//! 本模块只处理启动协议携带的数据，不承担页表、栈切换或固件调用。这样
//! Multiboot2、Linux boot protocol 和 UEFI EFI stub 可以共享同一套严格的
//! 边界校验，同时把真正的硬件入口留给 `boot`/`loader`。所有解析器都接收
//! 已经由入口代码验证可读的字节切片；它们不会通过裸指针猜测协议布局。

use alloc::vec::Vec;
use core::fmt;

use general::{
    StartBootInfo, StartBootProtocol, StartMemoryRegion, StartMemoryRegionKind, StartPhysRange,
};

/// Multiboot2 规范规定的 bootloader magic（传入 EAX）。
pub const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36d7_6289;
/// Multiboot2 镜像 header magic（位于镜像中的 header）。
pub const MULTIBOOT2_HEADER_MAGIC: u32 = 0xe852_50d6;
/// x86-compatible Multiboot2 header architecture value.
pub const MULTIBOOT2_HEADER_ARCH_I386: u32 = 0;
pub const MULTIBOOT2_TAG_END: u32 = 0;
pub const MULTIBOOT2_TAG_CMDLINE: u32 = 1;
pub const MULTIBOOT2_TAG_MODULE: u32 = 3;
pub const MULTIBOOT2_TAG_MEMORY_MAP: u32 = 6;
pub const MULTIBOOT2_TAG_EFI32: u32 = 11;
pub const MULTIBOOT2_TAG_EFI64: u32 = 12;
pub const MULTIBOOT2_TAG_ACPI_OLD: u32 = 14;
pub const MULTIBOOT2_TAG_ACPI_NEW: u32 = 15;
pub const MULTIBOOT2_TAG_EFI_MEMORY_MAP: u32 = 17;

/// 校验镜像中的 Multiboot2 固定 header。
///
/// `header` 从 header magic 开始，至少包含固定字段和一个 end tag；可选
/// header tags 会按 8 字节边界开始，tag 的 `size` 不包含对齐填充。
/// Multiboot2 的 checksum 只覆盖固定的前三个 `u32` 加 checksum 字段，
/// 不会把可选 tag 内容再次纳入求和。x86_64 仍使用规范的 i386
/// architecture 值，实际从保护模式切换到 long mode 由入口适配器负责。
pub fn validate_multiboot2_header(header: &[u8]) -> Result<usize, BootProtocolError> {
    if header.len() < 24 {
        return Err(BootProtocolError::Truncated("Multiboot2 image header"));
    }
    if read_u32(header, 0)? != MULTIBOOT2_HEADER_MAGIC {
        return Err(BootProtocolError::Invalid("Multiboot2 image header magic"));
    }
    if read_u32(header, 4)? != MULTIBOOT2_HEADER_ARCH_I386 {
        return Err(BootProtocolError::Unsupported(
            "Multiboot2 non-i386 header architecture",
        ));
    }
    let header_length = read_u32(header, 8)? as usize;
    if header_length < 24 || header_length > header.len() || !header_length.is_multiple_of(8) {
        return Err(BootProtocolError::Invalid("Multiboot2 image header length"));
    }
    let mut checksum = 0u32;
    // The specification defines this checksum over the fixed header only;
    // address/entry tags are data and may contain arbitrary linker values.
    for chunk in header[..16].chunks_exact(4) {
        checksum =
            checksum.wrapping_add(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    if checksum != 0 {
        return Err(BootProtocolError::Invalid(
            "Multiboot2 image header checksum",
        ));
    }

    // Tags are a linked buffer with 8-byte aligned starts.  Validate the
    // framing here so a bootloader cannot accept a header whose address or
    // entry tag would make it walk into arbitrary image bytes.  Unknown tags
    // remain valid and are skipped after their checked size.
    let mut offset = 16usize;
    let mut saw_end = false;
    while offset < header_length {
        if !offset.is_multiple_of(8) {
            return Err(BootProtocolError::Invalid(
                "Multiboot2 header tag alignment",
            ));
        }
        let tag = header
            .get(offset..offset + 8)
            .ok_or(BootProtocolError::Truncated("Multiboot2 header tag"))?;
        let tag_type = u16::from_le_bytes([tag[0], tag[1]]) as u32;
        let flags = u16::from_le_bytes([tag[2], tag[3]]);
        let size = u32::from_le_bytes([tag[4], tag[5], tag[6], tag[7]]) as usize;
        if size < 8 {
            return Err(BootProtocolError::Invalid("Multiboot2 header tag size"));
        }
        let padded_size = align_up(size, 8)?;
        let remaining = header_length - offset;
        if padded_size > remaining {
            return Err(BootProtocolError::Truncated(
                "Multiboot2 header tag payload",
            ));
        }
        if tag_type == MULTIBOOT2_TAG_END {
            if flags != 0 || size != 8 || padded_size != remaining {
                return Err(BootProtocolError::Invalid("Multiboot2 header end tag"));
            }
            saw_end = true;
            break;
        }
        offset += padded_size;
    }
    if !saw_end {
        return Err(BootProtocolError::Invalid(
            "Multiboot2 header end tag missing",
        ));
    }
    Ok(header_length)
}

/// x86 启动路径的统一协议身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X86BootProtocol {
    /// Multiboot2 信息结构交接。
    Multiboot2,
    /// UEFI/EFI stub 交接。
    Efi,
    /// Linux x86 boot protocol（`boot_params`）交接。
    LinuxBoot,
    /// coreboot payload 交接。表解析由 coreboot 适配器完成。
    Coreboot,
    /// 固件已经建立执行环境的直接入口。
    Direct,
}

impl X86BootProtocol {
    /// 转换为通用启动上下文中的协议枚举。
    pub const fn as_start_protocol(self) -> StartBootProtocol {
        match self {
            Self::Multiboot2 => StartBootProtocol::Multiboot2,
            Self::Efi => StartBootProtocol::Efi,
            Self::LinuxBoot => StartBootProtocol::LinuxBoot,
            Self::Coreboot => StartBootProtocol::Coreboot,
            Self::Direct => StartBootProtocol::Direct,
        }
    }
}

/// 启动协议输入损坏时的错误。错误值不携带来自输入的字符串，便于在早期
/// 启动阶段直接记录或 panic，而不会依赖堆分配。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootProtocolError {
    /// 输入在读取字段前已经结束。
    Truncated(&'static str),
    /// 字段值违反协议约束。
    Invalid(&'static str),
    /// 地址或长度计算溢出。
    Overflow(&'static str),
    /// 协议版本或描述符格式超出解析器能力。
    Unsupported(&'static str),
}

impl fmt::Display for BootProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, detail) = match self {
            Self::Truncated(detail) => ("truncated", detail),
            Self::Invalid(detail) => ("invalid", detail),
            Self::Overflow(detail) => ("overflow", detail),
            Self::Unsupported(detail) => ("unsupported", detail),
        };
        write!(formatter, "{kind} x86 boot protocol data: {detail}")
    }
}

/// 各类 x86 入口可用的原始参数快照。
///
/// 入口汇编只负责把寄存器复制到该结构；协议解释由本模块完成。字段全部
/// 使用整数，避免在固件尚未验证之前形成带生命周期的裸指针引用。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootRegisters {
    /// Multiboot2 的 EAX magic，或 Linux/EFI 路径的第一个入口参数。
    pub arg0: usize,
    /// Multiboot2 信息结构地址，或入口的第二个参数。
    pub arg1: usize,
    /// EFI image handle（若入口按 EFI ABI 进入）。
    pub arg2: usize,
    /// EFI system table（若入口按 EFI ABI 进入）。
    pub arg3: usize,
    /// 入口代码探测到的 boot CPU 硬件 ID。
    pub boot_cpu_id: usize,
}

impl BootRegisters {
    /// 以 Multiboot2 的寄存器约定构造快照。
    pub const fn multiboot2(magic: u32, info: usize, boot_cpu_id: usize) -> Self {
        Self {
            arg0: magic as usize,
            arg1: info,
            arg2: 0,
            arg3: 0,
            boot_cpu_id,
        }
    }

    /// 以 EFI stub 的 image handle/system table 约定构造快照。
    pub const fn efi(image_handle: usize, system_table: usize, boot_cpu_id: usize) -> Self {
        Self {
            arg0: image_handle,
            arg1: system_table,
            arg2: 0,
            arg3: 0,
            boot_cpu_id,
        }
    }

    /// 在不解引用任何地址的前提下，识别最明确的入口协议。
    pub const fn detect(self) -> Option<X86BootProtocol> {
        if self.arg0 == MULTIBOOT2_BOOTLOADER_MAGIC as usize {
            Some(X86BootProtocol::Multiboot2)
        } else if self.arg0 != 0 && self.arg1 != 0 {
            Some(X86BootProtocol::Efi)
        } else {
            None
        }
    }
}

/// 已验证的 Multiboot2 信息结构视图。
#[derive(Clone, Copy, Debug)]
pub struct Multiboot2Info<'a> {
    bytes: &'a [u8],
    total_size: usize,
}

impl<'a> Multiboot2Info<'a> {
    /// 解析 Multiboot2 信息结构。
    ///
    /// 调用者必须保证 `bytes` 覆盖 bootloader 提供的整个结构；解析器仍会
    /// 对 `total_size`、tag 长度、8 字节对齐和结束 tag 做完整检查。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BootProtocolError> {
        if bytes.len() < 16 {
            return Err(BootProtocolError::Truncated("Multiboot2 header"));
        }
        let total_size = read_u32(bytes, 0)? as usize;
        if total_size < 16 || total_size > bytes.len() {
            return Err(BootProtocolError::Invalid("Multiboot2 total_size"));
        }
        // The second word of a Multiboot2 information structure is reserved
        // and must be zero.  Rejecting it here keeps a malformed handoff from
        // being mistaken for a valid tag stream by later consumers.
        if read_u32(bytes, 4)? != 0 {
            return Err(BootProtocolError::Invalid("Multiboot2 reserved field"));
        }

        let mut offset = 8usize;
        let mut found_end = false;
        while offset < total_size {
            if offset % 8 != 0 {
                return Err(BootProtocolError::Invalid("Multiboot2 tag alignment"));
            }
            let tag_type = read_u32(bytes, offset)?;
            let tag_size = read_u32(bytes, offset + 4)? as usize;
            if tag_size < 8 {
                return Err(BootProtocolError::Invalid("Multiboot2 tag size"));
            }
            let tag_end = offset
                .checked_add(tag_size)
                .ok_or(BootProtocolError::Overflow("Multiboot2 tag end"))?;
            if tag_end > total_size {
                return Err(BootProtocolError::Truncated("Multiboot2 tag payload"));
            }
            if tag_type == MULTIBOOT2_TAG_END {
                if tag_size != 8 || tag_end != total_size {
                    return Err(BootProtocolError::Invalid("Multiboot2 end tag size"));
                }
                found_end = true;
                break;
            }
            offset = align_up(tag_end, 8)?;
            if offset > total_size {
                return Err(BootProtocolError::Truncated("Multiboot2 tag padding"));
            }
        }
        if !found_end {
            return Err(BootProtocolError::Invalid("Multiboot2 missing end tag"));
        }
        Ok(Self { bytes, total_size })
    }

    /// 返回结构声明的总长度。
    pub const fn total_size(self) -> usize {
        self.total_size
    }

    /// 遍历所有（不含结束 tag 的）Multiboot2 tag。
    pub fn tags(self) -> Multiboot2TagIter<'a> {
        Multiboot2TagIter {
            bytes: self.bytes,
            offset: 8,
            end: self.total_size,
        }
    }

    /// 读取 command-line tag，并去掉结尾 NUL。
    pub fn command_line(self) -> Option<&'a [u8]> {
        self.find_tag(MULTIBOOT2_TAG_CMDLINE)
            .map(|tag| trim_nul(tag.payload()))
    }

    /// 读取 ACPI RSDP tag 的稳定字节视图。
    pub fn acpi_rsdp(self) -> Option<&'a [u8]> {
        // A Multiboot2 handoff may include both RSDP variants. Prefer the
        // ACPI 2.0+ tag so the loader retains XSDT and 64-bit table pointers.
        self.find_tag(MULTIBOOT2_TAG_ACPI_NEW)
            .or_else(|| self.find_tag(MULTIBOOT2_TAG_ACPI_OLD))
            .map(|tag| tag.payload())
    }

    /// 读取 EFI system table 指针（仅返回地址，不解引用）。
    pub fn efi_system_table(self) -> Result<Option<u64>, BootProtocolError> {
        if let Some(tag) = self.find_tag(MULTIBOOT2_TAG_EFI64) {
            return Ok(Some(read_u64(tag.payload(), 0)?));
        }
        if let Some(tag) = self.find_tag(MULTIBOOT2_TAG_EFI32) {
            return Ok(Some(u64::from(read_u32(tag.payload(), 0)?)));
        }
        Ok(None)
    }

    /// 读取并规范化 Multiboot2 memory-map tag。
    pub fn memory_regions(self) -> Result<Vec<StartMemoryRegion>, BootProtocolError> {
        let Some(tag) = self.find_tag(MULTIBOOT2_TAG_MEMORY_MAP) else {
            return Err(BootProtocolError::Invalid(
                "Multiboot2 memory map tag missing",
            ));
        };
        parse_multiboot_memory_map(tag.payload())
    }

    /// 将 Multiboot2 memory-map 条目写入调用者提供的固定缓冲区。
    ///
    /// 早期入口尚未安装全局分配器，不能调用会分配 `Vec` 的
    /// [`memory_regions`](Self::memory_regions)。该窄接口保留与分配版本相同的
    /// 边界校验，并在缓冲区不足时返回显式错误，不会静默截断内存图。
    pub fn memory_regions_into(
        self,
        output: &mut [StartMemoryRegion],
    ) -> Result<usize, BootProtocolError> {
        let Some(tag) = self.find_tag(MULTIBOOT2_TAG_MEMORY_MAP) else {
            return Err(BootProtocolError::Invalid(
                "Multiboot2 memory map tag missing",
            ));
        };
        parse_multiboot_memory_map_into(tag.payload(), output)
    }

    /// 读取 Multiboot2 EFI memory-map tag。
    pub fn efi_memory_map(self) -> Result<Option<EfiMemoryMap<'a>>, BootProtocolError> {
        let Some(tag) = self.find_tag(MULTIBOOT2_TAG_EFI_MEMORY_MAP) else {
            return Ok(None);
        };
        EfiMemoryMap::from_multiboot_tag(tag.payload()).map(Some)
    }

    /// 查找指定类型的 tag。
    pub fn find_tag(self, tag_type: u32) -> Option<Multiboot2Tag<'a>> {
        self.tags().find(|tag| tag.tag_type() == tag_type)
    }
}

/// Multiboot2 单个 tag 的已验证视图。
#[derive(Clone, Copy, Debug)]
pub struct Multiboot2Tag<'a> {
    tag_type: u32,
    payload: &'a [u8],
}

impl<'a> Multiboot2Tag<'a> {
    pub const fn tag_type(self) -> u32 {
        self.tag_type
    }

    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }
}

/// Multiboot2 tag 迭代器。结构在 `Multiboot2Info::parse` 中完成完整校验，
/// 因此迭代阶段只需要安全地跳过已验证的对齐填充。
#[derive(Clone, Copy, Debug)]
pub struct Multiboot2TagIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    end: usize,
}

impl<'a> Iterator for Multiboot2TagIter<'a> {
    type Item = Multiboot2Tag<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.end {
            return None;
        }
        let tag_type = read_u32(self.bytes, self.offset).ok()?;
        let tag_size = read_u32(self.bytes, self.offset + 4).ok()? as usize;
        if tag_type == MULTIBOOT2_TAG_END {
            self.offset = self.end;
            return None;
        }
        let payload_end = self.offset.checked_add(tag_size)?;
        if payload_end > self.end || tag_size < 8 {
            self.offset = self.end;
            return None;
        }
        let payload = &self.bytes[self.offset + 8..payload_end];
        self.offset = align_up(payload_end, 8).ok()?;
        Some(Multiboot2Tag { tag_type, payload })
    }
}

/// Multiboot2 memory-map entry 的协议类型。
pub const MULTIBOOT_MEMORY_AVAILABLE: u32 = 1;
pub const MULTIBOOT_MEMORY_ACPI_RECLAIMABLE: u32 = 3;
pub const MULTIBOOT_MEMORY_NVS: u32 = 4;
pub const MULTIBOOT_MEMORY_BADRAM: u32 = 5;

fn parse_multiboot_memory_map(payload: &[u8]) -> Result<Vec<StartMemoryRegion>, BootProtocolError> {
    // Validate the framing before constructing a Vec.  In particular, do not
    // divide by a guessed `max(1)` entry size: a hostile tag with entry_size=0
    // must fail closed instead of turning its byte length into an allocation
    // request.
    let entry_count = multiboot_memory_map_shape(payload)?;
    let mut regions = Vec::new();
    regions
        .try_reserve_exact(entry_count)
        .map_err(|_| BootProtocolError::Overflow("Multiboot2 memory-map allocation"))?;
    regions.resize(
        entry_count,
        StartMemoryRegion::new(
            StartPhysRange::new(0, 1),
            StartMemoryRegionKind::Reserved,
            0,
        ),
    );
    let count = parse_multiboot_memory_map_into(payload, &mut regions)?;
    regions.truncate(count);
    Ok(regions)
}

fn parse_multiboot_memory_map_into(
    payload: &[u8],
    output: &mut [StartMemoryRegion],
) -> Result<usize, BootProtocolError> {
    let entry_count = multiboot_memory_map_shape(payload)?;
    let entry_size = read_u32(payload, 0)? as usize;
    // Entries with zero length are ignored, but capacity is checked against the
    // worst-case count before any output is written so callers never receive a
    // partially populated map on failure.
    if output.len() < entry_count {
        return Err(BootProtocolError::Unsupported(
            "Multiboot2 memory-map output buffer capacity",
        ));
    }
    let mut count = 0usize;
    let mut offset = 8usize;
    while offset < payload.len() {
        let entry = &payload[offset..offset + entry_size];
        let start = read_u64(entry, 0)?;
        let length = read_u64(entry, 8)?;
        let kind = read_multiboot_memory_kind(read_u32(entry, 16)?);
        let start = usize::try_from(start)
            .map_err(|_| BootProtocolError::Overflow("Multiboot2 physical address"))?;
        let length = usize::try_from(length)
            .map_err(|_| BootProtocolError::Overflow("Multiboot2 memory length"))?;
        let end = start
            .checked_add(length)
            .ok_or(BootProtocolError::Overflow("Multiboot2 memory range"))?;
        if end > start {
            output[count] = StartMemoryRegion::new(StartPhysRange::new(start, end), kind, 0)
                .with_source_type(read_u32(entry, 16)?);
            count += 1;
        }
        offset += entry_size;
    }
    Ok(count)
}

/// Validate the fixed memory-map header and return its exact entry count.
/// Keeping this helper shared by the allocating and fixed-buffer APIs ensures
/// malformed protocol data cannot reach either a slice index or an allocator.
fn multiboot_memory_map_shape(payload: &[u8]) -> Result<usize, BootProtocolError> {
    if payload.len() < 8 {
        return Err(BootProtocolError::Truncated("Multiboot2 memory-map header"));
    }
    let entry_size = read_u32(payload, 0)? as usize;
    if entry_size < 24 {
        return Err(BootProtocolError::Invalid(
            "Multiboot2 memory-map entry size",
        ));
    }
    let payload_len = payload.len() - 8;
    if !payload_len.is_multiple_of(entry_size) {
        return Err(BootProtocolError::Invalid(
            "Multiboot2 memory-map entry framing",
        ));
    }
    Ok(payload_len / entry_size)
}

fn read_multiboot_memory_kind(kind: u32) -> StartMemoryRegionKind {
    match kind {
        MULTIBOOT_MEMORY_AVAILABLE => StartMemoryRegionKind::UsableRam,
        MULTIBOOT_MEMORY_ACPI_RECLAIMABLE => StartMemoryRegionKind::AcpiReclaimable,
        MULTIBOOT_MEMORY_NVS => StartMemoryRegionKind::AcpiNonVolatileStorage,
        MULTIBOOT_MEMORY_BADRAM => StartMemoryRegionKind::Unusable,
        _ => StartMemoryRegionKind::Reserved,
    }
}

/// EFI memory descriptor 类型常量（UEFI 规范 Table 7.2）。
pub mod efi_memory_type {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL: u32 = 7;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
    pub const UNACCEPTED: u32 = 15;
}

/// EFI memory descriptor 的已验证字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EfiMemoryDescriptor {
    pub type_: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

impl EfiMemoryDescriptor {
    /// 将 EFI descriptor 转为 `StartContext` 使用的统一区域。
    pub fn to_start_region(self) -> Result<StartMemoryRegion, BootProtocolError> {
        let bytes = self
            .number_of_pages
            .checked_mul(4096)
            .ok_or(BootProtocolError::Overflow("EFI descriptor page count"))?;
        let start = usize::try_from(self.physical_start)
            .map_err(|_| BootProtocolError::Overflow("EFI physical address"))?;
        let length = usize::try_from(bytes)
            .map_err(|_| BootProtocolError::Overflow("EFI descriptor length"))?;
        let end = start
            .checked_add(length)
            .ok_or(BootProtocolError::Overflow("EFI descriptor range"))?;
        let kind = match self.type_ {
            efi_memory_type::LOADER_CODE | efi_memory_type::LOADER_DATA => {
                StartMemoryRegionKind::BootloaderReclaimable
            }
            efi_memory_type::BOOT_SERVICES_CODE | efi_memory_type::BOOT_SERVICES_DATA => {
                StartMemoryRegionKind::FirmwareReclaimable
            }
            efi_memory_type::RUNTIME_SERVICES_CODE | efi_memory_type::RUNTIME_SERVICES_DATA => {
                StartMemoryRegionKind::FirmwareRuntime
            }
            efi_memory_type::CONVENTIONAL => StartMemoryRegionKind::UsableRam,
            efi_memory_type::UNUSABLE => StartMemoryRegionKind::Unusable,
            efi_memory_type::ACPI_RECLAIM => StartMemoryRegionKind::AcpiReclaimable,
            efi_memory_type::ACPI_NVS => StartMemoryRegionKind::AcpiNonVolatileStorage,
            efi_memory_type::MMIO | efi_memory_type::MMIO_PORT => StartMemoryRegionKind::Mmio,
            _ => StartMemoryRegionKind::Reserved,
        };
        Ok(
            StartMemoryRegion::new(StartPhysRange::new(start, end), kind, self.attribute)
                .with_source_type(self.type_),
        )
    }
}

/// EFI memory map（描述符尺寸可由固件扩展，不能按固定结构体步进）。
#[derive(Clone, Copy, Debug)]
pub struct EfiMemoryMap<'a> {
    bytes: &'a [u8],
    descriptor_size: usize,
    descriptor_version: u32,
}

impl<'a> EfiMemoryMap<'a> {
    /// 从裸描述符数组构造视图。
    pub fn new(
        bytes: &'a [u8],
        descriptor_size: usize,
        descriptor_version: u32,
    ) -> Result<Self, BootProtocolError> {
        if descriptor_version < 1 {
            return Err(BootProtocolError::Unsupported(
                "EFI memory descriptor version",
            ));
        }
        if descriptor_size < 40 || !descriptor_size.is_multiple_of(8) {
            return Err(BootProtocolError::Invalid("EFI descriptor size"));
        }
        if bytes.is_empty() || !bytes.len().is_multiple_of(descriptor_size) {
            return Err(BootProtocolError::Invalid("EFI memory-map length"));
        }
        Ok(Self {
            bytes,
            descriptor_size,
            descriptor_version,
        })
    }

    fn from_multiboot_tag(payload: &'a [u8]) -> Result<Self, BootProtocolError> {
        if payload.len() < 8 {
            return Err(BootProtocolError::Truncated(
                "Multiboot2 EFI memory-map header",
            ));
        }
        Self::new(
            &payload[8..],
            read_u32(payload, 0)? as usize,
            read_u32(payload, 4)?,
        )
    }

    pub const fn descriptor_size(self) -> usize {
        self.descriptor_size
    }

    pub const fn descriptor_version(self) -> u32 {
        self.descriptor_version
    }

    pub fn iter(self) -> EfiMemoryDescriptorIter<'a> {
        EfiMemoryDescriptorIter {
            map: self,
            offset: 0,
        }
    }

    pub fn regions(self) -> Result<Vec<StartMemoryRegion>, BootProtocolError> {
        self.iter()
            .map(EfiMemoryDescriptor::to_start_region)
            .collect()
    }
}

/// EFI descriptor 迭代器。
#[derive(Clone, Copy, Debug)]
pub struct EfiMemoryDescriptorIter<'a> {
    map: EfiMemoryMap<'a>,
    offset: usize,
}

impl<'a> Iterator for EfiMemoryDescriptorIter<'a> {
    type Item = EfiMemoryDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.map.bytes.len() {
            return None;
        }
        let end = self.offset.checked_add(self.map.descriptor_size)?;
        if end > self.map.bytes.len() {
            self.offset = self.map.bytes.len();
            return None;
        }
        let descriptor = &self.map.bytes[self.offset..end];
        self.offset = end;
        Some(EfiMemoryDescriptor {
            type_: read_u32(descriptor, 0).ok()?,
            physical_start: read_u64(descriptor, 8).ok()?,
            virtual_start: read_u64(descriptor, 16).ok()?,
            number_of_pages: read_u64(descriptor, 24).ok()?,
            attribute: read_u64(descriptor, 32).ok()?,
        })
    }
}

/// E820 memory-map entry 的协议类型。
pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;
pub const E820_ACPI: u32 = 3;
pub const E820_NVS: u32 = 4;
pub const E820_UNUSABLE: u32 = 5;

/// 解析 Linux boot protocol 或 coreboot 提供的 E820 数组。
///
/// `entry_size` 至少为 20；大于 20 的扩展字段被保留在 `attributes` 的低
/// 32 位中（若存在），不会影响地址范围判定。
pub fn normalize_e820(
    bytes: &[u8],
    entry_size: usize,
) -> Result<Vec<StartMemoryRegion>, BootProtocolError> {
    if entry_size < 20 || bytes.is_empty() || !bytes.len().is_multiple_of(entry_size) {
        return Err(BootProtocolError::Invalid("E820 entry size or length"));
    }
    let mut regions = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let entry = &bytes[offset..offset + entry_size];
        let start = usize::try_from(read_u64(entry, 0)?)
            .map_err(|_| BootProtocolError::Overflow("E820 physical address"))?;
        let length = usize::try_from(read_u64(entry, 8)?)
            .map_err(|_| BootProtocolError::Overflow("E820 memory length"))?;
        let end = start
            .checked_add(length)
            .ok_or(BootProtocolError::Overflow("E820 memory range"))?;
        let source_type = read_u32(entry, 16)?;
        let attributes = if entry_size >= 24 {
            u64::from(read_u32(entry, 20)?)
        } else {
            0
        };
        let kind = match source_type {
            E820_RAM => StartMemoryRegionKind::UsableRam,
            E820_ACPI => StartMemoryRegionKind::AcpiReclaimable,
            E820_NVS => StartMemoryRegionKind::AcpiNonVolatileStorage,
            E820_UNUSABLE => StartMemoryRegionKind::Unusable,
            _ => StartMemoryRegionKind::Reserved,
        };
        if end > start {
            regions.push(
                StartMemoryRegion::new(StartPhysRange::new(start, end), kind, attributes)
                    .with_source_type(source_type),
            );
        }
        offset += entry_size;
    }
    Ok(regions)
}

/// Linux x86 boot protocol 中 setup header 的关键字段偏移。
pub mod linux_boot_offset {
    pub const BOOT_FLAG: usize = 0x1fe;
    pub const HEADER: usize = 0x202;
    pub const VERSION: usize = 0x206;
    pub const LOADFLAGS: usize = 0x211;
    pub const RAMDISK_IMAGE: usize = 0x218;
    pub const RAMDISK_SIZE: usize = 0x21c;
    pub const CMDLINE_PTR: usize = 0x228;
    pub const CMDLINE_SIZE: usize = 0x238;
    pub const HARDWARE_SUBARCH: usize = 0x23c;
}

/// 已验证的 Linux `boot_params` 视图。
#[derive(Clone, Copy, Debug)]
pub struct LinuxBootParams<'a> {
    bytes: &'a [u8],
}

impl<'a> LinuxBootParams<'a> {
    /// 校验 boot flag、`HdrS` 签名和最小协议版本。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BootProtocolError> {
        if bytes.len() < linux_boot_offset::CMDLINE_SIZE + 4 {
            return Err(BootProtocolError::Truncated(
                "Linux boot_params setup header",
            ));
        }
        if read_u16(bytes, linux_boot_offset::BOOT_FLAG)? != 0xaa55 {
            return Err(BootProtocolError::Invalid("Linux boot flag"));
        }
        if &bytes[linux_boot_offset::HEADER..linux_boot_offset::HEADER + 4] != b"HdrS" {
            return Err(BootProtocolError::Invalid("Linux boot protocol signature"));
        }
        if read_u16(bytes, linux_boot_offset::VERSION)? < 0x0200 {
            return Err(BootProtocolError::Unsupported(
                "Linux boot protocol before 2.00",
            ));
        }
        Ok(Self { bytes })
    }

    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    pub fn protocol_version(self) -> Result<u16, BootProtocolError> {
        read_u16(self.bytes, linux_boot_offset::VERSION)
    }

    pub fn load_flags(self) -> Result<u8, BootProtocolError> {
        read_u8(self.bytes, linux_boot_offset::LOADFLAGS)
    }

    pub fn command_line_ptr(self) -> Result<u32, BootProtocolError> {
        read_u32(self.bytes, linux_boot_offset::CMDLINE_PTR)
    }

    pub fn command_line_size(self) -> Result<u32, BootProtocolError> {
        read_u32(self.bytes, linux_boot_offset::CMDLINE_SIZE)
    }

    pub fn ramdisk(self) -> Result<(u32, u32), BootProtocolError> {
        Ok((
            read_u32(self.bytes, linux_boot_offset::RAMDISK_IMAGE)?,
            read_u32(self.bytes, linux_boot_offset::RAMDISK_SIZE)?,
        ))
    }

    pub fn hardware_subarch(self) -> Result<u32, BootProtocolError> {
        read_u32(self.bytes, linux_boot_offset::HARDWARE_SUBARCH)
    }
}

/// EFI stub 入口参数的整数快照。
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EfiStubArguments {
    pub image_handle: usize,
    pub system_table: usize,
}

impl EfiStubArguments {
    /// Construct an EFI hand-off snapshot. The third argument is retained for
    /// callers that also carry a boot-CPU id; it is deliberately not stored in
    /// the two-register EFI ABI payload.
    pub const fn efi(image_handle: usize, system_table: usize, _boot_cpu_id: usize) -> Self {
        Self {
            image_handle,
            system_table,
        }
    }

    /// 拒绝空句柄/空 system table，避免把普通直接入口误当作 EFI。
    pub const fn validate(self) -> Result<(), BootProtocolError> {
        if self.image_handle == 0 || self.system_table == 0 {
            Err(BootProtocolError::Invalid("EFI stub entry arguments"))
        } else {
            Ok(())
        }
    }
}

/// 构造 `StartBootInfo` 的窄适配器。命令行必须已经由 loader 复制到静态
/// 存储后再传入，以满足 `StartContext` 的稳定生命周期约束。
pub const fn start_boot_info(
    architecture: general::ArchitectureId,
    protocol: X86BootProtocol,
    boot_cpu_id: usize,
    command_line: Option<&'static [u8]>,
) -> StartBootInfo {
    StartBootInfo {
        architecture,
        protocol: protocol.as_start_protocol(),
        boot_cpu_id,
        command_line,
    }
}

fn trim_nul(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .position(|byte| *byte == 0)
        .map_or(bytes, |index| &bytes[..index])
}

fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, BootProtocolError> {
    bytes
        .get(offset)
        .copied()
        .ok_or(BootProtocolError::Truncated("u8 field"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, BootProtocolError> {
    let end = offset
        .checked_add(2)
        .ok_or(BootProtocolError::Overflow("u16 field offset"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or(BootProtocolError::Truncated("u16 field"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, BootProtocolError> {
    let end = offset
        .checked_add(4)
        .ok_or(BootProtocolError::Overflow("u32 field offset"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or(BootProtocolError::Truncated("u32 field"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, BootProtocolError> {
    let end = offset
        .checked_add(8)
        .ok_or(BootProtocolError::Overflow("u64 field offset"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or(BootProtocolError::Truncated("u64 field"))?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn align_up(value: usize, alignment: usize) -> Result<usize, BootProtocolError> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(BootProtocolError::Overflow("aligned tag offset"))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn mb2(tags: &[(u32, &[u8])]) -> Vec<u8> {
        let mut bytes = vec![0u8; 8];
        for (tag_type, tag) in tags {
            let size = 8 + tag.len();
            let aligned = (size + 7) & !7;
            let offset = bytes.len();
            bytes.resize(offset + aligned, 0);
            bytes[offset..offset + 4].copy_from_slice(&tag_type.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&(size as u32).to_le_bytes());
            bytes[offset + 8..offset + 8 + tag.len()].copy_from_slice(tag);
        }
        bytes.extend_from_slice(&[0; 8]);
        let end = bytes.len() - 8;
        bytes[end + 4..end + 8].copy_from_slice(&8u32.to_le_bytes());
        let total = bytes.len() as u32;
        bytes[..4].copy_from_slice(&total.to_le_bytes());
        bytes
    }

    #[test]
    fn multiboot2_rejects_missing_end_tag() {
        let mut bytes = vec![0u8; 16];
        bytes[..4].copy_from_slice(&(16u32).to_le_bytes());
        bytes[8..12].copy_from_slice(&MULTIBOOT2_TAG_CMDLINE.to_le_bytes());
        bytes[12..16].copy_from_slice(&(8u32).to_le_bytes());
        assert!(Multiboot2Info::parse(&bytes).is_err());
    }

    #[test]
    fn multiboot2_rejects_nonzero_reserved_word() {
        let mut bytes = mb2(&[]);
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            Multiboot2Info::parse(&bytes),
            Err(BootProtocolError::Invalid("Multiboot2 reserved field"))
        ));
    }

    #[test]
    fn multiboot2_reads_command_line_and_memory_map() {
        let mut cmd = Vec::new();
        cmd.extend_from_slice(b"root=/dev/vd0p1\0");
        let mut map = vec![0u8; 8 + 24];
        map[..4].copy_from_slice(&(24u32).to_le_bytes());
        map[8..16].copy_from_slice(&(0x1000u64).to_le_bytes());
        map[16..24].copy_from_slice(&(0x9000u64).to_le_bytes());
        map[24..28].copy_from_slice(&MULTIBOOT_MEMORY_AVAILABLE.to_le_bytes());
        let info = mb2(&[
            (MULTIBOOT2_TAG_CMDLINE, &cmd),
            (MULTIBOOT2_TAG_MEMORY_MAP, &map),
        ]);
        let parsed = Multiboot2Info::parse(&info).unwrap();
        assert_eq!(parsed.command_line(), Some(&b"root=/dev/vd0p1"[..]));
        let regions = parsed.memory_regions().unwrap();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].range, StartPhysRange::new(0x1000, 0xa000));
        assert_eq!(regions[0].kind, StartMemoryRegionKind::UsableRam);
    }

    #[test]
    fn multiboot2_prefers_the_new_acpi_rsdp_tag() {
        let old = b"old-rsdp";
        let new = b"new-rsdp";
        let info = mb2(&[
            (MULTIBOOT2_TAG_ACPI_OLD, old),
            (MULTIBOOT2_TAG_ACPI_NEW, new),
        ]);
        let parsed = Multiboot2Info::parse(&info).expect("valid Multiboot2 info");
        assert_eq!(parsed.acpi_rsdp(), Some(new.as_slice()));
    }

    #[test]
    fn efi_descriptor_size_and_types_are_checked() {
        let mut descriptor = vec![0u8; 40];
        descriptor[..4].copy_from_slice(&efi_memory_type::CONVENTIONAL.to_le_bytes());
        descriptor[8..16].copy_from_slice(&(0x20_0000u64).to_le_bytes());
        descriptor[24..32].copy_from_slice(&(2u64).to_le_bytes());
        let map = EfiMemoryMap::new(&descriptor, 40, 1).unwrap();
        let regions = map.regions().unwrap();
        assert_eq!(regions[0].kind, StartMemoryRegionKind::UsableRam);
        assert_eq!(regions[0].range, StartPhysRange::new(0x20_0000, 0x20_2000));
        assert!(EfiMemoryMap::new(&descriptor, 16, 1).is_err());
        assert!(EfiMemoryMap::new(&descriptor, 40, 0).is_err());
        assert!(EfiMemoryMap::new(&descriptor, 41, 1).is_err());
    }

    #[test]
    fn linux_boot_header_is_validated() {
        let mut bytes = vec![0u8; 0x240];
        bytes[linux_boot_offset::BOOT_FLAG..linux_boot_offset::BOOT_FLAG + 2]
            .copy_from_slice(&0xaa55u16.to_le_bytes());
        bytes[linux_boot_offset::HEADER..linux_boot_offset::HEADER + 4].copy_from_slice(b"HdrS");
        bytes[linux_boot_offset::VERSION..linux_boot_offset::VERSION + 2]
            .copy_from_slice(&0x020bu16.to_le_bytes());
        bytes[linux_boot_offset::CMDLINE_PTR..linux_boot_offset::CMDLINE_PTR + 4]
            .copy_from_slice(&0x1234_0000u32.to_le_bytes());
        let params = LinuxBootParams::parse(&bytes).unwrap();
        assert_eq!(params.protocol_version().unwrap(), 0x020b);
        assert_eq!(params.command_line_ptr().unwrap(), 0x1234_0000);
        bytes[linux_boot_offset::BOOT_FLAG] = 0;
        assert!(LinuxBootParams::parse(&bytes).is_err());
    }

    #[test]
    fn e820_normalization_preserves_unknown_as_reserved() {
        let mut bytes = vec![0u8; 40];
        bytes[0..8].copy_from_slice(&0x1000u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&0x2000u64.to_le_bytes());
        bytes[16..20].copy_from_slice(&99u32.to_le_bytes());
        let regions = normalize_e820(&bytes, 20).unwrap();
        assert_eq!(regions[0].kind, StartMemoryRegionKind::Reserved);
    }

    #[test]
    fn register_detection_does_not_dereference_addresses() {
        assert_eq!(
            BootRegisters::multiboot2(MULTIBOOT2_BOOTLOADER_MAGIC, 0xdead_beef, 0).detect(),
            Some(X86BootProtocol::Multiboot2)
        );
        assert_eq!(
            BootRegisters::efi(1, 2, 0).detect(),
            Some(X86BootProtocol::Efi)
        );
        assert_eq!(BootRegisters::default().detect(), None);
    }

    #[test]
    fn multiboot2_image_header_checksum_is_verified() {
        let mut header = vec![0u8; 24];
        header[0..4].copy_from_slice(&MULTIBOOT2_HEADER_MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&MULTIBOOT2_HEADER_ARCH_I386.to_le_bytes());
        header[8..12].copy_from_slice(&(24u32).to_le_bytes());
        let checksum = 0u32
            .wrapping_sub(MULTIBOOT2_HEADER_MAGIC)
            .wrapping_sub(MULTIBOOT2_HEADER_ARCH_I386)
            .wrapping_sub(24);
        header[12..16].copy_from_slice(&checksum.to_le_bytes());
        header[16..20].copy_from_slice(&MULTIBOOT2_TAG_END.to_le_bytes());
        header[20..24].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(validate_multiboot2_header(&header), Ok(24));
        header[0] ^= 1;
        assert!(validate_multiboot2_header(&header).is_err());
    }

    #[test]
    fn multiboot2_checksum_ignores_linker_tag_payloads() {
        // A real x86 header carries physical addresses in the optional tags.
        // Those values are intentionally unrelated to the fixed-header sum.
        let mut header = vec![0u8; 64];
        header[0..4].copy_from_slice(&MULTIBOOT2_HEADER_MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&MULTIBOOT2_HEADER_ARCH_I386.to_le_bytes());
        header[8..12].copy_from_slice(&(64u32).to_le_bytes());
        let checksum = 0u32
            .wrapping_sub(MULTIBOOT2_HEADER_MAGIC)
            .wrapping_sub(MULTIBOOT2_HEADER_ARCH_I386)
            .wrapping_sub(64);
        header[12..16].copy_from_slice(&checksum.to_le_bytes());
        header[16..20].copy_from_slice(&2u32.to_le_bytes());
        header[20..24].copy_from_slice(&24u32.to_le_bytes());
        header[24..28].copy_from_slice(&0x0020_0110u32.to_le_bytes());
        header[28..32].copy_from_slice(&0x0020_0000u32.to_le_bytes());
        header[36..40].copy_from_slice(&0x0630_7000u32.to_le_bytes());
        header[40..44].copy_from_slice(&3u32.to_le_bytes());
        header[44..48].copy_from_slice(&12u32.to_le_bytes());
        header[48..52].copy_from_slice(&0x0020_0000u32.to_le_bytes());
        // The 12-byte entry tag is followed by four bytes of alignment
        // padding; the end tag therefore starts at offset 56.
        header[56..60].copy_from_slice(&0u32.to_le_bytes());
        header[60..64].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(validate_multiboot2_header(&header), Ok(64));
    }

    #[test]
    fn multiboot2_header_rejects_unterminated_tags() {
        let mut header = vec![0u8; 24];
        header[0..4].copy_from_slice(&MULTIBOOT2_HEADER_MAGIC.to_le_bytes());
        header[8..12].copy_from_slice(&24u32.to_le_bytes());
        let checksum = 0u32.wrapping_sub(MULTIBOOT2_HEADER_MAGIC).wrapping_sub(24);
        header[12..16].copy_from_slice(&checksum.to_le_bytes());
        header[16..20].copy_from_slice(&1u32.to_le_bytes());
        header[20..24].copy_from_slice(&8u32.to_le_bytes());
        assert!(validate_multiboot2_header(&header).is_err());
    }

    #[test]
    fn multiboot2_info_rejects_bytes_after_end_tag() {
        let mut info = mb2(&[]);
        let total = info.len() as u32;
        info.extend_from_slice(&[0; 8]);
        info[..4].copy_from_slice(&(total + 8).to_le_bytes());
        assert!(Multiboot2Info::parse(&info).is_err());
    }
}
