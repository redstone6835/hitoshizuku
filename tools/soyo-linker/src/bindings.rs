//! 程序 manifest 到 C ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, ObjectInterface, REQUIREMENTS, RIGHTS, TargetArch,
    operation, status, wire as native_wire,
};
use soyo::registry::FeatureFlags;

use crate::contract::ProgramContract;

pub fn generate_c_header(target: TargetArch, contract: &ProgramContract) -> Vec<u8> {
    let mut output = String::new();
    writeln!(output, "#ifndef MYGO_PROGRAM_H").unwrap();
    writeln!(output, "#define MYGO_PROGRAM_H").unwrap();
    writeln!(output).unwrap();
    writeln!(output, "#include <stddef.h>").unwrap();
    writeln!(output, "#include <stdint.h>").unwrap();
    writeln!(output).unwrap();
    writeln!(output, "#define MYGO_TARGET_ARCH {}u", target as u16).unwrap();
    writeln!(
        output,
        "#define MYGO_ABI_FAMILY {}u",
        ABI_FAMILY_MYGO_NATIVE
    )
    .unwrap();
    writeln!(output, "#define MYGO_ABI_EPOCH {}u", ABI_EPOCH).unwrap();
    writeln!(
        output,
        "#define MYGO_PAGE_SIZE UINT64_C({})",
        native_abi::PAGE_SIZE
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_FEATURE_STATIC_TLS UINT64_C({})",
        FeatureFlags::STATIC_TLS.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_CALL_SLOT_COUNT {}u",
        contract.imports().len()
    )
    .unwrap();
    for (slot, import) in contract.imports().iter().enumerate() {
        let spec = operation(import.operation).expect("manifest import 已由 registry 归一化");
        writeln!(output, "#define MYGO_SLOT_{} {}u", spec.name, slot).unwrap();
    }
    writeln!(
        output,
        "#define MYGO_RUNTIME_STACK_SIZE UINT64_C({})",
        contract.runtime().stack_size
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_RUNTIME_STACK_GUARD_SIZE UINT64_C({})",
        contract.runtime().stack_guard_size
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_START_INFO_MAX_SIZE {}u",
        contract.runtime().start_info_max_size
    )
    .unwrap();
    writeln!(output).unwrap();

    write_registry_definitions(&mut output);
    write_capability_definitions(&mut output, contract);
    write_wire_types(&mut output);

    writeln!(output, "#endif").unwrap();
    output.into_bytes()
}

fn write_registry_definitions(output: &mut String) {
    for (name, value) in [
        ("PROCESS", ObjectInterface::Process as u16),
        ("ADDRESS_SPACE", ObjectInterface::AddressSpace as u16),
        ("STREAM", ObjectInterface::Stream as u16),
        ("CLOCK", ObjectInterface::Clock as u16),
    ] {
        writeln!(output, "#define MYGO_INTERFACE_{name} {value}u").unwrap();
    }

    writeln!(output, "#define MYGO_RIGHT_NONE UINT64_C(0)").unwrap();
    for right in RIGHTS {
        writeln!(
            output,
            "#define MYGO_RIGHT_{} UINT64_C({})",
            right.name,
            right.right.bits()
        )
        .unwrap();
    }

    for requirement in REQUIREMENTS {
        writeln!(
            output,
            "#define MYGO_REQUIREMENT_{} {}u",
            requirement.name, requirement.id as u32
        )
        .unwrap();
    }

    for operation in native_abi::OPERATIONS {
        writeln!(
            output,
            "#define MYGO_OPERATION_{} {}u",
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
        writeln!(output, "#define MYGO_STATUS_{name} UINT32_C(0x{value:08x})").unwrap();
    }
    writeln!(output).unwrap();
}

fn write_capability_definitions(output: &mut String, contract: &ProgramContract) {
    writeln!(
        output,
        "#define MYGO_CAPABILITY_COUNT {}u",
        contract.capabilities().len()
    )
    .unwrap();
    for capability in contract.capabilities() {
        let spec = native_abi::requirement(capability.requirement)
            .expect("manifest capability 已由 registry 归一化");
        writeln!(
            output,
            "#define MYGO_CAP_{}_REQUIRED {}u",
            spec.name,
            u32::from(capability.required)
        )
        .unwrap();
        writeln!(
            output,
            "#define MYGO_CAP_{}_RIGHTS UINT64_C({})",
            spec.name,
            capability.rights.bits()
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_wire_types(output: &mut String) {
    writeln!(
        output,
        "typedef struct mygo_string_ref {{ uint32_t offset; uint32_t length; }} mygo_string_ref;"
    )
    .unwrap();
    writeln!(output, "struct mygo_start_info {{").unwrap();
    writeln!(output, "    uint8_t magic[4];").unwrap();
    writeln!(output, "    uint16_t version;").unwrap();
    writeln!(output, "    uint16_t header_size;").unwrap();
    writeln!(output, "    uint32_t total_size;").unwrap();
    writeln!(output, "    uint32_t flags;").unwrap();
    writeln!(output, "    uint16_t abi_epoch;").unwrap();
    writeln!(output, "    uint16_t target_arch;").unwrap();
    writeln!(output, "    uint32_t reserved0;").unwrap();
    writeln!(output, "    uint64_t enabled_features;").unwrap();
    writeln!(output, "    uint64_t image_base;").unwrap();
    writeln!(output, "    uint64_t page_size;").unwrap();
    writeln!(output, "    uint64_t initial_tls_base;").unwrap();
    writeln!(output, "    uint64_t initial_tls_size;").unwrap();
    writeln!(output, "    uint64_t initial_thread_pointer;").unwrap();
    writeln!(output, "    uint32_t argc;").unwrap();
    writeln!(output, "    uint32_t envc;").unwrap();
    writeln!(output, "    uint32_t argv_offset;").unwrap();
    writeln!(output, "    uint32_t env_offset;").unwrap();
    writeln!(output, "    uint32_t string_bytes_offset;").unwrap();
    writeln!(output, "    uint32_t string_bytes_size;").unwrap();
    writeln!(output, "    uint32_t initial_handle_count;").unwrap();
    writeln!(output, "    uint16_t initial_handle_record_size;").unwrap();
    writeln!(output, "    uint16_t reserved1;").unwrap();
    writeln!(output, "    uint32_t initial_handle_offset;").unwrap();
    writeln!(output, "    uint32_t call_slot_count;").unwrap();
    writeln!(output, "    uint8_t random_seed[32];").unwrap();
    writeln!(output, "    uint64_t runtime_flags;").unwrap();
    writeln!(output, "    uint8_t reserved2[40];").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "struct mygo_initial_handle {{").unwrap();
    writeln!(output, "    uint32_t requirement_id;").unwrap();
    writeln!(output, "    uint16_t object_interface;").unwrap();
    writeln!(output, "    uint16_t flags;").unwrap();
    writeln!(output, "    uint64_t handle;").unwrap();
    writeln!(output, "    uint64_t granted_rights;").unwrap();
    writeln!(output, "    uint64_t reserved;").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "struct mygo_native_call {{").unwrap();
    writeln!(output, "    uint64_t slot;").unwrap();
    writeln!(output, "    uint64_t object_handle;").unwrap();
    writeln!(output, "    uint64_t args[5];").unwrap();
    writeln!(output, "    uint64_t reserved_arg;").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "struct mygo_native_result {{").unwrap();
    writeln!(output, "    uint32_t status;").unwrap();
    writeln!(output, "    uint32_t reserved;").unwrap();
    writeln!(output, "    uint64_t value0;").unwrap();
    writeln!(output, "    uint64_t value1;").unwrap();
    writeln!(output, "}};").unwrap();

    for (name, type_name, size) in [
        (
            "MYGO_STRING_REF_SIZE",
            "struct mygo_string_ref",
            native_wire::STRING_REF_SIZE,
        ),
        (
            "MYGO_START_INFO_SIZE",
            "struct mygo_start_info",
            native_wire::START_INFO_SIZE,
        ),
        (
            "MYGO_INITIAL_HANDLE_SIZE",
            "struct mygo_initial_handle",
            native_wire::INITIAL_HANDLE_SIZE,
        ),
        ("MYGO_NATIVE_CALL_SIZE", "struct mygo_native_call", 64),
        ("MYGO_NATIVE_RESULT_SIZE", "struct mygo_native_result", 24),
    ] {
        writeln!(
            output,
            "#define {name} {size}u\n_Static_assert(sizeof({type_name}) == {size}, \"{name}\");"
        )
        .unwrap();
    }
    write_wire_offsets(
        output,
        "STRING_REF",
        "struct mygo_string_ref",
        &[
            ("OFFSET", "offset", native_wire::string_ref::OFFSET),
            ("LENGTH", "length", native_wire::string_ref::LENGTH),
        ],
    );
    write_wire_offsets(
        output,
        "START_INFO",
        "struct mygo_start_info",
        &[
            ("MAGIC", "magic", native_wire::start_info::MAGIC),
            ("VERSION", "version", native_wire::start_info::VERSION),
            (
                "HEADER_SIZE",
                "header_size",
                native_wire::start_info::HEADER_SIZE,
            ),
            (
                "TOTAL_SIZE",
                "total_size",
                native_wire::start_info::TOTAL_SIZE,
            ),
            ("FLAGS", "flags", native_wire::start_info::FLAGS),
            ("ABI_EPOCH", "abi_epoch", native_wire::start_info::ABI_EPOCH),
            (
                "TARGET_ARCH",
                "target_arch",
                native_wire::start_info::TARGET_ARCH,
            ),
            ("RESERVED0", "reserved0", native_wire::start_info::RESERVED0),
            (
                "ENABLED_FEATURES",
                "enabled_features",
                native_wire::start_info::ENABLED_FEATURES,
            ),
            (
                "IMAGE_BASE",
                "image_base",
                native_wire::start_info::IMAGE_BASE,
            ),
            ("PAGE_SIZE", "page_size", native_wire::start_info::PAGE_SIZE),
            (
                "INITIAL_TLS_BASE",
                "initial_tls_base",
                native_wire::start_info::INITIAL_TLS_BASE,
            ),
            (
                "INITIAL_TLS_SIZE",
                "initial_tls_size",
                native_wire::start_info::INITIAL_TLS_SIZE,
            ),
            (
                "INITIAL_THREAD_POINTER",
                "initial_thread_pointer",
                native_wire::start_info::INITIAL_THREAD_POINTER,
            ),
            ("ARGC", "argc", native_wire::start_info::ARGC),
            ("ENVC", "envc", native_wire::start_info::ENVC),
            (
                "ARGV_OFFSET",
                "argv_offset",
                native_wire::start_info::ARGV_OFFSET,
            ),
            (
                "ENV_OFFSET",
                "env_offset",
                native_wire::start_info::ENV_OFFSET,
            ),
            (
                "STRING_BYTES_OFFSET",
                "string_bytes_offset",
                native_wire::start_info::STRING_BYTES_OFFSET,
            ),
            (
                "STRING_BYTES_SIZE",
                "string_bytes_size",
                native_wire::start_info::STRING_BYTES_SIZE,
            ),
            (
                "INITIAL_HANDLE_COUNT",
                "initial_handle_count",
                native_wire::start_info::INITIAL_HANDLE_COUNT,
            ),
            (
                "INITIAL_HANDLE_RECORD_SIZE",
                "initial_handle_record_size",
                native_wire::start_info::INITIAL_HANDLE_RECORD_SIZE,
            ),
            ("RESERVED1", "reserved1", native_wire::start_info::RESERVED1),
            (
                "INITIAL_HANDLE_OFFSET",
                "initial_handle_offset",
                native_wire::start_info::INITIAL_HANDLE_OFFSET,
            ),
            (
                "CALL_SLOT_COUNT",
                "call_slot_count",
                native_wire::start_info::CALL_SLOT_COUNT,
            ),
            (
                "RANDOM_SEED",
                "random_seed",
                native_wire::start_info::RANDOM_SEED,
            ),
            (
                "RUNTIME_FLAGS",
                "runtime_flags",
                native_wire::start_info::RUNTIME_FLAGS,
            ),
            ("RESERVED2", "reserved2", native_wire::start_info::RESERVED2),
        ],
    );
    write_wire_offsets(
        output,
        "INITIAL_HANDLE",
        "struct mygo_initial_handle",
        &[
            (
                "REQUIREMENT_ID",
                "requirement_id",
                native_wire::initial_handle::REQUIREMENT_ID,
            ),
            (
                "OBJECT_INTERFACE",
                "object_interface",
                native_wire::initial_handle::OBJECT_INTERFACE,
            ),
            ("FLAGS", "flags", native_wire::initial_handle::FLAGS),
            ("HANDLE", "handle", native_wire::initial_handle::HANDLE),
            (
                "GRANTED_RIGHTS",
                "granted_rights",
                native_wire::initial_handle::GRANTED_RIGHTS,
            ),
            (
                "RESERVED",
                "reserved",
                native_wire::initial_handle::RESERVED,
            ),
        ],
    );
    writeln!(output).unwrap();
}

fn write_wire_offsets(
    output: &mut String,
    prefix: &str,
    type_name: &str,
    fields: &[(&str, &str, usize)],
) {
    for (name, field, offset) in fields {
        writeln!(
            output,
            "#define MYGO_{prefix}_{name}_OFFSET {offset}u\n_Static_assert(offsetof({type_name}, {field}) == MYGO_{prefix}_{name}_OFFSET, \"{type_name}.{field}\");"
        )
        .unwrap();
    }
}
