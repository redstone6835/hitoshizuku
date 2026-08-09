//! SOYO 用户映像的内核装载基础。

use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;
use general::dev::random::{RandomReadMode, fill as fill_random};
use general::mm::{VmSpace, user_vm_layout};
use general::vfs::VfsContext;
use general::vfs::file::File;
use mm::{FileLike, VmFlags};
use native_abi::registry::RequirementId;
use native_abi::{
    NativeBindingPlan, RuntimeArrayInfo, StartInfoBuildError, StartInfoInput, TargetArch,
    build_start_info,
};
use soyo::{
    ImageSegment, SoyoError, SoyoMappedSegment, SoyoMetadata, SoyoReadAt, SoyoReadError,
    SoyoReadLimits, SoyoRuntimeLayoutInput, SoyoTargetPolicy, plan_mapped_segments,
    plan_runtime_layout, read_soyo,
    registry::{FeatureFlags, RelocationKind, SegmentKind, SegmentPermissions},
    validate_soyo,
};
use vfs::fdtable::FdTableSnapshot;

use crate::native_runtime::{
    ImageObject, NativeProcessState, PreparedNativeCapability, prepare_native_process_state,
    prepare_native_process_state_with_vfs,
};
use crate::user::{file_size, read_exact_file};

/// 已完成 SOYO 段映射与重定位、但尚未安装到任务的用户映像。
pub(crate) struct LoadedSoyoImage {
    pub(crate) vm: Arc<VmSpace>,
    pub(crate) entry_pc: usize,
    pub(crate) image_base: usize,
    pub(crate) metadata: Arc<SoyoMetadata>,
    pub(crate) native_binding: NativeBindingPlan,
    pub(crate) enabled_features: u64,
    tls_payload: Option<Vec<u8>>,
}

/// 已完成全部私有映射和 Native 资源准备的 SOYO 映像。
pub(crate) struct PreparedSoyoImage {
    pub(crate) vm: Arc<VmSpace>,
    pub(crate) entry_pc: usize,
    pub(crate) image_base: usize,
    pub(crate) image_end: usize,
    pub(crate) user_sp: usize,
    pub(crate) tls_base: usize,
    pub(crate) start_info_address: usize,
    pub(crate) start_info_size: usize,
    pub(crate) bootstrap_process: u64,
    pub(crate) personality: Arc<NativeProcessState>,
}

struct VfsSoyoReader {
    file: Arc<File>,
    size: u64,
}

impl SoyoReadAt for VfsSoyoReader {
    type Error = Errno;

    fn len(&self) -> u64 {
        self.size
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error> {
        read_exact_file(&self.file, offset, output)
    }
}

struct ExecutableImageReader<'a> {
    bytes: &'a [u8],
}

impl SoyoReadAt for ExecutableImageReader<'_> {
    type Error = Errno;

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error> {
        let start = usize::try_from(offset).map_err(|_| Errno::EIO)?;
        let end = start.checked_add(output.len()).ok_or(Errno::EIO)?;
        let source = self.bytes.get(start..end).ok_or(Errno::EIO)?;
        output.copy_from_slice(source);
        Ok(())
    }
}

fn current_target_arch() -> TargetArch {
    #[cfg(target_arch = "riscv64")]
    {
        TargetArch::Riscv64
    }
    #[cfg(target_arch = "loongarch64")]
    {
        TargetArch::LoongArch64
    }
}

const fn map_soyo_error(error: SoyoError) -> Errno {
    match error {
        SoyoError::ResourceExhausted(_) => Errno::E2BIG,
        SoyoError::AllocationFailed(_)
        | SoyoError::NativeAbi(native_abi::NativeAbiError::ResourceExhausted(_)) => Errno::ENOMEM,
        _ => Errno::ENOEXEC,
    }
}

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
const _: () = match map_soyo_error(SoyoError::ResourceExhausted(soyo::ResourceKind::FileSize)) {
    Errno::E2BIG => (),
    _ => panic!("SOYO 资源上限必须映射为 E2BIG"),
};

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
const _: () = match map_soyo_error(SoyoError::AllocationFailed(soyo::ResourceKind::TableBytes)) {
    Errno::ENOMEM => (),
    _ => panic!("SOYO 分配失败必须映射为 ENOMEM"),
};

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
const _: () = match map_soyo_error(SoyoError::NativeAbi(
    native_abi::NativeAbiError::ResourceExhausted(native_abi::ResourceKind::CallSlots),
)) {
    Errno::ENOMEM => (),
    _ => panic!("Native ABI 绑定分配失败必须映射为 ENOMEM"),
};

fn map_soyo_read_error(error: SoyoReadError<Errno>) -> Errno {
    match error {
        SoyoReadError::Source(error) => error,
        SoyoReadError::Format(error) => map_soyo_error(error),
        SoyoReadError::ResourceExhausted(_) => Errno::E2BIG,
        SoyoReadError::AllocationFailed(_) => Errno::ENOMEM,
    }
}

fn vm_flags_for_permissions(permissions: u16, writable: bool) -> VmFlags {
    let permissions = SegmentPermissions::from_bits(permissions);
    let mut flags = VmFlags::EMPTY.with(VmFlags::USER).with(VmFlags::READ);
    if writable && permissions.contains(SegmentPermissions::WRITE) {
        flags = flags.with(VmFlags::WRITE);
    }
    if permissions.contains(SegmentPermissions::EXECUTE) {
        flags = flags.with(VmFlags::EXEC);
    }
    flags
}

fn mapped_segment_for_index<'a>(
    metadata: &'a SoyoMetadata,
    index: u32,
) -> Result<&'a ImageSegment, Errno> {
    metadata.segments.get(index as usize).ok_or(Errno::ENOEXEC)
}

fn relocation_value(
    metadata: &SoyoMetadata,
    image_base: u64,
    kind: RelocationKind,
    source_segment_index: u32,
    addend: i64,
) -> Result<u64, Errno> {
    let addend = u64::try_from(addend).map_err(|_| Errno::ENOEXEC)?;
    let relative = match kind {
        RelocationKind::ImageBase64 => image_base,
        RelocationKind::SegmentBase64 => {
            let source = mapped_segment_for_index(metadata, source_segment_index)?;
            if source.kind == SegmentKind::TlsTemplate {
                return Err(Errno::ENOEXEC);
            }
            image_base
                .checked_add(source.virtual_offset)
                .ok_or(Errno::ENOEXEC)?
        }
    };
    relative.checked_add(addend).ok_or(Errno::ENOEXEC)
}

fn apply_relocations(vm: &VmSpace, metadata: &SoyoMetadata, image_base: u64) -> Result<(), Errno> {
    for relocation in &metadata.relocations {
        let target = mapped_segment_for_index(metadata, relocation.target_segment_index)?;
        if target.kind == SegmentKind::TlsTemplate {
            return Err(Errno::ENOEXEC);
        }
        let target = image_base
            .checked_add(target.virtual_offset)
            .and_then(|base| base.checked_add(relocation.target_offset))
            .ok_or(Errno::ENOEXEC)?;
        let value = relocation_value(
            metadata,
            image_base,
            relocation.kind,
            relocation.source_segment_index,
            relocation.addend,
        )?;
        let bytes = value.to_le_bytes();
        // commit_segment 已将所有页驻留，临时 RW 映射确保重定位不触发用户缺页。
        unsafe {
            vm.with_user_write_slice(target as usize, bytes.len(), |output| {
                output.copy_from_slice(&bytes);
            })?;
        }
    }
    Ok(())
}

fn seal_segments(vm: &VmSpace, segments: &[SoyoMappedSegment]) -> Result<(), Errno> {
    let page_size = usize::try_from(soyo::registry::PAGE_SIZE).map_err(|_| Errno::ENOEXEC)?;
    for segment in segments {
        let end = segment
            .virtual_start
            .checked_add(segment.memory_size)
            .and_then(|end| end.checked_add(page_size as u64 - 1))
            .ok_or(Errno::ENOEXEC)?;
        let end = end & !(page_size as u64 - 1);
        let start = usize::try_from(segment.virtual_start).map_err(|_| Errno::ENOEXEC)?;
        let end = usize::try_from(end).map_err(|_| Errno::ENOEXEC)?;
        vm.mprotect(
            start..end,
            vm_flags_for_permissions(segment.permissions, true),
        )?;
    }
    Ok(())
}

/// 从 VFS 文件构造 SOYO 用户映像。
pub(crate) fn load_soyo_image_from_file(file: Arc<File>) -> Result<LoadedSoyoImage, Errno> {
    let size = file_size(&file)?;
    let backing: Arc<dyn FileLike> = file.clone();
    let reader = VfsSoyoReader { file, size };
    load_soyo_image_from_reader_with_backing(&reader, Some(backing))
}

pub(crate) fn load_soyo_image_from_reader<R>(reader: &R) -> Result<LoadedSoyoImage, Errno>
where
    R: SoyoReadAt<Error = Errno>,
{
    load_soyo_image_from_reader_with_backing(reader, None)
}

fn load_soyo_image_from_reader_with_backing<R>(
    reader: &R,
    backing: Option<Arc<dyn FileLike>>,
) -> Result<LoadedSoyoImage, Errno>
where
    R: SoyoReadAt<Error = Errno>,
{
    let metadata = read_soyo(reader, SoyoReadLimits::portable()).map_err(map_soyo_read_error)?;
    let target_arch = current_target_arch();
    let policy = SoyoTargetPolicy::for_kernel(target_arch);
    let load_plan = validate_soyo(&metadata, policy).map_err(map_soyo_error)?;
    let binding = load_plan.native_binding;
    let enabled_features = load_plan.enabled_features;
    map_validated_soyo_image(
        reader,
        Arc::new(metadata),
        binding,
        enabled_features,
        backing,
    )
}

/// 从已经由 `image.create` 验证的不可变字节构造独立地址空间。
pub(crate) fn load_executable_image(image: &ImageObject) -> Result<LoadedSoyoImage, Errno> {
    let reader = ExecutableImageReader {
        bytes: image.bytes(),
    };
    map_validated_soyo_image(
        &reader,
        Arc::clone(&image.metadata),
        image.binding.clone(),
        image.enabled_features,
        Some(image.file_backing()),
    )
}

fn map_validated_soyo_image<R>(
    reader: &R,
    metadata: Arc<SoyoMetadata>,
    binding: NativeBindingPlan,
    enabled_features: u64,
    backing: Option<Arc<dyn FileLike>>,
) -> Result<LoadedSoyoImage, Errno>
where
    R: SoyoReadAt<Error = Errno>,
{
    let image_base = hal::user::main_pie_base();
    let image_base_u64 = u64::try_from(image_base).map_err(|_| Errno::ENOEXEC)?;
    let mapped = plan_mapped_segments(&metadata, image_base_u64).map_err(map_soyo_error)?;
    let vm = Arc::new(VmSpace::new());

    for (index, segment) in mapped.iter().enumerate() {
        let memory_size = usize::try_from(segment.memory_size).map_err(|_| Errno::ENOEXEC)?;
        let virtual_start = usize::try_from(segment.virtual_start).map_err(|_| Errno::ENOEXEC)?;
        let relocated = metadata
            .relocations
            .iter()
            .any(|relocation| relocation.target_segment_index as usize == index);
        if !relocated && let Some(backing) = backing.as_ref() {
            vm.commit_file_segment(
                virtual_start,
                memory_size,
                segment.file_offset,
                usize::try_from(segment.file_size).map_err(|_| Errno::ENOEXEC)?,
                Arc::clone(backing),
                vm_flags_for_permissions(segment.permissions, true),
            )?;
        } else {
            let payload = read_segment_payload(reader, segment.file_offset, segment.file_size)?;
            vm.commit_segment(
                virtual_start,
                memory_size,
                payload.len(),
                &payload,
                VmFlags::EMPTY
                    .with(VmFlags::USER)
                    .with(VmFlags::READ)
                    .with(VmFlags::WRITE),
            )?;
        }
    }

    apply_relocations(&vm, &metadata, image_base_u64)?;
    seal_segments(&vm, &mapped)?;
    let tls_payload = metadata
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::TlsTemplate)
        .map(|segment| read_segment_payload(reader, segment.file_offset, segment.file_size))
        .transpose()?;

    let entry_pc = image_base
        .checked_add(usize::try_from(metadata.header.entry_offset).map_err(|_| Errno::ENOEXEC)?)
        .ok_or(Errno::ENOEXEC)?;
    Ok(LoadedSoyoImage {
        vm,
        entry_pc,
        image_base,
        metadata,
        native_binding: binding,
        enabled_features,
        tls_payload,
    })
}

/// 补齐 SOYO 的运行时映射、初始 handle、StartInfo 与 personality payload。
pub(crate) fn prepare_soyo_runtime(
    loaded: LoadedSoyoImage,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    descriptors: Option<&FdTableSnapshot>,
) -> Result<PreparedSoyoImage, Errno> {
    prepare_soyo_runtime_with_vfs(loaded, argv, envp, descriptors, &[], None)
}

pub(crate) fn prepare_soyo_runtime_with_capabilities(
    loaded: LoadedSoyoImage,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    descriptors: Option<&FdTableSnapshot>,
    transferred: &[PreparedNativeCapability],
) -> Result<PreparedSoyoImage, Errno> {
    prepare_soyo_runtime_with_vfs(loaded, argv, envp, descriptors, transferred, None)
}

pub(crate) fn prepare_soyo_runtime_with_vfs(
    loaded: LoadedSoyoImage,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    descriptors: Option<&FdTableSnapshot>,
    transferred: &[PreparedNativeCapability],
    vfs_context: Option<Arc<VfsContext>>,
) -> Result<PreparedSoyoImage, Errno> {
    let LoadedSoyoImage {
        vm,
        entry_pc,
        image_base,
        metadata,
        native_binding,
        enabled_features,
        tls_payload,
    } = loaded;
    let runtime = metadata.runtime.as_ref().ok_or(Errno::ENOEXEC)?;
    let call_slot_count =
        u32::try_from(native_binding.call_slots.len()).map_err(|_| Errno::ENOEXEC)?;
    let (personality, initial_handles) = if transferred.is_empty() && vfs_context.is_none() {
        prepare_native_process_state(&metadata, native_binding, &vm, image_base, descriptors)?
    } else {
        prepare_native_process_state_with_vfs(
            &metadata,
            native_binding,
            &vm,
            image_base,
            descriptors,
            transferred,
            vfs_context,
        )?
    };
    let bootstrap_process = initial_handles
        .iter()
        .find(|record| record.requirement_id == RequirementId::SelfProcess)
        .map(|record| record.handle.raw())
        .ok_or(Errno::ENOEXEC)?;
    let tls = metadata
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::TlsTemplate);
    let static_tls_size = tls
        .map(|segment| checked_align_up(segment.memory_size, segment.alignment))
        .transpose()?
        .unwrap_or(0);
    let dynamic_components = enabled_features & FeatureFlags::DYNAMIC_COMPONENTS.bits() != 0;
    let initial_tls_size = if dynamic_components {
        crate::native_runtime::DYNAMIC_TLS_ARENA_SIZE as u64
    } else {
        static_tls_size
    };
    let random_seed = start_random_seed()?;
    let provisional_tls_base = if initial_tls_size != 0 {
        soyo::registry::PAGE_SIZE
    } else {
        0
    };
    let provisional = build_start_info(StartInfoInput {
        target_arch: metadata.header.target_arch,
        enabled_features,
        image_base: image_base as u64,
        initial_tls_base: provisional_tls_base,
        initial_tls_size,
        initial_thread_pointer: provisional_tls_base,
        argv,
        env: envp,
        initial_handles: &initial_handles,
        call_slot_count,
        random_seed,
        runtime_flags: runtime.runtime_flags,
        init_array: RuntimeArrayInfo {
            offset: runtime.init_array_offset,
            count: runtime.init_array_count,
            entry_size: runtime.init_array_entry_size,
        },
        fini_array: RuntimeArrayInfo {
            offset: runtime.fini_array_offset,
            count: runtime.fini_array_count,
            entry_size: runtime.fini_array_entry_size,
        },
        max_size: runtime.start_info_max_size,
    })
    .map_err(map_start_info_error)?;
    let vm_layout = user_vm_layout().ok_or(Errno::EIO)?;
    let process_layout = plan_runtime_layout(SoyoRuntimeLayoutInput {
        image_base: image_base as u64,
        image_virtual_size: metadata.header.image_virtual_size,
        stack_top: vm_layout.default_stack_top as u64,
        stack_size: runtime.stack_size,
        stack_guard_size: runtime.stack_guard_size,
        tls_memory_size: initial_tls_size,
        tls_alignment: if initial_tls_size == 0 {
            0
        } else {
            soyo::registry::PAGE_SIZE
        },
        start_info_size: provisional.as_bytes().len() as u64,
        user_lower_bound: soyo::registry::PAGE_SIZE,
    })
    .map_err(map_soyo_error)?;
    let tls_base = process_layout.tls.as_ref().map_or(0, |range| range.start);
    let start_info = build_start_info(StartInfoInput {
        target_arch: metadata.header.target_arch,
        enabled_features,
        image_base: image_base as u64,
        initial_tls_base: tls_base,
        initial_tls_size: process_layout.initial_tls_size,
        initial_thread_pointer: tls_base,
        argv,
        env: envp,
        initial_handles: &initial_handles,
        call_slot_count,
        random_seed,
        runtime_flags: runtime.runtime_flags,
        init_array: RuntimeArrayInfo {
            offset: runtime.init_array_offset,
            count: runtime.init_array_count,
            entry_size: runtime.init_array_entry_size,
        },
        fini_array: RuntimeArrayInfo {
            offset: runtime.fini_array_offset,
            count: runtime.fini_array_count,
            entry_size: runtime.fini_array_entry_size,
        },
        max_size: runtime.start_info_max_size,
    })
    .map_err(map_start_info_error)?;
    if start_info.as_bytes().len() != provisional.as_bytes().len() {
        return Err(Errno::EIO);
    }

    let stack = usize_range(&process_layout.stack)?;
    vm.map_anon(
        stack.clone(),
        VmFlags::EMPTY
            .with(VmFlags::USER)
            .with(VmFlags::READ)
            .with(VmFlags::WRITE),
    )?;
    match (tls, tls_payload.as_deref(), process_layout.tls.as_ref()) {
        (template, payload, Some(tls_range)) => {
            map_tls_arena(&vm, payload, template, tls_range)?;
        }
        (None, None, None) => {}
        _ => return Err(Errno::EIO),
    }
    let start_info_range = usize_range(&process_layout.start_info)?;
    vm.commit_segment(
        start_info_range.start,
        start_info_range.len(),
        start_info.as_bytes().len(),
        start_info.as_bytes(),
        VmFlags::EMPTY
            .with(VmFlags::USER)
            .with(VmFlags::READ)
            .with(VmFlags::WRITE),
    )?;
    vm.mprotect(
        start_info_range.clone(),
        VmFlags::EMPTY.with(VmFlags::USER).with(VmFlags::READ),
    )?;
    let tls_runtime_range = process_layout.tls.as_ref().map(usize_range).transpose()?;
    let static_tls_used = if static_tls_size == 0 {
        0
    } else {
        usize::try_from(checked_align_up(
            static_tls_size,
            soyo::registry::PAGE_SIZE,
        )?)
        .map_err(|_| Errno::ENOEXEC)?
    };
    personality
        .components
        .install_tls_arena(tls_runtime_range.clone(), static_tls_used)
        .map_err(|_| Errno::EIO)?;
    personality.install_runtime_ranges(stack.clone(), start_info_range.clone(), tls_runtime_range);

    Ok(PreparedSoyoImage {
        vm,
        entry_pc,
        image_base,
        image_end: usize::try_from(process_layout.image.end).map_err(|_| Errno::ENOEXEC)?,
        user_sp: stack.end,
        tls_base: usize::try_from(tls_base).map_err(|_| Errno::ENOEXEC)?,
        start_info_address: start_info_range.start,
        start_info_size: start_info.as_bytes().len(),
        bootstrap_process,
        personality,
    })
}

fn map_tls_arena(
    vm: &VmSpace,
    payload: Option<&[u8]>,
    template: Option<&ImageSegment>,
    range: &core::ops::Range<u64>,
) -> Result<(), Errno> {
    let mapped = usize_range(range)?;
    vm.map_anon(
        mapped.clone(),
        VmFlags::EMPTY
            .with(VmFlags::USER)
            .with(VmFlags::READ)
            .with(VmFlags::WRITE),
    )?;
    match (payload, template) {
        (Some(payload), Some(template)) => {
            let file_size = usize::try_from(template.file_size).map_err(|_| Errno::ENOEXEC)?;
            if payload.len() != file_size || file_size > mapped.len() {
                return Err(Errno::EIO);
            }
            let mut source = payload;
            let mut address = mapped.start;
            while !source.is_empty() {
                let copied = unsafe {
                    vm.with_user_write_slice(address, source.len(), |target| {
                        target.copy_from_slice(&source[..target.len()]);
                        target.len()
                    })
                }?;
                address = address.checked_add(copied).ok_or(Errno::ENOEXEC)?;
                source = &source[copied..];
            }
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(Errno::EIO),
    }
}

fn read_segment_payload<R>(reader: &R, offset: u64, size: u64) -> Result<Vec<u8>, Errno>
where
    R: SoyoReadAt<Error = Errno>,
{
    let size = usize::try_from(size).map_err(|_| Errno::ENOEXEC)?;
    let mut payload = Vec::new();
    payload.try_reserve_exact(size).map_err(|_| Errno::ENOMEM)?;
    payload.resize(size, 0);
    if size != 0 {
        reader.read_exact_at(offset, &mut payload)?;
    }
    Ok(payload)
}

fn start_random_seed() -> Result<[u8; 32], Errno> {
    let mut seed = [0u8; 32];
    let mut filled = 0usize;
    while filled < seed.len() {
        let produced = fill_random(
            &mut seed[filled..],
            // 架构启动熵会播种 CSPRNG，但未必具备可计量的安全熵；exec 不能因此
            // 永久等待 secure-ready。StartInfo 只要求每次使用非固定启动随机种子。
            RandomReadMode::Insecure,
        )
        .map_err(|_| Errno::EIO)?;
        if produced == 0 || produced > seed.len() - filled {
            return Err(Errno::EIO);
        }
        filled += produced;
    }
    if seed.iter().all(|byte| *byte == 0) {
        return Err(Errno::EIO);
    }
    Ok(seed)
}

fn checked_align_up(value: u64, alignment: u64) -> Result<u64, Errno> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(Errno::ENOEXEC)
}

fn usize_range(range: &core::ops::Range<u64>) -> Result<core::ops::Range<usize>, Errno> {
    Ok(usize::try_from(range.start).map_err(|_| Errno::ENOEXEC)?
        ..usize::try_from(range.end).map_err(|_| Errno::ENOEXEC)?)
}

const fn map_start_info_error(error: StartInfoBuildError) -> Errno {
    match error {
        StartInfoBuildError::ResourceExhausted => Errno::ENOMEM,
        StartInfoBuildError::TooLarge => Errno::E2BIG,
        StartInfoBuildError::InvalidInput => Errno::ENOEXEC,
    }
}

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
const _: () = match map_start_info_error(StartInfoBuildError::TooLarge) {
    Errno::E2BIG => (),
    _ => panic!("StartInfo 大小超限必须映射为 E2BIG"),
};

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
const _: () = match map_start_info_error(StartInfoBuildError::ResourceExhausted) {
    Errno::ENOMEM => (),
    _ => panic!("StartInfo 分配失败必须映射为 ENOMEM"),
};

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
mod test_image;
#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
mod tests;
