//! MyGO Native 进程状态与初始 capability 资源。

use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;
use general::mm::VmSpace;
use general::vfs::file::File;
use native_abi::{
    InitialHandleRecord, NativeBindingPlan, NativeHandleTable, RequirementId, Rights, requirement,
};
use sched::sync::Spinlock;
use soyo::SoyoMetadata;
use vfs::fdtable::{FdFlags, FdTableSnapshot};

/// Native handle 可引用的内核对象。
#[derive(Clone)]
pub(crate) enum KernelNativeObject {
    SelfProcess,
    AddressSpace(Arc<VmSpace>),
    Stream(Arc<File>),
    MonotonicClock,
}

/// 由线程组 personality 唯一持有的 Native 进程状态。
pub(crate) struct NativeProcessState {
    pub(crate) binding: NativeBindingPlan,
    pub(crate) handles: Spinlock<NativeHandleTable<KernelNativeObject>>,
    pub(crate) build_id: [u8; 32],
    pub(crate) content_hash: [u8; 32],
    pub(crate) image_base: usize,
}

pub(crate) fn prepare_native_process_state(
    metadata: &SoyoMetadata,
    binding: NativeBindingPlan,
    vm: &Arc<VmSpace>,
    image_base: usize,
    descriptors: Option<&FdTableSnapshot>,
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
                descriptors
                    .and_then(|snapshot| {
                        snapshot.descriptors().iter().find(|descriptor| {
                            descriptor.fd().as_raw() == fd
                                && !descriptor.flags().has(FdFlags::CLOEXEC)
                        })
                    })
                    .filter(|descriptor| stream_supports(descriptor.file(), rights))
                    .map(|descriptor| KernelNativeObject::Stream(Arc::clone(descriptor.file())))
            }
            RequirementId::MonotonicClock => Some(KernelNativeObject::MonotonicClock),
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
    });
    Ok((state, initial))
}

fn stream_supports(file: &File, rights: Rights) -> bool {
    (!Rights::READ.is_subset_of(rights) || file.flags().readable())
        && (!Rights::WRITE.is_subset_of(rights) || file.flags().writable())
}
