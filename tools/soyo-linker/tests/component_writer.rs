use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::{RequirementId, TargetArch};
use soyo::registry::{ArtifactKind, DynamicRelocationKind};
use soyo::{
    SignatureTrust, SignatureTrustPolicy, SliceSoyoReader, SoyoReadLimits, TrustedPublicKey,
    read_soyo, verify_metadata_signature,
};
use soyo_linker::contract::parse_component_manifest;
use soyo_linker::link::{InputObject, LinkRequest, apply_relocations, build_link_image};
use soyo_linker::writer::{encode_component_soyo, encode_signed_component_soyo};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MANIFEST: &str = r#"
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

fn linked_component() -> soyo_linker::link::LinkedImage {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/component.c");
    let output = std::env::temp_dir().join(format!(
        "soyo-component-writer-{}-{}.o",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let compilation = Command::new("clang")
        .arg("--target=riscv64-unknown-none-elf")
        .args([
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-pic",
            "-fno-pie",
            "-fno-asynchronous-unwind-tables",
            "-fno-unwind-tables",
            "-fvisibility=hidden",
            "-mno-relax",
            "-msmall-data-limit=0",
            "-mcmodel=medany",
            "-O2",
            "-c",
        ])
        .arg(source)
        .arg("-o")
        .arg(&output)
        .output()
        .expect("应能启动 clang");
    assert!(
        compilation.status.success(),
        "clang 生成组件对象失败: {}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    let bytes = fs::read(&output).unwrap();
    fs::remove_file(output).unwrap();
    let objects = [InputObject::new(PathBuf::from("component.c"), bytes)];
    apply_relocations(
        build_link_image(LinkRequest {
            target_arch: TargetArch::Riscv64,
            entry_symbol: "plugin_run",
            objects: &objects,
        })
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn signed_component_preserves_content_identity_and_verifies() {
    let image = linked_component();
    let contract = parse_component_manifest(MANIFEST).unwrap();
    let bytes = encode_signed_component_soyo(&image, &contract, [7; 32]).unwrap();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()).unwrap();
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let trusted = TrustedPublicKey::new(signing_key.verifying_key().to_bytes());
    assert_eq!(
        verify_metadata_signature(
            &metadata,
            SignatureTrustPolicy {
                allow_unsigned: false,
                trusted_keys: &[trusted],
                revoked_key_ids: &[],
                rejected_content_hashes: &[],
            }
        ),
        Ok(SignatureTrust::Trusted {
            key_id: trusted.key_id,
        })
    );
}

#[test]
fn component_writer_round_trips_real_relocatable_object() {
    let image = linked_component();
    let contract = parse_component_manifest(MANIFEST).unwrap();
    let first = encode_component_soyo(&image, &contract).unwrap();
    let second = encode_component_soyo(&image, &contract).unwrap();
    assert_eq!(first, second);

    let metadata = read_soyo(&SliceSoyoReader::new(&first), SoyoReadLimits::portable()).unwrap();
    assert_eq!(metadata.header.artifact_kind, ArtifactKind::SharedComponent);
    assert_eq!(metadata.header.entry_offset, 0);
    assert!(metadata.runtime.is_none());
    assert_eq!(metadata.capabilities.len(), 1);
    assert_eq!(
        metadata.capabilities[0].requirement_id,
        RequirementId::Stdout as u32
    );
    let component = metadata.component.unwrap();
    assert_ne!(component.info.init_offset, 0);
    assert_ne!(component.info.fini_offset, 0);
    assert_eq!(component.dependencies.len(), 1);
    assert_eq!(component.symbol_imports.len(), 1);
    assert_eq!(component.symbol_exports.len(), 1);
    assert_eq!(component.dynamic_relocations.len(), 3);
    let mut kinds = component
        .dynamic_relocations
        .iter()
        .map(|relocation| relocation.kind)
        .collect::<Vec<_>>();
    kinds.sort_by_key(|kind| *kind as u16);
    assert_eq!(
        kinds,
        [
            DynamicRelocationKind::AbiSlot64,
            DynamicRelocationKind::InterfaceGate,
            DynamicRelocationKind::TlsOffset64,
        ]
    );
}
