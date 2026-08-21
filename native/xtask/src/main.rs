use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    if let Err(error) = run() {
        eprintln!("native-xtask: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "cannot locate native workspace root".to_string())?
        .to_path_buf();
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("binding") => generate_binding(&root, args.collect()),
        Some("check") => check_anonlib(&root, args.collect()),
        Some("test") => test_anonlib(&root, args.collect()),
        Some("help") | None => {
            print_help();
            Ok(())
        }
        Some(command) => Err(format!(
            "unknown command {command:?}; run `cargo run --bin native-xtask -- help`"
        )),
    }
}

fn generate_binding(root: &Path, args: Vec<String>) -> Result<(), String> {
    let options = Options::parse(&args)?;
    let manifest = options
        .manifest
        .ok_or_else(|| "binding requires --manifest <program.json>".to_string())?;
    let output = options
        .output
        .ok_or_else(|| "binding requires --output <program.rs>".to_string())?;
    let target = options
        .target
        .ok_or_else(|| "binding requires --target <riscv64|loongarch64>".to_string())?;
    let output_path = absolute(root, &output);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建 binding 输出目录失败: {error}"))?;
    }
    let linker = find_soyo_linker(root)?;
    let status = Command::new(linker)
        .current_dir(root)
        .args([
            "--target",
            target.as_str(),
            "--manifest",
            manifest.as_str(),
            "--emit-rust-module",
            output_path
                .to_str()
                .ok_or_else(|| "binding 输出路径不是有效 UTF-8".to_string())?,
        ])
        .status()
        .map_err(|error| format!("启动 soyo-ld 失败: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("soyo-ld exited with {status}"))
    }
}

fn check_anonlib(root: &Path, args: Vec<String>) -> Result<(), String> {
    let options = Options::parse(&args)?;
    let binding = options
        .binding
        .ok_or_else(|| "check requires --binding <program.rs>".to_string())?;
    run_cargo(root, "check", &binding, options.target.as_deref())
}

fn test_anonlib(root: &Path, args: Vec<String>) -> Result<(), String> {
    let options = Options::parse(&args)?;
    let binding = options
        .binding
        .ok_or_else(|| "test requires --binding <program.rs>".to_string())?;
    run_cargo(root, "test", &binding, options.target.as_deref())
}

fn run_cargo(
    root: &Path,
    command: &str,
    binding: &str,
    target: Option<&str>,
) -> Result<(), String> {
    let mut cargo = Command::new("cargo");
    cargo
        .current_dir(root)
        .args([command, "-p", "anonlib"])
        .env("MYGO_PROGRAM_RS", absolute(root, binding));
    if let Some(target) = target {
        cargo.args(["--target", target]);
    }
    cargo
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cargo
        .status()
        .map_err(|error| format!("启动 cargo {command} 失败: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {command} exited with {status}"))
    }
}

fn find_soyo_linker(root: &Path) -> Result<OsString, String> {
    if let Some(path) = env::var_os("SOYO_LD") {
        return Ok(path);
    }
    if let Some(parent) = root.parent() {
        for profile in ["release", "debug"] {
            let sibling = parent.join(format!("tools/soyo-linker/target/{profile}/soyo-ld"));
            if sibling.is_file() {
                return Ok(sibling.into_os_string());
            }
        }
    }
    Err(
        "找不到 soyo-ld；请先在 tools/soyo-linker 运行 `cargo build --release`，或设置 SOYO_LD"
            .to_string(),
    )
}

fn absolute(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

struct Options {
    target: Option<String>,
    manifest: Option<String>,
    output: Option<String>,
    binding: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            target: None,
            manifest: None,
            output: None,
            binding: None,
        };
        let mut index = 0;
        while index < args.len() {
            let key = args[index].as_str();
            let value = args
                .get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{key} requires a value"))?;
            match key {
                "--target" => options.target = Some(value),
                "--manifest" => options.manifest = Some(value),
                "--output" => options.output = Some(value),
                "--binding" => options.binding = Some(value),
                other => return Err(format!("unknown option {other:?}")),
            }
            index += 2;
        }
        Ok(options)
    }
}

fn print_help() {
    println!(
        "native-xtask commands (run with `cargo run --bin native-xtask --`):\n\
  binding --target <riscv64|loongarch64> --manifest <program.json> --output <program.rs>\n\
  check --binding <program.rs> [--target <triple>]\n\
  test --binding <program.rs> [--target <triple>]\n\n\
C and Rust freestanding images remain separate targets; `soyo-ld` generates the\n\
manifest-specific binding before Cargo compiles `anonlib`."
    );
}
