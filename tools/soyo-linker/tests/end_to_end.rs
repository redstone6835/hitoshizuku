use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo::{SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, validate_soyo};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MANIFEST: &str = r#"
{
  "entry": "_start",
  "imports": [
    { "name": "PROCESS_EXIT", "required": true },
    { "name": "STREAM_WRITE", "required": true }
  ],
  "capabilities": [
    { "name": "SELF_PROCESS", "rights": ["TERMINATE_SELF"], "required": true },
    { "name": "STDOUT", "rights": ["WRITE"], "required": true }
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
