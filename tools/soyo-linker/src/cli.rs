//! `soyo-ld` 命令行边界与原子输出。

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use native_abi::TargetArch;

use crate::contract::parse_manifest;
use crate::elf::MAX_OBJECT_FILE_SIZE;
use crate::link::{InputObject, LinkRequest, apply_relocations, build_link_image};
use crate::writer::encode_soyo;

const HELP: &str = "\
SOYO 直接静态链接器

用法:
  soyo-ld --target <riscv64|loongarch64> --manifest <app.json> -o <app.soyo> <ELF ET_REL>...

选项:
  --target <arch>     输出目标架构
  --manifest <path>  程序 ABI 与 capability 契约
  -o <path>           输出 SOYO 文件
  -h, --help          显示帮助
  --version           显示版本
";

const MAX_MANIFEST_SIZE: u64 = 1024 * 1024;
const MAX_OBJECT_COUNT: usize = 256;
const MAX_OBJECT_SIZE: u64 = MAX_OBJECT_FILE_SIZE as u64;
const MAX_TOTAL_OBJECT_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
struct Options {
    target: TargetArch,
    manifest: PathBuf,
    output: PathBuf,
    objects: Vec<PathBuf>,
}

struct OpenObject {
    path: PathBuf,
    file: File,
    size: u64,
}

enum Action {
    Help,
    Version,
    Link(Options),
}

#[derive(Debug)]
struct CliError {
    usage: bool,
    detail: String,
}

impl CliError {
    fn usage(detail: impl Into<String>) -> Self {
        Self {
            usage: true,
            detail: detail.into(),
        }
    }

    fn operation(detail: impl Into<String>) -> Self {
        Self {
            usage: false,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

pub fn main_entry() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Action::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Action::Version) => {
            println!("soyo-ld {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Action::Link(options)) => match link(options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("soyo-ld: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            eprintln!("soyo-ld: {error}");
            if error.usage {
                eprintln!("尝试 'soyo-ld --help' 查看用法。");
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<Action, CliError> {
    let mut target = None;
    let mut manifest = None;
    let mut output = None;
    let mut objects = Vec::new();
    let mut positional_only = false;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        if !positional_only {
            if argument == "--" {
                positional_only = true;
                continue;
            }
            if argument == "-h" || argument == "--help" {
                return Ok(Action::Help);
            }
            if argument == "--version" {
                return Ok(Action::Version);
            }
            if argument == "--target" {
                set_once(
                    &mut target,
                    parse_target(next_value(&mut arguments, "--target")?)?,
                    "--target",
                )?;
                continue;
            }
            if argument == "--manifest" {
                set_once(
                    &mut manifest,
                    PathBuf::from(next_value(&mut arguments, "--manifest")?),
                    "--manifest",
                )?;
                continue;
            }
            if argument == "-o" {
                set_once(
                    &mut output,
                    PathBuf::from(next_value(&mut arguments, "-o")?),
                    "-o",
                )?;
                continue;
            }
            if argument.to_string_lossy().starts_with('-') {
                return Err(CliError::usage(format!(
                    "未知选项 {}",
                    argument.to_string_lossy()
                )));
            }
        }
        objects.push(PathBuf::from(argument));
    }

    let target = target.ok_or_else(|| CliError::usage("缺少 --target"))?;
    let manifest = manifest.ok_or_else(|| CliError::usage("缺少 --manifest"))?;
    let output = output.ok_or_else(|| CliError::usage("缺少 -o"))?;
    if objects.is_empty() {
        return Err(CliError::usage("缺少 ELF ET_REL 输入对象"));
    }
    if objects.len() > MAX_OBJECT_COUNT {
        return Err(CliError::operation(format!(
            "对象数量超过 {MAX_OBJECT_COUNT} 个上限"
        )));
    }
    Ok(Action::Link(Options {
        target,
        manifest,
        output,
        objects,
    }))
}

fn next_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<OsString, CliError> {
    arguments
        .next()
        .ok_or_else(|| CliError::usage(format!("{option} 缺少参数")))
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), CliError> {
    if slot.replace(value).is_some() {
        return Err(CliError::usage(format!("重复指定 {option}")));
    }
    Ok(())
}

fn parse_target(value: OsString) -> Result<TargetArch, CliError> {
    match value.to_str() {
        Some("riscv64") => Ok(TargetArch::Riscv64),
        Some("loongarch64") => Ok(TargetArch::LoongArch64),
        _ => Err(CliError::usage(format!(
            "不支持目标架构 {}",
            value.to_string_lossy()
        ))),
    }
}

fn link(options: Options) -> Result<(), CliError> {
    let manifest_source = read_manifest(&options.manifest)?;
    let contract = parse_manifest(&manifest_source)
        .map_err(|error| CliError::operation(format!("manifest 无效: {error}")))?;

    let objects = read_objects(open_objects(&options.objects)?)?;
    let image = build_link_image(LinkRequest {
        target_arch: options.target,
        entry_symbol: contract.entry(),
        objects: &objects,
    })
    .map_err(|error| CliError::operation(format!("链接失败: {error}")))?;
    let image = apply_relocations(image)
        .map_err(|error| CliError::operation(format!("重定位失败: {error}")))?;
    let output = encode_soyo(&image, &contract)
        .map_err(|error| CliError::operation(format!("SOYO 编码失败: {error}")))?;
    write_atomic(&options.output, &output)
}

fn read_manifest(path: &Path) -> Result<String, CliError> {
    let file = File::open(path).map_err(|error| {
        CliError::operation(format!("读取 manifest {} 失败: {error}", path.display()))
    })?;
    let size = file.metadata().map_err(|error| {
        CliError::operation(format!(
            "读取 manifest {} 元数据失败: {error}",
            path.display()
        ))
    })?;
    if size.len() > MAX_MANIFEST_SIZE {
        return Err(CliError::operation(format!(
            "manifest 超过 {MAX_MANIFEST_SIZE} 字节上限: {}",
            path.display()
        )));
    }
    let bytes = read_bounded(file, size.len(), MAX_MANIFEST_SIZE, || {
        format!("读取 manifest {}", path.display())
    })?;
    if bytes.len() as u64 > MAX_MANIFEST_SIZE {
        return Err(CliError::operation(format!(
            "manifest 超过 {MAX_MANIFEST_SIZE} 字节上限: {}",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| CliError::operation(format!("manifest 不是 UTF-8: {}", path.display())))
}

fn open_objects(paths: &[PathBuf]) -> Result<Vec<OpenObject>, CliError> {
    let mut total = 0u64;
    let mut objects = Vec::with_capacity(paths.len());
    for path in paths {
        let file = File::open(path).map_err(|error| {
            CliError::operation(format!("读取对象 {} 失败: {error}", path.display()))
        })?;
        let size = file.metadata().map_err(|error| {
            CliError::operation(format!("读取对象 {} 元数据失败: {error}", path.display()))
        })?;
        if size.len() > MAX_OBJECT_SIZE {
            return Err(CliError::operation(format!(
                "对象超过 {MAX_OBJECT_SIZE} 字节上限: {}",
                path.display()
            )));
        }
        total = total.checked_add(size.len()).ok_or_else(|| {
            CliError::operation(format!("对象总大小超过 {MAX_TOTAL_OBJECT_SIZE} 字节上限"))
        })?;
        if total > MAX_TOTAL_OBJECT_SIZE {
            return Err(CliError::operation(format!(
                "对象总大小超过 {MAX_TOTAL_OBJECT_SIZE} 字节上限"
            )));
        }
        objects.push(OpenObject {
            path: path.clone(),
            file,
            size: size.len(),
        });
    }
    Ok(objects)
}

fn read_objects(objects: Vec<OpenObject>) -> Result<Vec<InputObject>, CliError> {
    let mut total = 0u64;
    let mut inputs = Vec::with_capacity(objects.len());
    for object in objects {
        let remaining = MAX_TOTAL_OBJECT_SIZE - total;
        let limit = MAX_OBJECT_SIZE.min(remaining);
        let bytes = read_bounded(object.file, object.size, limit, || {
            format!("读取对象 {}", object.path.display())
        })?;
        if bytes.len() as u64 > limit {
            let detail = if limit == MAX_OBJECT_SIZE {
                format!(
                    "对象超过 {MAX_OBJECT_SIZE} 字节上限: {}",
                    object.path.display()
                )
            } else {
                format!("对象总大小超过 {MAX_TOTAL_OBJECT_SIZE} 字节上限")
            };
            return Err(CliError::operation(detail));
        }
        total += bytes.len() as u64;
        inputs.push(InputObject::new(object.path, bytes));
    }
    Ok(inputs)
}

fn read_bounded(
    file: File,
    expected_size: u64,
    limit: u64,
    context: impl FnOnce() -> String,
) -> Result<Vec<u8>, CliError> {
    let context = context();
    let capacity = usize::try_from(expected_size.min(limit))
        .map_err(|_| CliError::operation(format!("{context} 的大小超过宿主地址空间")))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| CliError::operation(format!("{context} 所需内存不足")))?;
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CliError::operation(format!("{context}失败: {error}")))?;
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| CliError::operation("输出路径缺少文件名"))?;
    let (temporary_path, mut file) = create_temporary(parent, name)?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| CliError::operation(format!("写入临时输出失败: {error}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))
                .map_err(|error| CliError::operation(format!("设置输出权限失败: {error}")))?;
        }
        file.sync_all()
            .map_err(|error| CliError::operation(format!("同步临时输出失败: {error}")))?;
        drop(file);
        fs::rename(&temporary_path, path).map_err(|error| {
            CliError::operation(format!("替换输出 {} 失败: {error}", path.display()))
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_temporary(parent: &Path, output_name: &OsStr) -> Result<(PathBuf, fs::File), CliError> {
    for attempt in 0..100u32 {
        let temporary_name = format!(
            ".{}.soyo-ld.{}.{}.tmp",
            output_name.to_string_lossy(),
            std::process::id(),
            attempt
        );
        let path = parent.join(temporary_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(CliError::operation(format!(
                    "创建输出临时文件失败: {error}"
                )));
            }
        }
    }
    Err(CliError::operation("无法获取唯一的输出临时文件名"))
}
