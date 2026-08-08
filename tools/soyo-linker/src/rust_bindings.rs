//! 程序 manifest 到 Rust ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, ObjectInterface, REQUIREMENTS, RIGHTS, TargetArch,
    operation, status,
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
}
