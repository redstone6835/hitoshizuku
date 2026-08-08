use native_abi::{OperationId, RequirementId, Rights};
use soyo_linker::contract::{ContractErrorKind, parse_manifest};

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
fn manifest_rejects_capability_escalation_and_missing_authority() {
    let escalation = VALID_MANIFEST.replace(r#"["write"]"#, r#"["write", "read"]"#);
    assert_eq!(
        parse_manifest(&escalation).unwrap_err().kind(),
        ContractErrorKind::RightsExceeded
    );

    let missing = VALID_MANIFEST
        .replace("stdout", "stdin")
        .replace(r#"["write"]"#, r#"["read"]"#);
    assert_eq!(
        parse_manifest(&missing).unwrap_err().kind(),
        ContractErrorKind::MissingCapability
    );
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
