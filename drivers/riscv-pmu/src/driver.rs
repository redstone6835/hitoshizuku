//! RISC-V 固件 PMU platform ELM 驱动。
//!
//! `riscv,event-to-mhpmcounters` 使用三-cell 矩阵描述一段 SBI hardware event
//! 可以使用的逻辑 counter 位图。本驱动把约束注册到通用 PMU 层；架构层安装的
//! SBI backend 负责真实 counter 的配置、启动、停止以及 CSR/firmware 读取。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pmu::{self, PmuDescriptor, PmuError, PmuEventCounterRange};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_RISCV_PMU: &str = "riscv,pmu";
const EVENT_COUNTER_PROPERTY: &str = "riscv,event-to-mhpmcounters";
/// SBI hardware raw event 的起始 index；本属性按 binding 不能包含 raw event。
const SBI_PMU_EVENT_RAW_INDEX: u32 = 0x0002_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RiscvPmuMapError {
    InvalidEncoding,
    RawEvent,
    OutOfMemory,
}

fn parse_event_counter_ranges(
    info: &PlatformDeviceInfo,
) -> Result<Vec<PmuEventCounterRange>, RiscvPmuMapError> {
    let Some(raw) = info.bytes_property(EVENT_COUNTER_PROPERTY) else {
        return Ok(Vec::new());
    };
    if raw.is_empty() {
        return Err(RiscvPmuMapError::InvalidEncoding);
    }
    let values = info
        .u32_list_property(EVENT_COUNTER_PROPERTY)
        .ok_or(RiscvPmuMapError::InvalidEncoding)?;
    let mut cells = Vec::new();
    cells
        .try_reserve(values.len())
        .map_err(|_| RiscvPmuMapError::OutOfMemory)?;
    cells.extend(values);
    let ranges = pmu::decode_event_counter_ranges(&cells).map_err(map_decode_error)?;
    if ranges
        .iter()
        .any(|range| range.last_event() >= SBI_PMU_EVENT_RAW_INDEX)
    {
        return Err(RiscvPmuMapError::RawEvent);
    }
    Ok(ranges)
}

fn map_decode_error(error: PmuError) -> RiscvPmuMapError {
    match error {
        PmuError::OutOfMemory => RiscvPmuMapError::OutOfMemory,
        _ => RiscvPmuMapError::InvalidEncoding,
    }
}

fn map_registration_error(error: PmuError) -> PnpError {
    match error {
        PmuError::OutOfMemory => PnpError::OutOfMemory,
        PmuError::Invalid | PmuError::InvalidEncoding | PmuError::OverlappingRanges => {
            PnpError::malformed(PnpResourceKind::Other("pmu"), "invalid PMU event map")
        }
        PmuError::AlreadyRegistered
        | PmuError::NotFound
        | PmuError::NoBackend
        | PmuError::Unsupported
        | PmuError::Busy
        | PmuError::WrongCpu
        | PmuError::AlreadyRunning
        | PmuError::NotRunning
        | PmuError::Backend(_) => PnpError::registration_failed(
            PnpResourceKind::Other("pmu"),
            "PMU registry rejected device",
        ),
    }
}

struct RiscvPmuPlatformDriver;

impl RiscvPmuPlatformDriver {
    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_RISCV_PMU)
    }
}

impl PnpDriver for RiscvPmuPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-riscv-pmu"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        info.as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let ranges = parse_event_counter_ranges(info).map_err(|error| match error {
            RiscvPmuMapError::OutOfMemory => PnpError::OutOfMemory,
            RiscvPmuMapError::InvalidEncoding => PnpError::malformed(
                PnpResourceKind::Other("pmu"),
                "riscv,event-to-mhpmcounters is not a valid three-cell matrix",
            ),
            RiscvPmuMapError::RawEvent => PnpError::malformed(
                PnpResourceKind::Other("pmu"),
                "riscv,event-to-mhpmcounters contains a raw event",
            ),
        })?;
        let range_count = ranges.len();
        let descriptor = PmuDescriptor::new("riscv-pmu".into(), info.fw_path.clone(), ranges)
            .map_err(map_registration_error)?;
        dev.reserve_owned_resources(1)?;
        let handle = pmu::register(descriptor).map_err(map_registration_error)?;
        if let Err(error) = dev.own_resource(pmu::pnp_resource(handle, "platform-riscv-pmu")) {
            let _ = pmu::unregister(handle);
            return Err(error);
        }
        log::printk!(
            "[platform-riscv-pmu] bound {} path={} event-ranges={}",
            dev.id,
            info.fw_path.as_deref().unwrap_or("<none>"),
            range_count
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        log::printk!("[platform-riscv-pmu] removed {}", dev.id);
    }
}

struct RiscvPmuFactory;

impl DriverFactory for RiscvPmuFactory {
    fn name(&self) -> &'static str {
        "platform-riscv-pmu"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(RiscvPmuPlatformDriver))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(RiscvPmuFactory))
}
