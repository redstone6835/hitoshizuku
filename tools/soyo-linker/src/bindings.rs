//! 程序 manifest 到 C ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, ObjectInterface, REQUIREMENTS, RIGHTS, TargetArch,
    status, wire as native_wire,
};
use soyo::registry::FeatureFlags;
use soyo::registry::RuntimeFlags;

use crate::contract::ProgramContract;

fn public_ident(name: &str) -> String {
    name.replace('.', "_")
}

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
        "#define MYGO_FEATURE_INIT_FINI_ARRAY UINT64_C({})",
        FeatureFlags::INIT_FINI_ARRAY.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_RUNTIME_RUN_INIT_ARRAY UINT64_C({})",
        RuntimeFlags::RUN_INIT_ARRAY.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_RUNTIME_RUN_FINI_ARRAY UINT64_C({})",
        RuntimeFlags::RUN_FINI_ARRAY.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_CALL_SLOT_COUNT {}u",
        contract.imports().len()
    )
    .unwrap();
    for operation_spec in native_abi::OPERATIONS {
        let slot = contract
            .imports()
            .iter()
            .position(|import| import.operation == operation_spec.id);
        writeln!(
            output,
            "#define MYGO_HAS_{} {}u",
            public_ident(operation_spec.name),
            u32::from(slot.is_some())
        )
        .unwrap();
        writeln!(
            output,
            "#define MYGO_SLOT_{} {}",
            public_ident(operation_spec.name),
            slot.map_or_else(|| "UINT64_MAX".to_string(), |slot| format!("{slot}u"))
        )
        .unwrap();
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
        ("process", ObjectInterface::Process as u16),
        ("address_space", ObjectInterface::AddressSpace as u16),
        ("stream", ObjectInterface::Stream as u16),
        ("clock", ObjectInterface::Clock as u16),
        ("executable_image", ObjectInterface::ExecutableImage as u16),
        ("event_port", ObjectInterface::EventPort as u16),
    ] {
        writeln!(output, "#define MYGO_INTERFACE_{name} {value}u").unwrap();
    }

    writeln!(output, "#define MYGO_RIGHT_NONE UINT64_C(0)").unwrap();
    for right in RIGHTS {
        writeln!(
            output,
            "#define MYGO_RIGHT_{} UINT64_C({})",
            public_ident(right.name),
            right.right.bits()
        )
        .unwrap();
    }

    for requirement in REQUIREMENTS {
        writeln!(
            output,
            "#define MYGO_REQUIREMENT_{} {}u",
            public_ident(requirement.name),
            requirement.id as u32
        )
        .unwrap();
    }

    for operation in native_abi::OPERATIONS {
        writeln!(
            output,
            "#define MYGO_OPERATION_{} {}u",
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
        writeln!(output, "#define MYGO_{name} {value}u").unwrap();
    }
    writeln!(
        output,
        "#define MYGO_HANDLE_TRANSFER_MOVE UINT64_C({})",
        native_wire::HANDLE_TRANSFER_MOVE
    )
    .unwrap();
    writeln!(output, "#define MYGO_MAX_EVENT_PORT_CAPACITY {}u", native_wire::MAX_EVENT_PORT_CAPACITY).unwrap();
    writeln!(output, "#define MYGO_MAX_EVENT_BATCH {}u", native_wire::MAX_EVENT_BATCH).unwrap();

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
        writeln!(
            output,
            "#define MYGO_STATUS_{} UINT32_C(0x{value:08x})",
            public_ident(name)
        )
        .unwrap();
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
            "#define MYGO_CAP_{}_required {}u",
            public_ident(spec.name),
            u32::from(capability.required)
        )
        .unwrap();
        writeln!(
            output,
            "#define MYGO_CAP_{}_rights UINT64_C({})",
            public_ident(spec.name),
            capability.rights.bits()
        )
        .unwrap();
    }
    writeln!(output, "#define MYGO_CAPABILITY_CONTRACT(X) \\").unwrap();
    for (index, capability) in contract.capabilities().iter().enumerate() {
        let spec = native_abi::requirement(capability.requirement)
            .expect("manifest capability 已由 registry 归一化");
        let interface = match spec.interface {
            ObjectInterface::Process => "process",
            ObjectInterface::AddressSpace => "address_space",
            ObjectInterface::Stream => "stream",
            ObjectInterface::Clock => "clock",
            ObjectInterface::ExecutableImage => "executable_image",
            ObjectInterface::EventPort => "event_port",
        };
        let suffix = if index + 1 == contract.capabilities().len() {
            ""
        } else {
            concat!(" ", "\\")
        };
        writeln!(
            output,
            "    X(MYGO_REQUIREMENT_{}, MYGO_INTERFACE_{}, UINT64_C({}), {}u){}",
            public_ident(spec.name),
            interface,
            capability.rights.bits(),
            u32::from(capability.required),
            suffix
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
    writeln!(output, "    uint64_t init_array_offset;").unwrap();
    writeln!(output, "    uint32_t init_array_count;").unwrap();
    writeln!(output, "    uint16_t init_array_entry_size;").unwrap();
    writeln!(output, "    uint16_t reserved2;").unwrap();
    writeln!(output, "    uint64_t fini_array_offset;").unwrap();
    writeln!(output, "    uint32_t fini_array_count;").unwrap();
    writeln!(output, "    uint16_t fini_array_entry_size;").unwrap();
    writeln!(output, "    uint16_t reserved3;").unwrap();
    writeln!(output, "    uint64_t reserved4;").unwrap();
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

    writeln!(output, "typedef struct mygo_process_string_ref {{ uint64_t ptr; uint64_t len; }} mygo_process_string_ref;").unwrap();
    writeln!(output, "typedef struct mygo_process_array_ref {{ uint64_t ptr; uint32_t count; uint32_t reserved; }} mygo_process_array_ref;").unwrap();
    writeln!(output, "typedef struct mygo_handle_transfer {{ uint32_t requirement_id; uint32_t reserved; uint64_t source_handle; uint64_t requested_rights; uint64_t flags; }} mygo_handle_transfer;").unwrap();
    writeln!(output, "typedef struct mygo_spawn_request {{ uint64_t image; mygo_process_array_ref argv; mygo_process_array_ref env; mygo_process_array_ref transfers; uint64_t resource_policy; }} mygo_spawn_request;").unwrap();
    writeln!(output, "typedef struct mygo_process_result {{ uint32_t state; uint32_t flags; uint32_t exit_code; uint32_t fault_kind; uint64_t detail0; uint64_t detail1; }} mygo_process_result;").unwrap();
    writeln!(output, "typedef struct mygo_event_record {{ uint32_t event_kind; uint32_t status; uint64_t source_handle; uint64_t sequence; uint64_t value0; uint64_t value1; }} mygo_event_record;").unwrap();

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
        ("MYGO_PROCESS_STRING_REF_SIZE", "mygo_process_string_ref", native_wire::PROCESS_STRING_REF_SIZE),
        ("MYGO_PROCESS_ARRAY_REF_SIZE", "mygo_process_array_ref", native_wire::PROCESS_ARRAY_REF_SIZE),
        ("MYGO_HANDLE_TRANSFER_SIZE", "mygo_handle_transfer", native_wire::HANDLE_TRANSFER_SIZE),
        ("MYGO_SPAWN_REQUEST_SIZE", "mygo_spawn_request", native_wire::SPAWN_REQUEST_SIZE),
        ("MYGO_PROCESS_RESULT_SIZE", "mygo_process_result", native_wire::PROCESS_RESULT_SIZE),
        ("MYGO_EVENT_RECORD_SIZE", "mygo_event_record", native_wire::EVENT_RECORD_SIZE),
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
            (
                "INIT_ARRAY_OFFSET",
                "init_array_offset",
                native_wire::start_info::INIT_ARRAY_OFFSET,
            ),
            (
                "INIT_ARRAY_COUNT",
                "init_array_count",
                native_wire::start_info::INIT_ARRAY_COUNT,
            ),
            (
                "INIT_ARRAY_ENTRY_SIZE",
                "init_array_entry_size",
                native_wire::start_info::INIT_ARRAY_ENTRY_SIZE,
            ),
            ("RESERVED2", "reserved2", native_wire::start_info::RESERVED2),
            (
                "FINI_ARRAY_OFFSET",
                "fini_array_offset",
                native_wire::start_info::FINI_ARRAY_OFFSET,
            ),
            (
                "FINI_ARRAY_COUNT",
                "fini_array_count",
                native_wire::start_info::FINI_ARRAY_COUNT,
            ),
            (
                "FINI_ARRAY_ENTRY_SIZE",
                "fini_array_entry_size",
                native_wire::start_info::FINI_ARRAY_ENTRY_SIZE,
            ),
            ("RESERVED3", "reserved3", native_wire::start_info::RESERVED3),
            ("RESERVED4", "reserved4", native_wire::start_info::RESERVED4),
        ],
    );
    write_wire_offsets(
        output,
        "PROCESS_STRING_REF",
        "mygo_process_string_ref",
        &[
            ("PTR", "ptr", native_wire::process_string_ref::PTR),
            ("LEN", "len", native_wire::process_string_ref::LEN),
        ],
    );
    write_wire_offsets(
        output,
        "PROCESS_ARRAY_REF",
        "mygo_process_array_ref",
        &[
            ("PTR", "ptr", native_wire::process_array_ref::PTR),
            ("COUNT", "count", native_wire::process_array_ref::COUNT),
            ("RESERVED", "reserved", native_wire::process_array_ref::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "HANDLE_TRANSFER",
        "mygo_handle_transfer",
        &[
            ("REQUIREMENT_ID", "requirement_id", native_wire::handle_transfer::REQUIREMENT_ID),
            ("RESERVED", "reserved", native_wire::handle_transfer::RESERVED),
            ("SOURCE_HANDLE", "source_handle", native_wire::handle_transfer::SOURCE_HANDLE),
            ("REQUESTED_RIGHTS", "requested_rights", native_wire::handle_transfer::REQUESTED_RIGHTS),
            ("FLAGS", "flags", native_wire::handle_transfer::FLAGS),
        ],
    );
    write_wire_offsets(
        output,
        "SPAWN_REQUEST",
        "mygo_spawn_request",
        &[
            ("IMAGE", "image", native_wire::spawn_request::IMAGE),
            ("ARGV", "argv", native_wire::spawn_request::ARGV),
            ("ENV", "env", native_wire::spawn_request::ENV),
            ("TRANSFERS", "transfers", native_wire::spawn_request::TRANSFERS),
            ("RESOURCE_POLICY", "resource_policy", native_wire::spawn_request::RESOURCE_POLICY),
        ],
    );
    write_wire_offsets(
        output,
        "PROCESS_RESULT",
        "mygo_process_result",
        &[
            ("STATE", "state", native_wire::process_result::STATE),
            ("FLAGS", "flags", native_wire::process_result::FLAGS),
            ("EXIT_CODE", "exit_code", native_wire::process_result::EXIT_CODE),
            ("FAULT_KIND", "fault_kind", native_wire::process_result::FAULT_KIND),
            ("DETAIL0", "detail0", native_wire::process_result::DETAIL0),
            ("DETAIL1", "detail1", native_wire::process_result::DETAIL1),
        ],
    );
    write_wire_offsets(
        output,
        "EVENT_RECORD",
        "mygo_event_record",
        &[
            ("EVENT_KIND", "event_kind", native_wire::event_record::EVENT_KIND),
            ("STATUS", "status", native_wire::event_record::STATUS),
            ("SOURCE_HANDLE", "source_handle", native_wire::event_record::SOURCE_HANDLE),
            ("SEQUENCE", "sequence", native_wire::event_record::SEQUENCE),
            ("VALUE0", "value0", native_wire::event_record::VALUE0),
            ("VALUE1", "value1", native_wire::event_record::VALUE1),
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
