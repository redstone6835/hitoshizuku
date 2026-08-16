//! StarFive JH7110 pinctrl / GPIO 平台驱动。
//!
//! 实现与 Linux pinctrl-starfive-jh7110 一致的 pinmux 语义：
//! - pinmux 值编码 | din(31:24) | dout(23:16) | doen(15:10) | func(9:8) | pin(7:0) |
//! - sys 域 GPIO 寄存器：DOEN=0x000、DOUT=0x040、GPI=0x080，每 4 pin 一个 u32；
//! - 功能选择寄存器按引脚查表（0x29c..0x2b0 区间，2-3 bit 每 pin）；
//! - padcfg（bias/驱动强度/输入使能/施密特/斜率）按 pin 位于 0x120 或 0x284 基址。
//!
//! 每个带 phandle 的 pin 配置子节点（如 uart0-0）注册独立 Pinctrl provider，
//! consumer 通过 pinctrl-N 引用拿到配置并按 Configure 请求应用。AON 域
//! pinctrl 仅注册占位 provider（引脚由固件配置）。

use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::dev::dt_provider::{
    self, DtbProvider, DtbProviderError, DtbProviderKey, DtbProviderKind, DtbResource,
    DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_SYS_PINCTRL: &str = "starfive,jh7110-sys-pinctrl";
const COMPAT_AON_PINCTRL: &str = "starfive,jh7110-aon-pinctrl";

// sys GPIO 寄存器基址（相对 pinctrl MMIO 基址）。
const SYS_DOEN: usize = 0x000;
const SYS_DOUT: usize = 0x040;
const SYS_GPI: usize = 0x080;
const SYS_PADCFG_A: usize = 0x120; // pin 0..74
const SYS_PADCFG_B: usize = 0x284; // pin 89..94

const PADCFG_IE: u32 = 1 << 0;
const PADCFG_DS_MASK: u32 = 0b11 << 1;
const PADCFG_PU: u32 = 1 << 3;
const PADCFG_PD: u32 = 1 << 4;
const PADCFG_SLEW: u32 = 1 << 5;
const PADCFG_SMT: u32 = 1 << 6;

/// pinmux 值中的 din 哨兵：不接 GPIO 输入。
const PINMUX_DIN_NONE: u32 = 0xff;

/// 功能选择表项：寄存器偏移、位偏移、最大功能号。0 偏移表示无表项。
#[derive(Clone, Copy)]
struct FuncSel {
    offset: u16,
    shift: u8,
    max: u8,
}

const NO_FUNC_SEL: FuncSel = FuncSel { offset: 0, shift: 0, max: 0 };

/// sys 域功能选择表（按 pin 编号索引，来源 Linux pinctrl-starfive-jh7110-sys.c）。
fn sys_func_sel(pin: u32) -> FuncSel {
    const TABLE: &[(u32, FuncSel)] = &[
        (6, FuncSel { offset: 0x2b0, shift: 0, max: 3 }),
        (7, FuncSel { offset: 0x2b0, shift: 2, max: 3 }),
        (8, FuncSel { offset: 0x2b0, shift: 5, max: 3 }),
        (9, FuncSel { offset: 0x2b0, shift: 8, max: 3 }),
        (10, FuncSel { offset: 0x29c, shift: 2, max: 3 }),
        (11, FuncSel { offset: 0x29c, shift: 5, max: 3 }),
        (12, FuncSel { offset: 0x29c, shift: 8, max: 3 }),
        (13, FuncSel { offset: 0x29c, shift: 11, max: 3 }),
        (14, FuncSel { offset: 0x29c, shift: 14, max: 3 }),
        (15, FuncSel { offset: 0x29c, shift: 17, max: 3 }),
        (16, FuncSel { offset: 0x29c, shift: 20, max: 3 }),
        (17, FuncSel { offset: 0x29c, shift: 23, max: 3 }),
        (18, FuncSel { offset: 0x29c, shift: 26, max: 3 }),
        (19, FuncSel { offset: 0x29c, shift: 29, max: 3 }),
        (20, FuncSel { offset: 0x2a0, shift: 0, max: 3 }),
        (21, FuncSel { offset: 0x2a0, shift: 3, max: 3 }),
        (22, FuncSel { offset: 0x2a0, shift: 6, max: 3 }),
        (23, FuncSel { offset: 0x2a0, shift: 9, max: 3 }),
        (24, FuncSel { offset: 0x2a0, shift: 12, max: 3 }),
        (25, FuncSel { offset: 0x2a0, shift: 15, max: 3 }),
        (26, FuncSel { offset: 0x2a0, shift: 18, max: 3 }),
        (27, FuncSel { offset: 0x2a0, shift: 21, max: 3 }),
        (28, FuncSel { offset: 0x2a0, shift: 24, max: 3 }),
        (29, FuncSel { offset: 0x2a0, shift: 27, max: 3 }),
        (30, FuncSel { offset: 0x2a4, shift: 0, max: 3 }),
        (31, FuncSel { offset: 0x2a4, shift: 3, max: 3 }),
        (32, FuncSel { offset: 0x2a4, shift: 6, max: 3 }),
        (33, FuncSel { offset: 0x2a4, shift: 9, max: 3 }),
        (34, FuncSel { offset: 0x2a4, shift: 12, max: 3 }),
        (35, FuncSel { offset: 0x2a4, shift: 15, max: 3 }),
        (36, FuncSel { offset: 0x2a4, shift: 17, max: 3 }),
        (37, FuncSel { offset: 0x2a4, shift: 20, max: 3 }),
        (38, FuncSel { offset: 0x2a4, shift: 23, max: 3 }),
        (39, FuncSel { offset: 0x2a4, shift: 26, max: 3 }),
        (40, FuncSel { offset: 0x2a4, shift: 29, max: 3 }),
        (41, FuncSel { offset: 0x2a8, shift: 0, max: 3 }),
        (42, FuncSel { offset: 0x2a8, shift: 3, max: 3 }),
        (43, FuncSel { offset: 0x2a8, shift: 6, max: 3 }),
        (44, FuncSel { offset: 0x2a8, shift: 9, max: 3 }),
        (45, FuncSel { offset: 0x2a8, shift: 12, max: 3 }),
        (46, FuncSel { offset: 0x2a8, shift: 15, max: 3 }),
        (47, FuncSel { offset: 0x2a8, shift: 18, max: 3 }),
        (48, FuncSel { offset: 0x2a8, shift: 21, max: 3 }),
        (49, FuncSel { offset: 0x2a8, shift: 24, max: 3 }),
        (50, FuncSel { offset: 0x2a8, shift: 27, max: 3 }),
        (51, FuncSel { offset: 0x2a8, shift: 30, max: 3 }),
        (52, FuncSel { offset: 0x2ac, shift: 0, max: 3 }),
        (53, FuncSel { offset: 0x2ac, shift: 2, max: 3 }),
        (54, FuncSel { offset: 0x2ac, shift: 4, max: 3 }),
        (55, FuncSel { offset: 0x2ac, shift: 6, max: 3 }),
        (56, FuncSel { offset: 0x2ac, shift: 9, max: 3 }),
        (57, FuncSel { offset: 0x2ac, shift: 12, max: 3 }),
        (58, FuncSel { offset: 0x2ac, shift: 15, max: 3 }),
        (59, FuncSel { offset: 0x2ac, shift:18, max: 3 }),
        (60, FuncSel { offset: 0x2ac, shift: 21, max: 3 }),
        (61, FuncSel { offset: 0x2ac, shift: 24, max: 3 }),
        (62, FuncSel { offset: 0x2ac, shift: 27, max: 3 }),
        (63, FuncSel { offset: 0x2ac, shift: 30, max: 3 }),
        (82, FuncSel { offset: 0x29c, shift: 0, max: 1 }), // GMAC1_RXC
    ];
    TABLE
        .iter()
        .find(|(entry_pin, _)| *entry_pin == pin)
        .map(|(_, sel)| *sel)
        .unwrap_or(NO_FUNC_SEL)
}

/// 一个 pin 配置子节点的应用描述。
#[derive(Clone)]
struct PinConfig {
    /// 原始 pinmux u32 值列表。
    pinmux: Vec<u32>,
    /// 附加 padcfg 属性。
    bias_disable: bool,
    bias_pull_up: bool,
    bias_pull_down: bool,
    input_enable: bool,
    input_schmitt_enable: bool,
    drive_strength_ma: Option<u32>,
    slew_rate: Option<u32>,
}

impl PinConfig {
    const fn empty() -> Self {
        Self {
            pinmux: Vec::new(),
            bias_disable: false,
            bias_pull_up: false,
            bias_pull_down: false,
            input_enable: false,
            input_schmitt_enable: false,
            drive_strength_ma: None,
            slew_rate: None,
        }
    }
}

/// 应用 pin 配置的 DT 资源。base=None 时仅接受不写寄存器（AON 占位）。
struct PinConfigResource {
    base: Option<usize>,
    config: Arc<PinConfig>,
}

impl PinConfigResource {
    fn read32(base: usize, offset: usize) -> u32 {
        // Safety: probe 已按 DT reg 窗口校验 MMIO 范围，寄存器访问在窗口内。
        unsafe { core::ptr::read_volatile(base.wrapping_add(offset) as *const u32) }
    }

    fn write32(base: usize, offset: usize, value: u32) {
        // Safety: 同 read32。
        unsafe { core::ptr::write_volatile(base.wrapping_add(offset) as *mut u32, value) }
    }

    /// 4 pin 一组的读改写。
    fn rmw_field(base: usize, reg_base: usize, pin: u32, bits: u32, value: u32) {
        let offset = 4 * (pin as usize / 4);
        let shift = 8 * (pin % 4);
        let mask = bits << shift;
        let reg = Self::read32(base, reg_base + offset);
        Self::write32(base, reg_base + offset, (reg & !mask) | ((value << shift) & mask));
    }

    fn padcfg_base(pin: u32) -> Option<usize> {
        match pin {
            0..=74 => Some(SYS_PADCFG_A),
            89..=94 => Some(SYS_PADCFG_B),
            _ => None,
        }
    }

    fn apply_pin(&self, base: usize, value: u32) {
        let pin = value & 0xff;
        let func = (value >> 8) & 0x3;
        let dout = (value >> 16) & 0xff;
        let doen = (value >> 10) & 0x3f;
        let din = (value >> 24) & 0xff;

        if pin < 64 && func == 0 {
            // GPIO 输出路径：DOUT 7bit / DOEN 6bit / 可选 GPI 输入路由。
            Self::rmw_field(base, SYS_DOUT, pin, 0x7f, dout);
            Self::rmw_field(base, SYS_DOEN, pin, 0x3f, doen);
            if din != PINMUX_DIN_NONE {
                Self::rmw_field(base, SYS_GPI, din, 0x7f, pin + 2);
            }
        }
        let sel = sys_func_sel(pin);
        if sel.offset != 0 && func <= u32::from(sel.max) {
            let offset = sel.offset as usize;
            let shift = sel.shift as u32;
            let mask = 0x3u32 << shift;
            let reg = Self::read32(base, offset);
            Self::write32(base, offset, (reg & !mask) | (func << shift));
        }
    }

    fn apply_padcfg(&self, base: usize, pin: u32) {
        let Some(padcfg_base) = Self::padcfg_base(pin) else {
            return;
        };
        let offset = padcfg_base + 4 * pin as usize;
        let mut cfg = Self::read32(base, offset);
        if self.config.bias_disable {
            cfg &= !(PADCFG_PU | PADCFG_PD);
        }
        if self.config.bias_pull_up {
            cfg = (cfg & !PADCFG_PD) | PADCFG_PU;
        }
        if self.config.bias_pull_down {
            cfg = (cfg & !PADCFG_PU) | PADCFG_PD;
        }
        if self.config.input_enable {
            cfg |= PADCFG_IE;
        }
        if self.config.input_schmitt_enable {
            cfg |= PADCFG_SMT;
        }
        if let Some(ma) = self.config.drive_strength_ma {
            let code = match ma {
                0..=2 => 0,
                3..=4 => 1,
                5..=8 => 2,
                _ => 3,
            };
            cfg = (cfg & !PADCFG_DS_MASK) | ((code as u32) << 1);
        }
        if let Some(slew) = self.config.slew_rate {
            if slew == 0 {
                cfg &= !PADCFG_SLEW;
            } else {
                cfg |= PADCFG_SLEW;
            }
        }
        Self::write32(base, offset, cfg);
    }

    fn apply(&self) {
        let Some(base) = self.base else {
            return;
        };
        for &value in &self.config.pinmux {
            let pin = value & 0xff;
            self.apply_pin(base, value);
            self.apply_padcfg(base, pin);
        }
    }
}

impl DtbResource for PinConfigResource {
    fn control(&self, request: DtbResourceRequest<'_>) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Configure(_) => {
                self.apply();
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Enable | DtbResourceRequest::Disable => {
                Ok(DtbResourceReply::Done)
            }
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

/// 每个 pin 配置子节点一个 provider（acquire 返回同一配置资源）。
struct PinConfigProvider {
    resource: Arc<PinConfigResource>,
}

impl DtbProvider for PinConfigProvider {
    fn acquire(&self, _specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        Ok(self.resource.clone())
    }
}

fn be32(bytes: &[u8]) -> Option<u32> {
    let array: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(array))
}

fn be32_list(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks_exact(4).filter_map(be32).collect()
}

struct JhPinctrlDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl JhPinctrlDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self { device_mmio_to_virt }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_SYS_PINCTRL) || info.has_id(COMPAT_AON_PINCTRL)
    }
}

impl PnpDriver for JhPinctrlDriver {
    fn name(&self) -> &'static str { "platform-jh7110-pinctrl" }

    fn bus_type(&self) -> BusType { BusType::PLATFORM }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info.as_any().downcast_ref::<PlatformDeviceInfo>().is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &alloc::sync::Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let is_aon = info.has_id(COMPAT_AON_PINCTRL);

        // sys 域需要 MMIO；AON 占位不要求。
        let base = if is_aon {
            None
        } else {
            let (phys, _size) = info.first_mmio().ok_or(PnpError::missing(
                PnpResourceKind::Mmio,
                "sys pinctrl missing reg",
            ))?;
            Some((self.device_mmio_to_virt)(phys))
        };

        let controller_phandle = info.properties.fw_phandle;

        // 枚举 pin 配置：owned nodes 是平铺的后代节点列表。pinmux 属性位于
        // 配置节点的子组（如 uart0-0/tx-pins），需要沿 parent_path 攀爬找到
        // 最近的带 phandle 的祖先（即 pinctrl-0 引用指向的配置节点）归组。
        let children: Vec<(u32, Arc<PinConfigResource>)> = info
            .dtb_owned_nodes()
            .map(|nodes| {
                // 按路径索引节点，便于 parent_path 攀爬。
                let mut merged: Vec<(u32, PinConfig)> = Vec::new();
                for node in nodes.iter().filter(|node| {
                    node.properties
                        .iter()
                        .any(|property| property.name.as_ref() == "pinmux")
                }) {
                    // 攀爬祖先链找到配置节点 phandle。
                    let mut cursor = Some(node);
                    let mut owner_phandle = None;
                    while let Some(current) = cursor {
                        if current.phandle.is_some() {
                            owner_phandle = current.phandle;
                            break;
                        }
                        cursor = current.parent_path.as_deref().and_then(|parent_path| {
                            nodes.iter().find(|candidate| candidate.path.as_ref() == parent_path)
                        });
                    }
                    let Some(phandle) = owner_phandle else {
                        continue;
                    };
                    let config = match merged
                        .iter_mut()
                        .find(|(existing, _)| *existing == phandle)
                    {
                        Some((_, config)) => config,
                        None => {
                            merged.push((phandle, PinConfig::empty()));
                            &mut merged.last_mut().expect("just pushed").1
                        }
                    };
                    for property in &node.properties {
                        match property.name.as_ref() {
                            "pinmux" => config.pinmux.extend(be32_list(&property.value)),
                            "bias-disable" => config.bias_disable = true,
                            "bias-pull-up" => config.bias_pull_up = true,
                            "bias-pull-down" => config.bias_pull_down = true,
                            "input-enable" => config.input_enable = true,
                            "input-schmitt-enable" => config.input_schmitt_enable = true,
                            "drive-strength" => config.drive_strength_ma = be32(&property.value),
                            "slew-rate" => config.slew_rate = be32(&property.value),
                            _ => {}
                        }
                    }
                }
                merged
                    .into_iter()
                    .map(|(phandle, config)| {
                        (
                            phandle,
                            Arc::new(PinConfigResource { base, config: Arc::new(config) }),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        dev.reserve_owned_resources(1 + children.len())?;

        // 控制器级 provider（兼容直接引用控制器 phandle 的消费者）。
        // 控制器节点可能未被引用而没有 phandle，此时只注册子配置 provider。
        if let Some(controller_phandle) = controller_phandle {
            let ctrl_key = DtbProviderKey::new(DtbProviderKind::Pinctrl, controller_phandle);
            let ctrl_resource = Arc::new(PinConfigResource {
                base,
                config: Arc::new(PinConfig::empty()),
            });
            let ctrl_handle = dt_provider::register(
                ctrl_key,
                Arc::new(PinConfigProvider { resource: Arc::clone(&ctrl_resource) }),
            )
            .map_err(DtbProviderError::into_pnp_error)?;
            if let Err(err) = dev.own_resource(dt_provider::provider_pnp_resource(
                ctrl_handle,
                "jh7110-pinctrl",
            )) {
                let _ = dt_provider::unregister(ctrl_handle);
                return Err(err);
            }
        }

        // 每个配置子节点注册独立 provider。
        let mut registered = 0usize;
        for (phandle, resource) in children {
            let key = DtbProviderKey::new(DtbProviderKind::Pinctrl, phandle);
            let Ok(handle) = dt_provider::register(
                key,
                Arc::new(PinConfigProvider { resource }),
            )
            .map_err(DtbProviderError::into_pnp_error)
            else {
                continue;
            };
            if dev
                .own_resource(dt_provider::provider_pnp_resource(handle, "jh7110-pinctrl-state"))
                .is_ok()
            {
                registered += 1;
            }
        }

        log::printk!("[jh7110-pinctrl] {} registered phandle={:?} states={} mmio={:?}",
            if is_aon { "aon" } else { "sys" },
            controller_phandle,
            registered,
            base,
        );
        Ok(())
    }

    fn remove(&self, _dev: &alloc::sync::Arc<PnpDevice>) {
        log::printk!("[jh7110-pinctrl] removed");
    }
}

struct JhPinctrlFactory;

impl DriverFactory for JhPinctrlFactory {
    fn name(&self) -> &'static str { "platform-jh7110-pinctrl" }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(JhPinctrlDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(JhPinctrlFactory))
}