//! 标准 Device Tree 固定 clock provider 的 ELM 驱动。
//!
//! 本模块把 `fixed-clock` 与 `fixed-factor-clock` 节点接入通用 DT provider
//! registry。provider 只接受由 `#clock-cells = <0>` 定义的空 specifier；固定
//! 比例 clock 的父 lease 与自身注册句柄都交给 PnP 设备管理，保证 probe 回滚、
//! overlay 移除和 ELM 卸载使用同一套逆序释放路径。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::mem::size_of;

use crate::dev::dt_provider::{
    self, DtbProvider, DtbProviderError, DtbProviderKey, DtbProviderKind, DtbResource,
    DtbResourceLease, DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResource, PnpResourceKind, PnpResourceReleaseError,
    PnpResourceReleaseOrder, register_driver_factory,
};

const COMPAT_FIXED_CLOCK: &str = "fixed-clock";
const COMPAT_FIXED_FACTOR_CLOCK: &str = "fixed-factor-clock";
const PROP_CLOCK_CELLS: &str = "#clock-cells";
const PROP_CLOCK_FREQUENCY: &str = "clock-frequency";
const PROP_CLOCK_MULT: &str = "clock-mult";
const PROP_CLOCK_DIV: &str = "clock-div";
const PROP_CLOCKS: &str = "clocks";
const CLOCK_RESOURCE_KIND: PnpResourceKind = PnpResourceKind::Other("clock");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixedClockKind {
    Rate,
    Factor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FixedFactorConfig {
    mult: u32,
    div: u32,
}

struct FixedClockResource {
    rate: u64,
}

impl DtbResource for FixedClockResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable | DtbResourceRequest::Disable => Ok(DtbResourceReply::Done),
            DtbResourceRequest::GetRate => Ok(DtbResourceReply::Value(self.rate)),
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

struct FixedClockProvider {
    resource: Arc<FixedClockResource>,
}

impl FixedClockProvider {
    fn new(rate: u64) -> Self {
        Self {
            resource: Arc::new(FixedClockResource { rate }),
        }
    }
}

impl DtbProvider for FixedClockProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        if !specifier.is_empty() {
            return Err(DtbProviderError::AcquireFailed);
        }
        Ok(self.resource.clone())
    }
}

/// 父 lease 被 provider 与 PnP 资源共同持有。provider handle 会先于 PnP 侧引用
/// 释放；在注销成功前，父 provider 因 lease 仍保持 busy，不能被提前卸载。
struct UpstreamClock {
    lease: DtbResourceLease,
}

impl UpstreamClock {
    fn done(&self, request: DtbResourceRequest<'_>) -> Result<DtbResourceReply, DtbProviderError> {
        match self.lease.control(request)? {
            DtbResourceReply::Done => Ok(DtbResourceReply::Done),
            _ => Err(DtbProviderError::HardwareFailure),
        }
    }

    fn rate(&self) -> Result<u64, DtbProviderError> {
        match self.lease.control(DtbResourceRequest::GetRate)? {
            DtbResourceReply::Value(rate) => Ok(rate),
            _ => Err(DtbProviderError::HardwareFailure),
        }
    }
}

struct SharedUpstreamClockPnpResource {
    upstream: Option<Arc<UpstreamClock>>,
}

impl SharedUpstreamClockPnpResource {
    fn new(upstream: Arc<UpstreamClock>) -> Self {
        Self {
            upstream: Some(upstream),
        }
    }
}

impl PnpResource for SharedUpstreamClockPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("dt-provider-lease")
    }

    fn label(&self) -> &'static str {
        "fixed-factor-parent-clock"
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        self.upstream
            .as_ref()
            .ok_or_else(|| {
                PnpResourceReleaseError::new(
                    self.kind(),
                    self.label(),
                    "upstream clock lease was already released",
                )
            })?
            .lease
            .prepare_pnp_release()
            .map_err(|_| {
                PnpResourceReleaseError::new(
                    self.kind(),
                    self.label(),
                    "upstream clock lease cannot be frozen",
                )
            })
    }

    fn cancel_release(&self) {
        if let Some(upstream) = self.upstream.as_ref() {
            upstream.lease.cancel_pnp_release();
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Consumer
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        drop(self.upstream.take());
        Ok(())
    }
}

struct FixedFactorClockResource {
    upstream: Arc<UpstreamClock>,
    config: FixedFactorConfig,
}

impl DtbResource for FixedFactorClockResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable => self.upstream.done(DtbResourceRequest::Enable),
            DtbResourceRequest::Disable => self.upstream.done(DtbResourceRequest::Disable),
            DtbResourceRequest::GetRate => {
                fixed_factor_rate(self.upstream.rate()?, self.config.mult, self.config.div)
                    .map(DtbResourceReply::Value)
            }
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

struct FixedFactorClockProvider {
    resource: Arc<FixedFactorClockResource>,
}

impl FixedFactorClockProvider {
    fn new(upstream: Arc<UpstreamClock>, config: FixedFactorConfig) -> Self {
        Self {
            resource: Arc::new(FixedFactorClockResource { upstream, config }),
        }
    }
}

impl DtbProvider for FixedFactorClockProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        if !specifier.is_empty() {
            return Err(DtbProviderError::AcquireFailed);
        }
        Ok(self.resource.clone())
    }
}

fn fixed_factor_rate(parent: u64, mult: u32, div: u32) -> Result<u64, DtbProviderError> {
    if div == 0 {
        return Err(DtbProviderError::Invalid);
    }
    let mult = u64::from(mult);
    let div = u64::from(div);
    // parent = quotient * div + remainder；先拆商余可保持 Linux 的
    // floor(parent * mult / div) 语义，同时避免为 ELM 引入 128 位除法运行时。
    let quotient = parent / div;
    let remainder = parent % div;
    quotient
        .checked_mul(mult)
        .and_then(|whole| {
            remainder
                .checked_mul(mult)
                .map(|fraction| fraction / div)
                .and_then(|fraction| whole.checked_add(fraction))
        })
        .ok_or(DtbProviderError::HardwareFailure)
}

fn fixed_clock_kind(info: &PlatformDeviceInfo) -> Result<FixedClockKind, PnpError> {
    match (
        info.has_id(COMPAT_FIXED_CLOCK),
        info.has_id(COMPAT_FIXED_FACTOR_CLOCK),
    ) {
        (true, false) => Ok(FixedClockKind::Rate),
        (false, true) => Ok(FixedClockKind::Factor),
        (true, true) => Err(PnpError::malformed(
            CLOCK_RESOURCE_KIND,
            "clock node declares incompatible fixed clock bindings",
        )),
        (false, false) => Err(PnpError::unsupported("fixed DT clock binding")),
    }
}

fn validate_clock_cells(info: &PlatformDeviceInfo) -> Result<(), PnpError> {
    let cells = required_u32_property(
        info,
        PROP_CLOCK_CELLS,
        "fixed clock is missing #clock-cells",
        "fixed clock #clock-cells must contain one u32 cell",
    )?;
    if cells != 0 {
        return Err(PnpError::malformed(
            CLOCK_RESOURCE_KIND,
            "fixed clock #clock-cells must be zero",
        ));
    }
    Ok(())
}

fn fixed_clock_rate(info: &PlatformDeviceInfo) -> Result<u64, PnpError> {
    validate_clock_cells(info)?;
    required_u32_property(
        info,
        PROP_CLOCK_FREQUENCY,
        "fixed-clock is missing clock-frequency",
        "fixed-clock clock-frequency must contain one u32 cell",
    )
    .map(u64::from)
}

fn fixed_factor_config(
    info: &PlatformDeviceInfo,
    provider_phandle: u32,
) -> Result<FixedFactorConfig, PnpError> {
    validate_clock_cells(info)?;
    let mult = required_u32_property(
        info,
        PROP_CLOCK_MULT,
        "fixed-factor-clock is missing clock-mult",
        "fixed-factor-clock clock-mult must contain one u32 cell",
    )?;
    let div = required_u32_property(
        info,
        PROP_CLOCK_DIV,
        "fixed-factor-clock is missing clock-div",
        "fixed-factor-clock clock-div must contain one u32 cell",
    )?;
    if div == 0 {
        return Err(PnpError::malformed(
            CLOCK_RESOURCE_KIND,
            "fixed-factor-clock clock-div must be non-zero",
        ));
    }

    let parent_phandle = {
        let mut parents = info.dtb_references(PROP_CLOCKS);
        let parent = parents.next().ok_or(PnpError::missing(
            CLOCK_RESOURCE_KIND,
            "fixed-factor-clock is missing its parent clock",
        ))?;
        if parents.next().is_some() {
            return Err(PnpError::malformed(
                CLOCK_RESOURCE_KIND,
                "fixed-factor-clock must reference exactly one parent clock",
            ));
        }
        parent.phandle
    };
    if parent_phandle == 0 || parent_phandle == provider_phandle {
        return Err(PnpError::malformed(
            CLOCK_RESOURCE_KIND,
            "fixed-factor-clock has an invalid parent clock",
        ));
    }
    Ok(FixedFactorConfig { mult, div })
}

fn required_u32_property(
    info: &PlatformDeviceInfo,
    name: &str,
    missing: &'static str,
    malformed: &'static str,
) -> Result<u32, PnpError> {
    let raw = info
        .bytes_property(name)
        .ok_or(PnpError::missing(CLOCK_RESOURCE_KIND, missing))?;
    if raw.len() != size_of::<u32>() {
        return Err(PnpError::malformed(CLOCK_RESOURCE_KIND, malformed));
    }
    info.u32_property(name)
        .ok_or(PnpError::malformed(CLOCK_RESOURCE_KIND, malformed))
}

fn register_provider(
    dev: &Arc<PnpDevice>,
    phandle: u32,
    provider: Arc<dyn DtbProvider>,
) -> Result<(), PnpError> {
    let key = DtbProviderKey::new(DtbProviderKind::Clock, phandle);
    let handle = dt_provider::register(key, provider).map_err(DtbProviderError::into_pnp_error)?;
    if let Err(error) = dev.own_resource(dt_provider::provider_pnp_resource(
        handle,
        "fixed-clock-provider",
    )) {
        if let Err(unregister_error) = dt_provider::unregister(handle) {
            log::error!(
                "[dt-providers] failed to rollback clock provider phandle={:#x}: {:?}",
                phandle,
                unregister_error
            );
        }
        return Err(error);
    }
    Ok(())
}

struct DtbFixedClockDriver;

impl DtbFixedClockDriver {
    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_FIXED_CLOCK) || info.has_id(COMPAT_FIXED_FACTOR_CLOCK)
    }
}

impl PnpDriver for DtbFixedClockDriver {
    fn name(&self) -> &'static str {
        "platform-dt-fixed-clock"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            CLOCK_RESOURCE_KIND,
            "fixed clock provider is missing a phandle",
        ))?;

        let kind = fixed_clock_kind(info)?;
        match kind {
            FixedClockKind::Rate => {
                let rate = fixed_clock_rate(info)?;
                dev.reserve_owned_resources(1)?;
                register_provider(dev, phandle, Arc::new(FixedClockProvider::new(rate)))?;
                log::printk!(
                    "[dt-providers] fixed-clock {} phandle={:#x} rate={}Hz",
                    info.fw_path.as_deref().unwrap_or("<none>"),
                    phandle,
                    rate
                );
            }
            FixedClockKind::Factor => {
                let config = fixed_factor_config(info, phandle)?;
                dev.reserve_owned_resources(2)?;
                let lease = info
                    .acquire_dtb_resource_at(PROP_CLOCKS, 0)
                    .map_err(DtbProviderError::into_pnp_error)?;
                let upstream = Arc::new(UpstreamClock { lease });
                // 先登记父 lease，再登记自身 handle；PnP 的 LIFO 释放会先撤销
                // provider，确保父 clock 不会在子 provider 仍可获取时消失。
                dev.own_resource(SharedUpstreamClockPnpResource::new(Arc::clone(&upstream)))?;
                register_provider(
                    dev,
                    phandle,
                    Arc::new(FixedFactorClockProvider::new(upstream, config)),
                )?;
                log::printk!(
                    "[dt-providers] fixed-factor-clock {} phandle={:#x} mult={} div={}",
                    info.fw_path.as_deref().unwrap_or("<none>"),
                    phandle,
                    config.mult,
                    config.div
                );
            }
        }
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        log::printk!("[dt-providers] removed {}", dev.id);
    }
}

struct DtbFixedClockFactory;

impl DriverFactory for DtbFixedClockFactory {
    fn name(&self) -> &'static str {
        "platform-dt-fixed-clock"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(DtbFixedClockDriver))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(DtbFixedClockFactory))
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use crate::dev::dma::DmaContext;
    use crate::dev::platform::{DeviceMatchId, DeviceProperties, FirmwareProperty};
    use general::firmware::dtb::{DtbPlatformBindings, DtbProviderReference};

    use super::*;

    struct MockClockResource {
        rate: u64,
        enables: AtomicUsize,
        disables: AtomicUsize,
    }

    impl DtbResource for MockClockResource {
        fn control(
            &self,
            request: DtbResourceRequest<'_>,
        ) -> Result<DtbResourceReply, DtbProviderError> {
            match request {
                DtbResourceRequest::Enable => {
                    self.enables.fetch_add(1, Ordering::Relaxed);
                    Ok(DtbResourceReply::Done)
                }
                DtbResourceRequest::Disable => {
                    self.disables.fetch_add(1, Ordering::Relaxed);
                    Ok(DtbResourceReply::Done)
                }
                DtbResourceRequest::GetRate => Ok(DtbResourceReply::Value(self.rate)),
                _ => Err(DtbProviderError::UnsupportedOperation),
            }
        }
    }

    struct MockClockProvider {
        resource: Arc<MockClockResource>,
    }

    impl DtbProvider for MockClockProvider {
        fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
            if !specifier.is_empty() {
                return Err(DtbProviderError::AcquireFailed);
            }
            Ok(self.resource.clone())
        }
    }

    #[test]
    fn fixed_clock_exposes_only_the_zero_cell_clock_contract() {
        let provider = FixedClockProvider::new(24_000_000);
        assert_eq!(
            provider
                .acquire(&[])
                .unwrap()
                .control(DtbResourceRequest::GetRate),
            Ok(DtbResourceReply::Value(24_000_000))
        );
        assert_eq!(
            provider
                .acquire(&[])
                .unwrap()
                .control(DtbResourceRequest::Enable),
            Ok(DtbResourceReply::Done)
        );
        assert!(matches!(
            provider.acquire(&[0]),
            Err(DtbProviderError::AcquireFailed)
        ));
    }

    #[test]
    fn fixed_factor_forwards_parent_control_and_scales_current_rate() {
        let key = DtbProviderKey::new(DtbProviderKind::Clock, 0xd7c0_1001);
        let parent = Arc::new(MockClockResource {
            rate: 25_000_000,
            enables: AtomicUsize::new(0),
            disables: AtomicUsize::new(0),
        });
        let handle = dt_provider::register(
            key,
            Arc::new(MockClockProvider {
                resource: Arc::clone(&parent),
            }),
        )
        .unwrap();
        let upstream = Arc::new(UpstreamClock {
            lease: dt_provider::acquire(key, &[]).unwrap(),
        });
        let provider = FixedFactorClockProvider::new(
            Arc::clone(&upstream),
            FixedFactorConfig { mult: 4, div: 5 },
        );
        let resource = provider.acquire(&[]).unwrap();

        assert_eq!(
            resource.control(DtbResourceRequest::GetRate),
            Ok(DtbResourceReply::Value(20_000_000))
        );
        assert_eq!(
            resource.control(DtbResourceRequest::Enable),
            Ok(DtbResourceReply::Done)
        );
        assert_eq!(
            resource.control(DtbResourceRequest::Disable),
            Ok(DtbResourceReply::Done)
        );
        assert_eq!(parent.enables.load(Ordering::Relaxed), 1);
        assert_eq!(parent.disables.load(Ordering::Relaxed), 1);

        drop(resource);
        drop(provider);
        drop(upstream);
        dt_provider::unregister(handle).unwrap();
    }

    #[test]
    fn fixed_factor_rate_rejects_zero_divisor_and_unrepresentable_rate() {
        assert_eq!(fixed_factor_rate(1, 1, 0), Err(DtbProviderError::Invalid));
        assert_eq!(fixed_factor_rate(5, 2, 3), Ok(3));
        assert_eq!(
            fixed_factor_rate(u64::MAX, u32::MAX, u32::MAX),
            Ok(u64::MAX)
        );
        assert_eq!(
            fixed_factor_rate(u64::MAX, u32::MAX, 1),
            Err(DtbProviderError::HardwareFailure)
        );
    }

    #[test]
    fn fixed_clock_properties_are_strict_single_cells() {
        let valid = platform_info(
            COMPAT_FIXED_CLOCK,
            0xd7c0_2001,
            vec![
                firmware_property(PROP_CLOCK_CELLS, &0u32.to_be_bytes()),
                firmware_property(PROP_CLOCK_FREQUENCY, &32_768u32.to_be_bytes()),
            ],
            None,
        );
        assert_eq!(fixed_clock_rate(&valid), Ok(32_768));

        let malformed = platform_info(
            COMPAT_FIXED_CLOCK,
            0xd7c0_2002,
            vec![
                firmware_property(PROP_CLOCK_CELLS, &0u32.to_be_bytes()),
                firmware_property(PROP_CLOCK_FREQUENCY, &[0, 1]),
            ],
            None,
        );
        assert!(matches!(
            fixed_clock_rate(&malformed),
            Err(PnpError::MalformedResource { .. })
        ));
    }

    #[test]
    fn fixed_factor_requires_one_non_self_parent_and_nonzero_divisor() {
        let reference = DtbProviderReference {
            property: PROP_CLOCKS.into(),
            name: None,
            provider: None,
            provider_path: Some("/clock-parent".into()),
            provider_available: Some(true),
            phandle: 0xd7c0_3001,
            args: Box::new([]),
        };
        let valid = platform_info(
            COMPAT_FIXED_FACTOR_CLOCK,
            0xd7c0_3002,
            vec![
                firmware_property(PROP_CLOCK_CELLS, &0u32.to_be_bytes()),
                firmware_property(PROP_CLOCK_MULT, &2u32.to_be_bytes()),
                firmware_property(PROP_CLOCK_DIV, &3u32.to_be_bytes()),
            ],
            Some(DtbPlatformBindings {
                references: vec![reference],
                ..DtbPlatformBindings::default()
            }),
        );
        assert_eq!(
            fixed_factor_config(&valid, 0xd7c0_3002),
            Ok(FixedFactorConfig { mult: 2, div: 3 })
        );

        let mut invalid = valid;
        *invalid
            .fw_properties
            .iter_mut()
            .find(|property| property.name.as_ref() == PROP_CLOCK_DIV)
            .unwrap() = firmware_property(PROP_CLOCK_DIV, &0u32.to_be_bytes());
        assert!(matches!(
            fixed_factor_config(&invalid, 0xd7c0_3002),
            Err(PnpError::MalformedResource { .. })
        ));

        *invalid
            .fw_properties
            .iter_mut()
            .find(|property| property.name.as_ref() == PROP_CLOCK_DIV)
            .unwrap() = firmware_property(PROP_CLOCK_DIV, &3u32.to_be_bytes());
        invalid.dtb_bindings.as_mut().unwrap().references[0].phandle = 0xd7c0_3002;
        assert!(matches!(
            fixed_factor_config(&invalid, 0xd7c0_3002),
            Err(PnpError::MalformedResource { .. })
        ));
    }

    fn platform_info(
        compatible: &str,
        phandle: u32,
        fw_properties: alloc::vec::Vec<FirmwareProperty>,
        dtb_bindings: Option<DtbPlatformBindings>,
    ) -> PlatformDeviceInfo {
        PlatformDeviceInfo {
            fw_name: "clock".into(),
            fw_path: Some("/clock".into()),
            fw_parent_path: Some("/".into()),
            ids: vec![DeviceMatchId::DtbCompatible(compatible.into())],
            resources: vec![],
            irq_names: vec![],
            properties: DeviceProperties {
                fw_phandle: Some(phandle),
                ..DeviceProperties::default()
            },
            fw_properties,
            dma: DmaContext::default_coherent(),
            dtb_bindings,
            dtb_pcie_host: None,
            dtb_owned_nodes: None,
        }
    }

    fn firmware_property(name: &str, value: &[u8]) -> FirmwareProperty {
        FirmwareProperty::new(name.into(), value.into())
    }
}
