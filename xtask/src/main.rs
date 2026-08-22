use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

const DEFAULT_TARGET: &str = "loongarch64-unknown-none";
const RISCV_TARGET: &str = "riscv64gc-unknown-none-elf";

fn main() {
    if let Err(error) = run() {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "cannot locate repository root".to_string())?
        .to_path_buf();
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    let rest = args.collect::<Vec<_>>();

    match command.as_str() {
        "config" | "oldconfig" | "defconfig" => configure(&root, &command),
        "modules" => build_modules(&root, &rest),
        "build" | "kernel" => build_kernel(&root, &rest),
        "clean" => cargo(&root, ["clean"], None),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command {other:?}; run `cargo xtask help`")),
    }
}

fn configure(root: &Path, mode: &str) -> Result<(), String> {
    let config = root.join(".config");
    if mode == "defconfig" && config.exists() {
        std::fs::remove_file(&config)
            .map_err(|error| format!("remove {}: {error}", config.display()))?;
    }
    cargo_elm(
        root,
        [
            "configure-set",
            "drivers/Modules.toml",
            "--config",
            ".config",
            "--mode",
            mode,
        ],
        None,
    )
}

fn build_modules(root: &Path, args: &[String]) -> Result<(), String> {
    let options = BuildOptions::parse(args)?;
    let target = options.target.as_deref().unwrap_or(DEFAULT_TARGET);
    let arch = target_arch(target)?;
    let config = options.config.as_deref().unwrap_or(".config");
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| format!("build/{arch}/modules"));
    let target_dir = options
        .target_dir
        .clone()
        .unwrap_or_else(|| format!("target/{arch}"));

    ensure_config(root, config)?;
    let cargo_target = root.join(&target_dir);
    cargo(
        root,
        ["build", "-p", "kernel", "--target", target, "--release"],
        Some(("CARGO_TARGET_DIR", cargo_target.as_os_str().to_owned())),
    )?;

    let kernel = cargo_target.join(target).join("release/kernel");
    let interface = root.join(format!("build/elm-interface/{arch}"));
    cargo_elm_with_env(
        root,
        vec![
            "profile-export".into(),
            kernel.as_os_str().to_owned(),
            "--target".into(),
            target.into(),
            "--profile".into(),
            "hitoshizuku-default".into(),
            "--output".into(),
            interface.as_os_str().to_owned(),
        ],
        &[("CARGO_TARGET_DIR", cargo_target.as_os_str().to_owned())],
    )?;

    let mut command: Vec<OsString> = vec![
        "build-set".into(),
        "drivers/Modules.toml".into(),
        "--config".into(),
        config.into(),
        "--target".into(),
        target.into(),
        "--output".into(),
        output.into(),
    ];
    if let Some(features) = options.features {
        command.push("--features".into());
        command.push(features.into());
    }
    cargo_elm_with_env(
        root,
        command,
        &[
            ("CARGO_TARGET_DIR", cargo_target.as_os_str().to_owned()),
            (
                "ELM_KERNEL_INTERFACE_ROOT",
                interface.as_os_str().to_owned(),
            ),
        ],
    )
}

fn build_kernel(root: &Path, args: &[String]) -> Result<(), String> {
    let options = BuildOptions::parse(args)?;
    let target = options.target.as_deref().unwrap_or(DEFAULT_TARGET);
    let arch = target_arch(target)?;
    let config = options.config.as_deref().unwrap_or(".config");
    ensure_config(root, config)?;

    let module_output = options
        .modules
        .clone()
        .unwrap_or_else(|| format!("build/{arch}/modules"));
    if !Path::new(&root.join(&module_output).join("modules.manifest")).exists() {
        build_modules(root, args)?;
    }

    let manifest = root.join(&module_output).join("modules.manifest");
    let archives = root.join(&module_output).join("integrated.archives");
    let mut environment = Vec::new();
    if manifest.is_file() {
        environment.push(("ELM_BUILD_BOUND_MANIFEST", manifest.into_os_string()));
    }
    if archives.is_file() {
        let archive_paths = std::fs::read_to_string(&archives)
            .map_err(|error| format!("read {}: {error}", archives.display()))?
            .lines()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(OsString::from)
            .collect::<Vec<_>>();
        if archive_paths.is_empty() {
            return Err(format!(
                "integrated archive list {} is empty",
                archives.display()
            ));
        }
        let archive_value = env::join_paths(archive_paths)
            .map_err(|error| format!("encode integrated archive paths: {error}"))?;
        environment.push(("ELM_INTEGRATED_ARCHIVES", archive_value));
    }
    if let Some(initramfs) = options.initramfs {
        environment.push(("INITRAMFS", initramfs.into()));
    }

    let mut command: Vec<OsString> = vec![
        "build".into(),
        "-p".into(),
        "kernel".into(),
        "--target".into(),
        target.into(),
        "--release".into(),
    ];
    if let Some(features) = options.features {
        command.push("--features".into());
        command.push(features.into());
    }
    if environment.iter().any(|(name, _)| *name == "INITRAMFS") {
        command.push("--features".into());
        command.push("embedded-initramfs".into());
    }
    cargo_with_env(root, command, &environment)
}

fn ensure_config(root: &Path, config: &str) -> Result<(), String> {
    let path = root.join(config);
    if path.is_file() {
        return Ok(());
    }
    let default = root.join("configs/default.config");
    std::fs::copy(&default, &path).map_err(|error| {
        format!(
            "create {} from {}: {error}",
            path.display(),
            default.display()
        )
    })?;
    Ok(())
}

fn target_arch(target: &str) -> Result<&'static str, String> {
    match target {
        DEFAULT_TARGET => Ok("loongarch64"),
        RISCV_TARGET => Ok("riscv64"),
        other => Err(format!("unsupported kernel target {other:?}")),
    }
}

struct BuildOptions {
    target: Option<String>,
    config: Option<String>,
    output: Option<String>,
    modules: Option<String>,
    target_dir: Option<String>,
    features: Option<String>,
    initramfs: Option<String>,
}

impl BuildOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            target: None,
            config: None,
            output: None,
            modules: None,
            target_dir: None,
            features: None,
            initramfs: None,
        };
        let mut index = 0;
        while index < args.len() {
            let key = args[index].as_str();
            let value = || {
                args.get(index + 1)
                    .cloned()
                    .ok_or_else(|| format!("{key} requires a value"))
            };
            match key {
                "--target" => options.target = Some(value()?),
                "--config" => options.config = Some(value()?),
                "--output" => options.output = Some(value()?),
                "--modules" => options.modules = Some(value()?),
                "--target-dir" => options.target_dir = Some(value()?),
                "--features" => options.features = Some(value()?),
                "--initramfs" => options.initramfs = Some(value()?),
                other => return Err(format!("unknown build option {other:?}")),
            }
            index += 2;
        }
        Ok(options)
    }
}

fn cargo<I, S>(root: &Path, args: I, env_var: Option<(&str, OsString)>) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("cargo");
    command.current_dir(root).args(args);
    if let Some((name, value)) = env_var {
        command.env(name, value);
    }
    run_command(command)
}

fn cargo_elm<I, S>(root: &Path, args: I, env_var: Option<(&str, OsString)>) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        .env("HITOSHIZUKU_KERNEL_ROOT", root)
        .arg("elm")
        .args(args);
    if let Some((name, value)) = env_var {
        command.env(name, value);
    }
    run_command(command).map_err(|error| {
        format!(
            "cargo-elm failed; install `cargo-elm` from https://github.com/redstone6835/hitoshizuku-elm-tools before retrying: {error}"
        )
    })
}

fn cargo_elm_with_env<I, S>(
    root: &Path,
    args: I,
    env_vars: &[(&str, OsString)],
) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        .env("HITOSHIZUKU_KERNEL_ROOT", root)
        .arg("elm")
        .args(args);
    for (name, value) in env_vars {
        command.env(name, value);
    }
    run_command(command).map_err(|error| {
        format!(
            "cargo-elm failed; install `cargo-elm` from https://github.com/redstone6835/hitoshizuku-elm-tools before retrying: {error}"
        )
    })
}

fn cargo_with_env<S: AsRef<std::ffi::OsStr>>(
    root: &Path,
    args: Vec<OsString>,
    env_vars: &[(S, OsString)],
) -> Result<(), String> {
    let mut command = Command::new("cargo");
    command.current_dir(root).args(args);
    for (name, value) in env_vars {
        command.env(name, value);
    }
    run_command(command)
}

fn run_command(mut command: Command) -> Result<(), String> {
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command
        .status()
        .map_err(|error| format!("run {:?}: {error}", command))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command {:?} exited with {status}", command))
    }
}

fn print_help() {
    println!(
        "cargo xtask commands:\n\
  config | oldconfig | defconfig\n\
  modules [--target <triple>] [--config <path>] [--output <dir>]\n\
  build [--target <triple>] [--config <path>] [--features <a,b>] [--initramfs <cpio>]\n\
  clean\n\n\
The initramfs image is an input to `build`; image generation is intentionally separate."
    );
}
