//! ELM 原生 EKI 镜像执行器。
//!
//! 本模块只处理 EKI 产出的 EBI 原生镜像：复制段、应用 EKI 原生重定位、切换 W^X
//! 权限并调用生命周期钩子。imports 的地址解析由 Core 提供，本模块只消费已解析地址；
//! 原生 provider handler、snapshot 与热替换迁移钩子都通过显式 symbol 暴露。

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use allocator::{KERNEL_ALLOCATOR, MemoryDomain, MemoryRequest, PAGE_SIZE, PagePolicy, Zeroing};
use elm_model::{
    ELM_CALL_STATUS_PROVIDER_FAULT, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_OK,
    ELM_MODULE_DESCRIPTOR_SYMBOL, ELM_NATIVE_ENTRY_ABI_VERSION,
    ELM_NATIVE_MANAGED_CALL_ABI_VERSION, ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED,
    ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, ElmCallFrame, ElmContext, ElmCurrentContext,
    ElmEbiImage, ElmEbiLoadStatus, ElmEbiProviderPortDecl, ElmEbiRelocationKind, ElmEbiSegmentKind,
    ElmEbiSegmentPayload, ElmError, ElmId, ElmLifecyclePhase, ElmModuleDescriptorV1,
    ElmNativeEntryFrameV1, ElmNativeHookContextV1, ElmNativeManagedCallV1,
    ElmNativeMigrationContextV1, ElmNativeProviderCallV1, ElmNativeProviderSnapshotV1,
    ElmReplyFrame, ElmResult, ElmState, Generation, LeaseId, PortId, relocation_width, state_code,
    try_enter_current_context,
};
use general::elm_guard::{
    ELM_GUARD_PHASE_ENTRY, ELM_GUARD_PHASE_HOOK, ELM_GUARD_PHASE_MANAGED_CALL,
    ELM_GUARD_PHASE_MIGRATION, ELM_GUARD_PHASE_PROVIDER_CALL, ELM_GUARD_PHASE_PROVIDER_SNAPSHOT,
    ElmExecutionDomain, ElmGuard,
};

use super::core::ElmLifecycleExecutor;

const ELM_NATIVE_STACK_SIZE: usize = 64 * 1024;
const ELM_NATIVE_STACK_GUARD_SIZE: usize = PAGE_SIZE;
const ELM_NATIVE_STACK_TOTAL_SIZE: usize = ELM_NATIVE_STACK_SIZE + ELM_NATIVE_STACK_GUARD_SIZE * 2;

struct NativeCallStack {
    base: usize,
}

struct NativeIrqStackSlot {
    stack: NativeCallStack,
    busy: AtomicBool,
}

/// IRQ top-half 使用的每 CPU 预分配 ELM 调用栈。
pub(crate) struct NativeIrqStackSet {
    slots: Vec<NativeIrqStackSlot>,
    owner: ElmId,
    accounted_bytes: u64,
}

impl NativeIrqStackSet {
    /// 为当前内核支持的每个 CPU 预分配隔离栈。
    pub(crate) fn allocate(owner: ElmId) -> Result<Self, i32> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(sched::NR_CPUS)
            .map_err(|_| kernel_api::device::KERNEL_DEVICE_STATUS_NO_MEMORY)?;
        for _ in 0..sched::NR_CPUS {
            slots.push(NativeIrqStackSlot {
                stack: NativeCallStack::allocate()
                    .map_err(|_| kernel_api::device::KERNEL_DEVICE_STATUS_NO_MEMORY)?,
                busy: AtomicBool::new(false),
            });
        }
        let accounted_bytes = u64::try_from(ELM_NATIVE_STACK_TOTAL_SIZE)
            .ok()
            .and_then(|bytes| bytes.checked_mul(sched::NR_CPUS as u64))
            .ok_or(kernel_api::device::KERNEL_DEVICE_STATUS_NO_MEMORY)?;
        if !super::resource_accounting::reserve_native_stack(owner, accounted_bytes) {
            return Err(kernel_api::device::KERNEL_DEVICE_STATUS_NO_MEMORY);
        }
        Ok(Self {
            slots,
            owner,
            accounted_bytes,
        })
    }

    /// 在当前 CPU 的预分配栈上执行一次 top-half 回调。
    pub(crate) fn invoke<T>(
        &self,
        address: u64,
        bounds: NativeExecutionBounds,
        context: ElmCurrentContext,
        frame: &mut T,
    ) -> i32 {
        let cpu = sched::current_cpu_id().min(self.slots.len().saturating_sub(1));
        let Some(slot) = self.slots.get(cpu) else {
            return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
        };
        if slot
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return kernel_api::device::KERNEL_DEVICE_STATUS_BUSY;
        }
        struct BusyGuard<'a>(&'a AtomicBool);
        impl Drop for BusyGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _busy = BusyGuard(&slot.busy);
        let Ok(address) = usize::try_from(address) else {
            return kernel_api::device::KERNEL_DEVICE_STATUS_INVALID;
        };
        let now_ns = sched::now_ns_public();
        let requested_deadline_ns =
            now_ns.saturating_add(kernel_api::device::KERNEL_DEVICE_IRQ_TOP_HALF_BUDGET_NS);
        let accounting = match super::resource_accounting::try_begin_native_call(
            context.cell_id,
            0,
            requested_deadline_ns,
            now_ns,
        ) {
            Ok(accounting) => accounting,
            Err(
                super::resource_accounting::NativeCallAdmissionError::RegistryBusy
                | super::resource_accounting::NativeCallAdmissionError::ConcurrentQuota,
            ) => return kernel_api::device::KERNEL_DEVICE_STATUS_BUSY,
            Err(_) => return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT,
        };
        let Some(guard) = ElmGuard::enter(
            context.cell_id.0,
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_IRQ,
            accounting.effective_deadline_ns(),
        ) else {
            return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
        };
        let context_start = frame as *mut T as usize;
        let Some(context_end) = context_start.checked_add(core::mem::size_of::<T>()) else {
            return kernel_api::device::KERNEL_DEVICE_STATUS_INVALID;
        };
        if !guard.configure_native_bounds(
            bounds.code_start,
            bounds.code_end,
            bounds.image_start,
            bounds.image_end,
            slot.stack.start(),
            slot.stack.top(),
            &[(context_start, context_end)],
        ) {
            return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
        }
        let elm_context = ElmContext::new(
            context.cell_id,
            context.parent_id,
            context.generation,
            context.state,
            context.phase,
            context.flags,
        )
        .with_kind(context.kind)
        .with_allowed_actions(context.allowed_actions);
        let Some(_current) = try_enter_current_context(&elm_context) else {
            return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
        };
        let Some(domain) = guard.enter_domain(ElmExecutionDomain::Interrupt) else {
            return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
        };
        // Safety: 地址和执行边界在注册时由当前 ELM guard 验证；frame 的完整范围
        // 已登记为 host range，调用使用当前 CPU 独占的预分配隔离栈。
        let status = unsafe {
            arch::call_elm_native(address, (frame as *mut T).cast::<u8>(), slot.stack.top())
        };
        let aborted = guard.aborted();
        drop(domain);
        let accounting = accounting.finish(sched::now_ns_public());
        if aborted
            || accounting.call_budget_exceeded
            || accounting.period_budget_exceeded
            || status != 0
        {
            kernel_api::device::KERNEL_DEVICE_STATUS_FAULT
        } else {
            kernel_api::device::KERNEL_DEVICE_STATUS_OK
        }
    }
}

impl Drop for NativeIrqStackSet {
    fn drop(&mut self) {
        super::resource_accounting::release_native_stack(self.owner, self.accounted_bytes);
    }
}

impl NativeCallStack {
    fn allocate() -> ElmResult<Self> {
        let request =
            MemoryRequest::new(MemoryDomain::Kernel, ELM_NATIVE_STACK_TOTAL_SIZE, PAGE_SIZE)
                .with_page_policy(PagePolicy::BaseOnly)
                .with_zeroing(Zeroing::Zeroed)
                .without_external_accounting();
        let record = KERNEL_ALLOCATOR
            .allocate(request)
            .map_err(|_| ElmError::LeaseBusy)?;
        let stack = Self { base: record.ptr };
        let lower_guard = general::elm_image::protect_elm_image_range(
            stack.base,
            ELM_NATIVE_STACK_GUARD_SIZE,
            false,
            false,
            false,
        );
        let upper_guard = general::elm_image::protect_elm_image_range(
            stack.base + ELM_NATIVE_STACK_GUARD_SIZE + ELM_NATIVE_STACK_SIZE,
            ELM_NATIVE_STACK_GUARD_SIZE,
            false,
            false,
            false,
        );
        let body = general::elm_image::protect_elm_image_range(
            stack.base + ELM_NATIVE_STACK_GUARD_SIZE,
            ELM_NATIVE_STACK_SIZE,
            true,
            true,
            false,
        );
        if lower_guard && upper_guard && body {
            Ok(stack)
        } else {
            drop(stack);
            Err(ElmError::InvalidTransition)
        }
    }

    fn top(&self) -> usize {
        self.base + ELM_NATIVE_STACK_GUARD_SIZE + ELM_NATIVE_STACK_SIZE
    }

    fn start(&self) -> usize {
        self.base + ELM_NATIVE_STACK_GUARD_SIZE
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeExecutionBounds {
    pub code_start: usize,
    pub code_end: usize,
    pub image_start: usize,
    pub image_end: usize,
}

/// 验证回调地址并复制当前装载镜像的执行边界。
///
/// 该入口供 ELM 在 `on_initialize` 中注册设备回调；此时镜像尚未提交到 Core 的
/// `native_images`，唯一可信来源是当前受保护调用帧中的装载器边界。
pub(crate) fn current_callback_bounds(address: usize) -> Option<NativeExecutionBounds> {
    if !general::elm_guard::validate_current_code_address(address) {
        return None;
    }
    let bounds = general::elm_guard::current_native_bounds()?;
    Some(NativeExecutionBounds {
        code_start: bounds.code_start,
        code_end: bounds.code_end,
        image_start: bounds.image_start,
        image_end: bounds.image_end,
    })
}

/// 在 ELM 隔离栈和 fault recovery 边界中执行一个设备回调。
pub(crate) fn invoke_device_callback<T>(
    address: usize,
    bounds: NativeExecutionBounds,
    context: ElmCurrentContext,
    phase: u32,
    frame: &mut T,
) -> i32 {
    if address == 0 || phase == general::elm_guard::ELM_GUARD_PHASE_NONE {
        return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
    }
    let Ok(invocation) = NativeInvocation::enter(context.cell_id, phase, 0) else {
        return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
    };
    let elm_context = ElmContext::new(
        context.cell_id,
        context.parent_id,
        context.generation,
        context.state,
        context.phase,
        context.flags,
    )
    .with_kind(context.kind)
    .with_allowed_actions(context.allowed_actions);
    let Some(_current) = try_enter_current_context(&elm_context) else {
        return kernel_api::device::KERNEL_DEVICE_STATUS_FAULT;
    };
    // Safety: 回调地址在注册时已经验证为当前 ELM 的 RX 代码；frame 使用固定
    // repr(C) 设备 ABI，并且整个可写范围作为 host range 交给调用门。
    let status = unsafe { invocation.invoke(address, frame as *mut T, bounds, &[]) };
    let outcome = invocation.finish();
    if outcome.aborted || outcome.budget_exceeded || status != 0 {
        kernel_api::device::KERNEL_DEVICE_STATUS_FAULT
    } else {
        kernel_api::device::KERNEL_DEVICE_STATUS_OK
    }
}

struct NativeInvocation {
    guard: ElmGuard,
    accounting: super::resource_accounting::NativeCallPermit,
    stack: NativeCallStack,
}

#[derive(Debug, Clone, Copy)]
struct NativeInvocationResult {
    aborted: bool,
    budget_exceeded: bool,
}

impl NativeInvocation {
    fn enter(cell: ElmId, phase: u32, requested_deadline_ns: u64) -> ElmResult<Self> {
        let now_ns = sched::now_ns_public();
        let accounting = super::resource_accounting::begin_native_call(
            cell,
            ELM_NATIVE_STACK_TOTAL_SIZE as u64,
            requested_deadline_ns,
            now_ns,
        )
        .map_err(|error| {
            log::error!(
                "[elm] 原生调用预算拒绝 cell={} phase={} error={:?}",
                cell.0,
                phase,
                error
            );
            ElmError::LeaseBusy
        })?;
        let stack = NativeCallStack::allocate().map_err(|error| {
            log::error!(
                "[elm] 原生调用隔离栈分配失败 cell={} phase={} error={:?}",
                cell.0,
                phase,
                error
            );
            error
        })?;
        let guard = ElmGuard::enter(cell.0, phase, accounting.effective_deadline_ns()).ok_or_else(
            || {
                log::error!(
                    "[elm] 原生调用保护域拒绝进入 cell={} phase={} deadline_ns={}",
                    cell.0,
                    phase,
                    accounting.effective_deadline_ns()
                );
                ElmError::InvalidTransition
            },
        )?;
        Ok(Self {
            guard,
            accounting,
            stack,
        })
    }

    unsafe fn invoke<T>(
        &self,
        address: usize,
        context: *mut T,
        bounds: NativeExecutionBounds,
        extra_host_ranges: &[(usize, usize)],
    ) -> i32 {
        let context_start = context as usize;
        let Some(context_end) = context_start.checked_add(core::mem::size_of::<T>()) else {
            return ELM_CALL_STATUS_PROVIDER_FAULT;
        };
        let mut host_ranges = [(0usize, 0usize); general::elm_guard::ELM_GUARD_MAX_HOST_RANGES];
        let required_ranges = 1usize.saturating_add(extra_host_ranges.len());
        if required_ranges > host_ranges.len()
            || context_start == 0
            || context_start >= context_end
            || address < bounds.code_start
            || address >= bounds.code_end
        {
            return ELM_CALL_STATUS_PROVIDER_FAULT;
        }
        host_ranges[0] = (context_start, context_end);
        for (index, range) in extra_host_ranges.iter().copied().enumerate() {
            host_ranges[index + 1] = range;
        }
        if !self.guard.configure_native_bounds(
            bounds.code_start,
            bounds.code_end,
            bounds.image_start,
            bounds.image_end,
            self.stack.start(),
            self.stack.top(),
            &host_ranges[..required_ranges],
        ) {
            return ELM_CALL_STATUS_PROVIDER_FAULT;
        }
        let Some(_domain) = self.guard.enter_domain(ElmExecutionDomain::ElmCode) else {
            return ELM_CALL_STATUS_PROVIDER_FAULT;
        };
        // 安全性：调用方保证入口地址与上下文 ABI；架构调用门只使用本对象持有的隔离栈。
        unsafe { arch::call_elm_native(address, context.cast::<u8>(), self.stack.top()) }
    }

    fn finish(self) -> NativeInvocationResult {
        let aborted = self.guard.aborted();
        let accounting = self.accounting.finish(sched::now_ns_public());
        NativeInvocationResult {
            aborted,
            budget_exceeded: accounting.call_budget_exceeded || accounting.period_budget_exceeded,
        }
    }
}

impl Drop for NativeCallStack {
    fn drop(&mut self) {
        if !general::elm_image::protect_elm_image_range(
            self.base,
            ELM_NATIVE_STACK_TOTAL_SIZE,
            true,
            true,
            false,
        ) {
            // 权限无法恢复时必须保留映射，避免 allocator 把仍带 guard/RX 属性的区间复用。
            log::error!(
                "[elm] 无法恢复原生调用栈权限，保留映射 base=0x{:x} size={}",
                self.base,
                ELM_NATIVE_STACK_TOTAL_SIZE
            );
            return;
        }
        if let Err(err) = KERNEL_ALLOCATOR.deallocate(self.base) {
            log::error!(
                "[elm] 无法释放原生调用栈 base=0x{:x} size={}: {:?}",
                self.base,
                ELM_NATIVE_STACK_TOTAL_SIZE,
                err
            );
        }
    }
}

#[derive(Debug, Clone)]
struct NativeSegment {
    unit_segment_index: u32,
    kind: ElmEbiSegmentKind,
    vaddr: usize,
    size: usize,
}

#[derive(Debug, Clone)]
struct NativeSymbol {
    name: String,
    address: usize,
}

pub(crate) struct LoadedElmImage {
    cell: ElmId,
    base: usize,
    size: usize,
    segments: Vec<NativeSegment>,
    symbols: Vec<NativeSymbol>,
    initialize: usize,
    finalize: usize,
    quiesce: Option<usize>,
    pause: Option<usize>,
    resume: Option<usize>,
    migrate_export: Option<usize>,
    migrate_import: Option<usize>,
    migrate_abort: Option<usize>,
    entry: Option<usize>,
}

impl LoadedElmImage {
    pub(crate) fn load(
        cell: ElmId,
        image: &ElmEbiImage,
        imports: &[usize],
    ) -> Result<Self, ElmEbiLoadStatus> {
        if !general::elm_image::elm_image_ops_registered() {
            return Err(ElmEbiLoadStatus::NativeCodeTodo);
        }
        if imports.len() != image.unit.imports.len() {
            return Err(ElmEbiLoadStatus::RuntimeRejected);
        }
        image.validate(image.unit.target.arch)?;

        let layouts = layout_runtime_segments(image)?;
        let total_size = layouts
            .last()
            .and_then(|segment| segment.vaddr.checked_add(segment.size))
            .ok_or(ElmEbiLoadStatus::InvalidSegment)?;
        let total_size = align_up(total_size, PAGE_SIZE).ok_or(ElmEbiLoadStatus::InvalidSegment)?;
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(layouts.len())
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        let request = MemoryRequest::new(MemoryDomain::Kernel, total_size, PAGE_SIZE)
            .with_page_policy(PagePolicy::BaseOnly)
            .with_zeroing(Zeroing::Zeroed)
            .without_external_accounting();
        let record = KERNEL_ALLOCATOR
            .allocate(request)
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        let base = record.ptr;
        for segment in layouts {
            segments.push(NativeSegment {
                unit_segment_index: segment.unit_segment_index,
                kind: segment.kind,
                vaddr: base + segment.vaddr,
                size: segment.size,
            });
        }

        let mut loaded = Self {
            cell,
            base,
            size: total_size,
            segments,
            symbols: Vec::new(),
            initialize: 0,
            finalize: 0,
            quiesce: None,
            pause: None,
            resume: None,
            migrate_export: None,
            migrate_import: None,
            migrate_abort: None,
            entry: None,
        };

        if let Err(status) = loaded.populate_segments(image) {
            drop(loaded);
            return Err(status);
        }
        if let Err(status) = loaded.apply_relocations(image, imports) {
            drop(loaded);
            return Err(status);
        }
        loaded.symbols = loaded.collect_symbols(image)?;
        let descriptor_address = loaded
            .symbol_address(ELM_MODULE_DESCRIPTOR_SYMBOL)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        let descriptor_symbol = image
            .symbol_location(ELM_MODULE_DESCRIPTOR_SYMBOL)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        if descriptor_symbol.size < core::mem::size_of::<ElmModuleDescriptorV1>() as u64
            || descriptor_address % core::mem::align_of::<ElmModuleDescriptorV1>() != 0
        {
            log::error!(
                "[elm] module descriptor layout invalid cell={} address=0x{:x} symbol_size={} expected_size={} align={}",
                cell.0,
                descriptor_address,
                descriptor_symbol.size,
                core::mem::size_of::<ElmModuleDescriptorV1>(),
                core::mem::align_of::<ElmModuleDescriptorV1>()
            );
            drop(loaded);
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        // Safety: symbol_location 已由 collect_symbols 校验位于已映射段内，且上面验证了尺寸和对齐。
        let descriptor = unsafe { &*(descriptor_address as *const ElmModuleDescriptorV1) };
        if !descriptor.valid() {
            log::error!(
                "[elm] module descriptor invalid cell={} magic={:02x?} abi={} size={} flags=0x{:x} instance_size={} instance_align={}",
                cell.0,
                descriptor.magic,
                descriptor.abi_version,
                descriptor.struct_size,
                descriptor.flags,
                descriptor.instance_size,
                descriptor.instance_align
            );
            drop(loaded);
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        let entries = [
            descriptor.initialize as usize,
            descriptor.finalize as usize,
            descriptor.quiesce as usize,
            descriptor.pause as usize,
            descriptor.resume as usize,
            descriptor.migrate_export as usize,
            descriptor.migrate_import as usize,
            descriptor.migrate_abort as usize,
            descriptor.entry as usize,
        ];
        if entries
            .iter()
            .any(|address| !loaded.code_contains(*address))
        {
            log::error!(
                "[elm] module descriptor entry outside code cell={} entries={:x?} code={:?}",
                cell.0,
                entries,
                loaded.execution_bounds()
            );
            drop(loaded);
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        loaded.initialize = descriptor.initialize as usize;
        loaded.finalize = descriptor.finalize as usize;
        loaded.quiesce = Some(descriptor.quiesce as usize);
        loaded.pause = Some(descriptor.pause as usize);
        loaded.resume = Some(descriptor.resume as usize);
        loaded.migrate_export = Some(descriptor.migrate_export as usize);
        loaded.migrate_import = Some(descriptor.migrate_import as usize);
        loaded.migrate_abort = Some(descriptor.migrate_abort as usize);
        loaded.entry = Some(descriptor.entry as usize);
        if !loaded.seal_permissions() {
            drop(loaded);
            return Err(ElmEbiLoadStatus::RuntimeRejected);
        }
        if !general::elm_image::sync_elm_image_icache() {
            drop(loaded);
            return Err(ElmEbiLoadStatus::RuntimeRejected);
        }
        Ok(loaded)
    }

    pub(crate) fn cell(&self) -> ElmId {
        self.cell
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn on_initialize(&self, context: &ElmContext) -> ElmResult<()> {
        self.call_hook(self.initialize, context)
    }

    pub(crate) fn on_entry(
        &self,
        parent: Option<ElmId>,
        generation: Generation,
        state: elm_model::ElmState,
    ) -> ElmResult<()> {
        call_optional_native_entry(
            self.entry,
            self.cell,
            parent,
            generation,
            state,
            self.execution_bounds()?,
        )
    }

    pub(crate) fn lifecycle_executor(&self) -> NativeHookExecutor {
        NativeHookExecutor {
            initialize: self.initialize,
            finalize: self.finalize,
            quiesce: self.quiesce,
            pause: self.pause,
            resume: self.resume,
            migrate_export: self.migrate_export,
            migrate_import: self.migrate_import,
            migrate_abort: self.migrate_abort,
            bounds: self.execution_bounds().unwrap_or(NativeExecutionBounds {
                code_start: 0,
                code_end: 0,
                image_start: 0,
                image_end: 0,
            }),
        }
    }

    pub(crate) fn execution_bounds(&self) -> Result<NativeExecutionBounds, ElmError> {
        let mut code_start = usize::MAX;
        let mut code_end = 0usize;
        for segment in self
            .segments
            .iter()
            .filter(|segment| segment.kind == ElmEbiSegmentKind::Code)
        {
            code_start = code_start.min(segment.vaddr);
            code_end = code_end.max(
                segment
                    .vaddr
                    .checked_add(segment.size)
                    .ok_or(ElmError::InvalidTransition)?,
            );
        }
        let image_end = self
            .base
            .checked_add(self.size)
            .ok_or(ElmError::InvalidTransition)?;
        if code_start == usize::MAX || code_start >= code_end || image_end <= self.base {
            return Err(ElmError::InvalidTransition);
        }
        Ok(NativeExecutionBounds {
            code_start,
            code_end,
            image_start: self.base,
            image_end,
        })
    }

    pub(crate) fn export_address(&self, name: &str) -> Result<usize, ElmEbiLoadStatus> {
        self.symbol_address(name)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)
    }

    pub(crate) fn provider_handler_for_decl(
        &self,
        decl: &ElmEbiProviderPortDecl,
    ) -> Result<Option<usize>, ElmEbiLoadStatus> {
        let Some(symbol) = &decl.handler_symbol else {
            return Ok(None);
        };
        self.symbol_address(symbol)
            .map(Some)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)
    }

    pub(crate) fn provider_snapshot_for_decl(
        &self,
        decl: &ElmEbiProviderPortDecl,
    ) -> Result<Option<usize>, ElmEbiLoadStatus> {
        let Some(symbol) = &decl.snapshot_symbol else {
            return Ok(None);
        };
        self.symbol_address(symbol)
            .map(Some)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)
    }

    fn populate_segments(&mut self, image: &ElmEbiImage) -> Result<(), ElmEbiLoadStatus> {
        for payload in &image.payloads {
            let Some(segment) = self.segment_by_unit_index(payload.segment_index) else {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            };
            if !matches!(
                payload.kind,
                ElmEbiSegmentKind::Code
                    | ElmEbiSegmentKind::ReadOnlyData
                    | ElmEbiSegmentKind::Data
                    | ElmEbiSegmentKind::Bss
            ) {
                continue;
            }
            copy_payload(segment, payload)?;
        }
        Ok(())
    }

    fn apply_relocations(
        &self,
        image: &ElmEbiImage,
        imports: &[usize],
    ) -> Result<(), ElmEbiLoadStatus> {
        for relocation in &image.relocations {
            let Some(target) = self.segment_by_unit_index(relocation.target_segment_index) else {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            };
            let width = relocation_width(relocation.kind) as usize;
            let target_offset = usize::try_from(relocation.target_offset)
                .map_err(|_| ElmEbiLoadStatus::InvalidSegment)?;
            let target_end = target_offset
                .checked_add(width)
                .ok_or(ElmEbiLoadStatus::InvalidSegment)?;
            if target_end > target.size {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
            let target_addr = target
                .vaddr
                .checked_add(target_offset)
                .ok_or(ElmEbiLoadStatus::InvalidSegment)?;
            let value =
                self.relocation_value(image, imports, relocation.kind, relocation.value_index)?;
            if matches!(
                relocation.kind,
                ElmEbiRelocationKind::ImportAbs64
                    | ElmEbiRelocationKind::ImportRel32
                    | ElmEbiRelocationKind::ImportRel64
            ) && image
                .unit
                .imports
                .get(relocation.value_index as usize)
                .is_some_and(|import| import.is_kernel_symbol())
            {
                log::info!(
                    "[elm] relocated direct kernel symbol import={} target=0x{:x} value=0x{:x}",
                    relocation.value_index,
                    target_addr,
                    value
                );
            }
            match relocation.kind {
                ElmEbiRelocationKind::SymbolRel32
                | ElmEbiRelocationKind::SymbolRel64
                | ElmEbiRelocationKind::ImportRel32
                | ElmEbiRelocationKind::ImportRel64 => {
                    let signed = signed_delta(value, relocation.addend, target_addr)?;
                    write_signed_relocation(target_addr, signed, width)?;
                }
                _ => {
                    let absolute = add_signed(value, relocation.addend)?;
                    write_unsigned_relocation(target_addr, absolute, width)?;
                }
            }
        }
        Ok(())
    }

    fn relocation_value(
        &self,
        image: &ElmEbiImage,
        imports: &[usize],
        kind: ElmEbiRelocationKind,
        value_index: u32,
    ) -> Result<usize, ElmEbiLoadStatus> {
        match kind {
            ElmEbiRelocationKind::ImageBase64 => Ok(self.base),
            ElmEbiRelocationKind::SegmentBase64 => self
                .segment_by_unit_index(value_index)
                .map(|segment| segment.vaddr)
                .ok_or(ElmEbiLoadStatus::InvalidSegment),
            ElmEbiRelocationKind::SymbolAbs64
            | ElmEbiRelocationKind::SymbolRel32
            | ElmEbiRelocationKind::SymbolRel64 => {
                let symbol = image
                    .symbol_locations
                    .get(value_index as usize)
                    .ok_or(ElmEbiLoadStatus::InvalidSegment)?;
                self.symbol_address_from_image(image, &symbol.name)
                    .ok_or(ElmEbiLoadStatus::InvalidSegment)
            }
            ElmEbiRelocationKind::ImportAbs64
            | ElmEbiRelocationKind::ImportRel32
            | ElmEbiRelocationKind::ImportRel64 => imports
                .get(value_index as usize)
                .copied()
                .ok_or(ElmEbiLoadStatus::InvalidSegment),
        }
    }

    fn collect_symbols(&self, image: &ElmEbiImage) -> Result<Vec<NativeSymbol>, ElmEbiLoadStatus> {
        let mut symbols = Vec::new();
        symbols
            .try_reserve_exact(image.symbol_locations.len())
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        for symbol in &image.symbol_locations {
            let segment = self
                .segment_by_unit_index(symbol.segment_index)
                .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
            let offset =
                usize::try_from(symbol.offset).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
            let size =
                usize::try_from(symbol.size).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
            offset
                .checked_add(size)
                .filter(|end| *end <= segment.size)
                .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
            let mut name = String::new();
            name.try_reserve_exact(symbol.name.len())
                .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
            name.push_str(&symbol.name);
            symbols.push(NativeSymbol {
                name,
                address: segment
                    .vaddr
                    .checked_add(offset)
                    .ok_or(ElmEbiLoadStatus::InvalidManifest)?,
            });
        }
        Ok(symbols)
    }

    fn symbol_address(&self, name: &str) -> Option<usize> {
        self.symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .map(|symbol| symbol.address)
    }

    fn code_contains(&self, address: usize) -> bool {
        self.segments
            .iter()
            .filter(|segment| segment.kind == ElmEbiSegmentKind::Code)
            .any(|segment| {
                segment
                    .vaddr
                    .checked_add(segment.size)
                    .is_some_and(|end| address >= segment.vaddr && address < end)
            })
    }

    fn symbol_address_from_image(&self, image: &ElmEbiImage, name: &str) -> Option<usize> {
        let symbol = image.symbol_location(name)?;
        let segment = self.segment_by_unit_index(symbol.segment_index)?;
        let offset = usize::try_from(symbol.offset).ok()?;
        let size = usize::try_from(symbol.size).ok()?;
        offset
            .checked_add(size)
            .filter(|end| *end <= segment.size)?;
        segment.vaddr.checked_add(offset)
    }

    fn seal_permissions(&self) -> bool {
        for segment in &self.segments {
            let (read, write, execute) = match segment.kind {
                ElmEbiSegmentKind::Code => (true, false, true),
                ElmEbiSegmentKind::ReadOnlyData => (true, false, false),
                ElmEbiSegmentKind::Data | ElmEbiSegmentKind::Bss => (true, true, false),
                ElmEbiSegmentKind::Note | ElmEbiSegmentKind::Relocation => (true, false, false),
            };
            if !general::elm_image::protect_elm_image_range(
                segment.vaddr,
                segment.size,
                read,
                write,
                execute,
            ) {
                return false;
            }
        }
        true
    }

    fn call_hook(&self, address: usize, context: &ElmContext) -> ElmResult<()> {
        call_native_hook(address, context, self.execution_bounds()?)
    }

    fn segment_by_unit_index(&self, unit_segment_index: u32) -> Option<&NativeSegment> {
        self.segments
            .iter()
            .find(|segment| segment.unit_segment_index == unit_segment_index)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeHookExecutor {
    initialize: usize,
    finalize: usize,
    quiesce: Option<usize>,
    pause: Option<usize>,
    resume: Option<usize>,
    migrate_export: Option<usize>,
    migrate_import: Option<usize>,
    migrate_abort: Option<usize>,
    bounds: NativeExecutionBounds,
}

impl ElmLifecycleExecutor for NativeHookExecutor {
    fn on_initialize(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_native_hook(self.initialize, context, self.bounds)
    }

    fn on_finalize(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_native_hook(self.finalize, context, self.bounds)
    }

    fn on_quiesce(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_optional_native_hook(self.quiesce, context, self.bounds)
    }

    fn on_pause(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_optional_native_hook(self.pause, context, self.bounds)
    }

    fn on_resume(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_optional_native_hook(self.resume, context, self.bounds)
    }

    fn on_migrate_export(
        &mut self,
        cell: ElmId,
        old_generation: Generation,
        new_generation: Generation,
        buffer: &mut [u8],
    ) -> ElmResult<usize> {
        call_native_migration_export(
            self.migrate_export,
            cell,
            old_generation,
            new_generation,
            buffer,
            self.bounds,
        )
    }

    fn on_migrate_import(
        &mut self,
        cell: ElmId,
        old_generation: Generation,
        new_generation: Generation,
        buffer: &mut [u8],
        len: usize,
    ) -> ElmResult<()> {
        call_native_migration_import(
            self.migrate_import,
            cell,
            old_generation,
            new_generation,
            buffer,
            len,
            self.bounds,
        )
    }

    fn on_migrate_abort(
        &mut self,
        cell: ElmId,
        old_generation: Generation,
        new_generation: Generation,
        buffer: &mut [u8],
        len: usize,
    ) -> ElmResult<()> {
        call_optional_native_migration_abort(
            self.migrate_abort,
            cell,
            old_generation,
            new_generation,
            buffer,
            len,
            self.bounds,
        )
    }
}

impl Drop for LoadedElmImage {
    fn drop(&mut self) {
        // 释放前恢复普通内核堆权限，避免 allocator 后续复用同一虚拟区间时继承 RX/RO。
        if !general::elm_image::protect_elm_image_range(self.base, self.size, true, true, false) {
            log::error!(
                "[elm] 无法恢复原生镜像权限，保留映射 cell={} base=0x{:x} size={}",
                self.cell.0,
                self.base,
                self.size
            );
            return;
        }
        if let Err(err) = KERNEL_ALLOCATOR.deallocate(self.base) {
            log::error!(
                "[elm] 无法释放原生镜像 cell={} base=0x{:x} size={}: {:?}",
                self.cell.0,
                self.base,
                self.size,
                err
            );
        }
    }
}

#[derive(Debug, Clone)]
struct SegmentLayout {
    unit_segment_index: u32,
    kind: ElmEbiSegmentKind,
    vaddr: usize,
    size: usize,
}

fn layout_runtime_segments(image: &ElmEbiImage) -> Result<Vec<SegmentLayout>, ElmEbiLoadStatus> {
    let mut offset = 0usize;
    let mut layouts = Vec::new();
    layouts
        .try_reserve_exact(image.unit.segments.len())
        .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
    for (segment_index, segment) in image.unit.segments.iter().enumerate() {
        if !matches!(
            segment.kind,
            ElmEbiSegmentKind::Code
                | ElmEbiSegmentKind::ReadOnlyData
                | ElmEbiSegmentKind::Data
                | ElmEbiSegmentKind::Bss
        ) {
            continue;
        }
        offset = align_up(offset, PAGE_SIZE).ok_or(ElmEbiLoadStatus::InvalidSegment)?;
        let mem_size =
            usize::try_from(segment.mem_size).map_err(|_| ElmEbiLoadStatus::InvalidSegment)?;
        let size = align_up(mem_size, PAGE_SIZE).ok_or(ElmEbiLoadStatus::InvalidSegment)?;
        layouts.push(SegmentLayout {
            unit_segment_index: segment_index as u32,
            kind: segment.kind,
            vaddr: offset,
            size,
        });
        offset = offset
            .checked_add(size)
            .ok_or(ElmEbiLoadStatus::InvalidSegment)?;
    }
    if layouts.is_empty() {
        return Err(ElmEbiLoadStatus::NativeCodeTodo);
    }
    Ok(layouts)
}

fn copy_payload(
    segment: &NativeSegment,
    payload: &ElmEbiSegmentPayload,
) -> Result<(), ElmEbiLoadStatus> {
    if payload.bytes.len() > segment.size {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    if payload.bytes.is_empty() {
        return Ok(());
    }
    // 安全性：目标地址来自本模块刚分配的镜像页，payload 边界已在上方检查。
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload.bytes.as_ptr(),
            segment.vaddr as *mut u8,
            payload.bytes.len(),
        );
    }
    Ok(())
}

fn add_signed(base: usize, addend: i64) -> Result<usize, ElmEbiLoadStatus> {
    if addend >= 0 {
        base.checked_add(addend as usize)
            .ok_or(ElmEbiLoadStatus::InvalidSegment)
    } else {
        base.checked_sub(addend.unsigned_abs() as usize)
            .ok_or(ElmEbiLoadStatus::InvalidSegment)
    }
}

fn signed_delta(value: usize, addend: i64, place: usize) -> Result<i64, ElmEbiLoadStatus> {
    let value = add_signed(value, addend)?;
    (value as i128)
        .checked_sub(place as i128)
        .and_then(|delta| i64::try_from(delta).ok())
        .ok_or(ElmEbiLoadStatus::InvalidSegment)
}

fn write_unsigned_relocation(
    target_addr: usize,
    value: usize,
    width: usize,
) -> Result<(), ElmEbiLoadStatus> {
    match width {
        8 => {
            let bytes = (value as u64).to_le_bytes();
            // 安全性：调用方已完成目标范围检查，写入宽度固定。
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), target_addr as *mut u8, bytes.len())
            };
            Ok(())
        }
        _ => Err(ElmEbiLoadStatus::InvalidSegment),
    }
}

fn write_signed_relocation(
    target_addr: usize,
    value: i64,
    width: usize,
) -> Result<(), ElmEbiLoadStatus> {
    match width {
        4 => {
            let narrowed = i32::try_from(value).map_err(|_| ElmEbiLoadStatus::InvalidSegment)?;
            let bytes = narrowed.to_le_bytes();
            // 安全性：调用方已完成目标范围检查，写入宽度固定。
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), target_addr as *mut u8, bytes.len())
            };
            Ok(())
        }
        8 => {
            let bytes = value.to_le_bytes();
            // 安全性：调用方已完成目标范围检查，写入宽度固定。
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), target_addr as *mut u8, bytes.len())
            };
            Ok(())
        }
        _ => Err(ElmEbiLoadStatus::InvalidSegment),
    }
}

fn call_native_hook(
    address: usize,
    context: &ElmContext,
    bounds: NativeExecutionBounds,
) -> ElmResult<()> {
    if address == 0 {
        log::error!(
            "[elm] 原生生命周期入口为空 cell={} phase={:?}",
            context.cell_id().0,
            context.phase()
        );
        return Err(ElmError::InvalidTransition);
    }
    let invocation = NativeInvocation::enter(context.cell_id(), ELM_GUARD_PHASE_HOOK, 0)?;
    let Some(_current) = try_enter_current_context(context) else {
        log::error!(
            "[elm] 原生生命周期上下文拒绝进入 cell={} phase={:?}",
            context.cell_id().0,
            context.phase()
        );
        return Err(ElmError::InvalidTransition);
    };
    let mut native_context = ElmNativeHookContextV1::from_context(context);
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内。
    // 调用约定固定为 ELM native hook v1：`fn(*mut ElmNativeHookContextV1) -> i32`。
    let status = unsafe {
        invocation.invoke(
            address,
            &mut native_context as *mut ElmNativeHookContextV1,
            bounds,
            &[],
        )
    };
    let outcome = invocation.finish();
    if outcome.aborted || outcome.budget_exceeded {
        log::error!(
            "[elm] native lifecycle aborted cell={} phase={:?} address=0x{:x} aborted={} budget_exceeded={} status={}",
            context.cell_id().0,
            context.phase(),
            address,
            outcome.aborted,
            outcome.budget_exceeded,
            status
        );
        return Err(ElmError::InvalidTransition);
    }
    if status == 0 {
        Ok(())
    } else {
        log::error!(
            "[elm] native lifecycle returned error cell={} phase={:?} address=0x{:x} status={}",
            context.cell_id().0,
            context.phase(),
            address,
            status
        );
        Err(ElmError::InvalidTransition)
    }
}

fn call_optional_native_hook(
    address: Option<usize>,
    context: &ElmContext,
    bounds: NativeExecutionBounds,
) -> ElmResult<()> {
    match address {
        Some(address) => call_native_hook(address, context, bounds),
        None => Ok(()),
    }
}

fn call_native_migration_export(
    address: Option<usize>,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
    bounds: NativeExecutionBounds,
) -> ElmResult<usize> {
    let Some(address) = address else {
        return Err(ElmError::InvalidTransition);
    };
    call_native_migration_hook(
        address,
        ElmLifecyclePhase::MigrateExport,
        cell,
        old_generation,
        new_generation,
        buffer,
        0,
        bounds,
    )
}

fn call_native_migration_import(
    address: Option<usize>,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
    len: usize,
    bounds: NativeExecutionBounds,
) -> ElmResult<()> {
    let Some(address) = address else {
        return Err(ElmError::InvalidTransition);
    };
    let out_len = call_native_migration_hook(
        address,
        ElmLifecyclePhase::MigrateImport,
        cell,
        old_generation,
        new_generation,
        buffer,
        len,
        bounds,
    )?;
    if out_len == len {
        Ok(())
    } else {
        Err(ElmError::InvalidTransition)
    }
}

fn call_optional_native_migration_abort(
    address: Option<usize>,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
    len: usize,
    bounds: NativeExecutionBounds,
) -> ElmResult<()> {
    let Some(address) = address else {
        return Ok(());
    };
    let _ = call_native_migration_hook(
        address,
        ElmLifecyclePhase::MigrateAbort,
        cell,
        old_generation,
        new_generation,
        buffer,
        len,
        bounds,
    )?;
    Ok(())
}

fn call_native_migration_hook(
    address: usize,
    phase: ElmLifecyclePhase,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
    len: usize,
    bounds: NativeExecutionBounds,
) -> ElmResult<usize> {
    if address == 0 || len > buffer.len() {
        return Err(ElmError::InvalidTransition);
    }
    let invocation = NativeInvocation::enter(cell, ELM_GUARD_PHASE_MIGRATION, 0)?;
    let elm_context = ElmContext::new(cell, None, new_generation, ElmState::Active, phase, 0);
    let Some(_current) = try_enter_current_context(&elm_context) else {
        return Err(ElmError::InvalidTransition);
    };
    let mut context = ElmNativeMigrationContextV1::new(
        phase,
        cell,
        old_generation,
        new_generation,
        buffer.as_mut_ptr() as u64,
        buffer.len() as u64,
        len as u64,
    );
    let expected_phase = context.phase;
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内；
    // 迁移缓冲区由内核托管，容量和初始长度已在调用前完成边界检查。
    let buffer_start = buffer.as_mut_ptr() as usize;
    let buffer_end = buffer_start
        .checked_add(buffer.len())
        .ok_or(ElmError::InvalidTransition)?;
    let status = unsafe {
        invocation.invoke(
            address,
            &mut context as *mut ElmNativeMigrationContextV1,
            bounds,
            &[(buffer_start, buffer_end)],
        )
    };
    let outcome = invocation.finish();
    if outcome.aborted || outcome.budget_exceeded {
        log::error!(
            "[elm] native entry aborted cell={} address=0x{:x} aborted={} budget_exceeded={} status={}",
            cell.0,
            address,
            outcome.aborted,
            outcome.budget_exceeded,
            status
        );
        return Err(ElmError::InvalidTransition);
    }
    if status == 0
        && context.abi_version == ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION
        && context.phase == expected_phase
        && context.flags == 0
        && context.cell_id == cell.0
        && context.old_generation == old_generation.0
        && context.new_generation == new_generation.0
        && context.buffer_ptr == buffer.as_mut_ptr() as u64
        && context.buffer_capacity == buffer.len() as u64
        && context.buffer_len <= context.buffer_capacity
        && context.status == 0
        && context.reserved == 0
    {
        Ok(context.buffer_len as usize)
    } else {
        Err(ElmError::InvalidTransition)
    }
}

fn call_optional_native_entry(
    address: Option<usize>,
    cell: ElmId,
    parent: Option<ElmId>,
    generation: Generation,
    state: elm_model::ElmState,
    bounds: NativeExecutionBounds,
) -> ElmResult<()> {
    let Some(address) = address else {
        return Ok(());
    };
    if address == 0 {
        log::error!("[elm] 原生模块入口为空 cell={}", cell.0);
        return Err(ElmError::InvalidTransition);
    }
    let invocation = NativeInvocation::enter(cell, ELM_GUARD_PHASE_ENTRY, 0)?;
    let elm_context = ElmContext::new(
        cell,
        parent,
        generation,
        state,
        ElmLifecyclePhase::Initialize,
        0,
    );
    let Some(_current) = try_enter_current_context(&elm_context) else {
        log::error!("[elm] 原生模块入口上下文拒绝进入 cell={}", cell.0);
        return Err(ElmError::InvalidTransition);
    };
    let mut frame = ElmNativeEntryFrameV1::new(
        cell.0,
        parent.map(|id| id.0).unwrap_or(0),
        generation.0,
        state_code(state),
    );
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内。
    // 调用约定固定为 ELM native entry v1。
    let status = unsafe {
        invocation.invoke(
            address,
            &mut frame as *mut ElmNativeEntryFrameV1,
            bounds,
            &[],
        )
    };
    let outcome = invocation.finish();
    if outcome.aborted || outcome.budget_exceeded {
        log::error!(
            "[elm] 原生模块入口中止 cell={} address=0x{:x} aborted={} budget_exceeded={} status={}",
            cell.0,
            address,
            outcome.aborted,
            outcome.budget_exceeded,
            status
        );
        return Err(ElmError::InvalidTransition);
    }
    if status == 0
        && frame.abi_version == ELM_NATIVE_ENTRY_ABI_VERSION
        && frame.flags == 0
        && frame.reserved0 == 0
        && frame.cell_id == cell.0
        && frame.parent_id == parent.map(|id| id.0).unwrap_or(0)
        && frame.generation == generation.0
        && frame.state == state_code(state)
        && frame.exit_code == 0
        && frame.reserved1 == 0
    {
        Ok(())
    } else {
        log::error!(
            "[elm] native entry invalid cell={} address=0x{:x} status={} frame={:?}",
            cell.0,
            address,
            status,
            frame
        );
        Err(ElmError::InvalidTransition)
    }
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_call_native_entry(
    address: usize,
    cell: ElmId,
    parent: Option<ElmId>,
    generation: Generation,
    state: elm_model::ElmState,
) -> ElmResult<()> {
    let _ = super::resource_accounting::register_cell(cell, elm_model::ElmResourceBudget::DEFAULT);
    let page_start = address & !(PAGE_SIZE - 1);
    let page_end = page_start
        .checked_add(PAGE_SIZE)
        .ok_or(ElmError::InvalidTransition)?;
    call_optional_native_entry(
        Some(address),
        cell,
        parent,
        generation,
        state,
        NativeExecutionBounds {
            code_start: page_start,
            code_end: page_end,
            image_start: page_start,
            image_end: page_end,
        },
    )
}

pub(crate) fn invoke_managed_export(
    address: usize,
    bounds: NativeExecutionBounds,
    import_handle: u64,
    caller: ElmId,
    caller_generation: Generation,
    callee: ElmId,
    callee_generation: Generation,
    request: ElmCallFrame,
    allowed_actions: u32,
) -> ElmReplyFrame {
    if address == 0 {
        return ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    }
    let Ok(invocation) = NativeInvocation::enter(callee, ELM_GUARD_PHASE_MANAGED_CALL, 0) else {
        return ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    };
    let elm_context = ElmContext::new(
        callee,
        None,
        callee_generation,
        ElmState::Active,
        ElmLifecyclePhase::Initialize,
        0,
    )
    .with_allowed_actions(allowed_actions);
    let Some(_current) = try_enter_current_context(&elm_context) else {
        return ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    };
    let mut call = ElmNativeManagedCallV1::new(
        import_handle,
        caller.0,
        caller_generation.0,
        callee.0,
        callee_generation.0,
        request,
    );
    let status = unsafe {
        invocation.invoke(
            address,
            &mut call as *mut ElmNativeManagedCallV1,
            bounds,
            &[],
        )
    };
    let outcome = invocation.finish();
    if outcome.aborted
        || outcome.budget_exceeded
        || status != 0
        || call.abi_version != ELM_NATIVE_MANAGED_CALL_ABI_VERSION
        || call.flags != 0
        || call.reserved0 != 0
        || call.import_handle != import_handle
        || call.caller_cell_id != caller.0
        || call.caller_generation != caller_generation.0
        || call.callee_cell_id != callee.0
        || call.callee_generation != callee_generation.0
        || call.reply.binding_id != request.binding_id
        || call.reply.call_id != request.call_id
        || call.reply.flags != 0
        || call.reply.reserved0 != 0
        || call.reply.reserved1 != 0
        || usize::from(call.reply.payload_len) > call.reply.payload.len()
    {
        return ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    }
    call.reply
}

pub(crate) fn invoke_provider_handler(
    address: usize,
    bounds: NativeExecutionBounds,
    cell: ElmId,
    generation: Generation,
    port: PortId,
    lease: LeaseId,
    frame: ElmCallFrame,
    deadline_ns: u64,
    allowed_actions: u32,
    reply_flags_mask: u32,
) -> ElmReplyFrame {
    if address == 0 {
        return ElmReplyFrame::empty(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    }
    let Ok(invocation) = NativeInvocation::enter(cell, ELM_GUARD_PHASE_PROVIDER_CALL, deadline_ns)
    else {
        return ElmReplyFrame::empty(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    };
    let elm_context = ElmContext::new(
        cell,
        None,
        generation,
        ElmState::Active,
        ElmLifecyclePhase::Initialize,
        0,
    )
    .with_allowed_actions(allowed_actions);
    let Some(_current) = try_enter_current_context(&elm_context) else {
        return ElmReplyFrame::empty(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    };
    let mut call = ElmNativeProviderCallV1::new(cell.0, port.0, lease.0, frame);
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内。
    // 调用约定固定为 ELM native provider call v1。
    let status = unsafe {
        invocation.invoke(
            address,
            &mut call as *mut ElmNativeProviderCallV1,
            bounds,
            &[],
        )
    };
    let outcome = invocation.finish();
    if outcome.aborted
        || outcome.budget_exceeded
        || status != 0
        || call.abi_version != ELM_NATIVE_PROVIDER_CALL_ABI_VERSION
        || call.flags != 0
        || call.reserved0 != 0
        || call.reply.binding_id != frame.binding_id
        || call.reply.call_id != frame.call_id
        || call.reply.flags & !reply_flags_mask != 0
        || call.reply.reserved0 != 0
        || call.reply.reserved1 != 0
        || usize::from(call.reply.payload_len) > call.reply.payload.len()
    {
        return ElmReplyFrame::empty(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    }
    call.reply
}

pub(crate) fn invoke_provider_snapshot(
    address: usize,
    bounds: NativeExecutionBounds,
    cell: ElmId,
    generation: Generation,
    port: PortId,
    binding_id: u64,
    lease: LeaseId,
    request_flags: u32,
    cursor: u32,
    payload: &mut [u8],
    allowed_actions: u32,
) -> (i32, usize, u32, u32, u32) {
    if address == 0 {
        return (ELM_MGR_STATUS_INVALID, 0, 0, 0, 0);
    }
    let Ok(invocation) = NativeInvocation::enter(cell, ELM_GUARD_PHASE_PROVIDER_SNAPSHOT, 0) else {
        return (ELM_MGR_STATUS_INVALID, 0, 0, 0, 0);
    };
    let elm_context = ElmContext::new(
        cell,
        None,
        generation,
        ElmState::Active,
        ElmLifecyclePhase::Initialize,
        0,
    )
    .with_allowed_actions(allowed_actions);
    let Some(_current) = try_enter_current_context(&elm_context) else {
        return (ELM_MGR_STATUS_INVALID, 0, 0, 0, 0);
    };
    let capacity = payload.len().min(u32::MAX as usize) as u32;
    let payload_addr = payload.as_mut_ptr() as u64;
    let mut frame = ElmNativeProviderSnapshotV1::new(
        cell.0,
        port.0,
        binding_id,
        lease.0,
        payload_addr,
        capacity,
    );
    let request_paged = request_flags & ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED != 0;
    if request_paged {
        frame.flags = ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED;
        frame.reserved2 = cursor;
    }
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内；
    // payload 指向 Core 准备的临时内核缓冲区，长度由 capacity 固定约束。
    let payload_start = payload.as_mut_ptr() as usize;
    let payload_end = match payload_start.checked_add(payload.len()) {
        Some(end) => end,
        None => return (ELM_MGR_STATUS_INVALID, 0, 0, 0, 0),
    };
    let call_status = unsafe {
        invocation.invoke(
            address,
            &mut frame as *mut ElmNativeProviderSnapshotV1,
            bounds,
            &[(payload_start, payload_end)],
        )
    };
    let outcome = invocation.finish();
    let allowed_flags = if request_paged {
        ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK
    } else {
        0
    };
    let response_more = frame.flags & ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE != 0;
    if outcome.aborted
        || outcome.budget_exceeded
        || call_status != 0
        || frame.abi_version != ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION
        || frame.flags & !allowed_flags != 0
        || frame.reserved0 != 0
        || frame.cell_id != cell.0
        || frame.port_id != port.0
        || frame.binding_id != binding_id
        || frame.lease_id != lease.0
        || frame.reserved1 != 0
        || (frame.status == ELM_MGR_STATUS_OK && !response_more && frame.reserved2 != 0)
        || (frame.status == ELM_MGR_STATUS_OK
            && response_more
            && (!request_paged || frame.reserved2 == 0 || frame.reserved2 == cursor))
        || frame.payload_addr != payload_addr
        || frame.capacity != capacity
        || frame.payload_len > frame.capacity
    {
        return (ELM_MGR_STATUS_INVALID, 0, 0, 0, 0);
    }
    if frame.status == ELM_MGR_STATUS_OK {
        let flags = if response_more {
            ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE
        } else {
            0
        };
        (
            frame.status,
            frame.payload_len as usize,
            frame.record_count,
            flags,
            frame.reserved2,
        )
    } else {
        (frame.status, 0, 0, 0, 0)
    }
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_rewrite_import_abs64(slot: &mut u64, new_address: usize) -> ElmResult<()> {
    write_unsigned_relocation(slot as *mut u64 as usize, new_address, 8)
        .map_err(|_| ElmError::InvalidTransition)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
