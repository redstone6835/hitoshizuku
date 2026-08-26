//! LoongArch64 启动期 16550 控制台的零分配 DT 配置解析。
//!
//! 该模块只处理在堆和正式设备模型建立前必须知道的最小信息：chosen 控制台、
//! 普通总线 `reg` 地址翻译、UART 时钟/波特率以及寄存器步长和访问宽度。解析失败
//! 由硬件访问层统一回退到传统 QEMU/Loongson 参数，绝不使用半解析配置。

use fdt::{Fdt, Node};

const DEFAULT_ADDRESS_CELLS: usize = 2;
const DEFAULT_SIZE_CELLS: usize = 1;
const MAX_SUPPORTED_CELLS: usize = 4;
const MAX_FIXED_CLOCK_DEPTH: usize = 16;
const UART_LSR_REGISTER: usize = 5;
const UART_DIVISOR_OVERSAMPLE: u32 = 16;

/// 没有可用 DT 控制台时保留的传统 LoongArch QEMU 安全配置。
#[cfg(not(mygo_la_board_ls2k1000))]
pub(crate) const FALLBACK_EARLY_UART_CONFIG: EarlyUartConfig = EarlyUartConfig {
    phys_base: 0x1fe0_01e0,
    clock_hz: 100_000_000,
    baud: 115_200,
    reg_offset: 0,
    reg_shift: 0,
    io_width: RegisterIoWidth::U8,
    endian: RegisterEndian::Little,
};

/// LS2K1000LA 板载 UART0 回退配置。
///
/// 该值只随显式 `MYGO_LA_BOARD=ls2k1000` 构建启用，防止任意 LoongArch
/// 固件解析失败时误写板级 MMIO 地址。
#[cfg(mygo_la_board_ls2k1000)]
pub(crate) const FALLBACK_EARLY_UART_CONFIG: EarlyUartConfig = EarlyUartConfig {
    phys_base: 0x1fe2_0000,
    clock_hz: 125_000_000,
    baud: 115_200,
    reg_offset: 0,
    reg_shift: 0,
    io_width: RegisterIoWidth::U8,
    endian: RegisterEndian::Little,
};

/// 16550 寄存器的单次 MMIO 访问宽度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisterIoWidth {
    U8,
    U16,
    U32,
}

impl RegisterIoWidth {
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }

    const fn from_bytes(bytes: u32) -> Option<Self> {
        match bytes {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            _ => None,
        }
    }
}

/// 多字节 UART 寄存器的设备字节序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum RegisterEndian {
    Little,
    Big,
}

/// 已完整校验、可直接交给最早期 MMIO 输出路径的 UART 配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EarlyUartConfig {
    pub(crate) phys_base: usize,
    pub(crate) clock_hz: u32,
    pub(crate) baud: u32,
    pub(crate) reg_offset: usize,
    pub(crate) reg_shift: u32,
    pub(crate) io_width: RegisterIoWidth,
    pub(crate) endian: RegisterEndian,
}

impl EarlyUartConfig {
    pub(crate) fn register_offset(self, register: usize) -> Option<usize> {
        register
            .checked_shl(self.reg_shift)
            .and_then(|offset| self.reg_offset.checked_add(offset))
    }

    pub(crate) fn divisor(self) -> Option<u16> {
        uart_divisor(self.clock_hz, self.baud)
    }
}

/// 启动期 DT 控制台配置不可安全使用的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EarlyUartConfigError {
    MissingStdout,
    InvalidStdout,
    InvalidPath,
    DisabledNode,
    UnsupportedUart,
    MissingReg,
    InvalidProperty,
    UnsupportedCellCount,
    MissingRanges,
    UnmappedAddress,
    AddressOverflow,
    MissingClock,
    InvalidClock,
    InvalidBaud,
    InvalidRegShift,
    InvalidIoWidth,
    ConflictingEndian,
    MisalignedRegister,
    RegisterWindowTooSmall,
    /// `earlycon=` 参数缺少访问方式段（io/mmio/mmio32/mmio32be）。
    MissingAccessWidth,
    /// `earlycon=` 参数缺少地址段，或地址段不是合法十六进制。
    InvalidAddress,
    /// `earlycon=` 地址解析后为空地址。
    ZeroAddress,
    /// `earlycon=` 地址超出当前早期映射窗口。
    AddressOutOfWindow,
}

/// 从完整校验的 FDT 中零分配解析启动控制台。
pub(crate) fn early_uart_config_from_fdt(
    fdt: Fdt<'_>,
) -> Result<EarlyUartConfig, EarlyUartConfigError> {
    let stdout = fdt
        .chosen_stdout()
        .map_err(|_| EarlyUartConfigError::InvalidStdout)?
        .ok_or(EarlyUartConfigError::MissingStdout)?;
    let target_depth = path_depth(stdout.path)?;
    if target_depth == 0 || node_at_depth(fdt, stdout.path, target_depth)? != stdout.node {
        return Err(EarlyUartConfigError::InvalidPath);
    }
    for depth in 0..=target_depth {
        if !node_is_available(node_at_depth(fdt, stdout.path, depth)?)? {
            return Err(EarlyUartConfigError::DisabledNode);
        }
    }
    if !node_is_16550(stdout.node)? {
        return Err(EarlyUartConfigError::UnsupportedUart);
    }

    let parent_depth = target_depth - 1;
    let parent = node_at_depth(fdt, stdout.path, parent_depth)?;
    let (mut address, size) = first_reg(stdout.node, parent)?;
    let mut bus_depth = parent_depth;
    while bus_depth != 0 {
        let bus = node_at_depth(fdt, stdout.path, bus_depth)?;
        let parent = node_at_depth(fdt, stdout.path, bus_depth - 1)?;
        address = translate_one_bus(bus, parent, address, size)?;
        bus_depth -= 1;
    }
    let phys_base = usize::try_from(address).map_err(|_| EarlyUartConfigError::AddressOverflow)?;

    let clock_hz = uart_clock_hz(fdt, stdout.node)?;
    if clock_hz == 0 {
        return Err(EarlyUartConfigError::InvalidClock);
    }
    let baud = match stdout.options {
        Some(options) if !options.is_empty() => baud_from_options(options)?,
        _ => optional_u32(stdout.node, "current-speed")?.unwrap_or(FALLBACK_EARLY_UART_CONFIG.baud),
    };
    if baud == 0 {
        return Err(EarlyUartConfigError::InvalidBaud);
    }
    let reg_offset = optional_u32(stdout.node, "reg-offset")?.unwrap_or(0) as usize;
    let reg_shift = optional_u32(stdout.node, "reg-shift")?.unwrap_or(0);
    if reg_shift >= usize::BITS {
        return Err(EarlyUartConfigError::InvalidRegShift);
    }
    let io_width =
        RegisterIoWidth::from_bytes(optional_u32(stdout.node, "reg-io-width")?.unwrap_or(1))
            .ok_or(EarlyUartConfigError::InvalidIoWidth)?;
    let endian = register_endian(stdout.node)?;
    let config = EarlyUartConfig {
        phys_base,
        clock_hz,
        baud,
        reg_offset,
        reg_shift,
        io_width,
        endian,
    };
    if config.divisor().is_none() {
        return Err(EarlyUartConfigError::InvalidBaud);
    }
    let first_register = config
        .phys_base
        .checked_add(config.reg_offset)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let register_stride = 1usize
        .checked_shl(config.reg_shift)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    if !first_register.is_multiple_of(io_width.bytes())
        || !register_stride.is_multiple_of(io_width.bytes())
    {
        return Err(EarlyUartConfigError::MisalignedRegister);
    }

    let last_offset = config
        .register_offset(UART_LSR_REGISTER)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let span = last_offset
        .checked_add(io_width.bytes())
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    config
        .phys_base
        .checked_add(span)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    if let Some(size) = size
        && span as u128 > size
    {
        return Err(EarlyUartConfigError::RegisterWindowTooSmall);
    }
    Ok(config)
}

/// 从 Linux 风格 `earlycon=` 命令行参数零分配构造启动控制台配置。
///
/// 支持语法（与 Linux `drivers/tty/serial/earlycon.c` 的 `uart`/`uart8250`
/// 入口一致）：
///
/// ```text
/// earlycon=uart[,options],<io|mmio|mmio32|mmio32be>,<addr>[,baud]
/// earlycon=uart8250[,options],<io|mmio|mmio32|mmio32be>,<addr>[,baud]
/// ```
///
/// `options` 段按 Linux 语义保留但不参与地址/宽度解析；`<addr>` 为十六进制
/// 物理地址（可带 `0x` 前缀），可选 `<baud>` 默认 115200。时钟频率取自调用方
/// 传入的 `fallback_clock_hz`——启动早期 DT/ACPI 尚未解析，cmdline 是唯一
/// 可用的控制台定位来源，波特率换算按该频率计算 divisor。
///
/// 返回的配置不包含 `reg-offset`/`reg-shift`（cmdline 无此表达），与 fallback
/// 一致为 0；宽度/字节序由访问方式段决定。所有算术与对齐在发布前校验。
pub(crate) fn early_uart_config_from_cmdline(
    value: &str,
    fallback_clock_hz: u32,
) -> Result<EarlyUartConfig, EarlyUartConfigError> {
    let mut parts = value.split(',');
    let driver = parts.next().unwrap_or("");
    if driver != "uart" && driver != "uart8250" {
        return Err(EarlyUartConfigError::UnsupportedUart);
    }
    // 可选 `options` 段：Linux 允许 `uart[8250],<options>,...`，本实现不消费
    // options 内容，只保证访问方式段能正确定位。
    let access = parts
        .next()
        .ok_or(EarlyUartConfigError::MissingAccessWidth)?;
    let (io_width, endian) = match access {
        "io" | "mmio" => (RegisterIoWidth::U8, RegisterEndian::Little),
        "mmio32" => (RegisterIoWidth::U32, RegisterEndian::Little),
        "mmio32be" => (RegisterIoWidth::U32, RegisterEndian::Big),
        // 该段必须是访问方式关键字；出现任何其它内容都视为缺少合法 access 段。
        _ => return Err(EarlyUartConfigError::MissingAccessWidth),
    };
    let address_text = parts.next().ok_or(EarlyUartConfigError::InvalidAddress)?;
    let phys_base = usize::from_str_radix(address_text.trim_start_matches("0x"), 16)
        .map_err(|_| EarlyUartConfigError::InvalidAddress)?;
    if phys_base == 0 {
        return Err(EarlyUartConfigError::ZeroAddress);
    }
    let baud = parts
        .next()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(115_200);
    if baud == 0 || fallback_clock_hz == 0 {
        return Err(EarlyUartConfigError::InvalidBaud);
    }

    let config = EarlyUartConfig {
        phys_base,
        clock_hz: fallback_clock_hz,
        baud,
        reg_offset: 0,
        reg_shift: 0,
        io_width,
        endian,
    };
    if config.divisor().is_none() {
        return Err(EarlyUartConfigError::InvalidBaud);
    }
    if !config.phys_base.is_multiple_of(io_width.bytes()) {
        return Err(EarlyUartConfigError::MisalignedRegister);
    }
    let last_offset = config
        .register_offset(UART_LSR_REGISTER)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let span = last_offset
        .checked_add(io_width.bytes())
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    config
        .phys_base
        .checked_add(span)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    Ok(config)
}

/// 按 16550 固定 16 倍过采样计算 16 位 divisor。
pub(crate) fn uart_divisor(clock_hz: u32, baud: u32) -> Option<u16> {
    let denominator = baud.checked_mul(UART_DIVISOR_OVERSAMPLE)?;
    if denominator == 0 {
        return None;
    }
    let divisor = clock_hz / denominator;
    if divisor == 0 || divisor > u32::from(u16::MAX) {
        None
    } else {
        Some(divisor as u16)
    }
}

fn path_depth(path: &str) -> Result<usize, EarlyUartConfigError> {
    let relative = path
        .strip_prefix('/')
        .ok_or(EarlyUartConfigError::InvalidPath)?;
    if relative.is_empty() {
        return Ok(0);
    }
    if relative.ends_with('/') || relative.split('/').any(str::is_empty) {
        return Err(EarlyUartConfigError::InvalidPath);
    }
    Ok(relative.split('/').count())
}

fn node_at_depth<'a>(
    fdt: Fdt<'a>,
    path: &str,
    depth: usize,
) -> Result<Node<'a>, EarlyUartConfigError> {
    let relative = path
        .strip_prefix('/')
        .ok_or(EarlyUartConfigError::InvalidPath)?;
    let mut node = fdt.root();
    for component in relative.split('/').take(depth) {
        if component.is_empty() {
            return Err(EarlyUartConfigError::InvalidPath);
        }
        node = find_path_child(node, component)?;
    }
    Ok(node)
}

fn find_path_child<'a>(node: Node<'a>, component: &str) -> Result<Node<'a>, EarlyUartConfigError> {
    let mut abbreviated = None;
    for child in node.children() {
        if child.name_bytes() == component.as_bytes() {
            return Ok(child);
        }
        if !component.contains('@') && child.base_name_bytes() == component.as_bytes() {
            if abbreviated.is_some() {
                return Err(EarlyUartConfigError::InvalidPath);
            }
            abbreviated = Some(child);
        }
    }
    abbreviated.ok_or(EarlyUartConfigError::InvalidPath)
}

fn node_is_available(node: Node<'_>) -> Result<bool, EarlyUartConfigError> {
    let Some(status) = node.property("status") else {
        return Ok(true);
    };
    let status = status
        .as_str()
        .map_err(|_| EarlyUartConfigError::InvalidProperty)?;
    Ok(matches!(status, "ok" | "okay"))
}

fn node_is_16550(node: Node<'_>) -> Result<bool, EarlyUartConfigError> {
    node_has_compatible(node, &["ns16550", "ns16550a"])
}

fn node_has_compatible(node: Node<'_>, expected: &[&str]) -> Result<bool, EarlyUartConfigError> {
    let Some(compatible) = node.property("compatible") else {
        return Ok(false);
    };
    let mut values = compatible
        .as_string_list()
        .map_err(|_| EarlyUartConfigError::InvalidProperty)?;
    Ok(values.any(|value| expected.contains(&value)))
}

fn first_reg(
    node: Node<'_>,
    parent: Node<'_>,
) -> Result<(u128, Option<u128>), EarlyUartConfigError> {
    let property = node
        .property("reg")
        .ok_or(EarlyUartConfigError::MissingReg)?;
    let address_cells = cell_count(parent, "#address-cells", DEFAULT_ADDRESS_CELLS)?;
    let size_cells = cell_count(parent, "#size-cells", DEFAULT_SIZE_CELLS)?;
    let stride = address_cells
        .checked_add(size_cells)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let stride_bytes = stride
        .checked_mul(4)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let value = property.value();
    if stride == 0 || value.len() < stride_bytes || !value.len().is_multiple_of(stride_bytes) {
        return Err(EarlyUartConfigError::InvalidProperty);
    }
    let (address, rest) = decode_cell_value(value, address_cells)?;
    let size = if size_cells == 0 {
        None
    } else {
        Some(decode_cell_value(rest, size_cells)?.0)
    };
    ensure_range_fits(address, size, address_cells)?;
    Ok((address, size))
}

fn translate_one_bus(
    bus: Node<'_>,
    parent: Node<'_>,
    address: u128,
    requested_size: Option<u128>,
) -> Result<u128, EarlyUartConfigError> {
    let ranges = bus
        .property("ranges")
        .ok_or(EarlyUartConfigError::MissingRanges)?;
    if ranges.value().is_empty() {
        ensure_range_fits(
            address,
            requested_size,
            cell_count(parent, "#address-cells", 2)?,
        )?;
        return Ok(address);
    }
    let child_cells = cell_count(bus, "#address-cells", DEFAULT_ADDRESS_CELLS)?;
    let parent_cells = cell_count(parent, "#address-cells", DEFAULT_ADDRESS_CELLS)?;
    let size_cells = cell_count(bus, "#size-cells", DEFAULT_SIZE_CELLS)?;
    let stride = child_cells
        .checked_add(parent_cells)
        .and_then(|value| value.checked_add(size_cells))
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let stride_bytes = stride
        .checked_mul(4)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let value = ranges.value();
    if stride == 0 || !value.len().is_multiple_of(stride_bytes) {
        return Err(EarlyUartConfigError::InvalidProperty);
    }

    for row in value.chunks_exact(stride_bytes) {
        let (child_base, row) = decode_cell_value(row, child_cells)?;
        let (parent_base, row) = decode_cell_value(row, parent_cells)?;
        let window_size = if size_cells == 0 {
            None
        } else {
            Some(decode_cell_value(row, size_cells)?.0)
        };
        ensure_range_fits(child_base, window_size, child_cells)?;
        ensure_range_fits(parent_base, window_size, parent_cells)?;
        if !mapping_contains(child_base, window_size, address, requested_size) {
            continue;
        }
        let delta = address
            .checked_sub(child_base)
            .ok_or(EarlyUartConfigError::AddressOverflow)?;
        let translated = parent_base
            .checked_add(delta)
            .ok_or(EarlyUartConfigError::AddressOverflow)?;
        ensure_range_fits(translated, requested_size, parent_cells)?;
        return Ok(translated);
    }
    Err(EarlyUartConfigError::UnmappedAddress)
}

fn mapping_contains(
    child_base: u128,
    window_size: Option<u128>,
    address: u128,
    requested_size: Option<u128>,
) -> bool {
    let Some(delta) = address.checked_sub(child_base) else {
        return false;
    };
    let Some(window_size) = window_size else {
        return delta == 0 && requested_size.is_none_or(|size| size == 0);
    };
    let requested_size = requested_size.unwrap_or(0);
    if requested_size == 0 {
        delta < window_size
    } else {
        delta < window_size && requested_size <= window_size - delta
    }
}

fn cell_count(
    node: Node<'_>,
    property: &str,
    default: usize,
) -> Result<usize, EarlyUartConfigError> {
    let count = match node.property(property) {
        Some(value) => usize::try_from(
            value
                .as_u32()
                .map_err(|_| EarlyUartConfigError::InvalidProperty)?,
        )
        .map_err(|_| EarlyUartConfigError::UnsupportedCellCount)?,
        None => default,
    };
    if count > MAX_SUPPORTED_CELLS {
        Err(EarlyUartConfigError::UnsupportedCellCount)
    } else {
        Ok(count)
    }
}

fn decode_cell_value(bytes: &[u8], cells: usize) -> Result<(u128, &[u8]), EarlyUartConfigError> {
    let byte_count = cells
        .checked_mul(4)
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    let (value, rest) = bytes
        .split_at_checked(byte_count)
        .ok_or(EarlyUartConfigError::InvalidProperty)?;
    let mut decoded = 0u128;
    for cell in value.chunks_exact(4) {
        decoded = decoded
            .checked_shl(32)
            .and_then(|value| {
                value.checked_add(u128::from(u32::from_be_bytes(
                    cell.try_into().expect("four-byte DT cell"),
                )))
            })
            .ok_or(EarlyUartConfigError::AddressOverflow)?;
    }
    Ok((decoded, rest))
}

fn ensure_range_fits(
    address: u128,
    size: Option<u128>,
    cells: usize,
) -> Result<(), EarlyUartConfigError> {
    let fits = match cells {
        0 => address == 0,
        1..=3 => {
            let limit = 1u128 << (cells * 32);
            address < limit && size.is_none_or(|size| size <= limit - address)
        }
        _ => size.is_none_or(|size| address.checked_add(size).is_some()),
    };
    if fits {
        Ok(())
    } else {
        Err(EarlyUartConfigError::AddressOverflow)
    }
}

fn uart_clock_hz(fdt: Fdt<'_>, uart: Node<'_>) -> Result<u32, EarlyUartConfigError> {
    if let Some(clock_hz) = optional_u32(uart, "clock-frequency")? {
        return (clock_hz != 0)
            .then_some(clock_hz)
            .ok_or(EarlyUartConfigError::InvalidClock);
    }
    let clocks = uart
        .property("clocks")
        .ok_or(EarlyUartConfigError::MissingClock)?;
    let index = baud_clock_index(uart)?;
    let provider = clock_provider_at(fdt, clocks.value(), index)?;
    resolve_fixed_clock_rate(fdt, provider, 0)
}

fn baud_clock_index(node: Node<'_>) -> Result<usize, EarlyUartConfigError> {
    let Some(names) = node.property("clock-names") else {
        return Ok(0);
    };
    let names = names
        .as_string_list()
        .map_err(|_| EarlyUartConfigError::InvalidProperty)?;
    let mut count = 0usize;
    let mut baud_index = None;
    for (index, name) in names.enumerate() {
        count = index + 1;
        if name == "baudclk" {
            baud_index = Some(index);
        }
    }
    baud_index
        .or_else(|| (count == 1).then_some(0))
        .ok_or(EarlyUartConfigError::InvalidClock)
}

fn clock_provider_at<'a>(
    fdt: Fdt<'a>,
    clocks: &[u8],
    wanted_index: usize,
) -> Result<Node<'a>, EarlyUartConfigError> {
    if clocks.is_empty() || !clocks.len().is_multiple_of(4) {
        return Err(EarlyUartConfigError::InvalidProperty);
    }
    let mut offset = 0usize;
    let mut index = 0usize;
    let mut selected = None;
    while offset < clocks.len() {
        let phandle = read_be_u32(&clocks[offset..])?;
        if phandle == 0 {
            return Err(EarlyUartConfigError::InvalidClock);
        }
        offset += 4;
        let provider = find_node_by_phandle(fdt, phandle)?;
        let argument_cells = required_cell_count(provider, "#clock-cells")?;
        let argument_bytes = argument_cells
            .checked_mul(4)
            .ok_or(EarlyUartConfigError::AddressOverflow)?;
        offset = offset
            .checked_add(argument_bytes)
            .filter(|end| *end <= clocks.len())
            .ok_or(EarlyUartConfigError::InvalidProperty)?;
        if index == wanted_index {
            selected = Some(provider);
        }
        index += 1;
    }
    selected.ok_or(EarlyUartConfigError::InvalidClock)
}

fn find_node_by_phandle<'a>(fdt: Fdt<'a>, wanted: u32) -> Result<Node<'a>, EarlyUartConfigError> {
    let mut found = None;
    for node in fdt.nodes() {
        if node_phandle(node)? != Some(wanted) {
            continue;
        }
        if found.is_some() {
            return Err(EarlyUartConfigError::InvalidClock);
        }
        found = Some(node);
    }
    found.ok_or(EarlyUartConfigError::InvalidClock)
}

fn node_phandle(node: Node<'_>) -> Result<Option<u32>, EarlyUartConfigError> {
    let primary = optional_u32(node, "phandle")?;
    let legacy = optional_u32(node, "linux,phandle")?;
    match (primary, legacy) {
        (Some(primary), Some(legacy)) if primary != legacy => {
            Err(EarlyUartConfigError::InvalidProperty)
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn resolve_fixed_clock_rate(
    fdt: Fdt<'_>,
    provider: Node<'_>,
    depth: usize,
) -> Result<u32, EarlyUartConfigError> {
    if depth >= MAX_FIXED_CLOCK_DEPTH || !node_is_available(provider)? {
        return Err(EarlyUartConfigError::InvalidClock);
    }
    if node_has_compatible(provider, &["fixed-clock"])? {
        let rate =
            optional_u32(provider, "clock-frequency")?.ok_or(EarlyUartConfigError::InvalidClock)?;
        return (rate != 0)
            .then_some(rate)
            .ok_or(EarlyUartConfigError::InvalidClock);
    }
    if node_has_compatible(provider, &["fixed-factor-clock"])? {
        let clocks = provider
            .property("clocks")
            .ok_or(EarlyUartConfigError::InvalidClock)?;
        let parent = clock_provider_at(fdt, clocks.value(), 0)?;
        let parent_rate = resolve_fixed_clock_rate(fdt, parent, depth + 1)?;
        let multiplier = optional_u32(provider, "clock-mult")?
            .filter(|value| *value != 0)
            .ok_or(EarlyUartConfigError::InvalidClock)?;
        let divisor = optional_u32(provider, "clock-div")?
            .filter(|value| *value != 0)
            .ok_or(EarlyUartConfigError::InvalidClock)?;
        let rate = u64::from(parent_rate)
            .checked_mul(u64::from(multiplier))
            .ok_or(EarlyUartConfigError::InvalidClock)?
            / u64::from(divisor);
        return u32::try_from(rate)
            .ok()
            .filter(|rate| *rate != 0)
            .ok_or(EarlyUartConfigError::InvalidClock);
    }
    Err(EarlyUartConfigError::InvalidClock)
}

fn required_cell_count(node: Node<'_>, property: &str) -> Result<usize, EarlyUartConfigError> {
    let value = optional_u32(node, property)?.ok_or(EarlyUartConfigError::InvalidProperty)?;
    usize::try_from(value).map_err(|_| EarlyUartConfigError::UnsupportedCellCount)
}

fn read_be_u32(bytes: &[u8]) -> Result<u32, EarlyUartConfigError> {
    let value = bytes
        .get(..4)
        .ok_or(EarlyUartConfigError::InvalidProperty)?;
    Ok(u32::from_be_bytes(
        value
            .try_into()
            .expect("four bytes were checked before DT u32 decode"),
    ))
}

fn optional_u32(node: Node<'_>, property: &str) -> Result<Option<u32>, EarlyUartConfigError> {
    node.property(property)
        .map(|value| {
            value
                .as_u32()
                .map_err(|_| EarlyUartConfigError::InvalidProperty)
        })
        .transpose()
}

fn strict_bool(node: Node<'_>, property: &str) -> Result<bool, EarlyUartConfigError> {
    match node.property(property) {
        None => Ok(false),
        Some(value) if value.value().is_empty() => Ok(true),
        Some(_) => Err(EarlyUartConfigError::InvalidProperty),
    }
}

fn register_endian(node: Node<'_>) -> Result<RegisterEndian, EarlyUartConfigError> {
    let big = strict_bool(node, "big-endian")?;
    let little = strict_bool(node, "little-endian")?;
    let native = strict_bool(node, "native-endian")?;
    if usize::from(big) + usize::from(little) + usize::from(native) > 1 {
        return Err(EarlyUartConfigError::ConflictingEndian);
    }
    if big || native && cfg!(target_endian = "big") {
        Ok(RegisterEndian::Big)
    } else {
        Ok(RegisterEndian::Little)
    }
}

fn baud_from_options(options: &str) -> Result<u32, EarlyUartConfigError> {
    let digits = options
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return Err(EarlyUartConfigError::InvalidBaud);
    }
    options[..digits]
        .parse::<u32>()
        .ok()
        .filter(|baud| *baud != 0)
        .ok_or(EarlyUartConfigError::InvalidBaud)
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use fdt::{Fdt, OwnedNode, OwnedTree};

    use super::*;

    fn cells(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    fn test_tree(
        path: &str,
        options: Option<&str>,
        serial: OwnedNode,
        nested: bool,
        extra_nodes: Vec<OwnedNode>,
    ) -> Vec<u8> {
        let mut root = OwnedNode::new("");
        root.set_property("#address-cells", cells(&[2]));
        root.set_property("#size-cells", cells(&[2]));

        let mut aliases = OwnedNode::new("aliases");
        let mut alias = path.as_bytes().to_vec();
        alias.push(0);
        aliases.set_property("serial0", alias);
        root.children.push(aliases);

        let mut chosen = OwnedNode::new("chosen");
        let mut stdout = b"serial0".to_vec();
        if let Some(options) = options {
            stdout.push(b':');
            stdout.extend_from_slice(options.as_bytes());
        }
        stdout.push(0);
        chosen.set_property("stdout-path", stdout);
        root.children.push(chosen);
        root.children.extend(extra_nodes);

        if nested {
            let mut soc = OwnedNode::new("soc@0");
            soc.set_property("#address-cells", cells(&[1]));
            soc.set_property("#size-cells", cells(&[1]));
            soc.set_property("ranges", cells(&[0, 0, 0x4000_0000, 0x1_0000]));
            soc.children.push(serial);
            root.children.push(soc);
        } else {
            root.children.push(serial);
        }

        OwnedTree {
            root,
            reservations: vec![],
            boot_cpuid_phys: None,
        }
        .to_dtb()
        .unwrap()
    }

    fn serial_node(name: &str, reg: &[u32]) -> OwnedNode {
        let mut serial = OwnedNode::new(name);
        serial.set_property("compatible", b"ns16550a\0".to_vec());
        serial.set_property("reg", cells(reg));
        serial.set_property("clock-frequency", cells(&[100_000_000]));
        serial
    }

    fn fixed_clock(name: &str, phandle: u32, rate: u32) -> OwnedNode {
        let mut clock = OwnedNode::new(name);
        clock.set_property("compatible", b"fixed-clock\0".to_vec());
        clock.set_property("#clock-cells", cells(&[0]));
        clock.set_property("phandle", cells(&[phandle]));
        clock.set_property("clock-frequency", cells(&[rate]));
        clock
    }

    fn fixed_factor_clock(
        name: &str,
        phandle: u32,
        parent: u32,
        multiplier: u32,
        divisor: u32,
    ) -> OwnedNode {
        let mut clock = OwnedNode::new(name);
        clock.set_property("compatible", b"fixed-factor-clock\0".to_vec());
        clock.set_property("#clock-cells", cells(&[0]));
        clock.set_property("phandle", cells(&[phandle]));
        clock.set_property("clocks", cells(&[parent]));
        clock.set_property("clock-mult", cells(&[multiplier]));
        clock.set_property("clock-div", cells(&[divisor]));
        clock
    }

    #[test]
    fn alias_options_and_register_layout_are_decoded_without_tree_allocation() {
        let mut serial = serial_node("serial@1fe001e0", &[0, 0x1fe0_01e0, 0, 0x100]);
        serial.set_property("current-speed", cells(&[230_400]));
        serial.set_property("reg-offset", cells(&[0x20]));
        serial.set_property("reg-shift", cells(&[2]));
        serial.set_property("reg-io-width", cells(&[4]));
        let blob = test_tree("/serial@1fe001e0", Some("57600n8"), serial, false, vec![]);

        let config = early_uart_config_from_fdt(Fdt::parse(&blob).unwrap()).unwrap();
        assert_eq!(config.phys_base, 0x1fe0_01e0);
        assert_eq!(config.clock_hz, 100_000_000);
        assert_eq!(config.baud, 57_600);
        assert_eq!(config.reg_offset, 0x20);
        assert_eq!(config.reg_shift, 2);
        assert_eq!(config.io_width, RegisterIoWidth::U32);
        assert_eq!(config.endian, RegisterEndian::Little);
        assert_eq!(config.register_offset(UART_LSR_REGISTER), Some(0x34));
        assert_eq!(config.divisor(), Some(108));
    }

    #[test]
    fn nested_simple_bus_ranges_and_current_speed_are_consumed() {
        let mut serial = serial_node("serial@1000", &[0x1000, 0x100]);
        serial.set_property("current-speed", cells(&[230_400]));
        let blob = test_tree("/soc@0/serial@1000", None, serial, true, vec![]);

        let config = early_uart_config_from_fdt(Fdt::parse(&blob).unwrap()).unwrap();
        assert_eq!(config.phys_base, 0x4000_1000);
        assert_eq!(config.baud, 230_400);
        assert_eq!(config.divisor(), Some(27));
    }

    #[test]
    fn fixed_clock_phandle_supplies_missing_uart_clock_frequency() {
        let mut serial = serial_node("serial@1fe001e0", &[0, 0x1fe0_01e0, 0, 0x100]);
        serial.remove_property("clock-frequency");
        serial.set_property("clocks", cells(&[1]));
        serial.set_property("clock-names", b"baudclk\0".to_vec());
        let blob = test_tree(
            "/serial@1fe001e0",
            None,
            serial,
            false,
            vec![fixed_clock("clock-24000000", 1, 24_000_000)],
        );

        let config = early_uart_config_from_fdt(Fdt::parse(&blob).unwrap()).unwrap();
        assert_eq!(config.clock_hz, 24_000_000);
        assert_eq!(config.divisor(), Some(13));
    }

    #[test]
    fn fixed_factor_clock_chain_is_resolved_without_allocation() {
        let mut serial = serial_node("serial@1fe001e0", &[0, 0x1fe0_01e0, 0, 0x100]);
        serial.remove_property("clock-frequency");
        serial.set_property("clocks", cells(&[2]));
        let blob = test_tree(
            "/serial@1fe001e0",
            None,
            serial,
            false,
            vec![
                fixed_clock("clock-24000000", 1, 24_000_000),
                fixed_factor_clock("uart-clock", 2, 1, 3, 2),
            ],
        );

        let config = early_uart_config_from_fdt(Fdt::parse(&blob).unwrap()).unwrap();
        assert_eq!(config.clock_hz, 36_000_000);
        assert_eq!(config.divisor(), Some(19));
    }

    #[test]
    fn malformed_layout_fails_instead_of_returning_partial_configuration() {
        let mut serial = serial_node("serial@1fe001e0", &[0, 0x1fe0_01e0, 0, 4]);
        serial.set_property("reg-io-width", cells(&[3]));
        let blob = test_tree("/serial@1fe001e0", None, serial, false, vec![]);
        assert_eq!(
            early_uart_config_from_fdt(Fdt::parse(&blob).unwrap()),
            Err(EarlyUartConfigError::InvalidIoWidth)
        );

        assert_eq!(uart_divisor(100_000_000, 0), None);
        assert_eq!(uart_divisor(1_000_000, 115_200), None);
        assert_eq!(uart_divisor(100_000_000, 115_200), Some(54));
    }

    #[test]
    fn register_endianness_flags_are_strict_and_preserved() {
        let mut serial = serial_node("serial@1fe001e0", &[0, 0x1fe0_01e0, 0, 0x100]);
        serial.set_property("reg-shift", cells(&[2]));
        serial.set_property("reg-io-width", cells(&[4]));
        serial.set_property("big-endian", vec![]);
        let blob = test_tree("/serial@1fe001e0", None, serial, false, vec![]);
        assert_eq!(
            early_uart_config_from_fdt(Fdt::parse(&blob).unwrap())
                .unwrap()
                .endian,
            RegisterEndian::Big
        );

        let mut conflicting = serial_node("serial@1fe001e0", &[0, 0x1fe0_01e0, 0, 0x100]);
        conflicting.set_property("big-endian", vec![]);
        conflicting.set_property("little-endian", vec![]);
        let blob = test_tree("/serial@1fe001e0", None, conflicting, false, vec![]);
        assert_eq!(
            early_uart_config_from_fdt(Fdt::parse(&blob).unwrap()),
            Err(EarlyUartConfigError::ConflictingEndian)
        );
    }

    #[test]
    fn cmdline_mmio32_address_and_default_baud_are_decoded() {
        let config =
            early_uart_config_from_cmdline("uart8250,mmio32,0x1fe20000", 100_000_000).unwrap();
        assert_eq!(config.phys_base, 0x1fe2_0000);
        assert_eq!(config.clock_hz, 100_000_000);
        assert_eq!(config.baud, 115_200);
        assert_eq!(config.reg_offset, 0);
        assert_eq!(config.reg_shift, 0);
        assert_eq!(config.io_width, RegisterIoWidth::U32);
        assert_eq!(config.endian, RegisterEndian::Little);
        assert_eq!(config.divisor(), Some(54));
    }

    #[test]
    fn cmdline_explicit_baud_overrides_default() {
        let config =
            early_uart_config_from_cmdline("uart,mmio,0x1fe20000,230400", 100_000_000).unwrap();
        assert_eq!(config.baud, 230_400);
        assert_eq!(config.io_width, RegisterIoWidth::U8);
        assert_eq!(config.divisor(), Some(27));
    }

    #[test]
    fn cmdline_mmio32be_sets_big_endian() {
        let config =
            early_uart_config_from_cmdline("uart8250,mmio32be,0x1fe20000", 100_000_000).unwrap();
        assert_eq!(config.io_width, RegisterIoWidth::U32);
        assert_eq!(config.endian, RegisterEndian::Big);
    }

    #[test]
    fn cmdline_address_without_0x_prefix_is_accepted() {
        let config = early_uart_config_from_cmdline("uart,mmio,1fe20000", 100_000_000).unwrap();
        assert_eq!(config.phys_base, 0x1fe2_0000);
    }

    #[test]
    fn cmdline_malformed_values_fail_strictly() {
        assert_eq!(
            early_uart_config_from_cmdline("ns16550a,mmio32,0x1fe20000", 100_000_000),
            Err(EarlyUartConfigError::UnsupportedUart)
        );
        assert_eq!(
            early_uart_config_from_cmdline("uart8250,0x1fe20000", 100_000_000),
            Err(EarlyUartConfigError::MissingAccessWidth)
        );
        assert_eq!(
            early_uart_config_from_cmdline("uart8250,mmio32", 100_000_000),
            Err(EarlyUartConfigError::InvalidAddress)
        );
        assert_eq!(
            early_uart_config_from_cmdline("uart8250,mmio32,xyz", 100_000_000),
            Err(EarlyUartConfigError::InvalidAddress)
        );
        assert_eq!(
            early_uart_config_from_cmdline("uart8250,mmio32,0x0", 100_000_000),
            Err(EarlyUartConfigError::ZeroAddress)
        );
        assert_eq!(
            early_uart_config_from_cmdline("uart8250,mmio32,0x1fe20000,0", 100_000_000),
            Err(EarlyUartConfigError::InvalidBaud)
        );
        assert_eq!(
            early_uart_config_from_cmdline("uart8250,mmio16,0x1fe20000", 100_000_000),
            Err(EarlyUartConfigError::MissingAccessWidth)
        );
    }
}
