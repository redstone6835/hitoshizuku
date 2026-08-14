//! Native MemoryObject 与显式地址空间映射。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use general::dev::dma::{DmaBuffer, DmaDirection};
use general::mm::VmSpace;
use general::syscall::NativeCallOutcome;
use mm::{FileLike, SharedAnonObject, VmFlags};
use native_abi::wire::{MemoryCreateRequest, MemoryInfo, MemoryMapRequest, MemoryStatistics};
use native_abi::{NativeHandle, ObjectInterface, Rights, status, wire};
use sched::sync::Spinlock;
use sched::{Task, WaitQueue};

use super::dispatch::native_return;
use super::operations::insert_native_handle;
use super::{
    KernelNativeObject, NativeProcessState, copy_user_value, copy_user_value_out, task_vm,
};

enum MemoryBacking {
    Anonymous(Arc<SharedAnonObject>),
    File {
        file: Arc<dyn FileLike>,
        offset: u64,
    },
    Dma(Spinlock<DmaBuffer>),
}

pub(crate) struct MemoryObject {
    size: u64,
    alignment: u64,
    kind: u32,
    flags: u32,
    generation: AtomicU64,
    mapping_count: AtomicU32,
    state: AtomicU32,
    read_operations: AtomicU64,
    write_operations: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    writeback_operations: AtomicU64,
    lifecycle: Spinlock<()>,
    active_accesses: AtomicU32,
    access_waiters: WaitQueue,
    mappings: Spinlock<Vec<NativeMemoryMapping>>,
    backing: MemoryBacking,
    source_size: AtomicU64,
}

pub(super) struct MemoryMappingRegistry {
    objects: Spinlock<Vec<Arc<MemoryObject>>>,
}

impl MemoryMappingRegistry {
    pub(super) const fn new() -> Self {
        Self {
            objects: Spinlock::new(Vec::new()),
        }
    }

    fn remove_object(&self, object: &Arc<MemoryObject>) {
        remove_object_index(&mut self.objects.lock(), object);
    }
}

pub(super) struct MemoryAccessGuard<'a> {
    object: &'a MemoryObject,
}

impl MemoryAccessGuard<'_> {
    pub(super) fn read_into(&self, offset: u64, output: &mut [u8]) -> Result<(), u32> {
        self.object.read_into_guarded(offset, output)
    }

    pub(super) fn write_from(&self, offset: u64, input: &[u8]) -> Result<(), u32> {
        self.object.write_from_guarded(offset, input)
    }
}

impl Drop for MemoryAccessGuard<'_> {
    fn drop(&mut self) {
        if self.object.active_accesses.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.object.access_waiters.wake_all();
        }
    }
}

impl MemoryObject {
    fn anonymous(size: u64, alignment: u64, flags: u32) -> Self {
        Self {
            size,
            alignment,
            kind: wire::MEMORY_KIND_ANONYMOUS,
            flags,
            generation: AtomicU64::new(1),
            mapping_count: AtomicU32::new(0),
            state: AtomicU32::new(wire::MEMORY_STATE_ACTIVE),
            read_operations: AtomicU64::new(0),
            write_operations: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            writeback_operations: AtomicU64::new(0),
            lifecycle: Spinlock::new(()),
            active_accesses: AtomicU32::new(0),
            access_waiters: WaitQueue::new(),
            mappings: Spinlock::new(Vec::new()),
            backing: MemoryBacking::Anonymous(Arc::new(SharedAnonObject::new())),
            source_size: AtomicU64::new(0),
        }
    }

    pub(super) fn file(
        size: u64,
        alignment: u64,
        flags: u32,
        file: Arc<dyn FileLike>,
        offset: u64,
        source_size: u64,
    ) -> Self {
        Self {
            size,
            alignment,
            kind: wire::MEMORY_KIND_FILE,
            flags,
            generation: AtomicU64::new(1),
            mapping_count: AtomicU32::new(0),
            state: AtomicU32::new(wire::MEMORY_STATE_ACTIVE),
            read_operations: AtomicU64::new(0),
            write_operations: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            writeback_operations: AtomicU64::new(0),
            lifecycle: Spinlock::new(()),
            active_accesses: AtomicU32::new(0),
            access_waiters: WaitQueue::new(),
            mappings: Spinlock::new(Vec::new()),
            backing: MemoryBacking::File { file, offset },
            source_size: AtomicU64::new(source_size),
        }
    }

    fn dma(size: u64, alignment: u64, flags: u32, buffer: DmaBuffer) -> Self {
        Self {
            size,
            alignment,
            kind: wire::MEMORY_KIND_DMA,
            flags,
            generation: AtomicU64::new(1),
            mapping_count: AtomicU32::new(0),
            state: AtomicU32::new(wire::MEMORY_STATE_ACTIVE),
            read_operations: AtomicU64::new(0),
            write_operations: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            writeback_operations: AtomicU64::new(0),
            lifecycle: Spinlock::new(()),
            active_accesses: AtomicU32::new(0),
            access_waiters: WaitQueue::new(),
            mappings: Spinlock::new(Vec::new()),
            backing: MemoryBacking::Dma(Spinlock::new(buffer)),
            source_size: AtomicU64::new(0),
        }
    }

    fn note_mapped(&self) {
        self.mapping_count.fetch_add(1, Ordering::AcqRel);
    }

    fn note_unmapped(&self) {
        self.mapping_count.fetch_sub(1, Ordering::AcqRel);
    }

    fn info(&self) -> MemoryInfo {
        MemoryInfo {
            size: self.size,
            alignment: self.alignment,
            kind: self.kind,
            flags: self.flags,
            mapping_count: self.mapping_count.load(Ordering::Acquire),
            state: self.state.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
            source_size: self.source_size.load(Ordering::Acquire),
            reserved: [0; 2],
        }
    }

    pub(super) fn size(&self) -> u64 {
        self.size
    }

    pub(super) fn is_file_backed(&self) -> bool {
        matches!(&self.backing, MemoryBacking::File { .. })
    }

    fn valid_map_range(&self, offset: u64, length: u64) -> bool {
        if offset.checked_add(length).is_none_or(|end| end > self.size) {
            return false;
        }
        match &self.backing {
            MemoryBacking::File {
                offset: source_offset,
                ..
            } => source_offset
                .checked_add(offset)
                .and_then(|start| start.checked_add(length))
                .is_some_and(|end| end <= self.source_size.load(Ordering::Acquire)),
            _ => true,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(super) fn access_status(&self) -> Result<(), u32> {
        match self.state.load(Ordering::Acquire) {
            wire::MEMORY_STATE_ACTIVE => Ok(()),
            wire::MEMORY_STATE_REVOKED => Err(status::MEMORY_REVOKED),
            _ => Err(status::MEMORY_POISONED),
        }
    }

    pub(super) fn begin_access(&self) -> Result<MemoryAccessGuard<'_>, u32> {
        self.access_status()?;
        self.active_accesses.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = self.access_status() {
            if self.active_accesses.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.access_waiters.wake_all();
            }
            return Err(error);
        }
        Ok(MemoryAccessGuard { object: self })
    }

    pub(super) fn active_generation(&self) -> Result<u64, u32> {
        let _lifecycle = self.lifecycle.lock();
        self.access_status()?;
        Ok(self.generation())
    }

    pub(super) fn read_into(&self, offset: u64, output: &mut [u8]) -> Result<(), u32> {
        let access = self.begin_access()?;
        access.read_into(offset, output)
    }

    fn read_into_guarded(&self, offset: u64, output: &mut [u8]) -> Result<(), u32> {
        self.validate_transfer(offset, output.len())?;
        match &self.backing {
            MemoryBacking::Anonymous(backing) => {
                general::mm::read_shared_anon(backing, offset, output).map_err(map_backing_error)
            }
            MemoryBacking::File {
                file,
                offset: source_offset,
            } => {
                let start = source_offset
                    .checked_add(offset)
                    .ok_or(status::MEMORY_INVALID_RANGE)?;
                transfer_file_backing(file.as_ref(), start, output)
            }
            MemoryBacking::Dma(buffer) => {
                let buffer = buffer.lock();
                buffer.sync_for_cpu();
                let start = usize::try_from(offset).map_err(|_| status::MEMORY_INVALID_RANGE)?;
                let end = start
                    .checked_add(output.len())
                    .ok_or(status::MEMORY_INVALID_RANGE)?;
                output.copy_from_slice(&buffer.as_slice()[start..end]);
                Ok(())
            }
        }?;
        self.read_operations.fetch_add(1, Ordering::Relaxed);
        self.bytes_read
            .fetch_add(output.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn write_from(&self, offset: u64, input: &[u8]) -> Result<(), u32> {
        let access = self.begin_access()?;
        access.write_from(offset, input)
    }

    fn write_from_guarded(&self, offset: u64, input: &[u8]) -> Result<(), u32> {
        self.validate_transfer(offset, input.len())?;
        let writeback = matches!(&self.backing, MemoryBacking::File { .. });
        match &self.backing {
            MemoryBacking::Anonymous(backing) => {
                general::mm::write_shared_anon(backing, offset, input).map_err(map_backing_error)
            }
            MemoryBacking::File {
                file,
                offset: source_offset,
            } => {
                let start = source_offset
                    .checked_add(offset)
                    .ok_or(status::MEMORY_INVALID_RANGE)?;
                transfer_file_backing_mut(file.as_ref(), start, input)
            }
            MemoryBacking::Dma(buffer) => {
                let mut buffer = buffer.lock();
                let start = usize::try_from(offset).map_err(|_| status::MEMORY_INVALID_RANGE)?;
                let end = start
                    .checked_add(input.len())
                    .ok_or(status::MEMORY_INVALID_RANGE)?;
                buffer.as_mut_slice()[start..end].copy_from_slice(input);
                buffer.sync_for_device();
                Ok(())
            }
        }?;
        self.write_operations.fetch_add(1, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(input.len() as u64, Ordering::Relaxed);
        if writeback {
            self.writeback_operations.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(super) fn validate_transfer(&self, offset: u64, length: usize) -> Result<(), u32> {
        let length = u64::try_from(length).map_err(|_| status::MEMORY_INVALID_RANGE)?;
        if offset.checked_add(length).is_none_or(|end| end > self.size) {
            return Err(status::MEMORY_INVALID_RANGE);
        }
        Ok(())
    }

    pub(super) fn invalidate_file_mappings_after_resize(&self, source_size: u64) {
        let MemoryBacking::File {
            offset: source_offset,
            ..
        } = &self.backing
        else {
            return;
        };
        self.source_size.store(source_size, Ordering::Release);
        let valid = source_size.saturating_sub(*source_offset).min(self.size);
        let page_size = general::mm::page_size() as u64;
        let first_invalid = valid / page_size * page_size;
        let mappings = self.mappings.lock();
        for mapping in mappings.iter() {
            let mapping_length = mapping.range.end.saturating_sub(mapping.range.start) as u64;
            let Some(mapping_end) = mapping.object_offset.checked_add(mapping_length) else {
                continue;
            };
            let invalid_start = first_invalid.max(mapping.object_offset);
            if invalid_start >= mapping_end {
                continue;
            }
            let Some(vm) = mapping.vm.upgrade() else {
                continue;
            };
            let Ok(relative) = usize::try_from(invalid_start - mapping.object_offset) else {
                continue;
            };
            let Some(start) = mapping.range.start.checked_add(relative) else {
                continue;
            };
            let _ = vm.discard_resident_range(start..mapping.range.end);
        }
    }

    fn statistics(self: &Arc<Self>) -> Result<MemoryStatistics, u32> {
        let page_size = general::mm::page_size();
        let page_count =
            usize::try_from(self.size).map_err(|_| status::MEMORY_INVALID_RANGE)? / page_size;
        let mut materialized = Vec::new();
        materialized
            .try_reserve_exact(page_count)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        materialized.resize(page_count, false);

        let mut mapped_pages = 0u64;
        let mut resident_mappings = 0u64;
        let mappings = self.mappings.lock();
        for mapping in mappings.iter() {
            let Some(vm) = mapping.vm.upgrade() else {
                continue;
            };
            let bitmap = vm
                .resident_bitmap(mapping.range.clone())
                .map_err(map_vm_error)?;
            mapped_pages = mapped_pages.saturating_add(bitmap.len() as u64);
            let first_page = usize::try_from(mapping.object_offset)
                .map_err(|_| status::MEMORY_INVALID_RANGE)?
                / page_size;
            for (index, resident) in bitmap.into_iter().enumerate() {
                if resident == 0 {
                    continue;
                }
                resident_mappings = resident_mappings.saturating_add(1);
                let object_page = first_page
                    .checked_add(index)
                    .ok_or(status::MEMORY_INVALID_RANGE)?;
                let Some(seen) = materialized.get_mut(object_page) else {
                    return Err(status::MEMORY_INVALID_RANGE);
                };
                *seen = true;
            }
        }
        let materialized_pages = materialized.iter().filter(|resident| **resident).count() as u64;
        Ok(MemoryStatistics {
            materialized_pages,
            resident_mappings,
            mapped_pages,
            shared_resident_mappings: resident_mappings.saturating_sub(materialized_pages),
            read_operations: self.read_operations.load(Ordering::Relaxed),
            write_operations: self.write_operations.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            writeback_operations: self.writeback_operations.load(Ordering::Relaxed),
            reserved: 0,
        })
    }
}

fn transfer_file_backing(file: &dyn FileLike, offset: u64, output: &mut [u8]) -> Result<(), u32> {
    let mut done = 0usize;
    while done < output.len() {
        let current = offset
            .checked_add(u64::try_from(done).map_err(|_| status::MEMORY_INVALID_RANGE)?)
            .ok_or(status::MEMORY_INVALID_RANGE)?;
        let count = file
            .read_at(current, &mut output[done..])
            .map_err(|_| status::FILESYSTEM_ERROR)?;
        if count == 0 || count > output.len() - done {
            return Err(status::FILESYSTEM_END);
        }
        done += count;
    }
    Ok(())
}

fn transfer_file_backing_mut(file: &dyn FileLike, offset: u64, input: &[u8]) -> Result<(), u32> {
    let mut done = 0usize;
    while done < input.len() {
        let current = offset
            .checked_add(u64::try_from(done).map_err(|_| status::MEMORY_INVALID_RANGE)?)
            .ok_or(status::MEMORY_INVALID_RANGE)?;
        let count = file
            .write_at(current, &input[done..])
            .map_err(|_| status::FILESYSTEM_ERROR)?;
        if count == 0 || count > input.len() - done {
            return Err(status::FILESYSTEM_ERROR);
        }
        done += count;
    }
    Ok(())
}

fn map_backing_error(error: errno::Errno) -> u32 {
    match error {
        errno::Errno::ENOMEM => status::CORE_RESOURCE_EXHAUSTED,
        _ => status::MEMORY_INVALID_RANGE,
    }
}

pub(crate) struct NativeMemoryMapping {
    owner_id: u64,
    owner_registry: Weak<MemoryMappingRegistry>,
    vm: Weak<VmSpace>,
    range: Range<usize>,
    object_offset: u64,
}

/// 内核为 Native 线程建立的 MemoryObject 映射租约。
pub(super) struct InternalMemoryMapping {
    vm: Arc<VmSpace>,
    object: Arc<MemoryObject>,
    pub(super) range: Range<usize>,
}

pub(super) fn map_internal_rw(
    state: &NativeProcessState,
    vm: &Arc<VmSpace>,
    object: &Arc<MemoryObject>,
    offset: u64,
    length: u64,
) -> Result<InternalMemoryMapping, u32> {
    if offset % native_abi::PAGE_SIZE != 0
        || length == 0
        || length % native_abi::PAGE_SIZE != 0
        || offset
            .checked_add(length)
            .is_none_or(|end| end > object.size)
    {
        return Err(status::MEMORY_INVALID_RANGE);
    }
    let _access = object.begin_access()?;
    let length = usize::try_from(length).map_err(|_| status::MEMORY_INVALID_RANGE)?;
    let alignment =
        usize::try_from(object.alignment).map_err(|_| status::MEMORY_INVALID_ALIGNMENT)?;
    let flags =
        VmFlags::from_bits(VmFlags::USER | VmFlags::READ | VmFlags::WRITE | VmFlags::SHARED);
    let range = match &object.backing {
        MemoryBacking::Anonymous(backing) => vm
            .map_shared_anon_any_aligned(length, alignment, Arc::clone(backing), offset, flags)
            .map_err(map_vm_error)?,
        MemoryBacking::File { .. } => return Err(status::MEMORY_INVALID_RANGE),
        MemoryBacking::Dma(_) => return Err(status::MEMORY_INVALID_RANGE),
    };

    if register_mapping(state, object, vm, range.clone(), offset).is_err() {
        let _ = vm.unmap_existing(range);
        return Err(status::CORE_RESOURCE_EXHAUSTED);
    }
    Ok(InternalMemoryMapping {
        vm: Arc::clone(vm),
        object: Arc::clone(object),
        range,
    })
}

pub(super) fn release_internal_mapping(state: &NativeProcessState, mapping: InternalMemoryMapping) {
    let mut objects = state.mapped_memory_objects.objects.lock();
    let mut mappings = mapping.object.mappings.lock();
    if let Some(index) = mappings.iter().position(|candidate| {
        candidate.owner_id == state.memory_owner_id
            && candidate.range == mapping.range
            && candidate
                .vm
                .upgrade()
                .is_some_and(|vm| Arc::ptr_eq(&vm, &mapping.vm))
    }) {
        mappings.swap_remove(index);
        remove_object_index(&mut objects, &mapping.object);
        mapping.object.note_unmapped();
    }
    drop(mappings);
    drop(objects);
    let _ = mapping.vm.unmap_existing(mapping.range);
}

fn register_mapping(
    state: &NativeProcessState,
    object: &Arc<MemoryObject>,
    vm: &Arc<VmSpace>,
    range: Range<usize>,
    object_offset: u64,
) -> Result<(), ()> {
    let mut objects = state.mapped_memory_objects.objects.lock();
    objects.try_reserve(1).map_err(|_| ())?;
    let mut mappings = object.mappings.lock();
    mappings.try_reserve(1).map_err(|_| ())?;
    objects.push(Arc::clone(object));
    mappings.push(NativeMemoryMapping {
        owner_id: state.memory_owner_id,
        owner_registry: Arc::downgrade(&state.mapped_memory_objects),
        vm: Arc::downgrade(vm),
        range,
        object_offset,
    });
    object.note_mapped();
    Ok(())
}

fn remove_object_index(objects: &mut Vec<Arc<MemoryObject>>, object: &Arc<MemoryObject>) {
    if let Some(index) = objects
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, object))
    {
        objects.swap_remove(index);
    }
}

pub(super) fn release_process_mappings(state: &NativeProcessState) {
    loop {
        let object = {
            let mut objects = state.mapped_memory_objects.objects.lock();
            objects.pop()
        };
        let Some(object) = object else {
            if state.mapped_memory_objects.objects.lock().is_empty() {
                return;
            }
            continue;
        };
        loop {
            let mapping = {
                let mut mappings = object.mappings.lock();
                mappings
                    .iter()
                    .position(|mapping| mapping.owner_id == state.memory_owner_id)
                    .map(|index| mappings.swap_remove(index))
            };
            let Some(mapping) = mapping else {
                break;
            };
            if let Some(vm) = mapping.vm.upgrade() {
                let _ = vm.unmap_existing(mapping.range);
            }
            object.note_unmapped();
        }
    }
}

pub(super) fn memory_create(
    task: &Arc<sched::Task>,
    state: &NativeProcessState,
    object: &KernelNativeObject,
    user: u64,
) -> NativeCallOutcome {
    if !matches!(object, KernelNativeObject::SelfProcess) {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    }
    let request = match copy_user_value::<MemoryCreateRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    if request.reserved != [0; 3] {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let Some(size) = round_to_page(request.size) else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    if !valid_alignment(request.alignment) {
        return native_return(status::MEMORY_INVALID_ALIGNMENT, 0, 0);
    }
    let object = match request.kind {
        wire::MEMORY_KIND_ANONYMOUS
            if request.flags & !wire::MEMORY_FLAG_SHARED == 0
                && request.source_handle == 0
                && request.source_offset == 0 =>
        {
            Arc::new(MemoryObject::anonymous(
                size,
                request.alignment,
                request.flags,
            ))
        }
        wire::MEMORY_KIND_DMA
            if request.flags
                & !(wire::MEMORY_FLAG_DEVICE_READ | wire::MEMORY_FLAG_DEVICE_WRITE)
                == 0
                && request.flags
                    & (wire::MEMORY_FLAG_DEVICE_READ | wire::MEMORY_FLAG_DEVICE_WRITE)
                    != 0
                && request.source_handle != 0
                && request.source_offset == 0 =>
        {
            let device = NativeHandle::from_raw(request.source_handle);
            let context = {
                let handles = state.handles.lock();
                let entry = match handles.lookup(
                    device,
                    Some(ObjectInterface::DeviceFunction),
                    Rights::MAP,
                ) {
                    Ok(entry) => entry,
                    Err(error) => return native_return(error, 0, 0),
                };
                let KernelNativeObject::DeviceFunction(device) = entry.object else {
                    return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
                };
                let Some(context) = device.dma_context() else {
                    return native_return(status::DEVICE_UNSUPPORTED, 0, 0);
                };
                context
            };
            let direction = match request.flags
                & (wire::MEMORY_FLAG_DEVICE_READ | wire::MEMORY_FLAG_DEVICE_WRITE)
            {
                wire::MEMORY_FLAG_DEVICE_READ => DmaDirection::ToDevice,
                wire::MEMORY_FLAG_DEVICE_WRITE => DmaDirection::FromDevice,
                _ => DmaDirection::Bidirectional,
            };
            let (Ok(size_usize), Ok(alignment)) =
                (usize::try_from(size), usize::try_from(request.alignment))
            else {
                return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
            };
            let buffer = match DmaBuffer::new_in(context, size_usize, alignment, direction) {
                Ok(buffer) => buffer,
                Err(_) => return native_return(status::DEVICE_UNSUPPORTED, 0, 0),
            };
            Arc::new(MemoryObject::dma(
                size,
                request.alignment,
                request.flags,
                buffer,
            ))
        }
        _ => return native_return(status::CORE_INVALID_ARGUMENT, 0, 0),
    };
    insert_native_handle(
        state,
        KernelNativeObject::MemoryObject(object),
        ObjectInterface::MemoryObject,
        Rights::READ
            | Rights::WRITE
            | Rights::DUPLICATE
            | Rights::MAP
            | Rights::MODIFY
            | Rights::INSPECT,
    )
}

pub(super) fn memory_map(
    task: &Arc<sched::Task>,
    state: &NativeProcessState,
    object: &Arc<MemoryObject>,
    handle_rights: Rights,
    user: u64,
) -> NativeCallOutcome {
    let request = match copy_user_value::<MemoryMapRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    if object.is_file_backed() {
        return super::fs::with_file_mapping_lock(|| {
            memory_map_inner(state, object, handle_rights, request)
        });
    }
    memory_map_inner(state, object, handle_rights, request)
}

fn memory_map_inner(
    state: &NativeProcessState,
    object: &Arc<MemoryObject>,
    handle_rights: Rights,
    request: MemoryMapRequest,
) -> NativeCallOutcome {
    if request.reserved != [0; 2]
        || request.flags & !wire::MEMORY_MAP_FIXED != 0
        || request.permissions == 0
        || request.permissions
            & !(wire::MEMORY_PERMISSION_READ
                | wire::MEMORY_PERMISSION_WRITE
                | wire::MEMORY_PERMISSION_EXECUTE)
            != 0
        || request.permissions & (wire::MEMORY_PERMISSION_WRITE | wire::MEMORY_PERMISSION_EXECUTE)
            == (wire::MEMORY_PERMISSION_WRITE | wire::MEMORY_PERMISSION_EXECUTE)
        || request.offset % native_abi::PAGE_SIZE != 0
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let Some(length) = round_to_page(request.length) else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    if !object.valid_map_range(request.offset, length) {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    }
    if !valid_alignment(request.alignment) {
        return native_return(status::MEMORY_INVALID_ALIGNMENT, 0, 0);
    }
    let mut required_rights = Rights::MAP;
    if request.permissions & wire::MEMORY_PERMISSION_READ != 0 {
        required_rights |= Rights::READ;
    }
    if request.permissions & wire::MEMORY_PERMISSION_WRITE != 0 {
        required_rights |= Rights::WRITE;
    }
    if request.permissions & wire::MEMORY_PERMISSION_EXECUTE != 0 {
        required_rights |= Rights::EXECUTE;
    }
    if !required_rights.is_subset_of(handle_rights) {
        return native_return(status::SECURITY_RIGHTS_DENIED, 0, 0);
    }

    let address_space = NativeHandle::from_raw(request.address_space);
    let vm = {
        let handles = state.handles.lock();
        let entry = match handles.lookup(
            address_space,
            Some(ObjectInterface::AddressSpace),
            Rights::ALLOCATE,
        ) {
            Ok(entry) => entry,
            Err(error) => return native_return(error, 0, 0),
        };
        let KernelNativeObject::AddressSpace(vm) = entry.object else {
            return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
        };
        Arc::clone(vm)
    };
    let Ok(length_usize) = usize::try_from(length) else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let Ok(alignment) = usize::try_from(request.alignment) else {
        return native_return(status::MEMORY_INVALID_ALIGNMENT, 0, 0);
    };
    let flags = permissions_to_vm_flags(request.permissions);
    let _access = match object.begin_access() {
        Ok(access) => access,
        Err(error) => return native_return(error, 0, 0),
    };
    let range = if request.flags & wire::MEMORY_MAP_FIXED != 0 {
        let Ok(start) = usize::try_from(request.address_hint) else {
            return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
        };
        if start == 0 || start % alignment != 0 {
            return native_return(status::MEMORY_INVALID_ALIGNMENT, 0, 0);
        }
        let Some(end) = start.checked_add(length_usize) else {
            return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
        };
        let range = start..end;
        let result = match &object.backing {
            MemoryBacking::Anonymous(backing) => {
                vm.map_shared_anon(range.clone(), Arc::clone(backing), request.offset, flags)
            }
            MemoryBacking::File { file, offset } => {
                let Some(file_offset) = offset.checked_add(request.offset) else {
                    return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                };
                vm.map_fixed_file(range.clone(), Arc::clone(file), file_offset, flags)
            }
            MemoryBacking::Dma(buffer) => {
                let buffer = buffer.lock();
                let Ok(object_offset) = usize::try_from(request.offset) else {
                    return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                };
                let Some(paddr) = buffer.paddr().checked_add(object_offset) else {
                    return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                };
                vm.map_direct(range.clone(), paddr, flags)
            }
        };
        if let Err(error) = result {
            return native_return(map_vm_error(error), 0, 0);
        }
        range
    } else {
        match &object.backing {
            MemoryBacking::Anonymous(backing) => match vm.map_shared_anon_any_aligned(
                length_usize,
                alignment,
                Arc::clone(backing),
                request.offset,
                flags,
            ) {
                Ok(range) => range,
                Err(error) => return native_return(map_vm_error(error), 0, 0),
            },
            MemoryBacking::File { file, offset } => {
                let Some(file_offset) = offset.checked_add(request.offset) else {
                    return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                };
                match vm.map_file_any_aligned(
                    length_usize,
                    alignment,
                    Arc::clone(file),
                    file_offset,
                    flags,
                ) {
                    Ok(range) => range,
                    Err(error) => return native_return(map_vm_error(error), 0, 0),
                }
            }
            MemoryBacking::Dma(buffer) => {
                let buffer = buffer.lock();
                let Ok(object_offset) = usize::try_from(request.offset) else {
                    return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                };
                let Some(paddr) = buffer.paddr().checked_add(object_offset) else {
                    return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                };
                match vm.map_direct_any_aligned(length_usize, alignment, paddr, flags) {
                    Ok(range) => range,
                    Err(error) => return native_return(map_vm_error(error), 0, 0),
                }
            }
        }
    };

    if register_mapping(state, object, &vm, range.clone(), request.offset).is_err() {
        let _ = vm.unmap_existing(range);
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    native_return(status::OK, range.start as u64, length)
}

pub(super) fn memory_revoke(task: &Arc<Task>, object: &Arc<MemoryObject>) -> NativeCallOutcome {
    match object.state.compare_exchange(
        wire::MEMORY_STATE_ACTIVE,
        wire::MEMORY_STATE_REVOKED,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(wire::MEMORY_STATE_REVOKED) => {
            return native_return(status::MEMORY_REVOKED, 0, 0);
        }
        Err(_) => return native_return(status::MEMORY_POISONED, 0, 0),
    }
    object.generation.fetch_add(1, Ordering::AcqRel);
    object
        .access_waiters
        .wait_event(task, || object.active_accesses.load(Ordering::Acquire) == 0);

    let mut poisoned = false;
    let mut revoked = 0u64;
    loop {
        let mapping = { object.mappings.lock().pop() };
        let Some(mapping) = mapping else {
            break;
        };
        if let Some(vm) = mapping.vm.upgrade()
            && vm.unmap_existing(mapping.range.clone()).is_err()
        {
            poisoned = true;
            let _ = vm.mprotect(mapping.range.clone(), VmFlags::EMPTY.with(VmFlags::USER));
        }
        if let Some(registry) = mapping.owner_registry.upgrade() {
            registry.remove_object(object);
        }
        object.note_unmapped();
        revoked = revoked.saturating_add(1);
    }
    if poisoned {
        object
            .state
            .store(wire::MEMORY_STATE_POISONED, Ordering::Release);
        return native_return(status::MEMORY_POISONED, 0, 0);
    }
    native_return(status::OK, revoked, 0)
}

pub(super) fn memory_unmap(
    state: &NativeProcessState,
    vm: &Arc<VmSpace>,
    address: u64,
    length: u64,
) -> NativeCallOutcome {
    if address % native_abi::PAGE_SIZE != 0 || length == 0 || length % native_abi::PAGE_SIZE != 0 {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    }
    let (Ok(start), Ok(length)) = (usize::try_from(address), usize::try_from(length)) else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let Some(end) = start.checked_add(length) else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let range = start..end;
    let mut objects = state.mapped_memory_objects.objects.lock();
    for object_index in 0..objects.len() {
        let object = Arc::clone(&objects[object_index]);
        let mut mappings = object.mappings.lock();
        let Some(mapping_index) = mappings.iter().position(|mapping| {
            mapping.owner_id == state.memory_owner_id
                && mapping.range == range
                && mapping
                    .vm
                    .upgrade()
                    .is_some_and(|mapped_vm| Arc::ptr_eq(&mapped_vm, vm))
        }) else {
            continue;
        };
        if let Err(error) = vm.unmap_existing(range) {
            return native_return(map_vm_error(error), 0, 0);
        }
        mappings.swap_remove(mapping_index);
        object.note_unmapped();
        objects.swap_remove(object_index);
        return native_return(status::OK, 0, 0);
    }
    native_return(status::MEMORY_NOT_OWNED, 0, 0)
}

pub(super) fn memory_query(
    task: &Arc<sched::Task>,
    object: &Arc<MemoryObject>,
    user: u64,
) -> NativeCallOutcome {
    match copy_user_value_out(task, user, &object.info()) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

pub(super) fn memory_statistics(
    task: &Arc<sched::Task>,
    state: &NativeProcessState,
    object: &Arc<MemoryObject>,
    user: u64,
) -> NativeCallOutcome {
    let _ = state;
    let statistics = match object.statistics() {
        Ok(statistics) => statistics,
        Err(error) => return native_return(error, 0, 0),
    };
    match copy_user_value_out(task, user, &statistics) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

/// 把已注册的对象区间解析为当前任务地址空间中的稳定映射地址。
pub(super) fn resolve_mapped_range(
    task: &Arc<sched::Task>,
    state: &NativeProcessState,
    object: &Arc<MemoryObject>,
    offset: u64,
    length: u64,
) -> Result<usize, u32> {
    if length == 0
        || offset
            .checked_add(length)
            .is_none_or(|end| end > object.size)
    {
        return Err(status::MEMORY_INVALID_RANGE);
    }
    let vm = task_vm(task)?;
    let mappings = object.mappings.lock();
    for mapping in mappings.iter() {
        if mapping.owner_id != state.memory_owner_id {
            continue;
        }
        let Some(mapped_vm) = mapping.vm.upgrade() else {
            continue;
        };
        if !Arc::ptr_eq(&mapped_vm, &vm) {
            continue;
        }
        let Some(relative) = offset.checked_sub(mapping.object_offset) else {
            continue;
        };
        let Ok(relative) = usize::try_from(relative) else {
            continue;
        };
        let Ok(length) = usize::try_from(length) else {
            return Err(status::MEMORY_INVALID_RANGE);
        };
        let Some(start) = mapping.range.start.checked_add(relative) else {
            continue;
        };
        let Some(end) = start.checked_add(length) else {
            continue;
        };
        if end <= mapping.range.end {
            return Ok(start);
        }
    }
    Err(status::MEMORY_NOT_OWNED)
}

fn permissions_to_vm_flags(permissions: u32) -> VmFlags {
    let mut flags = VmFlags::from_bits(VmFlags::USER | VmFlags::SHARED);
    if permissions & wire::MEMORY_PERMISSION_READ != 0 {
        flags = flags.with(VmFlags::READ);
    }
    if permissions & wire::MEMORY_PERMISSION_WRITE != 0 {
        flags = flags.with(VmFlags::WRITE);
    }
    if permissions & wire::MEMORY_PERMISSION_EXECUTE != 0 {
        flags = flags.with(VmFlags::EXEC);
    }
    flags
}

fn round_to_page(value: u64) -> Option<u64> {
    if value == 0 {
        return None;
    }
    value
        .checked_add(native_abi::PAGE_SIZE - 1)
        .map(|rounded| rounded / native_abi::PAGE_SIZE * native_abi::PAGE_SIZE)
}

fn valid_alignment(alignment: u64) -> bool {
    alignment >= native_abi::PAGE_SIZE
        && alignment.is_power_of_two()
        && alignment % native_abi::PAGE_SIZE == 0
}

fn map_vm_error(error: errno::Errno) -> u32 {
    match error {
        errno::Errno::EINVAL => status::MEMORY_INVALID_RANGE,
        errno::Errno::EEXIST => status::MEMORY_NOT_OWNED,
        _ => status::CORE_RESOURCE_EXHAUSTED,
    }
}
