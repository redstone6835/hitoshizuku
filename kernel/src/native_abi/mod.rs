//! Hitoshizuku Native 进程状态与初始 capability 资源。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use general::mm::VmSpace;
use general::vfs::VfsContext;
use general::vfs::file::File;
use native_abi::{
    InitialHandleRecord, NativeBindingPlan, NativeHandle, NativeHandleTable, RequirementId, Rights,
    requirement,
};
use sched::sync::Spinlock;
use soyo::SoyoMetadata;
use vfs::fdtable::{FdFlags, FdTableSnapshot};

use self::dispatch::dispatch_native_call_with_context;

mod channel;
mod component;
mod device;
mod dispatch;
mod event;
mod fs;
mod image;
mod memory;
mod operations;
mod process;
mod ring;
mod socket;
mod thread;
mod trust_policy;

use channel::ChannelObject;
pub(crate) use component::DYNAMIC_TLS_ARENA_SIZE;
use component::{ComponentManager, ComponentObject, ComponentTransaction, InterfaceObject};
use device::DeviceFunctionObject;
use event::EventPort;
pub(crate) use fs::{DirectoryObject, FileObject};
pub(crate) use image::ImageObject;
use memory::MemoryObject;
pub(crate) use process::ProcessObject;
use ring::SubmissionRingObject;
use socket::SocketObject;
pub(crate) use thread::record_task_exit;
pub(crate) use thread::{TASKEXT_NATIVE_THREAD, ThreadObject};

/// Native handle 可引用的内核对象。
#[derive(Clone)]
pub(crate) enum KernelNativeObject {
    SelfProcess,
    Process(Arc<ProcessObject>),
    Thread(Arc<ThreadObject>),
    AddressSpace(Arc<VmSpace>),
    Stream(Arc<File>),
    MonotonicClock,
    Image(Arc<ImageObject>),
    EventPort(Arc<EventPort>),
    Component(Arc<ComponentObject>),
    ComponentTransaction(Arc<ComponentTransaction>),
    /// 持有接口对象以维持组件和接口表的生命周期；分发不直接解包该句柄。
    Interface(#[allow(dead_code)] Arc<InterfaceObject>),
    MemoryObject(Arc<MemoryObject>),
    Directory(Arc<DirectoryObject>),
    File(Arc<FileObject>),
    Channel(Arc<ChannelObject>),
    SubmissionRing(Arc<SubmissionRingObject>),
    Socket(Arc<SocketObject>),
    DeviceFunction(Arc<DeviceFunctionObject>),
}

#[derive(Clone)]
pub(crate) struct PreparedNativeCapability {
    pub(crate) requirement_id: RequirementId,
    pub(crate) object: KernelNativeObject,
    pub(crate) interface: native_abi::ObjectInterface,
    pub(crate) rights: Rights,
    pub(crate) source_handle: NativeHandle,
    pub(crate) move_source: bool,
}

/// 由线程组 personality 唯一持有的 Native 进程状态。
pub(crate) struct NativeProcessState {
    pub(crate) binding: NativeBindingPlan,
    pub(crate) handles: Arc<Spinlock<NativeHandleTable<KernelNativeObject>>>,
    #[allow(dead_code)] // Retained for process identity and future diagnostics.
    pub(crate) build_id: [u8; 32],
    #[allow(dead_code)] // Retained for process identity and future diagnostics.
    pub(crate) content_hash: [u8; 32],
    #[allow(dead_code)] // Retained for image-relative Native diagnostics.
    pub(crate) image_base: usize,
    pub(crate) components: Arc<ComponentManager>,
    #[allow(dead_code)] // Keeps the launch namespace alive for component operations.
    pub(crate) vfs_context: Option<Arc<VfsContext>>,
    runtime_ranges: Spinlock<Option<NativeRuntimeRanges>>,
    allocations: Spinlock<Vec<Range<usize>>>,
    memory_owner_id: u64,
    mapped_memory_objects: Arc<memory::MemoryMappingRegistry>,
}

static NEXT_MEMORY_OWNER_ID: AtomicU64 = AtomicU64::new(1);

fn next_memory_owner_id() -> u64 {
    let id = NEXT_MEMORY_OWNER_ID.fetch_add(1, Ordering::Relaxed);
    assert!(id != 0, "Native memory owner identity 已耗尽");
    id
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

impl Drop for NativeProcessState {
    fn drop(&mut self) {
        memory::release_process_mappings(self);
    }
}

pub(super) fn task_vm(task: &sched::Task) -> Result<Arc<VmSpace>, u32> {
    task.ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
        .ok_or(native_abi::status::STREAM_FAULT)
}

pub(super) fn copy_user_bytes_in(
    task: &sched::Task,
    user: u64,
    output: &mut [u8],
) -> Result<(), u32> {
    if output.is_empty() {
        return Ok(());
    }
    let user = usize::try_from(user).map_err(|_| native_abi::status::STREAM_FAULT)?;
    if user == 0 {
        return Err(native_abi::status::STREAM_FAULT);
    }
    task_vm(task)?
        .copy_user_bytes_in(user, output)
        .map_err(|_| native_abi::status::STREAM_FAULT)
}

pub(super) fn copy_user_bytes_out(task: &sched::Task, user: u64, input: &[u8]) -> Result<(), u32> {
    if input.is_empty() {
        return Ok(());
    }
    let user = usize::try_from(user).map_err(|_| native_abi::status::STREAM_FAULT)?;
    if user == 0 {
        return Err(native_abi::status::STREAM_FAULT);
    }
    task_vm(task)?
        .copy_user_bytes_out(user, input)
        .map_err(|_| native_abi::status::STREAM_FAULT)
}

pub(super) fn copy_user_value<T: Copy + Default>(task: &sched::Task, user: u64) -> Result<T, u32> {
    let mut value = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut value as *mut T).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    };
    copy_user_bytes_in(task, user, bytes)?;
    Ok(value)
}

pub(super) fn copy_user_value_out<T: Copy>(
    task: &sched::Task,
    user: u64,
    value: &T,
) -> Result<(), u32> {
    let bytes = unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    };
    copy_user_bytes_out(task, user, bytes)
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
    prepare_native_process_state_with_vfs(
        metadata,
        binding,
        vm,
        image_base,
        descriptors,
        transferred,
        None,
    )
}

pub(crate) fn prepare_native_process_state_with_vfs(
    metadata: &SoyoMetadata,
    binding: NativeBindingPlan,
    vm: &Arc<VmSpace>,
    image_base: usize,
    descriptors: Option<&FdTableSnapshot>,
    transferred: &[PreparedNativeCapability],
    vfs_context: Option<Arc<VfsContext>>,
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
            RequirementId::RootDirectory => transferred_object.or_else(|| {
                vfs_context
                    .as_ref()
                    .map(|context| DirectoryObject::from_context(context))
                    .map(KernelNativeObject::Directory)
            }),
            RequirementId::DeviceFunction => {
                transferred_object.or_else(|| device::bootstrap_capability(vfs_context.as_ref()))
            }
            RequirementId::ServiceChannel => transferred_object,
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

    let handles = Arc::new(Spinlock::new(handles));
    let components = ComponentManager::new(Arc::clone(vm), &binding, Arc::clone(&handles))
        .map_err(|_| Errno::ENOMEM)?;
    let state = Arc::new(NativeProcessState {
        binding,
        handles,
        build_id: metadata.header.build_id,
        content_hash: metadata.header.content_hash,
        image_base,
        components,
        vfs_context,
        runtime_ranges: Spinlock::new(None),
        allocations: Spinlock::new(Vec::new()),
        memory_owner_id: next_memory_owner_id(),
        mapped_memory_objects: Arc::new(memory::MemoryMappingRegistry::new()),
    });
    Ok((state, initial))
}

fn stream_supports(file: &File, rights: Rights) -> bool {
    (!Rights::READ.is_subset_of(rights) || file.flags().readable())
        && (!Rights::WRITE.is_subset_of(rights) || file.flags().writable())
}

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
mod tests;
