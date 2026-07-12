use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmProjectManifest {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub source: String,
    pub menu: Option<ElmProjectMenu>,
    pub dependencies: Vec<ElmProjectDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmProjectMenu {
    pub label: String,
    pub description: String,
    pub route: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmProjectDependency {
    pub provider: String,
    pub contract: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Elm,
    Menu,
    Dependency(usize),
}

impl ElmProjectManifest {
    pub fn load(project: &Path) -> Result<Self, String> {
        let path = project.join("Elm.toml");
        let input = fs::read_to_string(&path)
            .map_err(|err| format!("读取 {} 失败: {err}", path.display()))?;
        Self::parse(&input)
    }

    pub fn parse(input: &str) -> Result<Self, String> {
        let mut section = None;
        let mut elm = BTreeMap::new();
        let mut menu = BTreeMap::new();
        let mut dependencies: Vec<BTreeMap<String, String>> = Vec::new();
        for (line_index, raw_line) in input.lines().enumerate() {
            let line_number = line_index + 1;
            let line = strip_comment(raw_line)?.trim();
            if line.is_empty() {
                continue;
            }
            if line == "[elm]" {
                section = Some(Section::Elm);
                continue;
            }
            if line == "[menu]" {
                section = Some(Section::Menu);
                continue;
            }
            if line == "[[dependencies]]" {
                dependencies.push(BTreeMap::new());
                section = Some(Section::Dependency(dependencies.len() - 1));
                continue;
            }
            if line.starts_with('[') {
                return Err(format!("Elm.toml 第 {line_number} 行包含未知 section"));
            }
            let (key, raw_value) = line
                .split_once('=')
                .ok_or_else(|| format!("Elm.toml 第 {line_number} 行缺少 '='"))?;
            let key = key.trim();
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            {
                return Err(format!("Elm.toml 第 {line_number} 行键名无效"));
            }
            let value = parse_basic_string(raw_value.trim(), line_number)?;
            let target = match section {
                Some(Section::Elm) => &mut elm,
                Some(Section::Menu) => &mut menu,
                Some(Section::Dependency(index)) => &mut dependencies[index],
                None => return Err(format!("Elm.toml 第 {line_number} 行位于 section 之外")),
            };
            if target.insert(key.to_string(), value).is_some() {
                return Err(format!("Elm.toml 第 {line_number} 行重复定义 {key}"));
            }
        }

        reject_unknown_keys(&elm, &["name", "version", "kind", "source"], "[elm]")?;
        reject_unknown_keys(&menu, &["label", "description", "route"], "[menu]")?;
        let name = take_required(&elm, "name", "[elm]")?;
        let version = take_required(&elm, "version", "[elm]")?;
        let kind = take_required(&elm, "kind", "[elm]")?;
        let source = take_required(&elm, "source", "[elm]")?;
        validate_identifier(&name, 128, "ELM 名称")?;
        validate_version(&version)?;
        validate_source(&source)?;
        if !matches!(
            kind.as_str(),
            "manager"
                | "service"
                | "driver"
                | "extension"
                | "filesystem"
                | "network"
                | "debug"
                | "other"
        ) {
            return Err(format!("未知 ELM kind: {kind}"));
        }

        let menu = if menu.is_empty() {
            None
        } else {
            let label = take_required(&menu, "label", "[menu]")?;
            let description = take_required(&menu, "description", "[menu]")?;
            let route = take_required(&menu, "route", "[menu]")?;
            if label.is_empty() || label.len() > 64 {
                return Err("菜单 label 长度必须位于 1..=64".to_string());
            }
            if description.len() > 160 {
                return Err("菜单 description 不得超过 160 字节".to_string());
            }
            if route.is_empty() || route.len() > 96 {
                return Err("菜单 route 长度必须位于 1..=96".to_string());
            }
            Some(ElmProjectMenu {
                label,
                description,
                route,
            })
        };

        let mut parsed_dependencies = Vec::new();
        for (index, dependency) in dependencies.iter().enumerate() {
            reject_unknown_keys(
                dependency,
                &["provider", "contract"],
                &format!("[[dependencies]] #{}", index + 1),
            )?;
            let provider = take_required(dependency, "provider", "[[dependencies]]")?;
            let contract = take_required(dependency, "contract", "[[dependencies]]")?;
            validate_identifier(&provider, 128, "依赖 provider 名称")?;
            validate_contract(&contract)?;
            if parsed_dependencies
                .iter()
                .any(|item: &ElmProjectDependency| {
                    item.provider == provider && item.contract == contract
                })
            {
                return Err(format!("重复依赖: {provider} {contract}"));
            }
            parsed_dependencies.push(ElmProjectDependency { provider, contract });
        }
        Ok(Self {
            name,
            version,
            kind,
            source,
            menu,
            dependencies: parsed_dependencies,
        })
    }

    pub fn cargo_name(&self) -> String {
        self.name.replace('.', "-")
    }
}

pub fn scaffold_project(
    directory: &Path,
    name: &str,
    kind: &str,
    source: &str,
) -> Result<(), String> {
    if directory.exists() {
        let mut entries = fs::read_dir(directory)
            .map_err(|err| format!("读取 {} 失败: {err}", directory.display()))?;
        if entries.next().is_some() {
            return Err(format!("目标目录非空: {}", directory.display()));
        }
    } else {
        fs::create_dir_all(directory)
            .map_err(|err| format!("创建 {} 失败: {err}", directory.display()))?;
    }
    validate_identifier(name, 128, "ELM 名称")?;
    validate_source(source)?;
    if !matches!(
        kind,
        "manager"
            | "service"
            | "driver"
            | "extension"
            | "filesystem"
            | "network"
            | "debug"
            | "other"
    ) {
        return Err(format!("未知 ELM kind: {kind}"));
    }
    let cargo_name = name.replace('.', "-");
    fs::create_dir_all(directory.join("src")).map_err(|err| format!("创建 src 失败: {err}"))?;
    fs::create_dir_all(directory.join(".cargo"))
        .map_err(|err| format!("创建 .cargo 失败: {err}"))?;
    write_new(
        &directory.join("Cargo.toml"),
        &cargo_toml(&cargo_name, kind),
    )?;
    write_new(&directory.join("Elm.toml"), &elm_toml(name, kind, source))?;
    write_new(&directory.join("src/main.rs"), &main_rs(name))?;
    write_new(&directory.join("elm.ld"), ELM_LINKER_SCRIPT)?;
    write_new(&directory.join(".cargo/config.toml"), ELM_CARGO_CONFIG)?;
    sync_framework(directory)
}

pub fn sync_framework(project: &Path) -> Result<(), String> {
    let manifest = project.join("Cargo.toml");
    let elm_manifest = project.join("Elm.toml");
    if !manifest.is_file() || !elm_manifest.is_file() {
        return Err(format!(
            "{} 不是 ELM 工程：缺少 Cargo.toml 或 Elm.toml",
            project.display()
        ));
    }
    let project_manifest = ElmProjectManifest::load(project)?;
    migrate_cargo_manifest(&manifest, &project_manifest.kind)?;
    let source = framework_source_root()?;
    let elm_source = source.join("libs/elm");
    let kernel_api_source = source.join("kernel-api");
    if !elm_source.join("Cargo.toml").is_file() {
        return Err(format!("找不到框架源目录: {}", elm_source.display()));
    }
    if !kernel_api_source.join("Cargo.toml").is_file() {
        return Err(format!(
            "找不到 Kernel API 门面源目录: {}",
            kernel_api_source.display()
        ));
    }
    let elm_root = project.join(".elm");
    fs::create_dir_all(&elm_root)
        .map_err(|err| format!("创建 {} 失败: {err}", elm_root.display()))?;
    let destination = elm_root.join("framework");
    let temporary = elm_root.join(format!("framework.tmp.{}", std::process::id()));
    let backup = elm_root.join(format!("framework.old.{}", std::process::id()));
    remove_if_exists(&temporary)?;
    remove_if_exists(&backup)?;
    fs::create_dir_all(&temporary)
        .map_err(|err| format!("创建 {} 失败: {err}", temporary.display()))?;
    copy_tree(&elm_source, &temporary.join("elm"))?;
    copy_tree(&kernel_api_source, &temporary.join("kernel-api"))?;
    rewrite_synced_kernel_api_manifest(&temporary.join("kernel-api/Cargo.toml"))?;
    if destination.exists() {
        fs::rename(&destination, &backup).map_err(|err| {
            format!(
                "备份现有框架 {} -> {} 失败: {err}",
                destination.display(),
                backup.display()
            )
        })?;
    }
    if let Err(err) = fs::rename(&temporary, &destination) {
        if backup.exists() {
            let _ = fs::rename(&backup, &destination);
        }
        return Err(format!("原子替换框架失败: {err}"));
    }
    remove_if_exists(&backup)?;
    Ok(())
}

pub fn cargo_build(project: &Path, target: &str, cargo_name: &str) -> Result<PathBuf, String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    let mut rustflags = vec![
        "-Clink-arg=-Telm.ld",
        "-Crelocation-model=pic",
        "-Ccode-model=small",
        "-Clink-arg=-pie",
        "-Clink-arg=-z",
        "-Clink-arg=notext",
        "-Clink-arg=--gc-sections",
        "-Clink-arg=--build-id=none",
    ];
    if target == "loongarch64-unknown-none" {
        rustflags.push("-Anamed_asm_labels");
    }
    let status = Command::new("cargo")
        .current_dir(&project)
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"))
        .arg("build")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .arg("--package")
        .arg(cargo_name)
        .arg("--bin")
        .arg(cargo_name)
        .arg("--target")
        .arg(target)
        .arg("--release")
        .status()
        .map_err(|err| format!("启动 cargo build 失败: {err}"))?;
    if !status.success() {
        return Err(format!("ELM Rust 构建失败，退出状态 {status}"));
    }
    Ok(project
        .join("target")
        .join(target)
        .join("release")
        .join(cargo_name))
}

fn framework_source_root() -> Result<PathBuf, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize()
        .map_err(|err| format!("定位 ELM 框架源码失败: {err}"))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|err| format!("创建 {} 失败: {err}", destination.display()))?;
    for entry in
        fs::read_dir(source).map_err(|err| format!("读取 {} 失败: {err}", source.display()))?
    {
        let entry = entry.map_err(|err| format!("读取目录项失败: {err}"))?;
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(name);
        let file_type = entry
            .file_type()
            .map_err(|err| format!("读取 {} 类型失败: {err}", source_path.display()))?;
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|err| {
                format!(
                    "复制 {} -> {} 失败: {err}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        } else {
            return Err(format!(
                "框架源包含不支持的文件类型: {}",
                source_path.display()
            ));
        }
    }
    Ok(())
}

fn rewrite_synced_kernel_api_manifest(path: &Path) -> Result<(), String> {
    let input = fs::read_to_string(path)
        .map_err(|err| format!("读取同步后的 {} 失败: {err}", path.display()))?;
    let output = input.replace("path = \"../libs/elm\"", "path = \"../elm\"");
    if output == input {
        return Err(format!("{} 不包含规范 elm 依赖路径", path.display()));
    }
    fs::write(path, output).map_err(|err| format!("重写 {} 失败: {err}", path.display()))
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|err| format!("删除 {} 失败: {err}", path.display()))?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|err| format!("删除 {} 失败: {err}", path.display()))?;
    }
    Ok(())
}

fn write_new(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("拒绝覆盖已有文件: {}", path.display()));
    }
    fs::write(path, contents).map_err(|err| format!("写入 {} 失败: {err}", path.display()))
}

fn cargo_toml(name: &str, kind: &str) -> String {
    let features = elm_features(kind);
    format!(
        r#"[workspace]
resolver = "2"
members = [
    ".",
    ".elm/framework/elm",
    ".elm/framework/elm/macros",
    ".elm/framework/kernel-api",
]

[workspace.package]
version = "0.1.0"
edition = "2024"

[package]
name = "{name}"
version.workspace = true
edition.workspace = true

[[bin]]
name = "{name}"
path = "src/main.rs"
test = false
bench = false

[dependencies]
elm = {{ path = ".elm/framework/elm", default-features = false, features = [{features}] }}
kernel-api = {{ path = ".elm/framework/kernel-api", default-features = false, features = ["module"] }}

[profile.release]
panic = "abort"
codegen-units = 1
lto = false
strip = false
"#
    )
}

fn elm_toml(name: &str, kind: &str, source: &str) -> String {
    format!(
        r#"[elm]
name = "{name}"
version = "0.1.0"
kind = "{kind}"
source = "{source}"
"#
    )
}

fn main_rs(name: &str) -> String {
    format!(
        r#"#![no_std]
#![no_main]

extern crate alloc;

use alloc::{{boxed::Box, string::String, sync::Arc, vec::Vec}};
use elm::{{HookResult, LifecycleContext}};

kernel_api::elm_global_allocator!();

#[elm::on_initialize]
fn initialize(_context: &LifecycleContext) -> HookResult {{
    let mut values = Vec::new();
    values.extend_from_slice(&[1_u32, 2, 3]);
    let boxed = Box::new(values.iter().copied().sum::<u32>());
    let shared = Arc::new(String::from("{name}: initialized"));
    core::hint::black_box((&values, &boxed, &shared));
    if *boxed != 6 || Arc::strong_count(&shared) != 1 {{
        return Err(elm::HookError::new(-1));
    }}
    elm::runtime::log(6, shared.as_str())
        .map_err(|_| elm::HookError::new(-1))?;
    Ok(())
}}

#[elm::on_finalize]
fn finalize(_context: &LifecycleContext) -> HookResult {{
    elm::runtime::log(6, "{name}: finalized")
        .map_err(|_| elm::HookError::new(-1))?;
    Ok(())
}}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {{
    elm::runtime::abort_panic()
}}
"#
    )
}

fn elm_features(kind: &str) -> &'static str {
    if kind == "manager" {
        "\"module\", \"macros\", \"management\""
    } else {
        "\"module\", \"macros\""
    }
}

fn migrate_cargo_manifest(path: &Path, kind: &str) -> Result<(), String> {
    let input =
        fs::read_to_string(path).map_err(|err| format!("读取 {} 失败: {err}", path.display()))?;
    let mut output = input
        .replace("    \".elm/framework/elmmgr\",\n", "")
        .replace("elmmgr = { path = \".elm/framework/elmmgr\" }\n", "");
    if output.contains("elmmgr") {
        return Err(format!(
            "{} 仍包含定制化 elmmgr 依赖或路径；ELM v1 只允许 elm::runtime 和 elm::management，请手动移除后重试",
            path.display()
        ));
    }

    let standard_module = "elm = { path = \".elm/framework/elm\", default-features = false, features = [\"module\", \"macros\"] }";
    let standard_manager = "elm = { path = \".elm/framework/elm\", default-features = false, features = [\"module\", \"macros\", \"management\"] }";
    let desired = if kind == "manager" {
        standard_manager
    } else {
        standard_module
    };
    if output.contains(standard_module) {
        output = output.replace(standard_module, desired);
    } else if output.contains(standard_manager) {
        output = output.replace(standard_manager, desired);
    } else if !output
        .lines()
        .any(|line| line.trim_start().starts_with("elm ="))
    {
        return Err(format!("{} 缺少 elm 框架依赖", path.display()));
    } else if kind == "manager" && !output.contains("\"management\"") {
        return Err(format!(
            "{} 使用定制化 elm 依赖，但 Manager 工程未启用 management feature",
            path.display()
        ));
    }

    if !output.contains(".elm/framework/kernel-api") {
        output = output.replace(
            "    \".elm/framework/elm/macros\",\n",
            "    \".elm/framework/elm/macros\",\n    \".elm/framework/kernel-api\",\n",
        );
        let dependency = "kernel-api = { path = \".elm/framework/kernel-api\", default-features = false, features = [\"module\"] }";
        if output.contains(desired) {
            output = output.replace(desired, format!("{desired}\n{dependency}").as_str());
        } else {
            let dependencies = "[dependencies]\n";
            if output.contains(dependencies) {
                output = output.replace(
                    dependencies,
                    format!("{dependencies}{dependency}\n").as_str(),
                );
            } else {
                return Err(format!("{} 缺少 [dependencies]", path.display()));
            }
        }
    }

    if output != input {
        fs::write(path, output).map_err(|err| format!("迁移 {} 失败: {err}", path.display()))?;
    }
    Ok(())
}

fn strip_comment(line: &str) -> Result<&str, String> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return Ok(&line[..index]),
            _ => {}
        }
    }
    if quoted || escaped {
        Err("Elm.toml 包含未闭合字符串".to_string())
    } else {
        Ok(line)
    }
}

fn parse_basic_string(raw: &str, line: usize) -> Result<String, String> {
    if !raw.starts_with('"') || !raw.ends_with('"') || raw.len() < 2 {
        return Err(format!("Elm.toml 第 {line} 行值必须是双引号基本字符串"));
    }
    let mut output = String::new();
    let mut chars = raw[1..raw.len() - 1].chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| format!("Elm.toml 第 {line} 行转义不完整"))?;
        output.push(match escaped {
            '"' => '"',
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            _ => return Err(format!("Elm.toml 第 {line} 行包含不支持的转义")),
        });
    }
    if output.as_bytes().contains(&0) {
        return Err(format!("Elm.toml 第 {line} 行字符串包含 NUL"));
    }
    Ok(output)
}

fn take_required(
    values: &BTreeMap<String, String>,
    key: &str,
    section: &str,
) -> Result<String, String> {
    values
        .get(key)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{section} 缺少非空字段 {key}"))
}

fn reject_unknown_keys(
    values: &BTreeMap<String, String>,
    allowed: &[&str],
    section: &str,
) -> Result<(), String> {
    if let Some(key) = values.keys().find(|key| !allowed.contains(&key.as_str())) {
        Err(format!("{section} 包含未知字段 {key}"))
    } else {
        Ok(())
    }
}

fn validate_identifier(value: &str, max_len: usize, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max_len
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(format!("{label} 不是有效 identifier: {value}"));
    }
    Ok(())
}

fn validate_contract(value: &str) -> Result<(), String> {
    let Some((name, version)) = value.rsplit_once('@') else {
        return Err(format!("契约缺少 @version: {value}"));
    };
    validate_identifier(name, 63, "契约名称")?;
    if value.len() > 64
        || version.is_empty()
        || !version.split('.').all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(format!("契约无效: {value}"));
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return Err(format!("ELM 版本无效: {value}"));
    }
    Ok(())
}

fn validate_source(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':' | b'@')
        })
    {
        return Err(format!("来源 identifier 无效: {value}"));
    }
    Ok(())
}

const ELM_CARGO_CONFIG: &str = r#"[target.riscv64gc-unknown-none-elf]
linker = "rust-lld"
rustflags = [
    "-C", "link-arg=-Telm.ld",
    "-C", "relocation-model=pic",
    "-C", "code-model=small",
    "-C", "link-arg=-pie",
    "-C", "link-arg=-z",
    "-C", "link-arg=notext",
    "-C", "link-arg=--gc-sections",
    "-C", "link-arg=--build-id=none",
]

[target.loongarch64-unknown-none]
linker = "rust-lld"
rustflags = [
    "-C", "link-arg=-Telm.ld",
    "-C", "relocation-model=pic",
    "-C", "code-model=small",
    "-C", "link-arg=-pie",
    "-C", "link-arg=-z",
    "-C", "link-arg=notext",
    "-C", "link-arg=--gc-sections",
    "-C", "link-arg=--build-id=none",
    "-A", "named_asm_labels",
]
"#;

const ELM_LINKER_SCRIPT: &str = r#"ENTRY(on_initialize)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0;
    .text : ALIGN(4096)
    {
        KEEP(*(.text.elm.abi))
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata :
    {
        *(.rodata .rodata.* .srodata .srodata.*)
        *(.eh_frame .eh_frame_hdr)
    } :rodata

    .rela.dyn : ALIGN(8)
    {
        KEEP(*(.rela.dyn))
    } :rodata

    . = ALIGN(4096);
    .data :
    {
        *(.data .data.* .sdata .sdata.*)
        *(.got .got.*)
    } :data

    .bss (NOLOAD) :
    {
        *(.bss .bss.* .sbss .sbss.*)
        *(COMMON)
    } :data

    .elm.meta 0 (INFO) :
    {
        KEEP(*(.elm.meta))
    }

    /DISCARD/ :
    {
        *(.comment)
        *(.note.gnu.build-id)
        *(.gnu_debuglink)
        *(.interp)
        *(.dynamic)
        *(.dynsym)
        *(.dynstr)
        *(.hash)
        *(.gnu.hash)
    }
}
"#;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "elm-tools-{name}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_complete_manifest() {
        let manifest = ElmProjectManifest::parse(
            r#"
[elm]
name = "demo.echo"
version = "1.2.3"
kind = "service"
source = "local.demo"

[menu]
label = "Echo"
description = "test"
route = "demo.echo"

[[dependencies]]
provider = "demo.base"
contract = "demo.echo@1"
"#,
        )
        .unwrap();
        assert_eq!(manifest.name, "demo.echo");
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.menu.unwrap().route, "demo.echo");
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let error = ElmProjectManifest::parse(
            r#"
[elm]
name = "demo"
version = "1"
kind = "service"
source = "local"
uri = "forbidden"
"#,
        )
        .unwrap_err();
        assert!(error.contains("未知字段 uri"));
    }

    #[test]
    fn scaffolds_single_framework_for_service_and_manager_projects() {
        let service = TestDirectory::new("service-project");
        scaffold_project(service.path(), "demo.service", "service", "local.test").unwrap();
        let service_cargo = fs::read_to_string(service.path().join("Cargo.toml")).unwrap();
        let service_source = fs::read_to_string(service.path().join("src/main.rs")).unwrap();
        assert!(service_cargo.contains("features = [\"module\", \"macros\"]"));
        assert!(service_cargo.contains(".elm/framework/kernel-api"));
        assert!(service_cargo.contains("kernel-api ="));
        assert!(!service_cargo.contains("management"));
        assert!(!service_cargo.contains("elmmgr"));
        assert!(service_source.contains("kernel_api::elm_global_allocator!()"));
        assert!(service_source.contains("extern crate alloc"));
        assert!(service_source.contains("Vec::new()"));
        assert!(service_source.contains("Box::new"));
        assert!(service_source.contains("Arc::new"));
        assert!(service_source.contains("core::hint::black_box"));
        assert!(service_source.contains("elm::runtime::log"));
        assert!(service_source.contains("elm::runtime::abort_panic"));
        assert!(
            service
                .path()
                .join(".elm/framework/elm/Cargo.toml")
                .is_file()
        );
        assert!(
            service
                .path()
                .join(".elm/framework/kernel-api/Cargo.toml")
                .is_file()
        );
        assert!(!service.path().join(".elm/framework/elmmgr").exists());

        let manager = TestDirectory::new("manager-project");
        scaffold_project(manager.path(), "demo.manager", "manager", "local.test").unwrap();
        let manager_cargo = fs::read_to_string(manager.path().join("Cargo.toml")).unwrap();
        assert!(manager_cargo.contains("features = [\"module\", \"macros\", \"management\"]"));
        assert!(!manager_cargo.contains("elmmgr"));
        assert!(!manager.path().join(".elm/framework/elmmgr").exists());
    }

    #[test]
    fn migrates_only_the_retired_standard_elmmgr_layout() {
        let directory = TestDirectory::new("legacy-migration");
        let manifest = directory.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"[workspace]
members = [
    ".",
    ".elm/framework/elm",
    ".elm/framework/elm/macros",
    ".elm/framework/elmmgr",
]

[dependencies]
elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros"] }
elmmgr = { path = ".elm/framework/elmmgr" }
"#,
        )
        .unwrap();
        migrate_cargo_manifest(&manifest, "manager").unwrap();
        let migrated = fs::read_to_string(&manifest).unwrap();
        assert!(!migrated.contains("elmmgr"));
        assert!(migrated.contains("features = [\"module\", \"macros\", \"management\"]"));
        assert!(migrated.contains(".elm/framework/kernel-api"));
        assert!(migrated.contains("kernel-api ="));

        fs::write(
            &manifest,
            r#"[dependencies]
elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros"] }
elmmgr = { path = "custom/elmmgr" }
"#,
        )
        .unwrap();
        let error = migrate_cargo_manifest(&manifest, "service").unwrap_err();
        assert!(error.contains("定制化 elmmgr"));
        assert!(
            fs::read_to_string(&manifest)
                .unwrap()
                .contains("custom/elmmgr")
        );
    }

    #[test]
    fn migration_enforces_management_feature_by_project_kind() {
        let directory = TestDirectory::new("feature-migration");
        let manifest = directory.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"[dependencies]
elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros", "management"] }
"#,
        )
        .unwrap();
        migrate_cargo_manifest(&manifest, "service").unwrap();
        let service = fs::read_to_string(&manifest).unwrap();
        assert!(service.contains("features = [\"module\", \"macros\"]"));
        assert!(!service.contains("management"));

        fs::write(
            &manifest,
            r#"[dependencies]
elm = { path = "custom/elm", default-features = false, features = ["module", "macros"] }
"#,
        )
        .unwrap();
        let error = migrate_cargo_manifest(&manifest, "manager").unwrap_err();
        assert!(error.contains("Manager 工程未启用 management feature"));
    }
}
