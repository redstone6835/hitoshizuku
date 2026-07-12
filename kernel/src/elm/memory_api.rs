//! `kernel.memory@1` 的内核集成入口。

use allocator::{
    AllocationKind, AllocationRecord, KERNEL_ALLOCATOR, MemoryDomain, MemoryRequest,
    OwnedAllocationError, Zeroing,
};
use elm_model::{
    ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT, ElmApiNamespaceDescriptorV1, ElmCurrentContext,
};
use kernel_api::memory::{
    KERNEL_MEMORY_API_IDENTIFIER, KERNEL_MEMORY_API_VERSION, KERNEL_MEMORY_CAP_ALLOCATE,
    KERNEL_MEMORY_CAP_QUERY, KERNEL_MEMORY_CAP_RESIZE, KERNEL_MEMORY_CAP_STATS,
    KERNEL_MEMORY_CAPABILITIES, KERNEL_MEMORY_KIND_LARGE, KERNEL_MEMORY_KIND_SMALL,
    KERNEL_MEMORY_LAYOUT_HASH_V1, KERNEL_MEMORY_REQUEST_ZEROED, KERNEL_MEMORY_STATUS_INVALID,
    KERNEL_MEMORY_STATUS_NOT_FOUND, KERNEL_MEMORY_STATUS_OK, KERNEL_MEMORY_STATUS_OUT_OF_MEMORY,
    KERNEL_MEMORY_STATUS_PERMISSION, KERNEL_MEMORY_STATUS_UNAVAILABLE, KernelMemoryAllocationV1,
    KernelMemoryApiV1, KernelMemoryRequestV1, KernelMemoryStatsV1,
};
use kernel_api::{ApiGrantTokenV1, ApiTableHeaderV1};

use super::api_registry::ApiRegistryError;

static KERNEL_MEMORY_API_V1: KernelMemoryApiV1 = KernelMemoryApiV1 {
    header: ApiTableHeaderV1::new::<KernelMemoryApiV1>(KERNEL_MEMORY_CAPABILITIES),
    allocate: memory_allocate_v1,
    deallocate: memory_deallocate_v1,
    reallocate: memory_reallocate_v1,
    query: memory_query_v1,
    stats: memory_stats_v1,
};

static KERNEL_MEMORY_NAMESPACE_V1: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
    KERNEL_MEMORY_API_IDENTIFIER,
    KERNEL_MEMORY_API_VERSION,
    ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT,
    KERNEL_MEMORY_CAPABILITIES,
    &KERNEL_MEMORY_API_V1,
    KERNEL_MEMORY_LAYOUT_HASH_V1,
);

pub(crate) fn init() -> Result<(), ApiRegistryError> {
    super::register_kernel_api_namespace(&KERNEL_MEMORY_NAMESPACE_V1)
}

fn with_authorized_memory_call(
    token: ApiGrantTokenV1,
    capability: u64,
    call: impl FnOnce(ElmCurrentContext) -> i32,
) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return KERNEL_MEMORY_STATUS_PERMISSION;
    };
    let context = match super::authorize_kernel_api_call(
        token,
        KERNEL_MEMORY_API_IDENTIFIER,
        KERNEL_MEMORY_API_VERSION,
        capability,
    ) {
        Ok(context) => context,
        Err(_) => return KERNEL_MEMORY_STATUS_PERMISSION,
    };
    call(context)
}

extern "C" fn memory_allocate_v1(
    token: ApiGrantTokenV1,
    request: KernelMemoryRequestV1,
    output: *mut KernelMemoryAllocationV1,
) -> i32 {
    with_authorized_memory_call(token, KERNEL_MEMORY_CAP_ALLOCATE, |context| {
        if !valid_output(output) {
            return KERNEL_MEMORY_STATUS_INVALID;
        }
        let request = match allocator_request(request) {
            Ok(request) => request,
            Err(status) => return status,
        };
        let record = match KERNEL_ALLOCATOR.allocate_owned(context.cell_id.0, request) {
            Ok(record) => record,
            Err(error) => return owned_error_status(error),
        };
        // Safety: 输出槽已经通过当前 ELM 执行边界的完整可写范围校验。
        unsafe { output.write(allocation_output(record)) };
        KERNEL_MEMORY_STATUS_OK
    })
}

unsafe extern "C" fn memory_deallocate_v1(token: ApiGrantTokenV1, address: u64) -> i32 {
    with_authorized_memory_call(token, KERNEL_MEMORY_CAP_ALLOCATE, |context| {
        let address = match usize::try_from(address) {
            Ok(0) | Err(_) => return KERNEL_MEMORY_STATUS_INVALID,
            Ok(address) => address,
        };
        match KERNEL_ALLOCATOR.deallocate_owned(context.cell_id.0, address) {
            Ok(()) => KERNEL_MEMORY_STATUS_OK,
            Err(error) => owned_error_status(error),
        }
    })
}

unsafe extern "C" fn memory_reallocate_v1(
    token: ApiGrantTokenV1,
    address: u64,
    request: KernelMemoryRequestV1,
    output: *mut KernelMemoryAllocationV1,
) -> i32 {
    with_authorized_memory_call(token, KERNEL_MEMORY_CAP_RESIZE, |context| {
        if !valid_output(output) {
            return KERNEL_MEMORY_STATUS_INVALID;
        }
        let address = match usize::try_from(address) {
            Ok(0) | Err(_) => return KERNEL_MEMORY_STATUS_INVALID,
            Ok(address) => address,
        };
        let request = match allocator_request(request) {
            Ok(request) => request,
            Err(status) => return status,
        };
        let record = match KERNEL_ALLOCATOR.reallocate_owned_excluding_range(
            context.cell_id.0,
            address,
            request,
            output as usize,
            core::mem::size_of::<KernelMemoryAllocationV1>(),
        ) {
            Ok(record) => record,
            Err(error) => return owned_error_status(error),
        };
        // Safety: 输出槽已经通过当前 ELM 执行边界的完整可写范围校验。
        unsafe { output.write(allocation_output(record)) };
        KERNEL_MEMORY_STATUS_OK
    })
}

extern "C" fn memory_query_v1(
    token: ApiGrantTokenV1,
    address: u64,
    output: *mut KernelMemoryAllocationV1,
) -> i32 {
    with_authorized_memory_call(token, KERNEL_MEMORY_CAP_QUERY, |context| {
        if !valid_output(output) {
            return KERNEL_MEMORY_STATUS_INVALID;
        }
        let address = match usize::try_from(address) {
            Ok(0) | Err(_) => return KERNEL_MEMORY_STATUS_INVALID,
            Ok(address) => address,
        };
        let record = match KERNEL_ALLOCATOR.query_owned_allocation(context.cell_id.0, address) {
            Ok(record) => record,
            Err(error) => return owned_error_status(error),
        };
        // Safety: 输出槽已经通过当前 ELM 执行边界的完整可写范围校验。
        unsafe { output.write(allocation_output(record)) };
        KERNEL_MEMORY_STATUS_OK
    })
}

extern "C" fn memory_stats_v1(token: ApiGrantTokenV1, output: *mut KernelMemoryStatsV1) -> i32 {
    with_authorized_memory_call(token, KERNEL_MEMORY_CAP_STATS, |context| {
        if output.is_null()
            || !general::elm_guard::validate_current_memory_range(
                output as usize,
                core::mem::size_of::<KernelMemoryStatsV1>(),
                true,
            )
        {
            return KERNEL_MEMORY_STATUS_INVALID;
        }
        if !super::resource_accounting::registered(context.cell_id) {
            return KERNEL_MEMORY_STATUS_PERMISSION;
        }
        let accounting =
            super::resource_accounting::snapshot(context.cell_id, sched::now_ns_public());
        let allocator = KERNEL_ALLOCATOR.stats();
        let value = KernelMemoryStatsV1 {
            struct_size: core::mem::size_of::<KernelMemoryStatsV1>() as u32,
            flags: 0,
            current_bytes: accounting.dynamic_alloc_bytes,
            peak_bytes: accounting.peak_dynamic_alloc_bytes,
            limit_bytes: accounting.max_dynamic_alloc_bytes,
            quota_denials: accounting.quota_denials,
            accounting_errors: accounting.accounting_errors,
            total_allocations: allocator.total_allocs,
            total_deallocations: allocator.total_deallocs,
            total_reallocations: allocator.total_reallocs,
            out_of_memory_count: allocator.oom_count,
            pressure_level: u32::from(KERNEL_ALLOCATOR.pressure_level()),
            reserved0: 0,
        };
        // Safety: 输出槽已经通过当前 ELM 执行边界的完整可写范围校验。
        unsafe { output.write(value) };
        KERNEL_MEMORY_STATUS_OK
    })
}

fn valid_output(output: *mut KernelMemoryAllocationV1) -> bool {
    !output.is_null()
        && general::elm_guard::validate_current_memory_range(
            output as usize,
            core::mem::size_of::<KernelMemoryAllocationV1>(),
            true,
        )
}

fn allocator_request(request: KernelMemoryRequestV1) -> Result<MemoryRequest, i32> {
    if !request.is_well_formed() {
        return Err(KERNEL_MEMORY_STATUS_INVALID);
    }
    let size = usize::try_from(request.size).map_err(|_| KERNEL_MEMORY_STATUS_INVALID)?;
    let align = usize::try_from(request.align).map_err(|_| KERNEL_MEMORY_STATUS_INVALID)?;
    let zeroing = if request.flags & KERNEL_MEMORY_REQUEST_ZEROED != 0 {
        Zeroing::Zeroed
    } else {
        Zeroing::Uninitialized
    };
    let request = MemoryRequest::new(MemoryDomain::Kernel, size, align).with_zeroing(zeroing);
    request.validate().map_err(|_| KERNEL_MEMORY_STATUS_INVALID)
}

fn allocation_output(record: AllocationRecord) -> KernelMemoryAllocationV1 {
    let kind = match record.kind {
        AllocationKind::Small => KERNEL_MEMORY_KIND_SMALL,
        AllocationKind::Large => KERNEL_MEMORY_KIND_LARGE,
        _ => 0,
    };
    KernelMemoryAllocationV1 {
        struct_size: core::mem::size_of::<KernelMemoryAllocationV1>() as u32,
        flags: 0,
        address: record.ptr as u64,
        size: record.size as u64,
        usable_size: record.usable_size as u64,
        align: record.align as u64,
        kind,
        reserved0: 0,
    }
}

fn owned_error_status(error: OwnedAllocationError) -> i32 {
    match error {
        OwnedAllocationError::InvalidOwner | OwnedAllocationError::PermissionDenied => {
            KERNEL_MEMORY_STATUS_PERMISSION
        }
        OwnedAllocationError::InvalidRequest | OwnedAllocationError::AliasedRange => {
            KERNEL_MEMORY_STATUS_INVALID
        }
        OwnedAllocationError::UnknownPointer => KERNEL_MEMORY_STATUS_NOT_FOUND,
        OwnedAllocationError::Unavailable | OwnedAllocationError::BackendFailure => {
            KERNEL_MEMORY_STATUS_UNAVAILABLE
        }
        OwnedAllocationError::OutOfMemory => KERNEL_MEMORY_STATUS_OUT_OF_MEMORY,
    }
}
