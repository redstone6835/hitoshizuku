//! ELM 原生 EKI 镜像执行器。
//!
//! 本模块只处理 EKI 产出的 EBI 原生镜像：复制段、应用 EKI 原生重定位、切换 W^X
//! 权限并调用生命周期钩子。imports 的地址解析由 Core 提供，本模块只消费已解析地址；
//! 原生 provider handler 和 snapshot 通过显式 symbol 暴露；热替换迁移仍由上层保留为
//! `TODO(elm)` 边界。

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use allocator::{KERNEL_ALLOCATOR, MemoryDomain, MemoryRequest, PAGE_SIZE, PagePolicy, Zeroing};
use elm_model::{
    ELM_CALL_STATUS_PROVIDER_FAULT, ELM_EBI_HOOK_ON_FINALIZE, ELM_EBI_HOOK_ON_INITIALIZE,
    ELM_EBI_HOOK_ON_MIGRATE_ABORT, ELM_EBI_HOOK_ON_MIGRATE_EXPORT, ELM_EBI_HOOK_ON_MIGRATE_IMPORT,
    ELM_EBI_HOOK_ON_PAUSE, ELM_EBI_HOOK_ON_QUIESCE, ELM_EBI_HOOK_ON_RESUME, ELM_MGR_STATUS_INVALID,
    ELM_MGR_STATUS_OK, ELM_NATIVE_ENTRY_ABI_VERSION, ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED,
    ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, ElmCallFrame, ElmContext, ElmEbiImage,
    ElmEbiLoadStatus, ElmEbiProviderPortDecl, ElmEbiRelocationKind, ElmEbiSegmentKind,
    ElmEbiSegmentPayload, ElmError, ElmId, ElmNativeEntryFrameV1, ElmNativeHookContextV1,
    ElmNativeMigrationContextV1, ElmNativeProviderCallV1, ElmNativeProviderSnapshotV1,
    ElmReplyFrame, ElmResult, Generation, LeaseId, PortId, relocation_width, state_code,
};
use general::elm_guard::{
    ELM_GUARD_PHASE_ENTRY, ELM_GUARD_PHASE_HOOK, ELM_GUARD_PHASE_MIGRATION,
    ELM_GUARD_PHASE_PROVIDER_CALL, ELM_GUARD_PHASE_PROVIDER_SNAPSHOT, ElmGuard,
};

use super::core::ElmLifecycleExecutor;

type NativeHook = unsafe extern "C" fn(*mut ElmNativeHookContextV1) -> i32;
type NativeEntry = unsafe extern "C" fn(*mut ElmNativeEntryFrameV1) -> i32;
type NativeMigrationHook = unsafe extern "C" fn(*mut ElmNativeMigrationContextV1) -> i32;
type NativeProviderHandler = unsafe extern "C" fn(*mut ElmNativeProviderCallV1) -> i32;
type NativeProviderSnapshot = unsafe extern "C" fn(*mut ElmNativeProviderSnapshotV1) -> i32;

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

#[derive(Debug, Clone)]
struct NativeImportRelocation {
    import_index: u32,
    kind: ElmEbiRelocationKind,
    target_addr: usize,
    addend: i64,
    rebindable: bool,
}

pub(crate) struct LoadedElmImage {
    cell: ElmId,
    base: usize,
    size: usize,
    segments: Vec<NativeSegment>,
    symbols: Vec<NativeSymbol>,
    import_relocations: Vec<NativeImportRelocation>,
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
        let request = MemoryRequest::new(MemoryDomain::Kernel, total_size, PAGE_SIZE)
            .with_page_policy(PagePolicy::BaseOnly)
            .with_zeroing(Zeroing::Zeroed);
        let record = KERNEL_ALLOCATOR
            .allocate(request)
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        let base = record.ptr;

        let mut loaded = Self {
            cell,
            base,
            size: total_size,
            segments: layouts
                .into_iter()
                .map(|segment| NativeSegment {
                    unit_segment_index: segment.unit_segment_index,
                    kind: segment.kind,
                    vaddr: base + segment.vaddr,
                    size: segment.size,
                })
                .collect(),
            symbols: Vec::new(),
            import_relocations: Vec::new(),
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
        loaded.import_relocations = match loaded.apply_relocations(image, imports) {
            Ok(import_relocations) => import_relocations,
            Err(status) => {
                drop(loaded);
                return Err(status);
            }
        };
        loaded.symbols = loaded.collect_symbols(image)?;
        loaded.initialize = loaded
            .symbol_address(ELM_EBI_HOOK_ON_INITIALIZE)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        loaded.finalize = loaded
            .symbol_address(ELM_EBI_HOOK_ON_FINALIZE)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        loaded.quiesce = loaded.symbol_address(ELM_EBI_HOOK_ON_QUIESCE);
        loaded.pause = loaded.symbol_address(ELM_EBI_HOOK_ON_PAUSE);
        loaded.resume = loaded.symbol_address(ELM_EBI_HOOK_ON_RESUME);
        loaded.migrate_export = loaded.symbol_address(ELM_EBI_HOOK_ON_MIGRATE_EXPORT);
        loaded.migrate_import = loaded.symbol_address(ELM_EBI_HOOK_ON_MIGRATE_IMPORT);
        loaded.migrate_abort = loaded.symbol_address(ELM_EBI_HOOK_ON_MIGRATE_ABORT);
        loaded.entry = match &image.unit.entry {
            Some(entry) => Some(
                loaded
                    .symbol_address(&entry.symbol)
                    .ok_or(ElmEbiLoadStatus::InvalidManifest)?,
            ),
            None => None,
        };
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

    pub(crate) fn on_initialize(&self, context: &ElmContext) -> ElmResult<()> {
        self.call_hook(self.initialize, context)
    }

    pub(crate) fn on_entry(
        &self,
        parent: Option<ElmId>,
        generation: Generation,
        state: elm_model::ElmState,
    ) -> ElmResult<()> {
        call_optional_native_entry(self.entry, self.cell, parent, generation, state)
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
        }
    }

    pub(crate) fn export_address(&self, name: &str) -> Result<usize, ElmEbiLoadStatus> {
        self.symbol_address(name)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)
    }

    pub(crate) fn can_rebind_import_to(&self, import_index: u32, new_address: usize) -> bool {
        let mut found = false;
        for relocation in self
            .import_relocations
            .iter()
            .filter(|relocation| relocation.import_index == import_index)
        {
            found = true;
            if !relocation.rebindable || !import_relocation_value_fits(relocation, new_address) {
                return false;
            }
        }
        found
    }

    pub(crate) fn rebind_import(
        &self,
        import_index: u32,
        new_address: usize,
    ) -> Result<(), ElmEbiLoadStatus> {
        if !self.can_rebind_import_to(import_index, new_address) {
            return Err(ElmEbiLoadStatus::RuntimeRejected);
        }
        let mut patched = 0usize;
        for relocation in self
            .import_relocations
            .iter()
            .filter(|relocation| relocation.import_index == import_index)
        {
            write_import_relocation(relocation, new_address)?;
            patched += 1;
        }
        if patched == 0 {
            Err(ElmEbiLoadStatus::RuntimeRejected)
        } else {
            Ok(())
        }
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
    ) -> Result<Vec<NativeImportRelocation>, ElmEbiLoadStatus> {
        let mut import_relocations = Vec::new();
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
            if matches!(
                relocation.kind,
                ElmEbiRelocationKind::ImportAbs64
                    | ElmEbiRelocationKind::ImportRel32
                    | ElmEbiRelocationKind::ImportRel64
            ) {
                import_relocations.push(NativeImportRelocation {
                    import_index: relocation.value_index,
                    kind: relocation.kind,
                    target_addr,
                    addend: relocation.addend,
                    rebindable: matches!(
                        target.kind,
                        ElmEbiSegmentKind::Data | ElmEbiSegmentKind::Bss
                    ),
                });
            }
        }
        Ok(import_relocations)
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
            symbols.push(NativeSymbol {
                name: symbol.name.to_string(),
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
        call_native_hook(address, context)
    }

    fn segment_by_unit_index(&self, unit_segment_index: u32) -> Option<&NativeSegment> {
        self.segments
            .iter()
            .find(|segment| segment.unit_segment_index == unit_segment_index)
    }
}

pub(crate) struct NativeHookExecutor {
    initialize: usize,
    finalize: usize,
    quiesce: Option<usize>,
    pause: Option<usize>,
    resume: Option<usize>,
    migrate_export: Option<usize>,
    migrate_import: Option<usize>,
    migrate_abort: Option<usize>,
}

impl ElmLifecycleExecutor for NativeHookExecutor {
    fn on_initialize(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_native_hook(self.initialize, context)
    }

    fn on_finalize(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_native_hook(self.finalize, context)
    }

    fn on_quiesce(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_optional_native_hook(self.quiesce, context)
    }

    fn on_pause(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_optional_native_hook(self.pause, context)
    }

    fn on_resume(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        call_optional_native_hook(self.resume, context)
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
        )
    }
}

impl Drop for LoadedElmImage {
    fn drop(&mut self) {
        // 释放前恢复普通内核堆权限，避免 allocator 后续复用同一虚拟区间时继承 RX/RO。
        let _ =
            general::elm_image::protect_elm_image_range(self.base, self.size, true, true, false);
        let _ = KERNEL_ALLOCATOR.deallocate(self.base);
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

fn write_import_relocation(
    relocation: &NativeImportRelocation,
    new_address: usize,
) -> Result<(), ElmEbiLoadStatus> {
    if !relocation.rebindable {
        return Err(ElmEbiLoadStatus::RuntimeRejected);
    }
    match relocation.kind {
        ElmEbiRelocationKind::ImportAbs64 => {
            let absolute = add_signed(new_address, relocation.addend)?;
            write_unsigned_relocation(relocation.target_addr, absolute, 8)
        }
        ElmEbiRelocationKind::ImportRel32 => {
            let signed = signed_delta(new_address, relocation.addend, relocation.target_addr)?;
            write_signed_relocation(relocation.target_addr, signed, 4)
        }
        ElmEbiRelocationKind::ImportRel64 => {
            let signed = signed_delta(new_address, relocation.addend, relocation.target_addr)?;
            write_signed_relocation(relocation.target_addr, signed, 8)
        }
        _ => Err(ElmEbiLoadStatus::InvalidSegment),
    }
}

fn import_relocation_value_fits(relocation: &NativeImportRelocation, new_address: usize) -> bool {
    match relocation.kind {
        ElmEbiRelocationKind::ImportAbs64 => add_signed(new_address, relocation.addend).is_ok(),
        ElmEbiRelocationKind::ImportRel32 => {
            let Ok(value) = signed_delta(new_address, relocation.addend, relocation.target_addr)
            else {
                return false;
            };
            i32::try_from(value).is_ok()
        }
        ElmEbiRelocationKind::ImportRel64 => {
            signed_delta(new_address, relocation.addend, relocation.target_addr).is_ok()
        }
        _ => false,
    }
}

fn call_native_hook(address: usize, context: &ElmContext) -> ElmResult<()> {
    if address == 0 {
        return Err(ElmError::InvalidTransition);
    }
    let Some(guard) = ElmGuard::enter(context.cell_id().0, ELM_GUARD_PHASE_HOOK, 0) else {
        return Err(ElmError::InvalidTransition);
    };
    let mut native_context = ElmNativeHookContextV1::from_context(context);
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内。
    // 调用约定固定为 ELM native hook v1：`fn(*mut ElmNativeHookContextV1) -> i32`。
    let hook: NativeHook = unsafe { core::mem::transmute(address) };
    let status = unsafe { hook(&mut native_context as *mut ElmNativeHookContextV1) };
    if guard.aborted() {
        return Err(ElmError::InvalidTransition);
    }
    if status == 0 {
        Ok(())
    } else {
        Err(ElmError::InvalidTransition)
    }
}

fn call_optional_native_hook(address: Option<usize>, context: &ElmContext) -> ElmResult<()> {
    match address {
        Some(address) => call_native_hook(address, context),
        None => Ok(()),
    }
}

fn call_native_migration_export(
    address: Option<usize>,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
) -> ElmResult<usize> {
    let Some(address) = address else {
        return Err(ElmError::InvalidTransition);
    };
    call_native_migration_hook(
        address,
        elm_model::ElmLifecyclePhase::MigrateExport,
        cell,
        old_generation,
        new_generation,
        buffer,
        0,
    )
}

fn call_native_migration_import(
    address: Option<usize>,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
    len: usize,
) -> ElmResult<()> {
    let Some(address) = address else {
        return Err(ElmError::InvalidTransition);
    };
    let out_len = call_native_migration_hook(
        address,
        elm_model::ElmLifecyclePhase::MigrateImport,
        cell,
        old_generation,
        new_generation,
        buffer,
        len,
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
) -> ElmResult<()> {
    let Some(address) = address else {
        return Ok(());
    };
    let _ = call_native_migration_hook(
        address,
        elm_model::ElmLifecyclePhase::MigrateAbort,
        cell,
        old_generation,
        new_generation,
        buffer,
        len,
    )?;
    Ok(())
}

fn call_native_migration_hook(
    address: usize,
    phase: elm_model::ElmLifecyclePhase,
    cell: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    buffer: &mut [u8],
    len: usize,
) -> ElmResult<usize> {
    if address == 0 || len > buffer.len() {
        return Err(ElmError::InvalidTransition);
    }
    let Some(guard) = ElmGuard::enter(cell.0, ELM_GUARD_PHASE_MIGRATION, 0) else {
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
    let hook: NativeMigrationHook = unsafe { core::mem::transmute(address) };
    let status = unsafe { hook(&mut context as *mut ElmNativeMigrationContextV1) };
    if guard.aborted() {
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
) -> ElmResult<()> {
    let Some(address) = address else {
        return Ok(());
    };
    if address == 0 {
        return Err(ElmError::InvalidTransition);
    }
    let Some(guard) = ElmGuard::enter(cell.0, ELM_GUARD_PHASE_ENTRY, 0) else {
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
    let entry: NativeEntry = unsafe { core::mem::transmute(address) };
    let status = unsafe { entry(&mut frame as *mut ElmNativeEntryFrameV1) };
    if guard.aborted() {
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
    call_optional_native_entry(Some(address), cell, parent, generation, state)
}

pub(crate) fn invoke_provider_handler(
    address: usize,
    cell: ElmId,
    port: PortId,
    lease: LeaseId,
    frame: ElmCallFrame,
    deadline_ns: u64,
) -> ElmReplyFrame {
    if address == 0 {
        return ElmReplyFrame::empty(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    }
    let Some(guard) = ElmGuard::enter(cell.0, ELM_GUARD_PHASE_PROVIDER_CALL, deadline_ns) else {
        return ElmReplyFrame::empty(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
    };
    let mut call = ElmNativeProviderCallV1::new(cell.0, port.0, lease.0, frame);
    // 安全性：地址来自已验证的 EKI 符号位置表，落在已 seal 为 RX 的 Code 段内。
    // 调用约定固定为 ELM native provider call v1。
    let handler: NativeProviderHandler = unsafe { core::mem::transmute(address) };
    let status = unsafe { handler(&mut call as *mut ElmNativeProviderCallV1) };
    if guard.aborted()
        || status != 0
        || call.abi_version != ELM_NATIVE_PROVIDER_CALL_ABI_VERSION
        || call.flags != 0
        || call.reserved0 != 0
        || call.reply.binding_id != frame.binding_id
        || call.reply.call_id != frame.call_id
        || call.reply.flags != 0
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
    cell: ElmId,
    port: PortId,
    binding_id: u64,
    lease: LeaseId,
    request_flags: u32,
    cursor: u32,
    payload: &mut [u8],
) -> (i32, usize, u32, u32, u32) {
    if address == 0 {
        return (ELM_MGR_STATUS_INVALID, 0, 0, 0, 0);
    }
    let Some(guard) = ElmGuard::enter(cell.0, ELM_GUARD_PHASE_PROVIDER_SNAPSHOT, 0) else {
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
    let snapshot: NativeProviderSnapshot = unsafe { core::mem::transmute(address) };
    let call_status = unsafe { snapshot(&mut frame as *mut ElmNativeProviderSnapshotV1) };
    let allowed_flags = if request_paged {
        ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK
    } else {
        0
    };
    let response_more = frame.flags & ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE != 0;
    if guard.aborted()
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
    let relocation = NativeImportRelocation {
        import_index: 0,
        kind: ElmEbiRelocationKind::ImportAbs64,
        target_addr: slot as *mut u64 as usize,
        addend: 0,
        rebindable: true,
    };
    write_import_relocation(&relocation, new_address).map_err(|_| ElmError::InvalidTransition)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
