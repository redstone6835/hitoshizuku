//! Rust ELM 开发侧安全边界。
//!
//! 本模块把 EBI v1 的裸函数指针、原始地址和固定布局帧收敛到少量内部调用门。
//! ELM 业务代码只处理借用、结果类型和显式固定线编码载荷。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::context::{
    ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION, ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
    ElmNativeHookContextV1, ElmNativeMigrationContextV1,
};
#[cfg(feature = "management")]
use crate::elmapi::ElmApiNamespaceV1;
use crate::elmapi::{
    ELM_API_ABORT_REASON_PANIC, ELM_API_ROOT_MAGIC, ELM_API_STATUS_BUFFER_TOO_SMALL,
    ELM_API_VERSION_V1, ElmApiContextV1, ElmApiRootV1, ElmRuntimeApiV1,
};
use crate::frame::{
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_OK, ELM_CALL_STATUS_PROVIDER_FAULT,
    ELM_FRAME_PAYLOAD_LEN, ELM_NATIVE_ENTRY_ABI_VERSION, ELM_NATIVE_MANAGED_CALL_ABI_VERSION,
    ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, ElmCallFrame, ElmNativeEntryFrameV1,
    ElmNativeManagedCallV1, ElmNativeProviderCallV1, ElmNativeProviderSnapshotV1, ElmReplyFrame,
};
use crate::module_wire::{
    MGR_EXTENSION_DISPATCH_RESPONSE_SIZE, MGR_EXTENSION_PAYLOAD_LEN, MGR_RESPONSE_HEADER_SIZE,
    MGR_STATUS_OK, MIXIN_REPLY_CONTINUE, MIXIN_REPLY_DENY, MIXIN_REPLY_REPLACE, MIXIN_REPLY_STOP,
    ModuleExtensionDispatchRequest, ModuleExtensionDispatchResponse, ModuleMgrResponseHeader,
};

pub const ELM_API_ROOT_SLOT_SYMBOL: &str = "__elm_api_root_slot_v1";
pub const ELM_MIXIN_STAGE_INGRESS: u32 = 1 << 0;
pub const ELM_MIXIN_STAGE_SUBSTITUTE: u32 = 1 << 1;
pub const ELM_MIXIN_STAGE_EGRESS: u32 = 1 << 2;
pub const ELM_MIXIN_STAGE_OBSERVE: u32 = 1 << 3;
pub const ELM_MIXIN_STAGES_ALL: u32 = ELM_MIXIN_STAGE_INGRESS
    | ELM_MIXIN_STAGE_SUBSTITUTE
    | ELM_MIXIN_STAGE_EGRESS
    | ELM_MIXIN_STAGE_OBSERVE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookError {
    status: i32,
}

impl HookError {
    pub const fn new(status: i32) -> Self {
        Self {
            status: if status == 0 {
                ELM_CALL_STATUS_INVALID
            } else {
                status
            },
        }
    }

    pub const fn status(self) -> i32 {
        self.status
    }
}

pub type HookResult = Result<(), HookError>;
pub type EntryResult = HookResult;
pub type PointResult = HookResult;
pub type MigrationExportResult = Result<usize, HookError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    BufferTooSmall,
    SizeMismatch,
    InvalidBoolean,
}

pub trait ElmPayload: Sized {
    const CONTRACT: &'static str;
    const WIRE_SIZE: usize;

    fn encode(&self, output: &mut [u8]) -> Result<usize, PayloadError>;
    fn decode(input: &[u8]) -> Result<Self, PayloadError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleContext {
    cell_id: u64,
    parent_id: u64,
    generation: u64,
    state: u32,
    phase: u16,
    flags: u32,
}

impl LifecycleContext {
    const fn from_raw(raw: ElmNativeHookContextV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            parent_id: raw.parent_id,
            generation: raw.generation,
            state: raw.state,
            phase: raw.phase,
            flags: raw.flags,
        }
    }

    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    pub const fn parent_id(self) -> Option<u64> {
        if self.parent_id == 0 {
            None
        } else {
            Some(self.parent_id)
        }
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }

    pub const fn state(self) -> u32 {
        self.state
    }

    pub const fn phase(self) -> u16 {
        self.phase
    }

    pub const fn flags(self) -> u32 {
        self.flags
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationContext {
    cell_id: u64,
    old_generation: u64,
    new_generation: u64,
    phase: u16,
}

impl MigrationContext {
    const fn from_raw(raw: &ElmNativeMigrationContextV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            old_generation: raw.old_generation,
            new_generation: raw.new_generation,
            phase: raw.phase,
        }
    }

    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    pub const fn old_generation(self) -> u64 {
        self.old_generation
    }

    pub const fn new_generation(self) -> u64 {
        self.new_generation
    }

    pub const fn phase(self) -> u16 {
        self.phase
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryContext {
    cell_id: u64,
    parent_id: u64,
    generation: u64,
    state: u32,
}

impl EntryContext {
    const fn from_raw(raw: ElmNativeEntryFrameV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            parent_id: raw.parent_id,
            generation: raw.generation,
            state: raw.state,
        }
    }

    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    pub const fn parent_id(self) -> Option<u64> {
        if self.parent_id == 0 {
            None
        } else {
            Some(self.parent_id)
        }
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }

    pub const fn state(self) -> u32 {
        self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRequest {
    pub cell_id: u64,
    pub port_id: u64,
    pub lease_id: u64,
    pub frame: ElmCallFrame,
}

impl ProviderRequest {
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    pub fn decode<T: ElmPayload>(&self) -> Result<T, PayloadError> {
        T::decode(self.payload())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedRequest {
    pub import_handle: u64,
    pub caller_cell_id: u64,
    pub caller_generation: u64,
    pub callee_cell_id: u64,
    pub callee_generation: u64,
    pub frame: ElmCallFrame,
}

impl ManagedRequest {
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    pub fn decode<T: ElmPayload>(&self) -> Result<T, PayloadError> {
        T::decode(self.payload())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderReply {
    status: i32,
    flags: u32,
    payload_len: u16,
    payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

impl ProviderReply {
    pub const fn empty(status: i32) -> Self {
        Self {
            status,
            flags: 0,
            payload_len: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    pub const fn ok() -> Self {
        Self::empty(ELM_CALL_STATUS_OK)
    }

    pub fn bytes(status: i32, payload: &[u8]) -> Result<Self, PayloadError> {
        if payload.len() > ELM_FRAME_PAYLOAD_LEN {
            return Err(PayloadError::BufferTooSmall);
        }
        let mut reply = Self::empty(status);
        reply.payload[..payload.len()].copy_from_slice(payload);
        reply.payload_len = payload.len() as u16;
        Ok(reply)
    }

    pub fn payload<T: ElmPayload>(status: i32, payload: &T) -> Result<Self, PayloadError> {
        if T::WIRE_SIZE > ELM_FRAME_PAYLOAD_LEN {
            return Err(PayloadError::BufferTooSmall);
        }
        let mut reply = Self::empty(status);
        let len = payload.encode(&mut reply.payload)?;
        reply.payload_len = len as u16;
        Ok(reply)
    }

    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    fn into_frame(self, binding_id: u64, call_id: u64) -> ElmReplyFrame {
        let mut frame = ElmReplyFrame::empty(binding_id, call_id, self.status);
        frame.flags = self.flags;
        frame.payload_len = self.payload_len;
        frame.payload = self.payload;
        frame
    }
}

pub type ProviderResult = Result<ProviderReply, HookError>;
pub type ManagedResult = ProviderResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedReply {
    frame: ElmReplyFrame,
}

impl ManagedReply {
    fn from_frame(frame: ElmReplyFrame) -> Result<Self, RuntimeApiError> {
        if frame.reserved0 != 0
            || frame.reserved1 != 0
            || usize::from(frame.payload_len) > frame.payload.len()
        {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(Self { frame })
    }

    pub const fn status(self) -> i32 {
        self.frame.status
    }

    pub const fn flags(self) -> u32 {
        self.frame.flags
    }

    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    pub fn decode<T: ElmPayload>(&self) -> Result<T, RuntimeApiError> {
        T::decode(self.payload()).map_err(RuntimeApiError::Payload)
    }

    pub const fn into_frame(self) -> ElmReplyFrame {
        self.frame
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRequest {
    pub cell_id: u64,
    pub port_id: u64,
    pub binding_id: u64,
    pub lease_id: u64,
    pub paged: bool,
    pub cursor: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotReply {
    pub status: i32,
    pub payload_len: usize,
    pub record_count: u32,
    pub next_cursor: Option<u32>,
}

impl SnapshotReply {
    pub const fn complete(payload_len: usize, record_count: u32) -> Self {
        Self {
            status: MGR_STATUS_OK,
            payload_len,
            record_count,
            next_cursor: None,
        }
    }

    pub const fn more(payload_len: usize, record_count: u32, next_cursor: u32) -> Self {
        Self {
            status: MGR_STATUS_OK,
            payload_len,
            record_count,
            next_cursor: Some(next_cursor),
        }
    }

    pub const fn error(status: i32) -> Self {
        Self {
            status,
            payload_len: 0,
            record_count: 0,
            next_cursor: None,
        }
    }
}

pub type SnapshotResult = Result<SnapshotReply, HookError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixinControl {
    Continue,
    Stop,
    Replace,
    ReplaceAndStop,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixinPointDescriptor {
    pub contract: &'static str,
    pub ingress: Option<&'static str>,
    pub substitute: Option<&'static str>,
    pub egress: Option<&'static str>,
    pub observe: Option<&'static str>,
}

#[repr(transparent)]
#[derive(Debug)]
pub struct ManagedImport {
    slot: ImportSlot,
}

impl ManagedImport {
    pub const fn new() -> Self {
        Self {
            slot: ImportSlot::new(),
        }
    }

    pub fn handle(&self) -> Option<u64> {
        let value = self.slot.read();
        (value != 0).then_some(value as u64)
    }

    pub fn invoke(&self, request: &ElmCallFrame) -> Result<ElmReplyFrame, RuntimeApiError> {
        let handle = self.handle().ok_or(RuntimeApiError::ImportUnavailable)?;
        runtime_api::invoke_managed(handle, request)
    }

    pub fn call_bytes(&self, opcode: u32, payload: &[u8]) -> Result<ManagedReply, RuntimeApiError> {
        if payload.len() > ELM_FRAME_PAYLOAD_LEN {
            return Err(RuntimeApiError::Payload(PayloadError::BufferTooSmall));
        }
        let request = ElmCallFrame::new(0, next_managed_call_id(), opcode, payload);
        ManagedReply::from_frame(self.invoke(&request)?)
    }

    pub fn call_payload<T: ElmPayload>(
        &self,
        opcode: u32,
        payload: &T,
    ) -> Result<ManagedReply, RuntimeApiError> {
        if T::WIRE_SIZE > ELM_FRAME_PAYLOAD_LEN {
            return Err(RuntimeApiError::Payload(PayloadError::BufferTooSmall));
        }
        let mut bytes = [0u8; ELM_FRAME_PAYLOAD_LEN];
        let len = payload.encode(&mut bytes)?;
        if len > bytes.len() {
            return Err(RuntimeApiError::MalformedResponse);
        }
        self.call_bytes(opcode, &bytes[..len])
    }

    pub fn call<T: ElmPayload, R: ElmPayload>(
        &self,
        opcode: u32,
        payload: &T,
    ) -> Result<R, RuntimeApiError> {
        let reply = self.call_payload(opcode, payload)?;
        if reply.status() != ELM_CALL_STATUS_OK {
            return Err(RuntimeApiError::Status(reply.status()));
        }
        reply.decode()
    }
}

impl Default for ManagedImport {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Debug)]
pub struct UnsafeDirectImport {
    slot: ImportSlot,
}

impl UnsafeDirectImport {
    pub const fn new() -> Self {
        Self {
            slot: ImportSlot::new(),
        }
    }

    /// 调用方必须自行证明目标函数签名、生命周期和代际固定关系均匹配。
    pub unsafe fn address(&self) -> Option<usize> {
        let value = self.slot.read();
        (value != 0).then_some(value)
    }
}

impl Default for UnsafeDirectImport {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct ImportSlot(UnsafeCell<usize>);

impl ImportSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    fn read(&self) -> usize {
        // 安全性：装载器只在激活前写入槽位；运行期只做易失只读访问。
        unsafe { core::ptr::read_volatile(self.0.get()) }
    }
}

unsafe impl Sync for ImportSlot {}

#[repr(transparent)]
struct RootImportSlot(UnsafeCell<usize>);

unsafe impl Sync for RootImportSlot {}

static NEXT_MANAGED_CALL_ID: AtomicU64 = AtomicU64::new(1);

fn next_managed_call_id() -> u64 {
    let id = NEXT_MANAGED_CALL_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        NEXT_MANAGED_CALL_ID.fetch_add(1, Ordering::Relaxed)
    } else {
        id
    }
}

#[unsafe(export_name = "__elm_api_root_slot_v1")]
#[unsafe(link_section = ".data.elm_imports")]
#[used]
static ELM_API_ROOT_SLOT: RootImportSlot = RootImportSlot(UnsafeCell::new(0));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeApiError {
    RootUnavailable,
    IncompatibleRoot,
    RuntimeUnavailable,
    ImportUnavailable,
    BufferTooSmall(usize),
    MalformedResponse,
    Status(i32),
    Payload(PayloadError),
}

impl From<PayloadError> for RuntimeApiError {
    fn from(value: PayloadError) -> Self {
        Self::Payload(value)
    }
}

pub(crate) mod runtime_api {
    use super::*;

    pub fn features() -> Result<u64, RuntimeApiError> {
        Ok(root()?.features)
    }

    pub fn log(level: u32, message: &str) -> Result<(), RuntimeApiError> {
        let status = (runtime()?.log)(level, message.as_ptr(), message.len());
        status_result(status)
    }

    pub fn abort_current(reason: u32) -> ! {
        match runtime() {
            Ok(runtime) => (runtime.abort_current)(reason),
            Err(_) => loop {
                core::hint::spin_loop();
            },
        }
    }

    pub fn abort_panic() -> ! {
        abort_current(ELM_API_ABORT_REASON_PANIC)
    }

    pub fn current_context() -> Result<ElmApiContextV1, RuntimeApiError> {
        let mut output = ElmApiContextV1::empty();
        let status = (runtime()?.current_context)(&mut output);
        status_result(status)?;
        Ok(output)
    }

    pub fn dispatch_mixin(input: &[u8], output: &mut [u8]) -> Result<usize, RuntimeApiError> {
        let mut output_len = 0usize;
        let status = (runtime()?.dispatch_mixin)(
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &mut output_len,
        );
        if status == ELM_API_STATUS_BUFFER_TOO_SMALL {
            return Err(RuntimeApiError::BufferTooSmall(output_len));
        }
        status_result(status)?;
        if output_len > output.len() {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(output_len)
    }

    pub fn invoke_managed(
        import_handle: u64,
        request: &ElmCallFrame,
    ) -> Result<ElmReplyFrame, RuntimeApiError> {
        let mut reply = ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
        let status = (runtime()?.invoke_managed)(import_handle, request, &mut reply);
        status_result(status)?;
        if reply.binding_id != request.binding_id || reply.call_id != request.call_id {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(reply)
    }

    #[cfg(feature = "management")]
    pub fn query_namespace(
        identifier: &str,
        versions: &[u16],
    ) -> Result<ElmApiNamespaceV1, RuntimeApiError> {
        let mut output = ElmApiNamespaceV1::empty();
        let status = (root()?.query_namespace)(
            identifier.as_ptr(),
            identifier.len(),
            versions.as_ptr(),
            versions.len(),
            &mut output,
        );
        status_result(status)?;
        Ok(output)
    }

    pub(crate) fn ensure_linked() {
        let _ = root_address();
    }

    fn root() -> Result<&'static ElmApiRootV1, RuntimeApiError> {
        let address = root_address();
        if address == 0 {
            return Err(RuntimeApiError::RootUnavailable);
        }
        // 安全性：槽位只由 ELM 装载器写入经过 ABI 校验的静态根表地址。
        let root = unsafe { &*(address as *const ElmApiRootV1) };
        if root.magic != ELM_API_ROOT_MAGIC
            || root.abi_version != ELM_API_VERSION_V1
            || root.selected_version != ELM_API_VERSION_V1
            || root.struct_size < core::mem::size_of::<ElmApiRootV1>() as u32
        {
            return Err(RuntimeApiError::IncompatibleRoot);
        }
        Ok(root)
    }

    fn runtime() -> Result<&'static ElmRuntimeApiV1, RuntimeApiError> {
        let root = root()?;
        if root.runtime_table.is_null()
            || root.runtime_table_size < core::mem::size_of::<ElmRuntimeApiV1>() as u32
        {
            return Err(RuntimeApiError::RuntimeUnavailable);
        }
        // 安全性：根表由内核发布，且已验证表地址和最小尺寸。
        let runtime = unsafe { &*root.runtime_table };
        if runtime.abi_version != ELM_API_VERSION_V1
            || runtime.struct_size < core::mem::size_of::<ElmRuntimeApiV1>() as u32
        {
            return Err(RuntimeApiError::RuntimeUnavailable);
        }
        Ok(runtime)
    }

    fn root_address() -> usize {
        // 安全性：装载阶段完成单次槽位重定位，运行阶段只做易失读取。
        unsafe { core::ptr::read_volatile(ELM_API_ROOT_SLOT.0.get()) }
    }

    fn status_result(status: i32) -> Result<(), RuntimeApiError> {
        if status == 0 {
            Ok(())
        } else {
            Err(RuntimeApiError::Status(status))
        }
    }
}

pub fn run_mixin_point<T: ElmPayload>(
    descriptor: MixinPointDescriptor,
    frame: &mut T,
    original: fn(&mut T) -> PointResult,
) -> PointResult {
    if let Some(point) = descriptor.ingress {
        dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    let substituted = match descriptor.substitute {
        Some(point) => dispatch_mixin_stage(point, descriptor.contract, frame)?,
        None => false,
    };
    if !substituted {
        original(frame)?;
    }
    if let Some(point) = descriptor.egress {
        dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    if let Some(point) = descriptor.observe {
        let _ = dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    Ok(())
}

fn dispatch_mixin_stage<T: ElmPayload>(
    point: &str,
    contract: &str,
    frame: &mut T,
) -> Result<bool, HookError> {
    if T::WIRE_SIZE > MGR_EXTENSION_PAYLOAD_LEN {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let context = runtime_api::current_context().map_err(runtime_error_to_hook)?;
    let mut request = ModuleExtensionDispatchRequest::new(context.cell_id, point, contract)
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    let payload_len = frame
        .encode(&mut request.payload)
        .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
    request.payload_len = payload_len as u16;
    let input = request.encode();
    let mut output = [0u8; MGR_RESPONSE_HEADER_SIZE + MGR_EXTENSION_DISPATCH_RESPONSE_SIZE];
    let output_len =
        runtime_api::dispatch_mixin(&input, &mut output).map_err(runtime_error_to_hook)?;
    let header_size = MGR_RESPONSE_HEADER_SIZE;
    let response_size = MGR_EXTENSION_DISPATCH_RESPONSE_SIZE;
    if output_len != header_size + response_size {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let header = ModuleMgrResponseHeader::decode(&output[..header_size])
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    if header.status != MGR_STATUS_OK
        || header.reserved != 0
        || header.payload_len as usize != response_size
    {
        return Err(HookError::new(header.status));
    }
    let response = ModuleExtensionDispatchResponse::decode(&output[header_size..])
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    if response.status != MGR_STATUS_OK || response.blockers != 0 {
        return Err(HookError::new(response.status));
    }
    if response.reply.flags & MIXIN_REPLY_DENY != 0 {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let replaced = response.reply.flags & MIXIN_REPLY_REPLACE != 0;
    if replaced {
        let len = usize::from(response.reply.payload_len);
        if len > response.reply.payload.len() {
            return Err(HookError::new(ELM_CALL_STATUS_INVALID));
        }
        *frame = T::decode(&response.reply.payload[..len])
            .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
    }
    Ok(replaced)
}

fn runtime_error_to_hook(error: RuntimeApiError) -> HookError {
    match error {
        RuntimeApiError::Status(status) => HookError::new(status),
        _ => HookError::new(ELM_CALL_STATUS_INVALID),
    }
}

#[doc(hidden)]
pub mod __private {
    use super::*;

    pub unsafe fn lifecycle_trampoline(
        raw: *mut ElmNativeHookContextV1,
        expected_phase: u16,
        handler: fn(&LifecycleContext) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_ref() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION
            || raw.phase != expected_phase
            || raw.reserved != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match handler(&LifecycleContext::from_raw(*raw)) {
            Ok(()) => 0,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn migration_export_trampoline(
        raw: *mut ElmNativeMigrationContextV1,
        handler: fn(&MigrationContext, &mut [u8]) -> MigrationExportResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, 6) {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(capacity) = usize::try_from(raw.buffer_capacity) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.buffer_ptr == 0 && capacity != 0 {
            return ELM_CALL_STATUS_INVALID;
        }
        let output = if capacity == 0 {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(raw.buffer_ptr as *mut u8, capacity) }
        };
        match handler(&MigrationContext::from_raw(raw), output) {
            Ok(len) if len <= capacity => {
                raw.buffer_len = len as u64;
                raw.status = 0;
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn migration_input_trampoline(
        raw: *mut ElmNativeMigrationContextV1,
        expected_phase: u16,
        handler: fn(&MigrationContext, &[u8]) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, expected_phase)
            || raw.buffer_len > raw.buffer_capacity
            || raw.buffer_ptr == 0 && raw.buffer_len != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(len) = usize::try_from(raw.buffer_len) else {
            return ELM_CALL_STATUS_INVALID;
        };
        let input = if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(raw.buffer_ptr as *const u8, len) }
        };
        match handler(&MigrationContext::from_raw(raw), input) {
            Ok(()) => {
                raw.status = 0;
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn entry_trampoline(
        raw: *mut ElmNativeEntryFrameV1,
        handler: fn(&EntryContext) -> EntryResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_ENTRY_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match handler(&EntryContext::from_raw(*raw)) {
            Ok(()) => {
                raw.exit_code = 0;
                0
            }
            Err(error) => {
                raw.exit_code = error.status();
                error.status()
            }
        }
    }

    pub unsafe fn provider_trampoline<F>(raw: *mut ElmNativeProviderCallV1, handler: F) -> i32
    where
        F: FnOnce(&ProviderRequest) -> ProviderResult,
    {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_PROVIDER_CALL_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.binding_id != raw.request.binding_id
            || usize::from(raw.request.payload_len) > raw.request.payload.len()
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let request = ProviderRequest {
            cell_id: raw.cell_id,
            port_id: raw.port_id,
            lease_id: raw.lease_id,
            frame: raw.request,
        };
        match handler(&request) {
            Ok(reply) => {
                raw.reply = reply.into_frame(raw.request.binding_id, raw.request.call_id);
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn managed_trampoline(
        raw: *mut ElmNativeManagedCallV1,
        handler: fn(&ManagedRequest) -> ManagedResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_MANAGED_CALL_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || usize::from(raw.request.payload_len) > raw.request.payload.len()
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let request = ManagedRequest {
            import_handle: raw.import_handle,
            caller_cell_id: raw.caller_cell_id,
            caller_generation: raw.caller_generation,
            callee_cell_id: raw.callee_cell_id,
            callee_generation: raw.callee_generation,
            frame: raw.request,
        };
        match handler(&request) {
            Ok(reply) => {
                raw.reply = reply.into_frame(raw.request.binding_id, raw.request.call_id);
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn snapshot_trampoline(
        raw: *mut ElmNativeProviderSnapshotV1,
        handler: fn(&SnapshotRequest, &mut [u8]) -> SnapshotResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION
            || raw.flags & !ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
            || raw.payload_addr == 0 && raw.capacity != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let paged = raw.flags & ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED != 0;
        let request = SnapshotRequest {
            cell_id: raw.cell_id,
            port_id: raw.port_id,
            binding_id: raw.binding_id,
            lease_id: raw.lease_id,
            paged,
            cursor: if paged { raw.reserved2 } else { 0 },
        };
        let output = if raw.capacity == 0 {
            &mut []
        } else {
            unsafe {
                core::slice::from_raw_parts_mut(raw.payload_addr as *mut u8, raw.capacity as usize)
            }
        };
        match handler(&request, output) {
            Ok(reply) if reply.payload_len <= output.len() => {
                raw.status = reply.status;
                raw.payload_len = reply.payload_len as u32;
                raw.record_count = reply.record_count;
                raw.flags = if paged {
                    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED
                } else {
                    0
                };
                if let Some(next) = reply.next_cursor {
                    if !paged || next == 0 || next == request.cursor {
                        return ELM_CALL_STATUS_INVALID;
                    }
                    raw.flags |= ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE;
                    raw.reserved2 = next;
                } else {
                    raw.reserved2 = 0;
                }
                if raw.flags & !ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK != 0 {
                    return ELM_CALL_STATUS_INVALID;
                }
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn mixin_trampoline<T: ElmPayload>(
        raw: *mut ElmNativeProviderCallV1,
        handler: fn(&mut T) -> MixinControl,
    ) -> i32 {
        unsafe {
            provider_trampoline(raw, |request| {
                let mut frame = request
                    .decode::<T>()
                    .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
                let control = handler(&mut frame);
                let flags = match control {
                    MixinControl::Continue => MIXIN_REPLY_CONTINUE,
                    MixinControl::Stop => MIXIN_REPLY_STOP,
                    MixinControl::Replace => MIXIN_REPLY_REPLACE,
                    MixinControl::ReplaceAndStop => MIXIN_REPLY_REPLACE | MIXIN_REPLY_STOP,
                    MixinControl::Deny => MIXIN_REPLY_DENY,
                };
                let reply = if flags & MIXIN_REPLY_REPLACE != 0 {
                    ProviderReply::payload(ELM_CALL_STATUS_OK, &frame)
                        .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?
                } else {
                    ProviderReply::ok()
                };
                Ok(reply.with_flags(flags))
            })
        }
    }

    pub fn write_bytes(
        output: &mut [u8],
        offset: &mut usize,
        bytes: &[u8],
    ) -> Result<(), PayloadError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(PayloadError::BufferTooSmall)?;
        let target = output
            .get_mut(*offset..end)
            .ok_or(PayloadError::BufferTooSmall)?;
        target.copy_from_slice(bytes);
        *offset = end;
        Ok(())
    }

    pub fn read_array<const N: usize>(
        input: &[u8],
        offset: &mut usize,
    ) -> Result<[u8; N], PayloadError> {
        let end = offset.checked_add(N).ok_or(PayloadError::SizeMismatch)?;
        let source = input.get(*offset..end).ok_or(PayloadError::SizeMismatch)?;
        let mut output = [0u8; N];
        output.copy_from_slice(source);
        *offset = end;
        Ok(output)
    }

    pub fn read_bool(input: &[u8], offset: &mut usize) -> Result<bool, PayloadError> {
        match read_array::<1>(input, offset)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PayloadError::InvalidBoolean),
        }
    }

    fn migration_context_valid(raw: &ElmNativeMigrationContextV1, phase: u16) -> bool {
        raw.abi_version == ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION
            && raw.phase == phase
            && raw.flags == 0
            && raw.status == 0
            && raw.reserved == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ElmLifecyclePhase, ElmNativeMigrationContextV1};
    use crate::ids::{ElmId, Generation};

    fn export_empty(_context: &MigrationContext, output: &mut [u8]) -> MigrationExportResult {
        assert!(output.is_empty());
        Ok(0)
    }

    fn snapshot_empty(_request: &SnapshotRequest, output: &mut [u8]) -> SnapshotResult {
        assert!(output.is_empty());
        Ok(SnapshotReply::complete(0, 0))
    }

    #[test]
    fn import_wrappers_have_one_word_layout() {
        assert_eq!(
            core::mem::size_of::<ManagedImport>(),
            core::mem::size_of::<usize>()
        );
        assert_eq!(
            core::mem::size_of::<UnsafeDirectImport>(),
            core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn zero_length_native_buffers_do_not_require_non_null_pointer() {
        let mut migration = ElmNativeMigrationContextV1::new(
            ElmLifecyclePhase::MigrateExport,
            ElmId(7),
            Generation(1),
            Generation(2),
            0,
            0,
            0,
        );
        let migration_status =
            unsafe { __private::migration_export_trampoline(&mut migration, export_empty) };
        assert_eq!(migration_status, 0);
        assert_eq!(migration.buffer_len, 0);

        let mut snapshot = ElmNativeProviderSnapshotV1::new(7, 8, 9, 10, 0, 0);
        let snapshot_status =
            unsafe { __private::snapshot_trampoline(&mut snapshot, snapshot_empty) };
        assert_eq!(snapshot_status, 0);
        assert_eq!(snapshot.payload_len, 0);
    }
}
