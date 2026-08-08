//! 程序 manifest 到 Rust ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, ObjectInterface, REQUIREMENTS, RIGHTS, TargetArch,
    operation, status,
};
use soyo::registry::FeatureFlags;

use crate::contract::ProgramContract;

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
        writeln!(output, "pub const MYGO_SLOT_{}: u64 = {slot};", spec.name).unwrap();
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
        ("PROCESS", ObjectInterface::Process as u16),
        ("ADDRESS_SPACE", ObjectInterface::AddressSpace as u16),
        ("STREAM", ObjectInterface::Stream as u16),
        ("CLOCK", ObjectInterface::Clock as u16),
    ] {
        writeln!(output, "pub const MYGO_INTERFACE_{name}: u16 = {value};").unwrap();
    }

    writeln!(output, "pub const MYGO_RIGHT_NONE: u64 = 0;").unwrap();
    for right in RIGHTS {
        writeln!(
            output,
            "pub const MYGO_RIGHT_{}: u64 = {};",
            right.name,
            right.right.bits()
        )
        .unwrap();
    }

    for requirement in REQUIREMENTS {
        writeln!(
            output,
            "pub const MYGO_REQUIREMENT_{}: u32 = {};",
            requirement.name, requirement.id as u32
        )
        .unwrap();
    }

    for operation in native_abi::OPERATIONS {
        writeln!(
            output,
            "pub const MYGO_OPERATION_{}: u32 = {};",
            operation.name, operation.id as u32
        )
        .unwrap();
    }

    for (name, value) in [
        ("OK", status::OK),
        ("CORE_INVALID_ARGUMENT", status::CORE_INVALID_ARGUMENT),
        ("CORE_OUT_OF_RANGE", status::CORE_OUT_OF_RANGE),
        ("CORE_RESOURCE_EXHAUSTED", status::CORE_RESOURCE_EXHAUSTED),
        ("ABI_BAD_SLOT", status::ABI_BAD_SLOT),
        ("ABI_SIGNATURE_MISMATCH", status::ABI_SIGNATURE_MISMATCH),
        (
            "ABI_UNSUPPORTED_OPERATION",
            status::ABI_UNSUPPORTED_OPERATION,
        ),
        ("HANDLE_INVALID", status::HANDLE_INVALID),
        ("HANDLE_STALE", status::HANDLE_STALE),
        ("HANDLE_WRONG_INTERFACE", status::HANDLE_WRONG_INTERFACE),
        ("SECURITY_RIGHTS_DENIED", status::SECURITY_RIGHTS_DENIED),
        ("IO_FAULT", status::IO_FAULT),
        ("IO_WOULD_BLOCK", status::IO_WOULD_BLOCK),
        ("IO_CLOSED", status::IO_CLOSED),
        ("IO_ERROR", status::IO_ERROR),
        ("VM_INVALID_RANGE", status::VM_INVALID_RANGE),
        ("VM_ADDRESS_CONFLICT", status::VM_ADDRESS_CONFLICT),
    ] {
        writeln!(output, "pub const MYGO_STATUS_{name}: u32 = 0x{value:08x};").unwrap();
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
        writeln!(
            output,
            "pub const MYGO_CAP_{}_REQUIRED: bool = {};",
            spec.name, capability.required
        )
        .unwrap();
        writeln!(
            output,
            "pub const MYGO_CAP_{}_RIGHTS: u64 = {};",
            spec.name,
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
