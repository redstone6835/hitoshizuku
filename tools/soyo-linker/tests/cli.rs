use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const VALID_MANIFEST: &str = r#"{
  "entry":"_start",
  "imports":[{"name":"PROCESS_EXIT","required":true}],
  "capabilities":[{
    "name":"SELF_PROCESS",
    "rights":["TERMINATE_SELF"],
    "required":true
  }],
  "runtime":{
    "stack_size":65536,
    "stack_guard_size":4096,
    "start_info_max_size":4096
  }
}"#;

fn temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "soyo-ld-cli-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

#[test]
fn help_describes_the_direct_linker_interface() {
    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("soyo-ld --target"));
    assert!(stdout.contains("ELF ET_REL"));
}

#[test]
fn usage_error_has_exit_code_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("缺少 --manifest")
    );
}

#[test]
fn header_only_mode_writes_generated_binding_without_objects() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    let header = directory.join("mygo_program.h");
    fs::write(&manifest, VALID_MANIFEST).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .args(["--emit-c-header"])
        .arg(&header)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "header-only 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = String::from_utf8(fs::read(&header).unwrap()).unwrap();
    assert!(generated.contains("#define MYGO_SLOT_PROCESS_EXIT 0u\n"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn header_only_mode_rejects_link_output_without_partial_header() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    let header = directory.join("mygo_program.h");
    let output_path = directory.join("app.soyo");
    fs::write(&manifest, VALID_MANIFEST).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .args(["--emit-c-header"])
        .arg(&header)
        .args(["-o"])
        .arg(&output_path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--emit-c-header 不能与 -o 或对象输入同时使用")
    );
    assert!(!header.exists());
    assert!(!output_path.exists());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn input_failure_does_not_replace_existing_output() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    let output_path = directory.join("app.soyo");
    fs::write(&manifest, VALID_MANIFEST).unwrap();
    fs::write(&output_path, b"existing").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .arg("-o")
        .arg(&output_path)
        .arg(directory.join("missing.o"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read(&output_path).unwrap(), b"existing");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_manifest_over_one_mib_before_parsing() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    fs::File::create(&manifest)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .arg("-o")
        .arg(directory.join("app.soyo"))
        .arg(directory.join("missing.o"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("manifest 超过 1048576 字节上限")
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_object_over_128_mib_before_reading() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    let object = directory.join("large.o");
    fs::write(&manifest, VALID_MANIFEST).unwrap();
    fs::File::create(&object)
        .unwrap()
        .set_len(128 * 1024 * 1024 + 1)
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .arg("-o")
        .arg(directory.join("app.soyo"))
        .arg(&object)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("对象超过 134217728 字节上限")
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_more_than_256_objects_before_reading() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    let object = directory.join("invalid.o");
    fs::write(&manifest, VALID_MANIFEST).unwrap();
    fs::write(&object, b"not an ELF object").unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_soyo-ld"));
    command
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .arg("-o")
        .arg(directory.join("app.soyo"));
    for _ in 0..257 {
        command.arg(&object);
    }
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("对象数量超过 256 个上限")
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_total_object_size_over_256_mib_before_reading() {
    let directory = temp_dir();
    let manifest = directory.join("app.json");
    fs::write(&manifest, VALID_MANIFEST).unwrap();
    let objects = (0..3)
        .map(|index| {
            let path = directory.join(format!("large-{index}.o"));
            fs::File::create(&path)
                .unwrap()
                .set_len(90 * 1024 * 1024)
                .unwrap();
            path
        })
        .collect::<Vec<_>>();

    let output = Command::new(env!("CARGO_BIN_EXE_soyo-ld"))
        .args(["--target", "riscv64", "--manifest"])
        .arg(&manifest)
        .arg("-o")
        .arg(directory.join("app.soyo"))
        .args(&objects)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("对象总大小超过 268435456 字节上限")
    );
    fs::remove_dir_all(directory).unwrap();
}
