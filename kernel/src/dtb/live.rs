//! live Device Tree overlay 与 platform PnP 的事务协调。
//!
//! sysfs 只负责 FDT overlay 的结构合并。本模块在 live blob 发布前建立完整固件摘要、
//! 拒绝不可热插拔的启动对象变化，并让规范节点图与 platform PnP 设备集合一起提交。

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use general::dev::pnp::{PNP_DRIVERS, PnpDevice, PnpError, PnpRemovalTransaction, PnpState};
use general::firmware::SerialPortInfo;
use general::firmware::dtb::{
    self as firmware_dtb, DtbCpuInfo, DtbFirmwareInfo, DtbGraphEndpoint, DtbInterruptInfo,
    DtbIommuMap, DtbNodeInfo, DtbNumaInfo, DtbPcieHostInfo, DtbPlatformBindings,
    DtbPlatformDeviceInfo, DtbProviderReference,
};
use general::firmware::power::PowerControlInfo;
use general::vfs::sysfs::{DeviceTreeOverlayRuntimeError, install_device_tree_overlay_commit_hook};
use vfs::sync::Spinlock;

use super::{
    RegisteredPlatformNode, platform_device_info_from_dtb, platform_probe_priority,
    register_platform_device_status,
};

/// 启动后不能通过 overlay 改写的固件语义。
pub(super) struct ImmutableDtbState {
    root_compatible: Vec<Box<str>>,
    cpus: Vec<DtbCpuInfo>,
    numa: DtbNumaInfo,
    reservation_block: Vec<(u128, u128)>,
    external_initramfs_range: Option<(usize, usize)>,
    stdout_serial: Option<SerialPortInfo>,
    power_controls: PowerControlInfo,
    boot_interrupt_controllers: BTreeSet<Box<str>>,
}

impl ImmutableDtbState {
    pub(super) fn from_firmware(firmware: &DtbFirmwareInfo) -> Self {
        Self {
            root_compatible: firmware.root_compatible.clone(),
            cpus: firmware.cpus.clone(),
            numa: firmware.numa.clone(),
            reservation_block: firmware
                .memory
                .reservation_block_ranges
                .iter()
                .map(|range| (range.address, range.size))
                .collect(),
            external_initramfs_range: firmware.external_initramfs_range,
            stdout_serial: firmware.stdout_serial.clone(),
            power_controls: firmware.power_controls,
            boot_interrupt_controllers: firmware
                .platform_devices
                .iter()
                .filter(|device| device.interrupt_controller)
                .map(|device| device.path.clone())
                .collect(),
        }
    }
}

struct LiveDtbState {
    graph_generation: u64,
    immutable: ImmutableDtbState,
    platform_devices: Vec<DtbPlatformDeviceInfo>,
    pcie_hosts: Vec<DtbPcieHostInfo>,
    registered: Vec<RegisteredPlatformNode>,
    stdout_phys: Option<usize>,
    tainted: bool,
}

static LIVE_DTB_STATE: Spinlock<Option<LiveDtbState>> = Spinlock::new(None);

/// 与 sysfs seed 清理保持一致，节点图不得成为启动秘密的第二个公开入口。
pub(super) fn scrub_boot_node_graph(nodes: &mut [DtbNodeInfo]) {
    for node in nodes
        .iter_mut()
        .filter(|node| matches!(node.path.as_ref(), "/chosen" | "/chosen@0"))
    {
        node.properties
            .retain(|property| !matches!(property.name.as_ref(), "rng-seed" | "kaslr-seed"));
    }
}

pub(super) fn install(
    immutable: ImmutableDtbState,
    platform_devices: Vec<DtbPlatformDeviceInfo>,
    pcie_hosts: Vec<DtbPcieHostInfo>,
    registered: Vec<RegisteredPlatformNode>,
    stdout_phys: Option<usize>,
) -> Result<(), &'static str> {
    let graph_generation =
        firmware_dtb::node_graph_generation().ok_or("DT node graph is not installed")?;
    {
        let mut state = LIVE_DTB_STATE.lock();
        if state.is_some() {
            return Err("live DT state is already installed");
        }
        *state = Some(LiveDtbState {
            graph_generation,
            immutable,
            platform_devices,
            pcie_hosts,
            registered,
            stdout_phys,
            tainted: false,
        });
    }
    if install_device_tree_overlay_commit_hook(commit_overlay).is_err() {
        LIVE_DTB_STATE.lock().take();
        return Err("live DT overlay hook is already installed");
    }
    Ok(())
}

fn commit_overlay(
    _base_blob: &[u8],
    candidate_blob: &[u8],
) -> Result<(), DeviceTreeOverlayRuntimeError> {
    let mut candidate = firmware_dtb::parse_blob(candidate_blob).map_err(|error| {
        log::error!(
            "[dt-overlay] normalized firmware validation failed: {:?}",
            error
        );
        DeviceTreeOverlayRuntimeError::InvalidFirmware
    })?;
    scrub_boot_node_graph(&mut candidate.nodes);

    let old_nodes = firmware_dtb::node_graph_snapshot();
    // probe/remove 可能进入任意 ELM vtable，不能在整个事务期间持有 spinlock。
    // sysfs live-tree 锁会串行化 commit hook；这里暂时取出状态，重入调用则明确失败。
    let mut state = LIVE_DTB_STATE
        .lock()
        .take()
        .ok_or(DeviceTreeOverlayRuntimeError::NodeGraph)?;
    let result = (|| {
        if state.tainted {
            log::error!("[dt-overlay] live DT device model is tainted; refusing another overlay");
            return Err(DeviceTreeOverlayRuntimeError::PlatformPnp);
        }
        validate_immutable_state(&state, &old_nodes, &candidate)?;

        let graph_update =
            firmware_dtb::begin_node_graph_update(state.graph_generation).map_err(|error| {
                log::error!("[dt-overlay] cannot prepare node graph update: {:?}", error);
                DeviceTreeOverlayRuntimeError::NodeGraph
            })?;
        let plan = plan_platform_diff(&state, &old_nodes, &candidate);
        let next_registered = apply_platform_diff(&mut state, &old_nodes, &candidate, &plan)?;
        let candidate_nodes = core::mem::take(&mut candidate.nodes).into_boxed_slice();

        let committed = graph_update.commit(candidate_nodes);
        state.graph_generation = committed.generation;
        state.platform_devices = candidate.platform_devices;
        state.pcie_hosts = candidate.pcie_hosts;
        state.registered = next_registered;
        let previous_nodes = committed.previous;
        drop(previous_nodes);

        log::info!(
            "[dt-overlay] committed generation {}: platform add={} change={} remove={}",
            committed.generation,
            plan.added,
            plan.changed,
            plan.removed
        );
        Ok(())
    })();
    let mut state_guard = LIVE_DTB_STATE.lock();
    debug_assert!(state_guard.is_none());
    *state_guard = Some(state);
    result
}

fn validate_immutable_state(
    state: &LiveDtbState,
    old_nodes: &[DtbNodeInfo],
    candidate: &DtbFirmwareInfo,
) -> Result<(), DeviceTreeOverlayRuntimeError> {
    let immutable = &state.immutable;
    let scalar_changed = immutable.root_compatible != candidate.root_compatible
        || immutable.cpus != candidate.cpus
        || immutable.numa != candidate.numa
        || immutable.reservation_block.len() != candidate.memory.reservation_block_ranges.len()
        || !immutable
            .reservation_block
            .iter()
            .zip(&candidate.memory.reservation_block_ranges)
            .all(|((address, size), range)| *address == range.address && *size == range.size)
        || immutable.external_initramfs_range != candidate.external_initramfs_range
        || !serial_port_eq(
            immutable.stdout_serial.as_ref(),
            candidate.stdout_serial.as_ref(),
        )
        || immutable.power_controls != candidate.power_controls;
    if scalar_changed || !protected_boot_nodes_equal(old_nodes, &candidate.nodes) {
        log::error!(
            "[dt-overlay] rejected CPU, memory, chosen, reservation, console, or power-control reconfiguration"
        );
        return Err(DeviceTreeOverlayRuntimeError::UnsupportedChange);
    }

    for path in &immutable.boot_interrupt_controllers {
        let Some(old) = state
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == path.as_ref())
        else {
            return Err(DeviceTreeOverlayRuntimeError::UnsupportedChange);
        };
        let Some(new) = candidate
            .platform_devices
            .iter()
            .find(|device| device.path.as_ref() == path.as_ref())
        else {
            log::error!(
                "[dt-overlay] rejected removal of boot interrupt controller {}",
                path
            );
            return Err(DeviceTreeOverlayRuntimeError::UnsupportedChange);
        };
        if !platform_device_equal(
            old,
            new,
            old_nodes,
            &candidate.nodes,
            &state.platform_devices,
            &candidate.platform_devices,
            &state.pcie_hosts,
            &candidate.pcie_hosts,
        ) {
            log::error!(
                "[dt-overlay] rejected reconfiguration of boot interrupt controller {}",
                path
            );
            return Err(DeviceTreeOverlayRuntimeError::UnsupportedChange);
        }
    }
    Ok(())
}

fn serial_port_eq(left: Option<&SerialPortInfo>, right: Option<&SerialPortInfo>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.name == right.name
                && left.phys_addr == right.phys_addr
                && left.reg_size == right.reg_size
                && left.clock_hz == right.clock_hz
                && left.baud == right.baud
        }
        _ => false,
    }
}

fn protected_boot_nodes_equal(old: &[DtbNodeInfo], new: &[DtbNodeInfo]) -> bool {
    old.iter()
        .filter(|node| is_protected_boot_node(node))
        .all(|node| {
            new.iter()
                .find(|candidate| candidate.path == node.path)
                .is_some_and(|candidate| raw_node_equal(node, candidate))
        })
        && new
            .iter()
            .filter(|node| is_protected_boot_node(node))
            .all(|node| old.iter().any(|candidate| candidate.path == node.path))
}

fn is_protected_boot_node(node: &DtbNodeInfo) -> bool {
    path_is_in(node.path.as_ref(), "/cpus")
        || path_is_in(node.path.as_ref(), "/reserved-memory")
        || matches!(node.path.as_ref(), "/chosen" | "/chosen@0")
        || (node.parent_path.as_deref() == Some("/")
            && node.properties.iter().any(|property| {
                property.name.as_ref() == "device_type" && property.value.as_ref() == b"memory\0"
            }))
}

fn path_is_in(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn raw_node_equal(left: &DtbNodeInfo, right: &DtbNodeInfo) -> bool {
    left.name == right.name
        && left.path == right.path
        && left.parent_path == right.parent_path
        && left.phandle == right.phandle
        && left.enabled == right.enabled
        && left.compatible == right.compatible
        && left.address_cells == right.address_cells
        && left.size_cells == right.size_cells
        && left.parent_address_cells == right.parent_address_cells
        && left.parent_size_cells == right.parent_size_cells
        && left.interrupt_controller == right.interrupt_controller
        && left.reg_entries == right.reg_entries
        && interrupts_equal(&left.interrupts, &right.interrupts)
        && bindings_equal(&left.bindings, &right.bindings)
        && left.properties == right.properties
}

struct PlatformDiff {
    old_remove: Vec<bool>,
    new_install: Vec<bool>,
    added: usize,
    changed: usize,
    removed: usize,
}

fn plan_platform_diff(
    state: &LiveDtbState,
    old_nodes: &[DtbNodeInfo],
    candidate: &DtbFirmwareInfo,
) -> PlatformDiff {
    let mut old_remove = vec![false; state.platform_devices.len()];
    let mut new_install = vec![false; candidate.platform_devices.len()];

    for (old_index, old) in state.platform_devices.iter().enumerate() {
        let Some((new_index, new)) = candidate
            .platform_devices
            .iter()
            .enumerate()
            .find(|(_, device)| device.path == old.path)
        else {
            old_remove[old_index] = true;
            continue;
        };
        if !platform_device_equal(
            old,
            new,
            old_nodes,
            &candidate.nodes,
            &state.platform_devices,
            &candidate.platform_devices,
            &state.pcie_hosts,
            &candidate.pcie_hosts,
        ) {
            old_remove[old_index] = true;
            new_install[new_index] = true;
        } else if registered_device(&state.registered, old.path.as_ref()).is_none() {
            new_install[new_index] = true;
        }
    }
    for (new_index, new) in candidate.platform_devices.iter().enumerate() {
        if !state
            .platform_devices
            .iter()
            .any(|old| old.path == new.path)
        {
            new_install[new_index] = true;
        }
    }

    // provider 或 platform parent 重建后，消费者也必须 rebind，不能继续持有旧资源。
    loop {
        let mut changed = false;
        for (new_index, new) in candidate.platform_devices.iter().enumerate() {
            if new_install[new_index] {
                continue;
            }
            let depends_on_replaced = new
                .parent_path
                .as_deref()
                .is_some_and(|parent| candidate_path_replaced(parent, candidate, &new_install))
                || new.bindings.references.iter().any(|reference| {
                    reference.provider_path.as_deref().is_some_and(|provider| {
                        candidate_path_replaced(provider, candidate, &new_install)
                    })
                });
            if !depends_on_replaced {
                continue;
            }
            new_install[new_index] = true;
            if let Some(old_index) = state
                .platform_devices
                .iter()
                .position(|old| old.path == new.path)
            {
                old_remove[old_index] = true;
            }
            changed = true;
        }
        if !changed {
            break;
        }
    }

    let added = candidate
        .platform_devices
        .iter()
        .enumerate()
        .filter(|(index, new)| {
            new_install[*index]
                && !state
                    .platform_devices
                    .iter()
                    .any(|old| old.path == new.path)
        })
        .count();
    let removed = state
        .platform_devices
        .iter()
        .enumerate()
        .filter(|(index, old)| {
            old_remove[*index]
                && !candidate
                    .platform_devices
                    .iter()
                    .any(|new| new.path == old.path)
        })
        .count();
    let changed = new_install.iter().filter(|value| **value).count() - added;
    PlatformDiff {
        old_remove,
        new_install,
        added,
        changed,
        removed,
    }
}

fn candidate_path_replaced(path: &str, candidate: &DtbFirmwareInfo, new_install: &[bool]) -> bool {
    candidate
        .platform_devices
        .iter()
        .position(|device| device.path.as_ref() == path)
        .is_some_and(|index| new_install[index])
}

fn registered_device(registered: &[RegisteredPlatformNode], path: &str) -> Option<Arc<PnpDevice>> {
    registered
        .iter()
        .find(|node| node.path.as_ref() == path && node.device.state() != PnpState::Gone)
        .map(|node| Arc::clone(&node.device))
}

struct PlatformSlot {
    path: Box<str>,
    parent_path: Option<Box<str>>,
    device: Option<Arc<PnpDevice>>,
    registration: Option<general::dev::platform::PlatformDeviceInfo>,
}

fn apply_platform_diff(
    state: &mut LiveDtbState,
    old_nodes: &[DtbNodeInfo],
    candidate: &DtbFirmwareInfo,
    plan: &PlatformDiff,
) -> Result<Vec<RegisteredPlatformNode>, DeviceTreeOverlayRuntimeError> {
    let mut old_slots = Vec::with_capacity(state.platform_devices.len());
    for (index, device) in state.platform_devices.iter().enumerate() {
        old_slots.push(PlatformSlot {
            path: device.path.clone(),
            parent_path: device.parent_path.clone(),
            device: registered_device(&state.registered, device.path.as_ref()),
            registration: plan.old_remove[index].then(|| {
                platform_device_info_from_dtb(
                    device,
                    state.stdout_phys,
                    pcie_host(&state.pcie_hosts, device.path.as_ref()),
                    old_nodes,
                    &state.platform_devices,
                )
            }),
        });
    }

    let mut next_slots = Vec::with_capacity(candidate.platform_devices.len());
    for (index, device) in candidate.platform_devices.iter().enumerate() {
        let existing = if plan.new_install[index] {
            None
        } else {
            registered_device(&state.registered, device.path.as_ref())
        };
        next_slots.push(PlatformSlot {
            path: device.path.clone(),
            parent_path: device.parent_path.clone(),
            device: existing,
            registration: plan.new_install[index].then(|| {
                platform_device_info_from_dtb(
                    device,
                    state.stdout_phys,
                    pcie_host(&candidate.pcie_hosts, device.path.as_ref()),
                    &candidate.nodes,
                    &candidate.platform_devices,
                )
            }),
        });
    }
    let mut created = vec![false; next_slots.len()];
    let conflicts = next_slots
        .iter()
        .map(|new| {
            old_slots
                .iter()
                .any(|old| old.path == new.path && old.device.is_some())
        })
        .collect::<Vec<_>>();

    // 没有旧身份冲突的新增节点可以在触碰既有拓扑前完成 probe。
    if register_candidate_slots(
        candidate,
        plan,
        &conflicts,
        &mut next_slots,
        &mut created,
        false,
    )
    .is_err()
    {
        if let Err((path, error)) =
            remove_created_slots(&mut next_slots, &created, &candidate.platform_devices)
        {
            state.tainted = true;
            log::error!(
                "[dt-overlay] candidate cleanup failed for {}: {:?}; live DT tainted",
                path,
                error
            );
        }
        return Err(DeviceTreeOverlayRuntimeError::PlatformPnp);
    }

    // 一次 prepare 覆盖全部旧节点，任何外部 lease/Busy 子节点都会在第一个设备
    // 进入 Removing 前拒绝整个切换。
    if let Err((path, error)) =
        remove_platform_slots(&mut old_slots, &state.platform_devices, &plan.old_remove)
    {
        log::error!(
            "[dt-overlay] cannot safely remove platform node {}: {:?}",
            path,
            error
        );
        if !rollback_platform_state(
            state,
            &mut old_slots,
            &mut next_slots,
            &created,
            &candidate.platform_devices,
        ) {
            state.tainted = true;
        }
        return Err(DeviceTreeOverlayRuntimeError::PlatformPnp);
    }

    if register_candidate_slots(
        candidate,
        plan,
        &conflicts,
        &mut next_slots,
        &mut created,
        true,
    )
    .is_err()
    {
        if !rollback_platform_state(
            state,
            &mut old_slots,
            &mut next_slots,
            &created,
            &candidate.platform_devices,
        ) {
            state.tainted = true;
        }
        return Err(DeviceTreeOverlayRuntimeError::PlatformPnp);
    }

    if let Err(error) = attach_slot_topology(&next_slots) {
        log::error!("[dt-overlay] candidate topology attach failed: {:?}", error);
        if !rollback_platform_state(
            state,
            &mut old_slots,
            &mut next_slots,
            &created,
            &candidate.platform_devices,
        ) {
            state.tainted = true;
        }
        return Err(DeviceTreeOverlayRuntimeError::PlatformPnp);
    }
    let _ = PNP_DRIVERS.retry_deferred_devices();
    Ok(records_from_slots(&next_slots))
}

fn remove_platform_slots(
    slots: &mut [PlatformSlot],
    _devices: &[DtbPlatformDeviceInfo],
    remove: &[bool],
) -> Result<(), (Box<str>, PnpError)> {
    debug_assert_eq!(slots.len(), _devices.len());
    debug_assert_eq!(slots.len(), remove.len());
    let mut selected = Vec::new();
    if selected.try_reserve(slots.len()).is_err() {
        let path = slots
            .iter()
            .zip(remove)
            .find(|(slot, remove)| **remove && slot.device.is_some())
            .map(|(slot, _)| slot.path.clone())
            .unwrap_or_else(|| "<platform-set>".into());
        return Err((path, PnpError::OutOfMemory));
    }
    for (index, slot) in slots.iter().enumerate() {
        if remove[index]
            && let Some(device) = slot.device.as_ref()
        {
            selected.push(Arc::clone(device));
        }
    }
    if selected.is_empty() {
        return Ok(());
    }
    let error_path = slots
        .iter()
        .enumerate()
        .find(|(index, slot)| remove[*index] && slot.device.is_some())
        .map(|(_, slot)| slot.path.clone())
        .expect("selected platform transaction has a diagnostic path");
    PnpRemovalTransaction::prepare(&selected)
        .and_then(PnpRemovalTransaction::commit)
        .map_err(|error| (error_path, error))?;
    for slot in slots {
        if slot
            .device
            .as_ref()
            .is_some_and(|device| device.state() == PnpState::Gone)
        {
            slot.device = None;
        }
    }
    Ok(())
}

fn register_candidate_slots(
    candidate: &DtbFirmwareInfo,
    plan: &PlatformDiff,
    conflicts: &[bool],
    next_slots: &mut [PlatformSlot],
    created: &mut [bool],
    conflicting: bool,
) -> Result<(), ()> {
    for priority in 0..=2 {
        for index in 0..next_slots.len() {
            if !plan.new_install[index]
                || next_slots[index].device.is_some()
                || platform_probe_priority(
                    &candidate.platform_devices[index],
                    &candidate.platform_devices,
                ) != priority
            {
                continue;
            }
            if conflicts[index] != conflicting {
                continue;
            }
            let info = next_slots[index]
                .registration
                .take()
                .expect("planned platform registration must retain its descriptor");
            let outcome = register_platform_device_status(info, "dt-overlay", true);
            let Some(device) = outcome.device else {
                log::error!(
                    "[dt-overlay] failed to register platform node {}",
                    next_slots[index].path
                );
                return Err(());
            };
            next_slots[index].device = Some(device);
            created[index] = true;
        }
    }
    Ok(())
}

fn rollback_platform_state(
    state: &mut LiveDtbState,
    old_slots: &mut [PlatformSlot],
    next_slots: &mut [PlatformSlot],
    created: &[bool],
    next_devices: &[DtbPlatformDeviceInfo],
) -> bool {
    detach_platform_topology(next_slots);
    let mut restored = true;
    if let Err((path, error)) = remove_created_slots(next_slots, created, next_devices) {
        restored = false;
        log::error!(
            "[dt-overlay] failed to remove candidate platform node {}: {:?}",
            path,
            error
        );
    }
    for priority in 0..=2 {
        for (index, old) in old_slots.iter_mut().enumerate() {
            if platform_probe_priority(&state.platform_devices[index], &state.platform_devices)
                != priority
                || old.device.is_some()
            {
                continue;
            }
            let Some(info) = old.registration.take() else {
                continue;
            };
            let outcome = register_platform_device_status(info, "dt-overlay-rollback", true);
            if let Some(device) = outcome.device {
                old.device = Some(device);
            } else {
                restored = false;
                log::error!("[dt-overlay] failed to restore platform node {}", old.path);
            }
        }
    }
    if let Err(error) = attach_slot_topology(old_slots) {
        restored = false;
        log::error!("[dt-overlay] failed to restore old topology: {:?}", error);
    }
    state.registered = records_from_slots(old_slots);
    if !restored {
        // 继续发布候选会隐藏恢复失败。这里保留旧固件视图并明确暴露严重设备错误。
        log::error!("[dt-overlay] platform rollback was incomplete; live DT remains unchanged");
    }
    restored
}

fn remove_created_slots(
    slots: &mut [PlatformSlot],
    created: &[bool],
    _devices: &[DtbPlatformDeviceInfo],
) -> Result<(), (Box<str>, PnpError)> {
    debug_assert_eq!(slots.len(), created.len());
    debug_assert_eq!(slots.len(), _devices.len());
    let mut candidates = Vec::new();
    if candidates.try_reserve(slots.len()).is_err() {
        let path = slots
            .iter()
            .zip(created)
            .find(|(slot, created)| **created && slot.device.is_some())
            .map(|(slot, _)| slot.path.clone())
            .unwrap_or_else(|| "<candidate-set>".into());
        return Err((path, PnpError::OutOfMemory));
    }
    for (index, slot) in slots.iter().enumerate() {
        if created[index]
            && let Some(device) = slot.device.as_ref()
        {
            candidates.push(Arc::clone(device));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let path = slots
        .iter()
        .enumerate()
        .find(|(index, slot)| created[*index] && slot.device.is_some())
        .map(|(_, slot)| slot.path.clone())
        .expect("candidate cleanup has a diagnostic path");
    PnpRemovalTransaction::prepare(&candidates)
        .and_then(PnpRemovalTransaction::commit)
        .map_err(|error| (path, error))?;
    for (index, slot) in slots.iter_mut().enumerate() {
        if created[index]
            && slot
                .device
                .as_ref()
                .is_some_and(|device| device.state() == PnpState::Gone)
        {
            slot.device = None;
        }
    }
    Ok(())
}

fn detach_platform_topology(slots: &[PlatformSlot]) {
    for child in slots {
        let (Some(parent_path), Some(child_device)) =
            (child.parent_path.as_deref(), child.device.as_ref())
        else {
            continue;
        };
        let Some(parent) = slots
            .iter()
            .find(|slot| slot.path.as_ref() == parent_path)
            .and_then(|slot| slot.device.as_ref())
        else {
            continue;
        };
        parent.detach_child(child_device);
    }
}

fn attach_slot_topology(slots: &[PlatformSlot]) -> Result<usize, general::dev::pnp::PnpError> {
    let mut attached = 0;
    for child in slots {
        let (Some(parent_path), Some(child_device)) =
            (child.parent_path.as_deref(), child.device.as_ref())
        else {
            continue;
        };
        let Some(parent) = slots
            .iter()
            .find(|slot| slot.path.as_ref() == parent_path)
            .and_then(|slot| slot.device.as_ref())
        else {
            continue;
        };
        if Arc::ptr_eq(parent, child_device) || child_device.parent().is_some() {
            continue;
        }
        parent.attach_child(child_device)?;
        attached += 1;
    }
    Ok(attached)
}

fn records_from_slots(slots: &[PlatformSlot]) -> Vec<RegisteredPlatformNode> {
    slots
        .iter()
        .filter_map(|slot| {
            Some(RegisteredPlatformNode {
                path: slot.path.clone(),
                parent_path: slot.parent_path.clone(),
                device: Arc::clone(slot.device.as_ref()?),
            })
        })
        .collect()
}

fn pcie_host<'a>(hosts: &'a [DtbPcieHostInfo], path: &str) -> Option<&'a DtbPcieHostInfo> {
    hosts.iter().find(|host| host.path.as_ref() == path)
}

fn platform_device_equal(
    left: &DtbPlatformDeviceInfo,
    right: &DtbPlatformDeviceInfo,
    left_nodes: &[DtbNodeInfo],
    right_nodes: &[DtbNodeInfo],
    left_devices: &[DtbPlatformDeviceInfo],
    right_devices: &[DtbPlatformDeviceInfo],
    left_hosts: &[DtbPcieHostInfo],
    right_hosts: &[DtbPcieHostInfo],
) -> bool {
    let basic = left.name == right.name
        && left.path == right.path
        && left.parent_path == right.parent_path
        && left.phandle == right.phandle
        && left.interrupt_parent == right.interrupt_parent
        && left.address_cells == right.address_cells
        && left.size_cells == right.size_cells
        && left.parent_address_cells == right.parent_address_cells
        && left.parent_size_cells == right.parent_size_cells
        && left.compatible == right.compatible
        && left.reg_entries == right.reg_entries
        && left.reg_ranges == right.reg_ranges
        && interrupts_equal(&left.interrupts, &right.interrupts)
        && left.interrupt_controller == right.interrupt_controller
        && left.clock_hz == right.clock_hz
        && bindings_equal(&left.bindings, &right.bindings)
        && left.properties == right.properties;
    if !basic
        || !raw_owned_subtree_equal(
            left_nodes,
            right_nodes,
            left.path.as_ref(),
            left_devices,
            right_devices,
        )
    {
        return false;
    }
    match (
        pcie_host(left_hosts, left.path.as_ref()),
        pcie_host(right_hosts, right.path.as_ref()),
    ) {
        (None, None) => true,
        (Some(_), Some(_)) => raw_subtree_equal(left_nodes, right_nodes, left.path.as_ref()),
        _ => false,
    }
}

fn raw_owned_subtree_equal(
    left: &[DtbNodeInfo],
    right: &[DtbNodeInfo],
    root: &str,
    left_devices: &[DtbPlatformDeviceInfo],
    right_devices: &[DtbPlatformDeviceInfo],
) -> bool {
    left.iter()
        .filter(|node| super::platform_owns_node(root, node.path.as_ref(), left_devices))
        .all(|node| {
            right
                .iter()
                .find(|candidate| candidate.path == node.path)
                .is_some_and(|candidate| {
                    super::platform_owns_node(root, candidate.path.as_ref(), right_devices)
                        && raw_node_equal(node, candidate)
                })
        })
        && right
            .iter()
            .filter(|node| super::platform_owns_node(root, node.path.as_ref(), right_devices))
            .all(|node| {
                left.iter().any(|candidate| {
                    candidate.path == node.path
                        && super::platform_owns_node(root, candidate.path.as_ref(), left_devices)
                })
            })
}

fn raw_subtree_equal(left: &[DtbNodeInfo], right: &[DtbNodeInfo], root: &str) -> bool {
    left.iter()
        .filter(|node| path_is_in(node.path.as_ref(), root))
        .all(|node| {
            right
                .iter()
                .find(|candidate| candidate.path == node.path)
                .is_some_and(|candidate| raw_node_equal(node, candidate))
        })
        && right
            .iter()
            .filter(|node| path_is_in(node.path.as_ref(), root))
            .all(|node| left.iter().any(|candidate| candidate.path == node.path))
}

fn interrupts_equal(left: &[DtbInterruptInfo], right: &[DtbInterruptInfo]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.provider_path == right.provider_path
                && left.parent == right.parent
                && left.specifier == right.specifier
        })
}

fn bindings_equal(left: &DtbPlatformBindings, right: &DtbPlatformBindings) -> bool {
    references_equal(&left.references, &right.references)
        && left.dma_ranges == right.dma_ranges
        && iommu_maps_equal(left.iommu_map.as_ref(), right.iommu_map.as_ref())
        && graph_endpoints_equal(&left.graph_endpoints, &right.graph_endpoints)
        && left.effective_dma == right.effective_dma
}

fn references_equal(left: &[DtbProviderReference], right: &[DtbProviderReference]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.property == right.property
                && left.name == right.name
                && left.provider_path == right.provider_path
                && left.provider_available == right.provider_available
                && left.phandle == right.phandle
                && left.args == right.args
        })
}

fn iommu_maps_equal(left: Option<&DtbIommuMap>, right: Option<&DtbIommuMap>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.mask == right.mask
                && left.entries.len() == right.entries.len()
                && left
                    .entries
                    .iter()
                    .zip(&right.entries)
                    .all(|(left, right)| {
                        left.input_base == right.input_base
                            && left.provider_path == right.provider_path
                            && left.provider_phandle == right.provider_phandle
                            && left.output_base == right.output_base
                            && left.length == right.length
                            && left.provider_available == right.provider_available
                    })
        }
        _ => false,
    }
}

fn graph_endpoints_equal(left: &[DtbGraphEndpoint], right: &[DtbGraphEndpoint]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.node_path == right.node_path
                && left.port_path == right.port_path
                && left.port_id == right.port_id
                && left.endpoint_id == right.endpoint_id
                && left.phandle == right.phandle
                && left.remote_path == right.remote_path
                && left.remote_phandle == right.remote_phandle
        })
}
