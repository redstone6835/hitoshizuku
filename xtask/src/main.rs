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
    let context = BuildContext::resolve(&options)?;
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| context.default_module_output());

    build_modules_to(root, &options, &context, &output)
}

fn build_modules_to(
    root: &Path,
    options: &BuildOptions,
    context: &BuildContext,
    output: &str,
) -> Result<(), String> {
    ensure_config(root, &context.config)?;
    let cargo_target = root.join(&context.target_dir);
    let mut kernel_environment = vec![("CARGO_TARGET_DIR", cargo_target.as_os_str().to_owned())];
    context
        .board
        .append_kernel_environment(&mut kernel_environment);
    cargo_with_env(
        root,
        vec![
            "build".into(),
            "-p".into(),
            "kernel".into(),
            "--target".into(),
            context.target.clone().into(),
            "--release".into(),
        ],
        &kernel_environment,
    )?;

    let kernel = cargo_target.join(&context.target).join("release/kernel");
    let interface = root.join(&context.interface_dir);
    cargo_elm_with_env(
        root,
        vec![
            "profile-export".into(),
            kernel.as_os_str().to_owned(),
            "--target".into(),
            context.target.clone().into(),
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
        context.config.clone().into(),
        "--target".into(),
        context.target.clone().into(),
        "--output".into(),
        output.into(),
    ];
    if let Some(features) = options.features.as_deref() {
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
    let context = BuildContext::resolve(&options)?;
    ensure_config(root, &context.config)?;

    let module_output = options
        .modules
        .clone()
        .unwrap_or_else(|| context.default_module_output());
    if !root.join(&module_output).join("modules.manifest").is_file() {
        build_modules_to(root, &options, &context, &module_output)?;
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
    environment.push((
        "CARGO_TARGET_DIR",
        root.join(&context.target_dir).into_os_string(),
    ));
    context.board.append_kernel_environment(&mut environment);

    let mut command: Vec<OsString> = vec![
        "build".into(),
        "-p".into(),
        "kernel".into(),
        "--target".into(),
        context.target.into(),
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
    board: Board,
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
            board: Board::Qemu,
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
                "--board" => options.board = Board::parse(&value()?)?,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Board {
    Qemu,
    Ls2k1000,
    VisionFive2,
}

impl Board {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "qemu" => Ok(Self::Qemu),
            "ls2k1000" => Ok(Self::Ls2k1000),
            "visionfive2" => Ok(Self::VisionFive2),
            other => Err(format!(
                "unsupported board {other:?}; expected qemu, ls2k1000, or visionfive2"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Ls2k1000 => "ls2k1000",
            Self::VisionFive2 => "visionfive2",
        }
    }

    fn default_target(self) -> &'static str {
        match self {
            Self::Qemu | Self::Ls2k1000 => DEFAULT_TARGET,
            Self::VisionFive2 => RISCV_TARGET,
        }
    }

    fn default_config(self) -> &'static str {
        match self {
            Self::Qemu => ".config",
            Self::Ls2k1000 => "configs/ls2k1000.config",
            Self::VisionFive2 => "configs/visionfive2.config",
        }
    }

    fn validate_target(self, target: &str) -> Result<(), String> {
        let valid = match self {
            Self::Qemu => matches!(target, DEFAULT_TARGET | RISCV_TARGET),
            Self::Ls2k1000 => target == DEFAULT_TARGET,
            Self::VisionFive2 => target == RISCV_TARGET,
        };
        if valid {
            Ok(())
        } else {
            Err(format!(
                "board {:?} does not support target {target:?}; expected {:?}",
                self.name(),
                self.default_target()
            ))
        }
    }

    fn append_kernel_environment(self, environment: &mut Vec<(&'static str, OsString)>) {
        let value = match self {
            Self::Ls2k1000 => "ls2k1000",
            Self::Qemu | Self::VisionFive2 => "",
        };
        environment.push(("MYGO_LA_BOARD", value.into()));
    }
}

struct BuildContext {
    board: Board,
    target: String,
    arch: &'static str,
    config: String,
    target_dir: String,
    interface_dir: String,
}

impl BuildContext {
    fn resolve(options: &BuildOptions) -> Result<Self, String> {
        let target = options
            .target
            .as_deref()
            .unwrap_or_else(|| options.board.default_target());
        options.board.validate_target(target)?;
        let arch = target_arch(target)?;
        let board_suffix = (options.board != Board::Qemu).then(|| options.board.name());
        let target_dir = options
            .target_dir
            .clone()
            .unwrap_or_else(|| match board_suffix {
                Some(board) => format!("target/{arch}/{board}"),
                None => format!("target/{arch}"),
            });
        let interface_dir = match board_suffix {
            Some(board) => format!("build/elm-interface/{arch}/{board}"),
            None => format!("build/elm-interface/{arch}"),
        };
        Ok(Self {
            board: options.board,
            target: target.to_string(),
            arch,
            config: options
                .config
                .clone()
                .unwrap_or_else(|| options.board.default_config().to_string()),
            target_dir,
            interface_dir,
        })
    }

    fn default_module_output(&self) -> String {
        match self.board {
            Board::Qemu => format!("build/{}/modules", self.arch),
            board => format!("build/{}/{}/modules", self.arch, board.name()),
        }
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
  modules [--board <qemu|ls2k1000|visionfive2>] [--target <triple>] [--config <path>] [--output <dir>]\n\
  build [--board <qemu|ls2k1000|visionfive2>] [--target <triple>] [--config <path>] [--features <a,b>] [--initramfs <cpio>]\n\
  clean\n\n\
Board defaults select the matching target and config. QEMU keeps the existing architecture-level output paths; physical boards use isolated board-level paths.\n\
The initramfs image is an input to `build`; image generation is intentionally separate."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> BuildOptions {
        BuildOptions::parse(
            &args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
        )
        .expect("valid build options")
    }

    #[test]
    fn qemu_defaults_remain_architecture_scoped() {
        let context = BuildContext::resolve(&options(&[])).expect("resolve QEMU defaults");
        assert_eq!(context.board, Board::Qemu);
        assert_eq!(context.target, DEFAULT_TARGET);
        assert_eq!(context.config, ".config");
        assert_eq!(context.target_dir, "target/loongarch64");
        assert_eq!(context.interface_dir, "build/elm-interface/loongarch64");
        assert_eq!(context.default_module_output(), "build/loongarch64/modules");
    }

    #[test]
    fn physical_board_defaults_are_isolated() {
        let context = BuildContext::resolve(&options(&["--board", "visionfive2"]))
            .expect("resolve VisionFive 2 defaults");
        assert_eq!(context.target, RISCV_TARGET);
        assert_eq!(context.config, "configs/visionfive2.config");
        assert_eq!(context.target_dir, "target/riscv64/visionfive2");
        assert_eq!(
            context.interface_dir,
            "build/elm-interface/riscv64/visionfive2"
        );
        assert_eq!(
            context.default_module_output(),
            "build/riscv64/visionfive2/modules"
        );
    }

    #[test]
    fn explicit_paths_override_board_defaults() {
        let context = BuildContext::resolve(&options(&[
            "--board",
            "ls2k1000",
            "--config",
            "local.config",
            "--target-dir",
            "target/custom",
        ]))
        .expect("resolve path overrides");
        assert_eq!(context.config, "local.config");
        assert_eq!(context.target_dir, "target/custom");
    }

    #[test]
    fn ls2k1000_selects_board_linker_for_kernel_builds() {
        let context = BuildContext::resolve(&options(&["--board", "ls2k1000"]))
            .expect("resolve LS2K1000 defaults");
        let mut environment = Vec::new();
        context.board.append_kernel_environment(&mut environment);
        assert_eq!(
            environment,
            vec![("MYGO_LA_BOARD", OsString::from("ls2k1000"))]
        );
    }

    #[test]
    fn qemu_clears_inherited_physical_board_selection() {
        let context =
            BuildContext::resolve(&options(&["--board", "qemu"])).expect("resolve QEMU defaults");
        let mut environment = Vec::new();
        context.board.append_kernel_environment(&mut environment);
        assert_eq!(environment, vec![("MYGO_LA_BOARD", OsString::new())]);
    }

    #[test]
    fn board_target_mismatch_is_rejected() {
        let error = BuildContext::resolve(&options(&[
            "--board",
            "visionfive2",
            "--target",
            DEFAULT_TARGET,
        ]))
        .err()
        .expect("mismatched target must fail");
        assert!(error.contains("visionfive2"));
        assert!(error.contains(DEFAULT_TARGET));
    }

    #[test]
    fn unknown_board_is_rejected() {
        let error = BuildOptions::parse(&["--board".into(), "unknown".into()])
            .err()
            .expect("unknown board must fail");
        assert!(error.contains("unsupported board"));
    }
}
