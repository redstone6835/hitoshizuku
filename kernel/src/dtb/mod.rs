//! 基于 DTB 的内核启动初始化逻辑。
//!
//! 本模块负责解析启动上下文中的 DTB，完成内存管理、设备发现与注册、
//! 文件系统挂载以及控制台绑定等核心启动流程。
//!
//! 固件解析结果拥有路径与属性数据；设备模型在注册时接管或复制这些值，
//! 不通过泄漏分配伪造静态生命周期。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, compiler_fence};

use allocator::{KERNEL_ALLOCATOR, MemorySegment};
use general::dev::block::BlockDevice;
use general::dev::dma::{DmaBouncePolicy, DmaConstraints, DmaContext, DmaWindow};
use general::dev::enumerate::DEVICES;
use general::dev::iommu::{self, IommuAttachment, IommuRequester};
use general::dev::platform::{
    DeviceMatchId, DeviceProperties, DeviceResource, FirmwareProperty, PlatformDeviceInfo,
    PlatformProbeStatus, register_and_probe_platform_device,
};
use general::dev::pnp::DevInitContext;
use general::dev::pnp::PnpDevice;
use general::firmware::dtb as firmware_dtb;
use general::vfs::FS_REGISTRY;
use general::vfs::VfsContext;
use general::vfs::cred::Credentials;
use general::vfs::dentry::VfsRoot;
use general::vfs::device_files::projection::active_block_devices;
use general::vfs::limits::VfsLimits;
use general::vfs::mount::{Mount, MountFlags, MountNamespace};
use general::vfs::stat::FileMode;
use general::vfs::superblock::Superblock;
use general::{StartBootProtocol, StartContext, StartFirmware, StartNoMapSupport, StartPhysRange};
use log::printk;

use crate::start;

mod live;

/// DTB 启动路径的主入口。
///
/// 从 `StartContext` 中提取 DTB 固件视图，依次完成平台基础信息解析、
/// 内存分配器初始化、设备枚举和注册、tmpfs/devtmpfs 文件系统挂载、
/// 控制台注册以及日志系统绑定。
pub fn kernel_start_init(context: &StartContext) {
    log::debug!("[kernel-start][dtb] jumped into kernel_start_init()");

    // 步骤 0 先确认本次启动确实走的是 DTB 固件路径，并拿到稳定 DTB 视图。
    // 只有在这里把 DTB 视图固定下来，后续所有解析步骤才有共同的语义起点。
    let dtb = match context.firmware {
        StartFirmware::Dtb(dtb) => dtb,
        StartFirmware::Acpi(_) => {
            panic!("[kernel-start][dtb] StartContext firmware does not match DTB path")
        }
    };
    // 步骤 1 先把原始 DTB 交给平台无关的固件解析器。解析器会统一建立全树索引、
    // 处理 aliases/phandle/status/reg/ranges，并产出内核启动需要的标准描述符；
    // 这里不再假设设备一定挂在根节点或 `/soc` 下。
    let firmware = firmware_dtb::parse(dtb).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][dtb] failed to parse DTB firmware info: {:?}",
            err
        )
    });
    let immutable_live_state = live::ImmutableDtbState::from_firmware(&firmware);
    general::vfs::sysfs::install_device_tree(&dtb).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][dtb] failed to install /sys/firmware Device Tree view: {:?}",
            err
        )
    });
    let firmware_dtb::DtbFirmwareInfo {
        root_compatible,
        cpu_count,
        cpus,
        numa,
        memory,
        external_initramfs_range,
        mut rng_seed,
        stdout_serial,
        power_controls,
        serial_ports,
        mut nodes,
        platform_devices,
        pcie_hosts,
    } = firmware;
    live::scrub_boot_node_graph(&mut nodes);
    let node_count = nodes.len();
    general::firmware::dtb::install_node_graph(nodes.clone()).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][dtb] failed to install normalized node graph: {:?}",
            err
        )
    });

    // 启动协议决定 RAM 的权威来源：UEFI 必须忽略 DT `/memory`，而直接启动
    // 才使用 DT memory 节点并与架构加载器的白名单求交。chosen 的 kdump 限制
    // 对两种来源都生效。
    let boot_memory_segments = context.memory.boot_map.usable_segments();
    let mut base_memory = if matches!(context.boot.protocol, StartBootProtocol::Efi) {
        boot_memory_segments.unwrap_or_else(|| {
            panic!("[kernel-start][dtb] UEFI DT boot requires a usable GetMemoryMap snapshot")
        })
    } else {
        let described = firmware_dtb::described_memory_segments(&memory)
            .unwrap_or_else(|err| panic!("[kernel-start][dtb] invalid DT memory range: {:?}", err));
        if let Some(boot_segments) = boot_memory_segments {
            if described.is_empty() {
                panic!("[kernel-start][dtb] non-UEFI DT boot is missing usable /memory nodes");
            }
            start::intersect_memory_segments(&described, &boot_segments).unwrap_or_else(|| {
                panic!(
                    "[kernel-start][dtb] DTB memory description does not overlap usable boot memory"
                )
            })
        } else if described.is_empty() {
            panic!("[kernel-start][dtb] DT boot has neither /memory nor a UEFI memory map");
        } else {
            described
        }
    };
    base_memory = firmware_dtb::apply_chosen_usable_ranges(base_memory, &memory)
        .unwrap_or_else(|err| panic!("[kernel-start][dtb] invalid chosen memory limit: {:?}", err));
    if base_memory.is_empty() {
        panic!("[kernel-start][dtb] chosen memory limits leave no usable RAM");
    }

    // 动态 reserved-memory 必须在内核镜像和外部 initramfs 之后分配；否则一个
    // 合法的 `size` 请求可能覆盖尚未交给 buddy 的启动对象。
    let kernel_image = context.memory.kernel_image;
    let mut kernel_reserved = Vec::new();
    let mut additional_reserved = Vec::new();
    let kernel_segment = MemorySegment {
        start: kernel_image.start,
        size: kernel_image.end - kernel_image.start,
    };
    kernel_reserved.push((kernel_image.start, kernel_image.end));
    additional_reserved.push(kernel_segment);
    if let Some((start, end)) = external_initramfs_range {
        if end <= start {
            panic!("[kernel-start][dtb] invalid external initramfs range");
        }
        kernel_reserved.push((start, end));
        additional_reserved.push(MemorySegment {
            start,
            size: end - start,
        });
        printk!(
            "[kernel-start][dtb] external initramfs reserved: phys={:#x}..{:#x} ({} bytes)",
            start,
            end,
            end - start
        );
    }

    let mut memory_layout =
        firmware_dtb::resolve_memory_layout(&memory, base_memory, &additional_reserved)
            .unwrap_or_else(|err| panic!("[kernel-start][dtb] invalid memory layout: {:?}", err));
    let no_map_support = context.allocator.map(|ops| ops.no_map);
    if let Some(granule) = no_map_support.and_then(StartNoMapSupport::granule) {
        firmware_dtb::apply_no_map_granule(&mut memory_layout, granule, &additional_reserved)
            .unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][dtb] no-map cannot be enforced at architecture granule: {:?}",
                    err
                )
            });
    }
    let firmware_dtb::DtbMemoryLayout {
        usable_segments,
        reserved_segments,
        reserved_memory,
        no_map_segments,
    } = memory_layout;
    let numa_memory_layout = firmware_dtb::split_numa_memory_segments(&memory, usable_segments)
        .unwrap_or_else(|err| panic!("[kernel-start][dtb] invalid NUMA memory layout: {:?}", err));
    let memory_segments = numa_memory_layout.usable_segments;
    let numa_memory_ranges = numa_memory_layout.numa_ranges;
    if memory_segments.is_empty() {
        panic!("[kernel-start][dtb] DT reservations consume all usable memory");
    }
    if matches!(context.boot.protocol, StartBootProtocol::Efi) {
        let efi_map = context
            .memory
            .boot_map
            .regions()
            .expect("UEFI DT boot map was required above");
        firmware_dtb::validate_uefi_reserved_memory(&reserved_memory, efi_map).unwrap_or_else(
            |err| {
                panic!(
                    "[kernel-start][dtb] UEFI map does not protect static reserved-memory: {:?}",
                    err
                )
            },
        );
    }
    let reserved_memory_count = reserved_memory.len();
    general::firmware::dtb::install_reserved_memory(reserved_memory).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][dtb] failed to install reserved-memory snapshot: {:?}",
            err
        )
    });

    let no_map_count = no_map_segments.len();
    let no_map_ranges: Vec<StartPhysRange> = no_map_segments
        .into_iter()
        .map(|segment| StartPhysRange::new(segment.start, segment.end()))
        .collect();
    match no_map_support {
        Some(StartNoMapSupport::Enforced { granule, prepare }) => {
            if granule == 0 || !granule.is_power_of_two() {
                panic!("[kernel-start][dtb] architecture published an invalid no-map granule");
            }
            prepare(&no_map_ranges).unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][dtb] architecture could not enforce DT no-map ranges: {:?}",
                    err
                )
            });
        }
        Some(StartNoMapSupport::ReservedOnly { mechanism, .. }) if !no_map_ranges.is_empty() => {
            printk!(
                "[kernel-start][dtb] no-map ranges are excluded from the physical allocator, but fixed direct aliases remain available: {}",
                mechanism
            );
        }
        Some(StartNoMapSupport::Unsupported { mechanism }) if !no_map_ranges.is_empty() => {
            panic!(
                "[kernel-start][dtb] DT declares no-map memory but architecture cannot enforce it: {}",
                mechanism
            );
        }
        Some(StartNoMapSupport::None)
        | Some(StartNoMapSupport::ReservedOnly { .. })
        | Some(StartNoMapSupport::Unsupported { .. }) => {}
        None if !no_map_ranges.is_empty() => {
            panic!(
                "[kernel-start][dtb] DT declares no-map memory without an architecture capability"
            );
        }
        None => {}
    }

    printk!(
        "[kernel-start][dtb] firmware parsed: root-compatible={} cpu={} memory={} reserved={} reserved-nodes={} no-map={} serial={} nodes={} platform={} pcie-host={}",
        root_compatible
            .first()
            .map(|value| value.as_ref())
            .unwrap_or("<none>"),
        cpu_count,
        memory_segments.len(),
        reserved_segments.len(),
        reserved_memory_count,
        no_map_count,
        serial_ports.len(),
        node_count,
        platform_devices.len(),
        pcie_hosts.len()
    );

    // 步骤 2 把刚刚解析好的电源控制信息安装到固件抽象层。这样内核后续无论是
    // 正常关机、重启还是错误路径上的兜底退出，都能通过统一接口回到本平台提供的
    // syscon 寄存器写入方案，而不需要再次接触 DTB 原始节点。

    general::firmware::power::install(power_controls, context.address.device_mmio_to_virt);

    // 步骤 3 初始化分层分配器。这个阶段会消费上面整理好的内存段、内核镜像占用区
    // 以及可选的外部 initramfs 范围。这里先建立物理地址与虚拟地址转换关系，再
    // 逐层启用物理页、内核虚拟内存、堆和 slab，使后续驱动注册与 VFS 挂载都能在
    // 同一套分配框架中进行。

    // 小步骤 3.1 先绑定平台提供的物理地址与虚拟地址转换函数。
    KERNEL_ALLOCATOR
        .bind_address_translation(context.address.phys_to_virt, context.address.virt_to_phys);

    // 小步骤 3.2 然后整理启动早期必须避开的保留区，包括内核镜像本身以及可选的
    // 外部 initramfs 地址范围。
    // 小步骤 3.3 先初始化物理页分配器，使之后的页级资源请求可以建立在 DTB 解析出的
    // 可用 RAM 之上。
    KERNEL_ALLOCATOR
        .init_phys(&memory_segments, &kernel_reserved)
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to init physical allocator: {:?}",
                err
            )
        });
    KERNEL_ALLOCATOR
        .install_numa_ranges(&numa_memory_ranges)
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to install allocator NUMA ranges: {:?}",
                err
            )
        });

    // 小步骤 3.4 如果当前平台提供了完整的虚拟内存初始化回调，这里继续把内核页表、
    // 虚拟内存、堆和 slab 一次性拉起来，并在最后切换全局分配器。
    if let Some(alloc_ops) = context.allocator {
        KERNEL_ALLOCATOR.bind_kernel_heap_ops(
            alloc_ops.kernel_heap_region,
            alloc_ops.tracked_heap_region,
            alloc_ops.map_kernel_heap_range,
            alloc_ops.unmap_kernel_heap_range,
        );
        general::elm_image::register_elm_image_ops(
            alloc_ops.protect_kernel_heap_range,
            alloc_ops.validate_kernel_heap_range,
            alloc_ops.sync_icache,
        );
        (alloc_ops.init_kernel_page_table)();
        KERNEL_ALLOCATOR
            .init_vmem(&kernel_reserved)
            .unwrap_or_else(|err| panic!("[kernel-start][dtb] failed to init vmem: {:?}", err));
        KERNEL_ALLOCATOR.init_kheap();
        KERNEL_ALLOCATOR.init_slab(cpu_count);
        KERNEL_ALLOCATOR.activate_global().unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to activate global allocator: {:?}",
                err
            )
        });
    }

    printk!(
        "[kernel-start][dtb] memory allocator ready: {} RAM segment(s)",
        memory_segments.len()
    );

    let cpu_numa_topology: Vec<general::dev::cpu::CpuNumaEntry> = cpus
        .iter()
        .filter_map(|cpu| {
            cpu.numa_node_id
                .map(|node_id| general::dev::cpu::CpuNumaEntry {
                    logical_id: cpu.logical_id,
                    node_id,
                })
        })
        .collect();
    let cpu_numa_nodes: Vec<u32> = cpu_numa_topology
        .iter()
        .map(|entry: &general::dev::cpu::CpuNumaEntry| entry.node_id)
        .collect();
    let cpu_topology: Vec<_> = cpus
        .into_iter()
        .map(|cpu| general::dev::cpu::CpuTopologyEntry {
            logical_id: cpu.logical_id,
            reg: cpu.reg,
            phandle: cpu.phandle,
            interrupt_controller_phandles: cpu.interrupt_controller_phandles,
            compatible: cpu
                .compatible
                .into_iter()
                .map(|compatible| compatible.into())
                .collect(),
            socket_id: cpu.socket_id,
            cluster_path: cpu.cluster_path,
            core_id: cpu.core_id,
            thread_id: cpu.thread_id,
            capacity_dmips_mhz: cpu.capacity_dmips_mhz,
        })
        .collect();
    let cpu_topology_count = cpu_topology.len();
    general::dev::cpu::install_topology(cpu_topology);
    general::dev::cpu::install_numa_topology(cpu_numa_topology);
    general::dev::numa::install_topology(
        cpu_numa_nodes,
        numa.distances
            .into_iter()
            .map(|distance| general::dev::numa::NumaDistance {
                from: distance.from,
                to: distance.to,
                distance: distance.distance,
            })
            .collect(),
        numa_memory_ranges
            .into_iter()
            .map(|range| general::dev::numa::NumaMemoryRange {
                start: range.range.start,
                size: range.range.size,
                node_id: range.node_id,
            })
            .collect(),
    );
    printk!(
        "[kernel-start][dtb] installed CPU topology: {} CPU node(s)",
        cpu_topology_count
    );

    let cmdline = context
        .boot
        .command_line
        .map(general::cmdline::Cmdline::new);
    let external_initramfs = external_initramfs_range.map(|(start, end)| {
        let virt = (context.address.phys_to_virt)(start);
        let len = end - start;
        let bytes = unsafe { core::slice::from_raw_parts(virt as *const u8, len) };
        crate::initramfs::InitramfsImage {
            bytes,
            source: crate::initramfs::InitramfsSource::External,
        }
    });

    // 步骤 4 进入文件系统准备阶段。启动代码会先准备好 tmpfs、devtmpfs、procfs
    // 和 sysfs 所需的驱动与 superblock，然后根据是否存在 initramfs 或块设备根盘
    // 来决定最终 `/` 的来源。devtmpfs 会被提前建立，以便总线扫描和设备注册期间
    // 就能为后续 `/dev` 挂载准备好节点。

    // 小步骤 5.1 先注册启动阶段一定会用到的文件系统驱动，并创建全局 devtmpfs
    // superblock。DTB 后续的 platform/PCI probe 会直接把设备节点写入这份树。
    crate::device_init::register_core_filesystems("dtb");
    let dev_sb = crate::device_init::mount_devtmpfs("dtb");

    // 小步骤 5.2 接着把 devtmpfs 与 PnP 层连接起来。这样 PCI 扫描过程中一旦发现
    // 可驱动设备，对应的字符设备或块设备节点就能直接写入 devtmpfs；等最终根文件
    // 系统选定之后，再把这份已经填充好的 devtmpfs 挂到 `/dev` 即可。
    crate::device_init::activate_device_subsystem(
        "dtb",
        Arc::clone(&dev_sb),
        DevInitContext::new(context.address.device_mmio_to_virt)
            .with_boot_cpu_id(context.boot.boot_cpu_id)
            .with_realtime_clock(crate::vdso::set_realtime_ns)
            .with_realtime_source_hooks(
                crate::vdso::install_realtime_source,
                crate::vdso::unregister_realtime_source,
            ),
        rng_seed.as_deref(),
    );
    if let Some(seed) = rng_seed.as_deref_mut() {
        wipe_secret(seed);
    }

    let stdout_phys = stdout_serial.as_ref().map(|port| port.phys_addr);
    let mut platform_bound = 0usize;
    let mut registered_platform_nodes = Vec::new();
    // 先建立中断 domain，再建立被其它节点引用的 provider，最后才是普通
    // consumer。每一层都允许有限多轮重试，因此 DT 文本顺序不会成为驱动绑定
    // 的隐式约束，provider 之间的级联也能由精确 deferred dependency 解开。
    for priority in 0..=2 {
        let mut pending: Vec<usize> = platform_devices
            .iter()
            .enumerate()
            .filter_map(|(index, device)| {
                (platform_probe_priority(device, &platform_devices) == priority).then_some(index)
            })
            .collect();
        let max_passes = pending.len();
        for _ in 0..max_passes {
            if pending.is_empty() {
                break;
            }
            let before = pending.len();
            let mut retry = Vec::new();
            for index in pending {
                let device = &platform_devices[index];
                let pcie_host = pcie_hosts
                    .iter()
                    .find(|host| host.path.as_ref() == device.path.as_ref());
                let info = platform_device_info_from_dtb(
                    device,
                    stdout_phys,
                    pcie_host,
                    &nodes,
                    &platform_devices,
                );
                let outcome = register_platform_device_status(info, "dtb", priority == 2);
                if let Some(pnp_device) = outcome.device {
                    remember_registered_platform_node(
                        &mut registered_platform_nodes,
                        &device.path,
                        device.parent_path.as_deref(),
                        pnp_device,
                    );
                }
                match outcome.status {
                    PlatformRegisterStatus::Bound => platform_bound += 1,
                    PlatformRegisterStatus::Unbound => {}
                    PlatformRegisterStatus::Deferred | PlatformRegisterStatus::Failed => {
                        retry.push(index)
                    }
                }
            }
            if retry.len() == before {
                pending = retry;
                break;
            }
            pending = retry;
        }
        if !pending.is_empty() {
            log::debug!(
                "[kernel-start][dtb] {} priority-{} platform node(s) remained unbound after dependency retries",
                pending.len(),
                priority
            );
        }
    }
    let attached_platform_edges = attach_platform_topology(&registered_platform_nodes);
    printk!(
        "[kernel-start][dtb] platform PnP discovery complete: {} candidate(s), {} bound, {} topology edge(s)",
        platform_devices.len(),
        platform_bound,
        attached_platform_edges
    );

    // PCI host 与其它 platform 设备走同一条 PnP/ELM probe 路径；启动层只确认
    // 固件是否提供候选节点，不再直接安装 ECAM、分配 BAR 或扫描 endpoint。
    if pcie_hosts.is_empty() {
        printk!("[kernel-start][dtb] no pcie node in DTB");
    }
    live::install(
        immutable_live_state,
        platform_devices,
        pcie_hosts,
        registered_platform_nodes,
        stdout_phys,
    )
    .unwrap_or_else(|error| {
        panic!(
            "[kernel-start][dtb] failed to install live DT overlay coordinator: {}",
            error
        )
    });

    // 小步骤 5.4 再决定根文件系统的来源。优先级是外部/内建 initramfs，其次才是
    // 已经注册好的块设备根盘。
    let selected_initramfs = external_initramfs.or_else(crate::initramfs::embedded_image);
    let cred = Credentials::root();
    let (root_sb, root_source) = if let Some(image) = selected_initramfs {
        let sb = mount_tmpfs_superblock();
        (
            sb,
            match image.source {
                crate::initramfs::InitramfsSource::Embedded => "embedded initramfs",
                crate::initramfs::InitramfsSource::External => "external initramfs",
            },
        )
    } else {
        mount_first_block_root()
            .unwrap_or_else(|err| panic!("[kernel-start][dtb] failed to mount block root: {}", err))
    };
    printk!("[kernel-start][dtb] root source selected: {}", root_source);

    // 小步骤 5.5 在确定根 superblock 之后，创建 mount namespace 和启动期 VFS
    // 上下文，并在需要时把 initramfs 内容解包到最终根目录。
    let root_mount = Mount::new(
        Arc::clone(&root_sb),
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_sb.root_dentry),
        MountFlags::default(),
        None,
    );
    let mount_ns = MountNamespace::new(1, Arc::clone(&root_mount));

    let vfs_ctx = VfsContext::new(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        VfsRoot::new(Arc::clone(&root_sb.root_dentry), Arc::clone(&root_mount)),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
        FileMode::new(0),
        VfsLimits::default_arc(),
    );

    if let Some(image) = selected_initramfs {
        crate::initramfs::unpack_newc(image, &vfs_ctx).unwrap_or_else(|err| {
            panic!("[kernel-start][dtb] failed to unpack initramfs: {:?}", err)
        });
        printk!("[kernel-start][dtb] initramfs unpacked into root");
    }

    // 小步骤 5.6 最后把 devtmpfs 和标准用户接口伪文件系统挂到公共路径。
    crate::device_init::mount_devtmpfs_on_dev("dtb", &vfs_ctx, Arc::clone(&dev_sb));
    crate::device_init::mount_standard_user_api_filesystems("dtb", &vfs_ctx);

    printk!(
        "[kernel-start][dtb] VFS ready: '{}' mounted as '/' + devtmpfs '/dev' + tmpfs '/dev/shm' + sysfs '/sys'",
        root_source
    );

    // 步骤 6 最后确定启动期控制台，并把日志出口绑定到已经就绪的字符设备上。
    // 这里优先尊重命令行显式指定的 console，其次回退到 DTB 的 stdout-path 结果。
    // 一旦控制台建立成功，后续 printk 与日志系统都会共享同一条稳定的输出路径。

    // 小步骤 6.1 先把根目录、挂载点和凭据等 VFS 组件交给 sched shim 保管；随后
    // sched::boot_init 会据此给 init 任务挂上 TASKEXT_VFS_CONTEXT / TASKEXT_VFS_FDTABLE。
    crate::sched::stash_boot_vfs_parts(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
    );

    // 小步骤 6.2 然后解析控制台来源。这里优先看命令行 console 参数，找不到时再
    // 用 stdout-path 反查已经注册好的串口驱动实例。
    let console_selector = if let Some(name) = cmdline.as_ref().and_then(|cl| {
        cl.find("console")
            .map(|value| value.split_once(',').map_or(value, |(device, _)| device))
    }) {
        printk!("[kernel-start][dtb] console requested by cmdline: {}", name);
        Some(crate::device_init::BootConsoleSelector::DeviceName(
            alloc::string::String::from(name),
        ))
    } else if let Some(port) = stdout_serial.as_ref() {
        printk!(
            "[kernel-start][dtb] console requested by stdout-path: {}",
            port.name
        );
        Some(crate::device_init::BootConsoleSelector::FirmwareName(
            alloc::string::String::from(port.name.as_ref()),
        ))
    } else {
        None
    };
    if let Some(selector) = console_selector {
        let _ = crate::device_init::bind_or_defer_boot_console(
            "dtb",
            &vfs_ctx,
            Arc::clone(&dev_sb),
            selector,
        );
    } else {
        printk!("[kernel-start][dtb] no console selected");
    }

    printk!("[kernel-start][dtb] kernel initialization complete, jumping to main entry");
}

fn wipe_secret(bytes: &mut [u8]) {
    for byte in bytes {
        // Safety: byte 来自独占可变切片，指针有效且按 u8 对齐；volatile 写避免
        // 编译器把释放前的秘密擦除当作无可观察副作用删除。
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

fn mount_tmpfs_superblock() -> Arc<Superblock> {
    FS_REGISTRY
        .find("tmpfs")
        .expect("[kernel-start][dtb] tmpfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][dtb] failed to mount tmpfs root")
}

fn mount_block_root(
    dev: Arc<BlockDevice>,
) -> Result<(Arc<Superblock>, &'static str), &'static str> {
    general::vfs::mount_block_device_auto(dev, "")
        .map_err(|_| "unsupported or invalid root filesystem")
}

fn mount_first_block_root() -> Result<(Arc<Superblock>, &'static str), &'static str> {
    let devices = active_block_devices(&DEVICES.functions);
    if devices.is_empty() {
        return Err("no initramfs and no active block device found");
    }

    for dev in devices {
        match mount_block_root(Arc::clone(&dev)) {
            Ok(root) => return Ok(root),
            Err(err) => {
                log::debug!(
                    "[kernel-start][dtb] block device {} is not root candidate: {}",
                    dev.name(),
                    err
                );
            }
        }
    }

    Err("no active block device contains a supported root filesystem")
}

fn platform_device_info_from_dtb(
    device: &firmware_dtb::DtbPlatformDeviceInfo,
    stdout_phys: Option<usize>,
    pcie_host: Option<&firmware_dtb::DtbPcieHostInfo>,
    nodes: &[firmware_dtb::DtbNodeInfo],
    platform_devices: &[firmware_dtb::DtbPlatformDeviceInfo],
) -> PlatformDeviceInfo {
    let ids = device
        .compatible
        .iter()
        .map(|compatible| DeviceMatchId::DtbCompatible(compatible.clone()))
        .collect();
    let mut resources: Vec<DeviceResource> = device
        .reg_ranges
        .iter()
        .map(|range| DeviceResource::mmio(range.phys_addr, range.size))
        .collect();
    resources.extend(
        device
            .interrupts
            .iter()
            .map(|irq| DeviceResource::irq(irq.parent, irq.specifier.clone())),
    );
    let irq_names = device
        .interrupts
        .iter()
        .map(|irq| irq.name.clone())
        .collect();
    let first_phys = device.reg_ranges.first().map(|range| range.phys_addr);
    let fw_properties = device
        .properties
        .iter()
        .map(|property| FirmwareProperty::new(property.name.clone(), property.value.clone()))
        .collect::<Vec<_>>();
    let baud = fw_properties
        .iter()
        .find(|property| property.name.as_ref() == "current-speed")
        .and_then(FirmwareProperty::as_u32);

    PlatformDeviceInfo {
        fw_name: device.name.clone(),
        fw_path: Some(device.path.clone()),
        fw_parent_path: device.parent_path.clone(),
        ids,
        resources,
        irq_names,
        properties: DeviceProperties {
            clock_hz: device.clock_hz,
            baud,
            numa_node_id: device.numa_node_id,
            fw_phandle: device.phandle,
            fw_interrupt_parent: device.interrupt_parent,
            interrupt_controller: device.interrupt_controller,
            fw_address_cells: u8::try_from(device.address_cells).ok(),
            fw_size_cells: u8::try_from(device.size_cells).ok(),
            fw_parent_address_cells: u8::try_from(device.parent_address_cells).ok(),
            fw_parent_size_cells: u8::try_from(device.parent_size_cells).ok(),
            stdout: first_phys == stdout_phys,
        },
        fw_properties,
        dma: platform_dma_context(device),
        dtb_bindings: Some(device.bindings.clone()),
        dtb_pcie_host: pcie_host.cloned(),
        dtb_owned_nodes: Some(Arc::from(
            nodes
                .iter()
                .filter(|node| {
                    platform_owns_node(device.path.as_ref(), node.path.as_ref(), platform_devices)
                })
                .cloned()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )),
    }
}

pub(super) fn platform_owns_node(
    owner_path: &str,
    node_path: &str,
    platform_devices: &[firmware_dtb::DtbPlatformDeviceInfo],
) -> bool {
    if !path_is_in(node_path, owner_path) {
        return false;
    }
    !platform_devices.iter().any(|nested| {
        nested.path.as_ref() != owner_path
            && path_is_in(nested.path.as_ref(), owner_path)
            && path_is_in(node_path, nested.path.as_ref())
    })
}

fn path_is_in(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn platform_dma_context(device: &firmware_dtb::DtbPlatformDeviceInfo) -> DmaContext {
    let dma = &device.bindings.effective_dma;
    let prefer_numa = |context: DmaContext| context.with_preferred_numa_node(device.numa_node_id);
    let constraints = DmaConstraints {
        address_mask: usize::MAX,
        max_segment_size: usize::MAX,
        max_segments: 1,
        coherent: dma.coherent,
        supports_scatter_gather: false,
        bounce: DmaBouncePolicy::Disabled,
    };
    if dma.unsupported {
        return prefer_numa(DmaContext::blocked(constraints));
    }

    let attachments: Vec<IommuAttachment> = device
        .bindings
        .references
        .iter()
        .filter(|reference| {
            reference.property.as_ref() == "iommus" && reference.provider_available == Some(true)
        })
        .map(|reference| IommuAttachment::new(reference.phandle, reference.args.clone()))
        .collect();
    if !attachments.is_empty() {
        let requester = IommuRequester::platform(platform_requester_id(device));
        return match iommu::lazy_iommu_context(constraints, requester, attachments) {
            Ok(context) => prefer_numa(context),
            Err(error) => {
                log::error!(
                    "[kernel-start][dtb] invalid IOMMU attachment for {}: {:?}",
                    device.path,
                    error
                );
                prefer_numa(DmaContext::blocked(constraints))
            }
        };
    }
    if dma.iommu_required {
        // 这里只会剩下需要总线 requester ID 的 generic iommu-map。在专用总线
        // 把 ID 交给 IOMMU 层之前必须 fail closed，不能猜测 identity DMA。
        return prefer_numa(DmaContext::blocked(constraints));
    }
    let Some(windows) = dma.windows.as_ref() else {
        return prefer_numa(DmaContext::with_constraints(constraints));
    };
    if windows.is_empty() {
        return prefer_numa(DmaContext::with_constraints(constraints));
    }
    let windows: Vec<DmaWindow> = windows
        .iter()
        .map(|window| DmaWindow {
            cpu_start: window.cpu_start,
            dma_start: window.dma_start,
            size: window.size,
        })
        .collect();
    prefer_numa(DmaContext::with_owned_windows(
        constraints,
        Arc::from(windows.into_boxed_slice()),
    ))
}

fn platform_requester_id(device: &firmware_dtb::DtbPlatformDeviceInfo) -> u64 {
    if let Some(phandle) = device.phandle {
        return u64::from(phandle);
    }

    // phandle 不是 platform 节点的必选属性。对这类 consumer 使用拥有型绝对路径
    // 的稳定 FNV-1a 身份，并置顶位与 32-bit phandle 命名空间分离。
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in device.path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (1u64 << 63) | (hash & !(1u64 << 63))
}

pub(super) fn platform_probe_priority(
    device: &firmware_dtb::DtbPlatformDeviceInfo,
    devices: &[firmware_dtb::DtbPlatformDeviceInfo],
) -> u8 {
    if device.interrupt_controller {
        return 0;
    }
    let referenced = devices.iter().any(|consumer| {
        consumer.bindings.references.iter().any(|reference| {
            reference.provider_available == Some(true)
                && (reference.provider_path.as_deref() == Some(device.path.as_ref())
                    || device
                        .phandle
                        .is_some_and(|phandle| reference.phandle == phandle))
        })
    });
    let declares_provider = device.properties.iter().any(|property| {
        matches!(
            property.name.as_ref(),
            "#clock-cells"
                | "#reset-cells"
                | "#dma-cells"
                | "#iommu-cells"
                | "#phy-cells"
                | "#power-domain-cells"
                | "#interconnect-cells"
                | "#pwm-cells"
                | "#mbox-cells"
                | "#io-channel-cells"
                | "#thermal-sensor-cells"
                | "#sound-dai-cells"
                | "#msi-cells"
                | "#gpio-cells"
        )
    });
    if referenced || declares_provider {
        1
    } else {
        2
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlatformRegisterStatus {
    Bound,
    Unbound,
    Deferred,
    Failed,
}

struct PlatformRegisterOutcome {
    status: PlatformRegisterStatus,
    device: Option<Arc<PnpDevice>>,
}

struct RegisteredPlatformNode {
    path: Box<str>,
    parent_path: Option<Box<str>>,
    device: Arc<PnpDevice>,
}

fn remember_registered_platform_node(
    nodes: &mut Vec<RegisteredPlatformNode>,
    path: &str,
    parent_path: Option<&str>,
    device: Arc<PnpDevice>,
) {
    if nodes
        .iter()
        .any(|node| node.path.as_ref() == path && Arc::ptr_eq(&node.device, &device))
    {
        return;
    }
    nodes.push(RegisteredPlatformNode {
        path: path.into(),
        parent_path: parent_path.map(Into::into),
        device,
    });
}

fn register_platform_device_status(
    info: PlatformDeviceInfo,
    tag: &str,
    noisy_failure: bool,
) -> PlatformRegisterOutcome {
    match register_and_probe_platform_device(info) {
        Ok(reg) => match reg.status {
            PlatformProbeStatus::Bound => PlatformRegisterOutcome {
                status: PlatformRegisterStatus::Bound,
                device: Some(reg.device),
            },
            PlatformProbeStatus::NoDriver => {
                log::debug!(
                    "[kernel-start][{}] no platform driver for {}",
                    tag,
                    reg.device.id
                );
                PlatformRegisterOutcome {
                    status: PlatformRegisterStatus::Unbound,
                    device: Some(reg.device),
                }
            }
            PlatformProbeStatus::Deferred => {
                log::debug!(
                    "[kernel-start][{}] deferred platform probe for {}",
                    tag,
                    reg.device.id
                );
                PlatformRegisterOutcome {
                    status: PlatformRegisterStatus::Deferred,
                    device: Some(reg.device),
                }
            }
        },
        Err(err) => {
            if noisy_failure {
                printk!(
                    "[kernel-start][{}] failed to register/probe platform device: {:?}",
                    tag,
                    err
                );
            } else {
                log::debug!(
                    "[kernel-start][{}] deferred platform probe after failure: {:?}",
                    tag,
                    err
                );
            }
            PlatformRegisterOutcome {
                status: PlatformRegisterStatus::Failed,
                device: None,
            }
        }
    }
}

fn attach_platform_topology(nodes: &[RegisteredPlatformNode]) -> usize {
    let mut attached = 0usize;
    for child in nodes {
        let Some(parent_path) = child.parent_path.as_deref() else {
            continue;
        };
        let Some(parent) = nodes
            .iter()
            .find(|candidate| candidate.path.as_ref() == parent_path)
            .map(|candidate| Arc::clone(&candidate.device))
        else {
            continue;
        };
        if Arc::ptr_eq(&parent, &child.device) || child.device.parent().is_some() {
            continue;
        }
        match parent.attach_child(&child.device) {
            Ok(()) => attached += 1,
            Err(err) => {
                log::debug!(
                    "[kernel-start][dtb] failed to attach platform topology {} -> {}: {:?}",
                    child.path,
                    parent_path,
                    err
                );
            }
        }
    }
    attached
}
