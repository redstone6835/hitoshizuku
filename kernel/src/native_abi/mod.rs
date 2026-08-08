//! MyGO Native 进程状态与初始 capability 资源。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use errno::Errno;
use general::mm::VmSpace;
use general::vfs::file::File;
use native_abi::{
    InitialHandleRecord, NativeBindingPlan, NativeHandle, NativeHandleTable, RequirementId, Rights,
    requirement,
};
use sched::sync::Spinlock;
use soyo::SoyoMetadata;
use vfs::fdtable::{FdFlags, FdTableSnapshot};

use self::dispatch::dispatch_native_call_with_context;

mod dispatch;
mod event;
mod image;
mod operations;
mod process;

use event::EventPort;
pub(crate) use image::ExecutableImage;
pub(crate) use process::ProcessObject;

/// Native handle 可引用的内核对象。
#[derive(Clone)]
pub(crate) enum KernelNativeObject {
    SelfProcess,
    Process(Arc<ProcessObject>),
    AddressSpace(Arc<VmSpace>),
    Stream(Arc<File>),
    MonotonicClock,
    ExecutableImage(Arc<ExecutableImage>),
    EventPort(Arc<EventPort>),
}

#[derive(Clone)]
pub(crate) struct PreparedNativeCapability {
    pub(crate) requirement_id: RequirementId,
    pub(crate) object: KernelNativeObject,
    pub(crate) interface: native_abi::ObjectInterface,
    pub(crate) rights: Rights,
    pub(crate) source_handle: Option<NativeHandle>,
}

/// 由线程组 personality 唯一持有的 Native 进程状态。
pub(crate) struct NativeProcessState {
    pub(crate) binding: NativeBindingPlan,
    pub(crate) handles: Spinlock<NativeHandleTable<KernelNativeObject>>,
    pub(crate) build_id: [u8; 32],
    pub(crate) content_hash: [u8; 32],
    pub(crate) image_base: usize,
    runtime_ranges: Spinlock<Option<NativeRuntimeRanges>>,
    allocations: Spinlock<Vec<Range<usize>>>,
}

struct NativeRuntimeRanges {
    stack: Range<usize>,
    start_info: Range<usize>,
    tls: Option<Range<usize>>,
}

impl NativeProcessState {
    pub(crate) fn install_runtime_ranges(
        &self,
        stack: Range<usize>,
        start_info: Range<usize>,
        tls: Option<Range<usize>>,
    ) {
        let mut ranges = self.runtime_ranges.lock();
        assert!(ranges.is_none(), "Native runtime range 只能安装一次");
        *ranges = Some(NativeRuntimeRanges {
            stack,
            start_info,
            tls,
        });
    }

    fn overlaps_runtime_range(&self, range: &Range<usize>) -> bool {
        let ranges = self.runtime_ranges.lock();
        let Some(ranges) = ranges.as_ref() else {
            return true;
        };
        ranges.stack.start < range.end && range.start < ranges.stack.end
            || ranges.start_info.start < range.end && range.start < ranges.start_info.end
            || ranges
                .tls
                .as_ref()
                .is_some_and(|tls| tls.start < range.end && range.start < tls.end)
    }

    fn owns_allocation(&self, range: &Range<usize>) -> bool {
        self.allocations.lock().iter().any(|owned| owned == range)
    }

    fn record_allocation(&self, range: Range<usize>) -> Result<(), ()> {
        let mut allocations = self.allocations.lock();
        allocations.try_reserve(1).map_err(|_| ())?;
        allocations.push(range);
        Ok(())
    }

    fn remove_allocation(&self, range: &Range<usize>) -> bool {
        let mut allocations = self.allocations.lock();
        let Some(index) = allocations.iter().position(|owned| owned == range) else {
            return false;
        };
        allocations.swap_remove(index);
        true
    }
}

pub(crate) fn register() {
    general::syscall::register_native_dispatcher(dispatch_native_call_with_context);
}

pub(crate) fn prepare_native_process_state(
    metadata: &SoyoMetadata,
    binding: NativeBindingPlan,
    vm: &Arc<VmSpace>,
    image_base: usize,
    descriptors: Option<&FdTableSnapshot>,
) -> Result<(Arc<NativeProcessState>, Vec<InitialHandleRecord>), Errno> {
    prepare_native_process_state_with_capabilities(
        metadata,
        binding,
        vm,
        image_base,
        descriptors,
        &[],
    )
}

pub(crate) fn prepare_native_process_state_with_capabilities(
    metadata: &SoyoMetadata,
    binding: NativeBindingPlan,
    vm: &Arc<VmSpace>,
    image_base: usize,
    descriptors: Option<&FdTableSnapshot>,
    transferred: &[PreparedNativeCapability],
) -> Result<(Arc<NativeProcessState>, Vec<InitialHandleRecord>), Errno> {
    if descriptors.is_some_and(|snapshot| {
        snapshot.descriptors().iter().any(|descriptor| {
            !descriptor.flags().has(FdFlags::CLOEXEC) && descriptor.fd().as_raw() >= 3
        })
    }) {
        return Err(Errno::EOPNOTSUPP);
    }

    let mut handles = NativeHandleTable::new().map_err(|_| Errno::ENOMEM)?;
    let mut initial = Vec::new();
    initial
        .try_reserve_exact(metadata.capabilities.len())
        .map_err(|_| Errno::ENOMEM)?;

    for capability in &metadata.capabilities {
        let Some(requirement_id) = RequirementId::from_raw(capability.requirement_id) else {
            if capability.required() {
                return Err(Errno::EACCES);
            }
            continue;
        };
        let rights = Rights::from_bits(capability.required_rights);
        let transferred_object = transferred
            .iter()
            .find(|candidate| candidate.requirement_id == requirement_id)
            .filter(|candidate| {
                candidate.interface
                    == requirement(requirement_id)
                        .map(|spec| spec.interface)
                        .unwrap_or(candidate.interface)
                    && rights.is_subset_of(candidate.rights)
            })
            .map(|candidate| candidate.object.clone());
        let object = match requirement_id {
            RequirementId::SelfProcess => Some(KernelNativeObject::SelfProcess),
            RequirementId::CurrentAddressSpace => {
                Some(KernelNativeObject::AddressSpace(Arc::clone(vm)))
            }
            RequirementId::Stdin | RequirementId::Stdout | RequirementId::Stderr => {
                let fd = match requirement_id {
                    RequirementId::Stdin => 0,
                    RequirementId::Stdout => 1,
                    RequirementId::Stderr => 2,
                    _ => unreachable!(),
                };
                transferred_object.or_else(|| {
                    descriptors
                        .and_then(|snapshot| {
                            snapshot.descriptors().iter().find(|descriptor| {
                                descriptor.fd().as_raw() == fd
                                    && !descriptor.flags().has(FdFlags::CLOEXEC)
                            })
                        })
                        .filter(|descriptor| stream_supports(descriptor.file(), rights))
                        .map(|descriptor| KernelNativeObject::Stream(Arc::clone(descriptor.file())))
                })
            }
            RequirementId::MonotonicClock => {
                transferred_object.or(Some(KernelNativeObject::MonotonicClock))
            }
        };
        let Some(object) = object else {
            if capability.required() {
                return Err(Errno::EACCES);
            }
            continue;
        };
        let interface = requirement(requirement_id).ok_or(Errno::ENOEXEC)?.interface;
        let handle = handles
            .insert(object, interface, rights)
            .map_err(|_| Errno::ENOMEM)?;
        initial.push(InitialHandleRecord {
            requirement_id,
            object_interface: interface,
            handle,
            granted_rights: rights,
        });
    }

    let state = Arc::new(NativeProcessState {
        binding,
        handles: Spinlock::new(handles),
        build_id: metadata.header.build_id,
        content_hash: metadata.header.content_hash,
        image_base,
        runtime_ranges: Spinlock::new(None),
        allocations: Spinlock::new(Vec::new()),
    });
    Ok((state, initial))
}

fn stream_supports(file: &File, rights: Rights) -> bool {
    (!Rights::READ.is_subset_of(rights) || file.flags().readable())
        && (!Rights::WRITE.is_subset_of(rights) || file.flags().writable())
}

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
mod tests;
