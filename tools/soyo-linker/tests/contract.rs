use native_abi::{OperationId, RequirementId, Rights};
use soyo_linker::contract::{ContractErrorKind, parse_component_manifest, parse_manifest};

const VALID_MANIFEST: &str = r#"
{
  "manifest_version": 1,
  "abi_epoch": 1,
  "entry": "_start",
  "imports": [
    { "operation": "stream.write", "required": true },
    { "operation": "process.exit", "required": true }
  ],
  "capabilities": [
    { "requirement": "stdout", "rights": ["write"], "required": true },
    { "requirement": "self_process", "rights": ["exit"], "required": true }
  ],
  "runtime": {
    "stack_size": 65536,
    "stack_guard_size": 4096,
    "start_info_max_size": 4096
  }
}
"#;

#[test]
fn manifest_becomes_a_registry_ordered_program_contract() {
    let contract = parse_manifest(VALID_MANIFEST).unwrap();

    assert_eq!(contract.entry(), "_start");
    assert_eq!(
        contract
            .imports()
            .iter()
            .map(|import| import.operation)
            .collect::<Vec<_>>(),
        [OperationId::ProcessExit, OperationId::StreamWrite]
    );
    assert_eq!(
        contract
            .capabilities()
            .iter()
            .map(|capability| capability.requirement)
            .collect::<Vec<_>>(),
        [RequirementId::SelfProcess, RequirementId::Stdout]
    );
    assert_eq!(contract.capabilities()[1].rights, Rights::WRITE);
    assert_eq!(contract.runtime().stack_size, 65536);
}

#[test]
fn manifest_rejects_unknown_and_duplicate_registry_names() {
    let unknown = VALID_MANIFEST.replace("stream.write", "future.write");
    assert_eq!(
        parse_manifest(&unknown).unwrap_err().kind(),
        ContractErrorKind::UnknownOperation
    );

    let duplicate = VALID_MANIFEST.replace(
        r#"{ "operation": "process.exit", "required": true }"#,
        r#"{ "operation": "stream.write", "required": true }"#,
    );
    assert_eq!(
        parse_manifest(&duplicate).unwrap_err().kind(),
        ContractErrorKind::DuplicateOperation
    );
}

#[test]
fn manifest_rejects_capability_escalation() {
    let escalation = VALID_MANIFEST.replace(r#"["write"]"#, r#"["write", "read"]"#);
    assert_eq!(
        parse_manifest(&escalation).unwrap_err().kind(),
        ContractErrorKind::RightsExceeded
    );
}

#[test]
fn manifest_allows_operations_on_objects_created_at_runtime() {
    let dynamic_object_manifest = r#"
{
  "manifest_version": 1,
  "abi_epoch": 1,
  "entry": "_start",
  "imports": [
    { "operation": "event.create", "required": true },
    { "operation": "event.bind", "required": true },
    { "operation": "event.wait", "required": true }
  ],
  "capabilities": [
    { "requirement": "self_process", "rights": ["create"], "required": true }
  ],
  "runtime": {
    "stack_size": 65536,
    "stack_guard_size": 4096,
    "start_info_max_size": 4096
  }
}
"#;

    let contract = parse_manifest(dynamic_object_manifest)
        .expect("EventPort 由 event.create 创建，不是初始 capability");

    assert_eq!(contract.imports().len(), 3);
    assert_eq!(contract.capabilities().len(), 1);
    assert_eq!(contract.capabilities()[0].rights, Rights::CREATE);
}

#[test]
fn manifest_rejects_invalid_runtime_and_unknown_fields() {
    let invalid_runtime = VALID_MANIFEST.replace("65536", "4096");
    assert_eq!(
        parse_manifest(&invalid_runtime).unwrap_err().kind(),
        ContractErrorKind::InvalidRuntime
    );

    let unknown_field = VALID_MANIFEST.replace(
        r#""start_info_max_size": 4096"#,
        r#""start_info_max_size": 4096, "magic": true"#,
    );
    assert_eq!(
        parse_manifest(&unknown_field).unwrap_err().kind(),
        ContractErrorKind::InvalidJson
    );
}

const COMPONENT_MANIFEST: &str = r#"
{
  "manifest_version": 1,
  "abi_epoch": 1,
  "component_id": "00112233445566778899aabbccddeeff",
  "abi_id": "102132435465768798a9bacbdcedfe0f",
  "init": "component_init",
  "fini": "component_fini",
  "tls_offset_symbol": "component_tls_offset",
  "imports": [
    { "operation": "clock.read", "required": true, "slot_symbol": "clock_slot" }
  ],
  "capabilities": [
    { "requirement": "stdout", "rights": ["write"], "required": true }
  ],
  "dependencies": [
    {
      "component_id": "ffeeddccbbaa99887766554433221100",
      "abi_id": "00ffeeddccbbaa998877665544332211",
      "content_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "name": "math"
    }
  ],
  "symbol_imports": [
    {
      "dependency_component_id": "ffeeddccbbaa99887766554433221100",
      "interface_id": "11111111111111111111111111111111",
      "symbol_id": "22222222222222222222222222222222",
      "signature_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "binding_symbol": "math_add_gate",
      "name": "math.add"
    }
  ],
  "symbol_exports": [
    {
      "interface_id": "33333333333333333333333333333333",
      "symbol_id": "44444444444444444444444444444444",
      "signature_hash": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "symbol": "plugin_run",
      "name": "plugin.run"
    }
  ]
}
"#;

#[test]
fn component_manifest_becomes_a_canonical_exact_contract() {
    let contract = parse_component_manifest(COMPONENT_MANIFEST).unwrap();

    assert_eq!(
        contract.component_id(),
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
            0xdd, 0xee, 0xff,
        ]
    );
    assert_eq!(contract.init(), Some("component_init"));
    assert_eq!(contract.fini(), Some("component_fini"));
    assert_eq!(contract.tls_offset_symbol(), Some("component_tls_offset"));
    assert_eq!(contract.imports()[0].operation, OperationId::ClockRead);
    assert_eq!(contract.imports()[0].slot_symbol, "clock_slot");
    assert_eq!(contract.capabilities().len(), 1);
    assert_eq!(contract.capabilities()[0].requirement, RequirementId::Stdout);
    assert_eq!(contract.capabilities()[0].rights, Rights::WRITE);
    assert_eq!(contract.dependencies().len(), 1);
    assert_eq!(contract.symbol_imports()[0].dependency_index, 0);
    assert_eq!(contract.symbol_imports()[0].binding_symbol, "math_add_gate");
    assert_eq!(contract.symbol_exports()[0].symbol, "plugin_run");
}

#[test]
fn component_manifest_rejects_bad_identity_and_missing_dependency() {
    let bad_identity = COMPONENT_MANIFEST.replace(
        "00112233445566778899aabbccddeeff",
        "not-a-component-id",
    );
    assert_eq!(
        parse_component_manifest(&bad_identity).unwrap_err().kind(),
        ContractErrorKind::InvalidIdentity
    );

    let missing = COMPONENT_MANIFEST.replacen(
        "ffeeddccbbaa99887766554433221100",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        1,
    );
    assert_eq!(
        parse_component_manifest(&missing).unwrap_err().kind(),
        ContractErrorKind::MissingDependency
    );
}
