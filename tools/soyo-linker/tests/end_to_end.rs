use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo::registry::ArtifactKind;
use soyo::{SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, validate_soyo};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MANIFEST: &str = r#"
{
  "manifest_version": 1,
  "abi_epoch": 1,
  "entry": "_start",
  "imports": [
    { "operation": "process.exit", "required": true },
    { "operation": "stream.write", "required": true }
  ],
  "capabilities": [
    { "requirement": "self_process", "rights": ["exit"], "required": true },
    { "requirement": "stdout", "rights": ["write"], "required": true }
  ],
  "runtime": {
    "stack_size": 65536,
    "stack_guard_size": 4096,
    "start_info_max_size": 4096
  }
}
"#;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(target: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "soyo-ld-e2e-{}-{}-{target}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn compile_objects(directory: &Path, triple: &str, extra_flags: &[&str]) -> Vec<PathBuf> {
    ["entry.c", "library.c", "pointer.c"]
        .into_iter()
        .map(|fixture| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture);
            let output = directory.join(format!("{fixture}.o"));
            let compilation = Command::new("clang")
                .arg(format!("--target={triple}"))
                .args([
                    "-ffreestanding",
                    "-fno-stack-protector",
                    "-fno-pic",
                    "-fno-pie",
                    "-fno-asynchronous-unwind-tables",
                    "-fno-unwind-tables",
                    "-fvisibility=hidden",
                    "-O0",
                    "-c",
                ])
                .args(extra_flags)
                .arg(source)
                .arg("-o")
                .arg(&output)
                .output()
                .expect("应能启动 clang");
            assert!(
                compilation.status.success(),
                "clang 生成 {triple} 对象失败: {}",
                String::from_utf8_lossy(&compilation.stderr)
            );

            let header = Command::new("readelf")
                .arg("-h")
                .arg(&output)
                .output()
                .expect("应能启动 readelf");
            assert!(header.status.success());
            assert!(String::from_utf8_lossy(&header.stdout).contains("REL (Relocatable file)"));
            output
        })
        .collect()
}

fn link_twice_and_validate(target_name: &str, target: TargetArch, triple: &str, flags: &[&str]) {
    let directory = TestDirectory::new(target_name);
    let objects = compile_objects(&directory.0, triple, flags);
    let manifest = directory.0.join("app.json");
    let first_path = directory.0.join("app.soyo");
    let second_path = directory.0.join("app-again.soyo");
    fs::write(&manifest, MANIFEST).unwrap();

    for output_path in [&first_path, &second_path] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_soyo-ld"));
        command
            .args(["--target", target_name, "--manifest"])
            .arg(&manifest)
            .arg("-o")
            .arg(output_path)
            .args(&objects);
        let output = command.output().expect("应能启动 soyo-ld");
        assert!(
            output.status.success(),
            "soyo-ld {target_name} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first = fs::read(&first_path).unwrap();
    let second = fs::read(&second_path).unwrap();
    assert_eq!(first, second);
    assert_eq!(&first[..4], b"soyo");
    let metadata = read_soyo(&SliceSoyoReader::new(&first), SoyoReadLimits::portable()).unwrap();
    assert_eq!(metadata.header.target_arch, target);
    validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(target)).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(&first_path).unwrap().permissions().mode() & 0o111,
            0
        );
    }
    assert!(
        fs::read_dir(&directory.0)
            .unwrap()
            .all(|entry| entry.unwrap().path().extension() != Some("elf".as_ref()))
    );
}

#[test]
fn rv64_sources_link_directly_to_deterministic_soyo() {
    link_twice_and_validate(
        "riscv64",
        TargetArch::Riscv64,
        "riscv64-unknown-none-elf",
        &["-mno-relax", "-msmall-data-limit=0", "-mcmodel=medany"],
    );
}

#[test]
fn la64_sources_link_directly_to_deterministic_soyo() {
    link_twice_and_validate(
        "loongarch64",
        TargetArch::LoongArch64,
        "loongarch64-unknown-none",
        &[],
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

fn link_component(target_name: &str, target: TargetArch, triple: &str, flags: &[&str]) {
    let directory = TestDirectory::new(&format!("component-{target_name}"));
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/component.c");
    let object = directory.0.join("component.o");
    let compilation = Command::new("clang")
        .arg(format!("--target={triple}"))
        .args([
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-pic",
            "-fno-pie",
            "-fno-asynchronous-unwind-tables",
            "-fno-unwind-tables",
            "-fvisibility=hidden",
            "-O2",
            "-c",
        ])
        .args(flags)
        .arg(source)
        .arg("-o")
        .arg(&object)
        .output()
        .expect("应能启动 clang");
    assert!(
        compilation.status.success(),
        "clang 生成 {target_name} component 失败: {}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    let manifest = directory.0.join("component.json");
    let output_path = directory.0.join("component.soyo");
    fs::write(&manifest, COMPONENT_MANIFEST).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--component", "--target", target_name, "--manifest"])
        .arg(&manifest)
        .arg("-o")
        .arg(&output_path)
        .arg(&object)
        .output()
        .expect("应能启动 soyo-ld");
    assert!(
        output.status.success(),
        "soyo-ld {target_name} component 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = fs::read(output_path).unwrap();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()).unwrap();
    assert_eq!(metadata.header.target_arch, target);
    assert_eq!(metadata.header.artifact_kind, ArtifactKind::SharedComponent);
}

#[test]
fn rv64_component_links_directly_from_relocatable_object() {
    link_component(
        "riscv64",
        TargetArch::Riscv64,
        "riscv64-unknown-none-elf",
        &["-mno-relax", "-msmall-data-limit=0", "-mcmodel=medany"],
    );
}

#[test]
fn la64_component_links_directly_from_relocatable_object() {
    link_component(
        "loongarch64",
        TargetArch::LoongArch64,
        "loongarch64-unknown-none",
        &[],
    );
}
