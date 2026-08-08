//! 程序 manifest 到 Rust ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, ObjectInterface, REQUIREMENTS, RIGHTS, TargetArch,
    operation, status, wire as native_wire,
};
use soyo::registry::FeatureFlags;

use crate::contract::ProgramContract;

fn public_ident(name: &str) -> String {
    name.replace('.', "_")
}

pub fn generate_rust_module(target: TargetArch, contract: &ProgramContract) -> Vec<u8> {
    let mut output = String::new();
    writeln!(output, "// 由 soyo-ld 生成，请勿手工修改。").unwrap();
    writeln!(
        output,
        "pub const MYGO_TARGET_ARCH: u16 = {};",
        target as u16
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_ABI_FAMILY: u16 = {ABI_FAMILY_MYGO_NATIVE};"
    )
    .unwrap();
    writeln!(output, "pub const MYGO_ABI_EPOCH: u16 = {ABI_EPOCH};").unwrap();
    writeln!(
        output,
        "pub const MYGO_PAGE_SIZE: u64 = {};",
        native_abi::PAGE_SIZE
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_FEATURE_STATIC_TLS: u64 = {};",
        FeatureFlags::STATIC_TLS.bits()
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_CALL_SLOT_COUNT: u32 = {};",
        contract.imports().len()
    )
    .unwrap();
    for (slot, import) in contract.imports().iter().enumerate() {
        let spec = operation(import.operation).expect("manifest import 已由 registry 归一化");
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_SLOT_{}: u64 = {slot};",
            public_ident(spec.name)
        )
        .unwrap();
    }
    writeln!(
        output,
        "pub const MYGO_RUNTIME_STACK_SIZE: u64 = {};",
        contract.runtime().stack_size
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_RUNTIME_STACK_GUARD_SIZE: u64 = {};",
        contract.runtime().stack_guard_size
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_START_INFO_MAX_SIZE: u32 = {};",
        contract.runtime().start_info_max_size
    )
    .unwrap();
    writeln!(output).unwrap();

    write_registry_definitions(&mut output);
    write_capability_definitions(&mut output, contract);
    write_wire_types(&mut output);
    output.into_bytes()
}

fn write_registry_definitions(output: &mut String) {
    for (name, value) in [
        ("process", ObjectInterface::Process as u16),
        ("address_space", ObjectInterface::AddressSpace as u16),
        ("stream", ObjectInterface::Stream as u16),
        ("clock", ObjectInterface::Clock as u16),
        ("executable_image", ObjectInterface::ExecutableImage as u16),
        ("event_port", ObjectInterface::EventPort as u16),
    ] {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(output, "pub const MYGO_INTERFACE_{name}: u16 = {value};").unwrap();
    }

    writeln!(output, "pub const MYGO_RIGHT_NONE: u64 = 0;").unwrap();
    for right in RIGHTS {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_RIGHT_{}: u64 = {};",
            public_ident(right.name),
            right.right.bits()
        )
        .unwrap();
    }

    for requirement in REQUIREMENTS {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_REQUIREMENT_{}: u32 = {};",
            public_ident(requirement.name),
            requirement.id as u32
        )
        .unwrap();
    }

    for operation in native_abi::OPERATIONS {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_OPERATION_{}: u32 = {};",
            public_ident(operation.name),
            operation.id as u32
        )
        .unwrap();
    }

    for (name, value) in [
        ("PROCESS_STATE_RUNNING", native_wire::PROCESS_STATE_RUNNING),
        ("PROCESS_STATE_TERMINATING", native_wire::PROCESS_STATE_TERMINATING),
        ("PROCESS_STATE_EXITED", native_wire::PROCESS_STATE_EXITED),
        ("PROCESS_STATE_FAULTED", native_wire::PROCESS_STATE_FAULTED),
        ("PROCESS_STATE_REAPED", native_wire::PROCESS_STATE_REAPED),
        ("PROCESS_FAULT_MEMORY", native_wire::PROCESS_FAULT_MEMORY),
        ("PROCESS_FAULT_ILLEGAL_INSTRUCTION", native_wire::PROCESS_FAULT_ILLEGAL_INSTRUCTION),
        ("PROCESS_FAULT_BREAKPOINT", native_wire::PROCESS_FAULT_BREAKPOINT),
        ("PROCESS_FAULT_ADDRESS", native_wire::PROCESS_FAULT_ADDRESS),
        ("PROCESS_FAULT_ARITHMETIC", native_wire::PROCESS_FAULT_ARITHMETIC),
        ("PROCESS_FAULT_RESOURCE", native_wire::PROCESS_FAULT_RESOURCE),
        ("PROCESS_FAULT_OTHER", native_wire::PROCESS_FAULT_OTHER),
        ("EVENT_KIND_PROCESS_EXITED", native_wire::EVENT_KIND_PROCESS_EXITED),
        ("EVENT_KIND_PROCESS_FAULT", native_wire::EVENT_KIND_PROCESS_FAULT),
        ("EVENT_KIND_STREAM_READY", native_wire::EVENT_KIND_STREAM_READY),
        ("EVENT_KIND_TIMER_EXPIRED", native_wire::EVENT_KIND_TIMER_EXPIRED),
        ("EVENT_STREAM_READABLE", native_wire::EVENT_STREAM_READABLE),
        ("EVENT_STREAM_WRITABLE", native_wire::EVENT_STREAM_WRITABLE),
        ("EVENT_STREAM_ERROR", native_wire::EVENT_STREAM_ERROR),
        ("EVENT_STREAM_CLOSED", native_wire::EVENT_STREAM_CLOSED),
    ] {
        writeln!(output, "pub const MYGO_{name}: u32 = {value};").unwrap();
    }
    writeln!(output, "pub const MYGO_HANDLE_TRANSFER_MOVE: u64 = {};", native_wire::HANDLE_TRANSFER_MOVE).unwrap();
    writeln!(output, "pub const MYGO_MAX_EVENT_PORT_CAPACITY: u32 = {};", native_wire::MAX_EVENT_PORT_CAPACITY).unwrap();
    writeln!(output, "pub const MYGO_MAX_EVENT_BATCH: u32 = {};", native_wire::MAX_EVENT_BATCH).unwrap();

    for (name, value) in [
        ("ok", status::OK),
        ("core.invalid_argument", status::CORE_INVALID_ARGUMENT),
        ("core.out_of_range", status::CORE_OUT_OF_RANGE),
        ("core.resource_exhausted", status::CORE_RESOURCE_EXHAUSTED),
        ("abi.bad_slot", status::ABI_BAD_SLOT),
        ("abi.signature_mismatch", status::ABI_SIGNATURE_MISMATCH),
        (
            "abi.unsupported_operation",
            status::ABI_UNSUPPORTED_OPERATION,
        ),
        ("handle.invalid", status::HANDLE_INVALID),
        ("handle.stale", status::HANDLE_STALE),
        ("handle.wrong_interface", status::HANDLE_WRONG_INTERFACE),
        ("security.rights_denied", status::SECURITY_RIGHTS_DENIED),
        ("stream.fault", status::STREAM_FAULT),
        ("stream.would_block", status::STREAM_WOULD_BLOCK),
        ("stream.end", status::STREAM_END),
        ("stream.closed", status::STREAM_CLOSED),
        ("stream.error", status::STREAM_ERROR),
        ("memory.invalid_range", status::MEMORY_INVALID_RANGE),
        ("memory.invalid_alignment", status::MEMORY_INVALID_ALIGNMENT),
        ("memory.not_owned", status::MEMORY_NOT_OWNED),
        ("process.not_child", status::PROCESS_NOT_CHILD),
        ("process.already_reaped", status::PROCESS_ALREADY_REAPED),
        ("process.would_block", status::PROCESS_WOULD_BLOCK),
        ("process.invalid_state", status::PROCESS_INVALID_STATE),
        ("process.wait_in_progress", status::PROCESS_WAIT_IN_PROGRESS),
        ("image.invalid", status::IMAGE_INVALID),
        ("image.arch_mismatch", status::IMAGE_ARCH_MISMATCH),
        ("image.not_executable", status::IMAGE_NOT_EXECUTABLE),
        ("event.invalid_token", status::EVENT_INVALID_TOKEN),
        ("event.source_unsupported", status::EVENT_SOURCE_UNSUPPORTED),
        ("event.would_block", status::EVENT_WOULD_BLOCK),
        ("event.timeout", status::EVENT_TIMEOUT),
        ("event.queue_exhausted", status::EVENT_QUEUE_EXHAUSTED),
        ("event.cancelled", status::EVENT_CANCELLED),
    ] {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_STATUS_{}: u32 = 0x{value:08x};",
            public_ident(name)
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_capability_definitions(output: &mut String, contract: &ProgramContract) {
    writeln!(
        output,
        "pub const MYGO_CAPABILITY_COUNT: u32 = {};",
        contract.capabilities().len()
    )
    .unwrap();
    for capability in contract.capabilities() {
        let spec = native_abi::requirement(capability.requirement)
            .expect("manifest capability 已由 registry 归一化");
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_CAP_{}_required: bool = {};",
            public_ident(spec.name),
            capability.required
        )
        .unwrap();
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_CAP_{}_rights: u64 = {};",
            public_ident(spec.name),
            capability.rights.bits()
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_wire_types(output: &mut String) {
    writeln!(output, "#[repr(C)]").unwrap();
    writeln!(
        output,
        "#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]"
    )
    .unwrap();
    writeln!(output, "pub struct MygoNativeCall {{").unwrap();
    writeln!(output, "    pub slot: u64,").unwrap();
    writeln!(output, "    pub object_handle: u64,").unwrap();
    writeln!(output, "    pub args: [u64; 5],").unwrap();
    writeln!(output, "    pub reserved_arg: u64,").unwrap();
    writeln!(output, "}}").unwrap();
    writeln!(output).unwrap();

    writeln!(output, "#[repr(C)]").unwrap();
    writeln!(
        output,
        "#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]"
    )
    .unwrap();
    writeln!(output, "pub struct MygoNativeResult {{").unwrap();
    writeln!(output, "    pub status: u32,").unwrap();
    writeln!(output, "    pub reserved: u32,").unwrap();
    writeln!(output, "    pub value0: u64,").unwrap();
    writeln!(output, "    pub value1: u64,").unwrap();
    writeln!(output, "}}").unwrap();
    writeln!(output).unwrap();

    writeln!(
        output,
        "const _: () = assert!(core::mem::size_of::<MygoNativeCall>() == 64);"
    )
    .unwrap();
    writeln!(
        output,
        "const _: () = assert!(core::mem::offset_of!(MygoNativeCall, args) == 16);"
    )
    .unwrap();
    writeln!(
        output,
        "const _: () = assert!(core::mem::size_of::<MygoNativeResult>() == 24);"
    )
    .unwrap();
    writeln!(
        output,
        "const _: () = assert!(core::mem::offset_of!(MygoNativeResult, value0) == 8);"
    )
    .unwrap();

    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoProcessStringRef {{ pub ptr: u64, pub len: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoProcessArrayRef {{ pub ptr: u64, pub count: u32, pub reserved: u32 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoHandleTransfer {{ pub requirement_id: u32, pub reserved: u32, pub source_handle: u64, pub requested_rights: u64, pub flags: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoSpawnRequest {{ pub image: u64, pub argv: MygoProcessArrayRef, pub env: MygoProcessArrayRef, pub transfers: MygoProcessArrayRef, pub resource_policy: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoProcessResult {{ pub state: u32, pub flags: u32, pub exit_code: u32, pub fault_kind: u32, pub detail0: u64, pub detail1: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoEventRecord {{ pub event_kind: u32, pub status: u32, pub source_handle: u64, pub sequence: u64, pub value0: u64, pub value1: u64 }}").unwrap();
    for (type_name, size) in [
        ("MygoProcessStringRef", native_wire::PROCESS_STRING_REF_SIZE),
        ("MygoProcessArrayRef", native_wire::PROCESS_ARRAY_REF_SIZE),
        ("MygoHandleTransfer", native_wire::HANDLE_TRANSFER_SIZE),
        ("MygoSpawnRequest", native_wire::SPAWN_REQUEST_SIZE),
        ("MygoProcessResult", native_wire::PROCESS_RESULT_SIZE),
        ("MygoEventRecord", native_wire::EVENT_RECORD_SIZE),
    ] {
        writeln!(output, "const _: () = assert!(core::mem::size_of::<{type_name}>() == {size});").unwrap();
    }
    for (type_name, field, offset) in [
        ("MygoProcessStringRef", "ptr", native_wire::process_string_ref::PTR),
        ("MygoProcessStringRef", "len", native_wire::process_string_ref::LEN),
        ("MygoProcessArrayRef", "ptr", native_wire::process_array_ref::PTR),
        ("MygoProcessArrayRef", "count", native_wire::process_array_ref::COUNT),
        ("MygoProcessArrayRef", "reserved", native_wire::process_array_ref::RESERVED),
        ("MygoHandleTransfer", "requirement_id", native_wire::handle_transfer::REQUIREMENT_ID),
        ("MygoHandleTransfer", "source_handle", native_wire::handle_transfer::SOURCE_HANDLE),
        ("MygoHandleTransfer", "requested_rights", native_wire::handle_transfer::REQUESTED_RIGHTS),
        ("MygoHandleTransfer", "flags", native_wire::handle_transfer::FLAGS),
        ("MygoSpawnRequest", "image", native_wire::spawn_request::IMAGE),
        ("MygoSpawnRequest", "argv", native_wire::spawn_request::ARGV),
        ("MygoSpawnRequest", "env", native_wire::spawn_request::ENV),
        ("MygoSpawnRequest", "transfers", native_wire::spawn_request::TRANSFERS),
        ("MygoSpawnRequest", "resource_policy", native_wire::spawn_request::RESOURCE_POLICY),
        ("MygoProcessResult", "state", native_wire::process_result::STATE),
        ("MygoProcessResult", "exit_code", native_wire::process_result::EXIT_CODE),
        ("MygoProcessResult", "fault_kind", native_wire::process_result::FAULT_KIND),
        ("MygoProcessResult", "detail0", native_wire::process_result::DETAIL0),
        ("MygoProcessResult", "detail1", native_wire::process_result::DETAIL1),
        ("MygoEventRecord", "event_kind", native_wire::event_record::EVENT_KIND),
        ("MygoEventRecord", "source_handle", native_wire::event_record::SOURCE_HANDLE),
        ("MygoEventRecord", "sequence", native_wire::event_record::SEQUENCE),
        ("MygoEventRecord", "value0", native_wire::event_record::VALUE0),
        ("MygoEventRecord", "value1", native_wire::event_record::VALUE1),
    ] {
        writeln!(output, "const _: () = assert!(core::mem::offset_of!({type_name}, {field}) == {offset});").unwrap();
    }
}
