use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use elm::Sha256;

use crate::kernel_interface::{
    KernelInterfaceManifest, LSP_SOURCE_IDENTITY_FILE, LSP_SOURCE_MAGIC, hex_digest,
    metadata_facade_manifest, metadata_facade_source, packaged_framework_hash,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmProjectManifest {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub source: String,
    pub mode: ElmBuildMode,
    pub api: Option<ElmProjectApi>,
    pub menu: Option<ElmProjectMenu>,
    pub dependencies: Vec<ElmProjectDependency>,
    pub profiles: Vec<ElmProjectProfile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmBuildMode {
    Integrated,
    Managed,
    Disabled,
}

impl ElmBuildMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Integrated => "y",
            Self::Managed => "m",
            Self::Disabled => "n",
        }
    }
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
    pub crate_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmProjectApi {
    pub crate_name: String,
    pub path: String,
    pub contract: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmProjectProfile {
    pub id: String,
    pub priority: u32,
}

#[derive(Debug, Clone)]
pub struct KernelInterfaceBundle {
    pub root: PathBuf,
    pub manifest: KernelInterfaceManifest,
    pub priority: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Elm,
    Api,
    Menu,
    Dependency(usize),
    Profile(usize),
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
        let mut api = BTreeMap::new();
        let mut menu = BTreeMap::new();
        let mut dependencies: Vec<BTreeMap<String, String>> = Vec::new();
        let mut profiles: Vec<BTreeMap<String, String>> = Vec::new();
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
            if line == "[api]" {
                section = Some(Section::Api);
                continue;
            }
            if line == "[[dependencies]]" {
                dependencies.push(BTreeMap::new());
                section = Some(Section::Dependency(dependencies.len() - 1));
                continue;
            }
            if line == "[[profiles]]" {
                profiles.push(BTreeMap::new());
                section = Some(Section::Profile(profiles.len() - 1));
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
                Some(Section::Api) => &mut api,
                Some(Section::Menu) => &mut menu,
                Some(Section::Dependency(index)) => &mut dependencies[index],
                Some(Section::Profile(index)) => &mut profiles[index],
                None => return Err(format!("Elm.toml 第 {line_number} 行位于 section 之外")),
            };
            if target.insert(key.to_string(), value).is_some() {
                return Err(format!("Elm.toml 第 {line_number} 行重复定义 {key}"));
            }
        }

        reject_unknown_keys(
            &elm,
            &["name", "version", "kind", "source", "mode"],
            "[elm]",
        )?;
        reject_unknown_keys(&menu, &["label", "description", "route"], "[menu]")?;
        reject_unknown_keys(&api, &["crate", "path", "contract", "version"], "[api]")?;
        let name = take_required(&elm, "name", "[elm]")?;
        let version = take_required(&elm, "version", "[elm]")?;
        let kind = take_required(&elm, "kind", "[elm]")?;
        let source = take_required(&elm, "source", "[elm]")?;
        let mode = match elm.get("mode").map(String::as_str).unwrap_or("m") {
            "y" => ElmBuildMode::Integrated,
            "m" => ElmBuildMode::Managed,
            "n" => ElmBuildMode::Disabled,
            mode => return Err(format!("未知 ELM 构建模式: {mode}")),
        };
        let mode = match std::env::var("ELM_BUILD_MODE_OVERRIDE").ok().as_deref() {
            Some("y") => ElmBuildMode::Integrated,
            Some("m") => ElmBuildMode::Managed,
            Some("n") => ElmBuildMode::Disabled,
            Some(mode) => return Err(format!("未知 ELM 构建模式覆盖值: {mode}")),
            None => mode,
        };
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

        let api = if api.is_empty() {
            None
        } else {
            let crate_name = take_required(&api, "crate", "[api]")?;
            let path = take_required(&api, "path", "[api]")?;
            let contract = take_required(&api, "contract", "[api]")?;
            let version = take_required(&api, "version", "[api]")?
                .parse::<u32>()
                .map_err(|_| "[api] version 不是 u32".to_string())?;
            validate_crate_name(&crate_name, "API crate 名称")?;
            validate_relative_path(&path, "API crate 路径")?;
            validate_contract(&contract)?;
            if version == 0 {
                return Err("[api] version 必须大于 0".to_string());
            }
            Some(ElmProjectApi {
                crate_name,
                path,
                contract,
                version,
            })
        };

        let mut parsed_dependencies = Vec::new();
        for (index, dependency) in dependencies.iter().enumerate() {
            reject_unknown_keys(
                dependency,
                &["provider", "contract", "crate"],
                &format!("[[dependencies]] #{}", index + 1),
            )?;
            let provider = take_required(dependency, "provider", "[[dependencies]]")?;
            let contract = take_required(dependency, "contract", "[[dependencies]]")?;
            let crate_name = dependency.get("crate").cloned();
            validate_identifier(&provider, 128, "依赖 provider 名称")?;
            validate_contract(&contract)?;
            if let Some(crate_name) = &crate_name {
                validate_crate_name(crate_name, "依赖 API crate 名称")?;
            }
            if parsed_dependencies
                .iter()
                .any(|item: &ElmProjectDependency| {
                    item.provider == provider && item.contract == contract
                })
            {
                return Err(format!("重复依赖: {provider} {contract}"));
            }
            parsed_dependencies.push(ElmProjectDependency {
                provider,
                contract,
                crate_name,
            });
        }
        let mut parsed_profiles = Vec::new();
        for (index, profile) in profiles.iter().enumerate() {
            reject_unknown_keys(
                profile,
                &["id", "priority"],
                &format!("[[profiles]] #{}", index + 1),
            )?;
            let id = take_required(profile, "id", "[[profiles]]")?;
            validate_identifier(&id, 64, "内核 API Profile")?;
            let priority = profile
                .get("priority")
                .map(String::as_str)
                .unwrap_or("0")
                .parse::<u32>()
                .map_err(|_| format!("Profile {id} 的 priority 不是 u32"))?;
            if parsed_profiles
                .iter()
                .any(|item: &ElmProjectProfile| item.id == id)
            {
                return Err(format!("重复内核 API Profile: {id}"));
            }
            parsed_profiles.push(ElmProjectProfile { id, priority });
        }
        Ok(Self {
            name,
            version,
            kind,
            source,
            mode,
            api,
            menu,
            dependencies: parsed_dependencies,
            profiles: parsed_profiles,
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
    write_new(&directory.join("src/main.rs"), &module_rs(name))?;
    write_new(&directory.join("elm.ld"), ELM_LINKER_SCRIPT)?;
    write_new(&directory.join("rust-toolchain.toml"), ELM_RUST_TOOLCHAIN)?;
    write_new(
        &directory.join(".cargo/config.toml"),
        &elm_cargo_config(None, &[], &[]),
    )?;
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
    migrate_cargo_manifest(&manifest, &project_manifest)?;
    let elm_root = project.join(".elm");
    fs::create_dir_all(&elm_root)
        .map_err(|err| format!("创建 {} 失败: {err}", elm_root.display()))?;
    let destination = elm_root.join("framework");
    let temporary = elm_root.join(format!("framework.tmp.{}", std::process::id()));
    let backup = elm_root.join(format!("framework.old.{}", std::process::id()));
    remove_if_exists(&temporary)?;
    remove_if_exists(&backup)?;
    sync_dependency_apis(project, &project_manifest)?;
    fs::create_dir_all(&temporary)
        .map_err(|err| format!("创建 {} 失败: {err}", temporary.display()))?;
    if let Some(packaged) = packaged_framework_root(&project_manifest)? {
        copy_tree(&packaged, &temporary)?;
    } else {
        let source = framework_source_root()?;
        let elm_source = source.join("libs/elm");
        let kernel_symbols_source = source.join("libs/kernel-symbols");
        if !elm_source.join("Cargo.toml").is_file() {
            return Err(format!("找不到框架源目录: {}", elm_source.display()));
        }
        if !kernel_symbols_source.join("Cargo.toml").is_file() {
            return Err(format!(
                "找不到内核符号契约源目录: {}",
                kernel_symbols_source.display()
            ));
        }
        copy_tree(&elm_source, &temporary.join("elm"))?;
        copy_tree(&kernel_symbols_source, &temporary.join("kernel-symbols"))?;
        write_metadata_facade(
            &temporary.join("allocator"),
            "allocator",
            "__elm_host_allocator",
        )?;
        write_metadata_facade(&temporary.join("general"), "general", "__elm_host_general")?;
        write_metadata_facade(&temporary.join("log"), "log", "__elm_host_log")?;
        write_metadata_facade(&temporary.join("sched"), "sched", "__elm_host_sched")?;
        fs::write(
            temporary.join("Cargo.toml"),
            crate::kernel_interface::framework_workspace_manifest(),
        )
        .map_err(|err| format!("写入框架 workspace manifest 失败: {err}"))?;
    }
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
    fs::write(project.join("elm.ld"), ELM_LINKER_SCRIPT)
        .map_err(|err| format!("更新 ELM linker script 失败: {err}"))?;
    fs::write(project.join("rust-toolchain.toml"), ELM_RUST_TOOLCHAIN)
        .map_err(|err| format!("更新 ELM Rust 工具链声明失败: {err}"))?;
    fs::create_dir_all(project.join(".cargo"))
        .map_err(|err| format!("创建 ELM Cargo 配置目录失败: {err}"))?;
    sync_available_target_interfaces(project, &project_manifest)?;
    let active_interface = ["riscv64gc-unknown-none-elf", "loongarch64-unknown-none"]
        .into_iter()
        .find_map(|target| {
            KernelInterfaceManifest::load(
                &project
                    .join(".elm/kernel-interface")
                    .join(target)
                    .join("manifest.txt"),
            )
            .ok()
        });
    let (api_profiles, profile_hashes) = active_interface
        .as_ref()
        .and_then(|interface| kernel_profile_cfg_values(&project_manifest, &interface.target).ok())
        .unwrap_or_default();
    fs::write(
        project.join(".cargo/config.toml"),
        elm_cargo_config(active_interface.as_ref(), &api_profiles, &profile_hashes),
    )
    .map_err(|err| format!("更新 ELM Cargo 配置失败: {err}"))?;
    write_elm_lock(project, &project_manifest)?;
    Ok(())
}

const ELM_API_SNAPSHOT_MAGIC: &str = "ELM-API-SNAPSHOT-V1";

pub fn publish_project_api(project: &Path, repository: &Path) -> Result<Option<PathBuf>, String> {
    let manifest = ElmProjectManifest::load(project)?;
    let Some(api) = manifest.api.as_ref() else {
        return Ok(None);
    };
    let source = project.join(&api.path);
    if !source.join("Cargo.toml").is_file() || !source.join("src").is_dir() {
        return Err(format!(
            "ELM {} 的 API crate 不完整: {}",
            manifest.name,
            source.display()
        ));
    }
    let destination = repository.join(&manifest.name).join(&api.crate_name);
    let temporary = repository.join(&manifest.name).join(format!(
        ".{}.tmp.{}",
        api.crate_name,
        std::process::id()
    ));
    remove_if_exists(&temporary)?;
    copy_api_tree(&source, &temporary)?;
    normalize_published_api_manifest(&temporary)?;
    let digest = api_tree_digest(&temporary)?;
    let snapshot = format!(
        "{ELM_API_SNAPSHOT_MAGIC}\nprovider={}\ncontract={}\nversion={}\ncrate={}\nsha256={}\n",
        manifest.name,
        api.contract,
        api.version,
        api.crate_name,
        hex_digest(&digest)
    );
    fs::write(temporary.join("elm-api.txt"), snapshot)
        .map_err(|err| format!("写入 ELM API 快照清单失败: {err}"))?;
    remove_if_exists(&destination)?;
    fs::rename(&temporary, &destination)
        .map_err(|err| format!("安装 ELM API 快照 {} 失败: {err}", destination.display()))?;
    Ok(Some(destination))
}

fn sync_dependency_apis(project: &Path, manifest: &ElmProjectManifest) -> Result<(), String> {
    let dependencies = manifest
        .dependencies
        .iter()
        .filter(|dependency| dependency.crate_name.is_some())
        .collect::<Vec<_>>();
    let destination_root = project.join(".elm/dependencies");
    if dependencies.is_empty() {
        remove_if_exists(&destination_root)?;
        return Ok(());
    }
    let repository = dependency_api_root()?;
    let temporary = project
        .join(".elm")
        .join(format!("dependencies.tmp.{}", std::process::id()));
    remove_if_exists(&temporary)?;
    fs::create_dir_all(&temporary).map_err(|err| format!("创建依赖 API 临时目录失败: {err}"))?;
    let result = (|| {
        let mut crate_names = BTreeSet::new();
        for dependency in dependencies {
            let crate_name = dependency
                .crate_name
                .as_deref()
                .expect("已过滤无 API crate 的依赖");
            if !crate_names.insert(crate_name) {
                return Err(format!("重复依赖 API crate 名称: {crate_name}"));
            }
            let source = repository.join(&dependency.provider).join(crate_name);
            validate_api_snapshot(&source, dependency, crate_name)?;
            copy_api_tree(&source, &temporary.join(crate_name))?;
        }
        Ok(())
    })();
    if let Err(err) = result {
        let _ = remove_if_exists(&temporary);
        return Err(err);
    }
    remove_if_exists(&destination_root)?;
    fs::rename(&temporary, &destination_root).map_err(|err| format!("安装依赖 API 快照失败: {err}"))
}

fn dependency_api_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("ELM_DEPENDENCY_API_ROOT") {
        return Ok(PathBuf::from(root));
    }
    if let Some(home) = std::env::var_os("ELM_HOME") {
        return Ok(PathBuf::from(home).join("apis"));
    }
    Err("无法定位 ELM API 仓库；请设置 ELM_DEPENDENCY_API_ROOT".to_string())
}

fn validate_api_snapshot(
    source: &Path,
    dependency: &ElmProjectDependency,
    crate_name: &str,
) -> Result<(), String> {
    let path = source.join("elm-api.txt");
    let input = fs::read_to_string(&path)
        .map_err(|err| format!("读取依赖 API 快照 {} 失败: {err}", path.display()))?;
    let mut lines = input.lines();
    if lines.next() != Some(ELM_API_SNAPSHOT_MAGIC) {
        return Err(format!("{} 不是 ELM API 快照", path.display()));
    }
    let mut values = BTreeMap::new();
    for line in lines {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("{} 包含无效记录", path.display()))?;
        if values.insert(key, value).is_some() {
            return Err(format!("{} 重复定义 {key}", path.display()));
        }
    }
    if values.get("provider") != Some(&dependency.provider.as_str())
        || values.get("contract") != Some(&dependency.contract.as_str())
        || values.get("crate") != Some(&crate_name)
    {
        return Err(format!("依赖 API 快照身份不匹配: {}", source.display()));
    }
    let expected = values
        .get("sha256")
        .ok_or_else(|| format!("{} 缺少 sha256", path.display()))?;
    let actual = hex_digest(&api_tree_digest(source)?);
    if *expected != actual {
        return Err(format!("依赖 API 快照摘要不匹配: {}", source.display()));
    }
    Ok(())
}

fn copy_api_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|err| format!("创建 {} 失败: {err}", destination.display()))?;
    let mut entries = fs::read_dir(source)
        .map_err(|err| format!("读取 {} 失败: {err}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取 API 目录项失败: {err}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("target" | ".git" | ".elm" | "elm-api.txt")
        ) {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(name);
        if source_path.is_dir() {
            copy_api_tree(&source_path, &destination_path)?;
        } else if source_path.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|err| {
                format!(
                    "复制 API 文件 {} -> {} 失败: {err}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn normalize_published_api_manifest(root: &Path) -> Result<(), String> {
    let path = root.join("Cargo.toml");
    let input =
        fs::read_to_string(&path).map_err(|err| format!("读取 API Cargo.toml 失败: {err}"))?;
    let output = input.replace("../.elm/framework/", "../../framework/");
    if output == input && input.contains(".elm/framework/") {
        return Err("API Cargo.toml 的框架路径必须以 ../.elm/framework/ 开头".to_string());
    }
    fs::write(&path, output).map_err(|err| format!("规范化 API Cargo.toml 失败: {err}"))
}

fn api_tree_digest(root: &Path) -> Result<[u8; 32], String> {
    fn collect(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut entries = fs::read_dir(current)
            .map_err(|err| format!("读取 {} 失败: {err}", current.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("读取 API 摘要目录项失败: {err}"))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() == "elm-api.txt" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, files)?;
            } else if path.is_file() {
                files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"ELM-API-SNAPSHOT-V1\0");
    for relative in files {
        let path = root.join(&relative);
        let bytes = fs::read(&path)
            .map_err(|err| format!("读取 API 文件 {} 失败: {err}", path.display()))?;
        let name = relative.to_string_lossy();
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finish())
}

fn write_elm_lock(project: &Path, manifest: &ElmProjectManifest) -> Result<(), String> {
    let rustc = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--version")
        .output()
        .map_err(|err| format!("读取 rustc 版本失败: {err}"))?;
    if !rustc.status.success() {
        return Err("rustc --version 执行失败".to_string());
    }
    let rustc = String::from_utf8(rustc.stdout)
        .map_err(|_| "rustc 版本不是 UTF-8".to_string())?
        .trim()
        .to_string();
    let mut interfaces = Vec::new();
    for target in ["riscv64gc-unknown-none-elf", "loongarch64-unknown-none"] {
        let Ok(available) = available_kernel_interfaces(target) else {
            continue;
        };
        for bundle in available {
            if manifest.profiles.is_empty()
                || manifest
                    .profiles
                    .iter()
                    .any(|profile| profile.id == bundle.manifest.profile)
            {
                interfaces.push(bundle.manifest);
            }
        }
    }
    interfaces.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then_with(|| left.profile.cmp(&right.profile))
            .then_with(|| left.interface_hash.cmp(&right.interface_hash))
            .then_with(|| left.kernel_hash.cmp(&right.kernel_hash))
    });
    let mut seen = BTreeSet::new();
    interfaces
        .retain(|interface| seen.insert((interface.target.clone(), interface.interface_hash)));
    let mut output = String::from("ELM-LOCK-V1\n");
    output.push_str(&format!("module={}\n", manifest.name));
    output.push_str(&format!("version={}\n", manifest.version));
    output.push_str(&format!("mode={}\n", manifest.mode.as_str()));
    output.push_str(&format!("cargo_elm={}\n", env!("CARGO_PKG_VERSION")));
    output.push_str(&format!("rustc={}\n", rustc));
    for interface in interfaces {
        output.push_str(&format!(
            "profile\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            interface.target,
            interface.profile,
            interface.bridge_abi_version,
            hex_digest(&interface.interface_hash),
            hex_digest(&interface.source_hash),
            hex_digest(&interface.framework_hash),
            hex_digest(&interface.kernel_hash),
        ));
    }
    let temporary = project.join(format!("Elm.lock.tmp.{}", std::process::id()));
    fs::write(&temporary, output)
        .map_err(|err| format!("写入 {} 失败: {err}", temporary.display()))?;
    fs::rename(&temporary, project.join("Elm.lock"))
        .map_err(|err| format!("安装 Elm.lock 失败: {err}"))
}

pub fn cargo_build(project: &Path, target: &str, cargo_name: &str) -> Result<PathBuf, String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    prepare_target_interface(&project, target)?;
    let project_manifest = ElmProjectManifest::load(&project)?;
    write_elm_lock(&project, &project_manifest)?;
    let interface_root = project.join(".elm/kernel-interface").join(target);
    let manifest = interface_root.join("manifest.txt");
    let interface = KernelInterfaceManifest::load(&manifest)?;
    let import_library = interface_root.join(&interface.import_library);
    let support_library = interface_root.join(&interface.support_library);
    if !support_library.is_file() {
        return Err(format!(
            "目标接口包缺少 Rust 支持归档: {}",
            support_library.display()
        ));
    }
    if !import_library.is_file() {
        return Err(format!(
            "目标接口包缺少内核导入库: {}",
            import_library.display()
        ));
    }
    let mut rustflags = vec![
        "-Clink-arg=-Telm.ld".to_string(),
        "-Crelocation-model=pic".to_string(),
        "-Ccode-model=small".to_string(),
        "-Clink-arg=-pie".to_string(),
        "-Clink-arg=-z".to_string(),
        "-Clink-arg=notext".to_string(),
        "-Clink-arg=--gc-sections".to_string(),
        "-Clink-arg=--build-id=none".to_string(),
        format!("-Clink-arg={}", support_library.display()),
        "-Clink-arg=--no-as-needed".to_string(),
        format!("-Clink-arg={}", import_library.display()),
        "-Zplt=yes".to_string(),
        "-Zshare-generics=no".to_string(),
    ];
    let metadata = interface_root.join("metadata");
    rustflags.push(format!("-Ldependency={}", metadata.display()));
    rustflags.push(format!(
        "--extern=__elm_host_allocator={}",
        metadata.join(&interface.allocator_metadata).display()
    ));
    rustflags.push(format!(
        "--extern=__elm_host_general={}",
        metadata.join(&interface.general_metadata).display()
    ));
    rustflags.push(format!(
        "--extern=__elm_host_log={}",
        metadata.join(&interface.log_metadata).display()
    ));
    rustflags.push(format!(
        "--extern=__elm_host_sched={}",
        metadata.join(&interface.sched_metadata).display()
    ));
    let (api_profiles, profile_hashes) = kernel_profile_cfg_values(&project_manifest, target)?;
    append_kernel_profile_flags(&mut rustflags, &interface, &api_profiles, &profile_hashes);
    if target == "loongarch64-unknown-none" {
        rustflags.push("-Anamed_asm_labels".to_string());
    }
    let mut command = Command::new("cargo");
    command
        .current_dir(&project)
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"))
        .arg("build")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .arg("--bin")
        .arg(cargo_name)
        .arg("--no-default-features");
    append_extra_features(&mut command, &[]);
    let status = command
        .arg("--target")
        .arg(target)
        .arg("--release")
        .status()
        .map_err(|err| format!("启动 cargo build 失败: {err}"))?;
    if !status.success() {
        return Err(format!("ELM Rust 构建失败，退出状态 {status}"));
    }
    Ok(cargo_target_directory(&project)
        .join(target)
        .join("release")
        .join(cargo_name))
}

pub fn cargo_build_integrated(
    project: &Path,
    target: &str,
    cargo_name: &str,
) -> Result<PathBuf, String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    prepare_target_interface(&project, target)?;
    let project_manifest = ElmProjectManifest::load(&project)?;
    write_elm_lock(&project, &project_manifest)?;
    let interface_root = project.join(".elm/kernel-interface").join(target);
    let interface = KernelInterfaceManifest::load(&interface_root.join("manifest.txt"))?;
    let metadata = interface_root.join("metadata");
    let mut rustflags = vec![
        format!("-Ldependency={}", metadata.display()),
        format!(
            "--extern=__elm_host_allocator={}",
            metadata.join(&interface.allocator_metadata).display()
        ),
        format!(
            "--extern=__elm_host_general={}",
            metadata.join(&interface.general_metadata).display()
        ),
        format!(
            "--extern=__elm_host_log={}",
            metadata.join(&interface.log_metadata).display()
        ),
        format!(
            "--extern=__elm_host_sched={}",
            metadata.join(&interface.sched_metadata).display()
        ),
    ];
    let (api_profiles, profile_hashes) = kernel_profile_cfg_values(&project_manifest, target)?;
    append_kernel_profile_flags(&mut rustflags, &interface, &api_profiles, &profile_hashes);
    let mut command = Command::new("cargo");
    command
        .current_dir(&project)
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"))
        .env("ELM_KERNEL_PROFILE_ID", &interface.profile)
        .env(
            "ELM_KERNEL_PROFILE_HASH",
            hex_digest(&interface.interface_hash),
        )
        .arg("rustc")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .arg("--lib")
        .arg("--no-default-features");
    append_extra_features(&mut command, &["elm-integrated"]);
    let status = command
        .arg("--target")
        .arg(target)
        .arg("--release")
        .arg("--")
        .arg("--emit=link")
        .status()
        .map_err(|err| format!("启动集成组件 cargo rustc 失败: {err}"))?;
    if !status.success() {
        return Err(format!("集成组件 Rust 构建失败，退出状态 {status}"));
    }

    let crate_name = cargo_name.replace('-', "_");
    let target_directory = cargo_target_directory(&project);
    let rlib = target_directory
        .join(target)
        .join("release")
        .join(format!("lib{crate_name}.rlib"));
    if !rlib.is_file() {
        return Err(format!("集成组件构建没有生成 {}", rlib.display()));
    }
    let temporary = target_directory
        .join("elm/integrated")
        .join(format!("{target}.tmp.{}", std::process::id()));
    remove_if_exists(&temporary)?;
    fs::create_dir_all(&temporary)
        .map_err(|err| format!("创建 {} 失败: {err}", temporary.display()))?;
    let mut archives = vec![rlib];
    let mut local_api_crates = project_manifest
        .dependencies
        .iter()
        .filter_map(|dependency| dependency.crate_name.clone())
        .collect::<Vec<_>>();
    if let Some(api) = &project_manifest.api {
        local_api_crates.push(api.crate_name.clone());
    }
    local_api_crates.sort();
    local_api_crates.dedup();
    let deps = target_directory.join(target).join("release/deps");
    for crate_name in local_api_crates {
        archives.push(newest_rlib_for_crate(&deps, &crate_name)?);
    }
    let mut objects = Vec::new();
    for (index, archive) in archives.iter().enumerate() {
        let extract_dir = temporary.join(format!("archive-{index:04}"));
        fs::create_dir_all(&extract_dir)
            .map_err(|err| format!("创建 {} 失败: {err}", extract_dir.display()))?;
        let extract = Command::new("llvm-ar")
            .current_dir(&extract_dir)
            .arg("x")
            .arg(archive)
            .output()
            .map_err(|err| format!("解包 {} 失败: {err}", archive.display()))?;
        if !extract.status.success() {
            return Err(format!(
                "llvm-ar 无法解包集成组件 {}: {}",
                archive.display(),
                String::from_utf8_lossy(&extract.stderr)
            ));
        }
        let mut extracted = fs::read_dir(&extract_dir)
            .map_err(|err| format!("读取 {} 失败: {err}", extract_dir.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("读取集成组件对象失败: {err}"))?
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "o"))
            .collect::<Vec<_>>();
        objects.append(&mut extracted);
    }
    objects.sort();
    if objects.is_empty() {
        return Err("集成组件 rlib 不包含目标对象".to_string());
    }
    let mut has_initcall = false;
    for object in &objects {
        let sections = Command::new("llvm-objdump")
            .arg("-h")
            .arg(object)
            .output()
            .map_err(|err| format!("检查 {} 段表失败: {err}", object.display()))?;
        if !sections.status.success() {
            return Err(format!("llvm-objdump 无法读取 {}", object.display()));
        }
        has_initcall |=
            String::from_utf8_lossy(&sections.stdout).contains(".kernel.integrated_components");
    }
    if !has_initcall {
        return Err("集成构建没有生成普通内核 initcall 描述符".to_string());
    }
    let output_dir = project.join("dist");
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("创建 {} 失败: {err}", output_dir.display()))?;
    let output = output_dir.join(format!("{cargo_name}-{target}.integrated.a"));
    remove_if_exists(&output)?;
    let archive = Command::new("llvm-ar")
        .arg("crs")
        .arg(&output)
        .args(&objects)
        .output()
        .map_err(|err| format!("生成集成组件归档失败: {err}"))?;
    if !archive.status.success() {
        return Err(format!(
            "llvm-ar 生成集成组件归档失败: {}",
            String::from_utf8_lossy(&archive.stderr)
        ));
    }
    remove_if_exists(&temporary)?;
    Ok(output)
}

fn newest_rlib_for_crate(directory: &Path, crate_name: &str) -> Result<PathBuf, String> {
    let prefix = format!("lib{}-", crate_name.replace('-', "_"));
    let mut candidates = fs::read_dir(directory)
        .map_err(|err| format!("读取依赖目录 {} 失败: {err}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取依赖目录项失败: {err}"))?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".rlib"))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    candidates.pop().ok_or_else(|| {
        format!(
            "集成组件缺少本地 API crate {crate_name} 的 rlib: {}",
            directory.display()
        )
    })
}

fn append_extra_features(command: &mut Command, required: &[&str]) {
    let mut features = required
        .iter()
        .map(|feature| feature.to_string())
        .collect::<Vec<_>>();
    if let Some(extra) = std::env::var_os("ELM_EXTRA_FEATURES") {
        features.extend(
            extra
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|feature| !feature.is_empty())
                .map(str::to_string),
        );
    }
    features.sort();
    features.dedup();
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
}

pub fn cargo_check(project: &Path, target: &str, cargo_name: &str) -> Result<(), String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    prepare_target_interface(&project, target)?;
    let project_manifest = ElmProjectManifest::load(&project)?;
    write_elm_lock(&project, &project_manifest)?;
    let interface_root = project.join(".elm/kernel-interface").join(target);
    let interface = KernelInterfaceManifest::load(&interface_root.join("manifest.txt"))?;
    let metadata = interface_root.join("metadata");
    let mut rustflags = vec![
        format!("-Ldependency={}", metadata.display()),
        format!(
            "--extern=__elm_host_allocator={}",
            metadata.join(&interface.allocator_metadata).display()
        ),
        format!(
            "--extern=__elm_host_general={}",
            metadata.join(&interface.general_metadata).display()
        ),
        format!(
            "--extern=__elm_host_log={}",
            metadata.join(&interface.log_metadata).display()
        ),
        format!(
            "--extern=__elm_host_sched={}",
            metadata.join(&interface.sched_metadata).display()
        ),
    ];
    let (api_profiles, profile_hashes) = kernel_profile_cfg_values(&project_manifest, target)?;
    append_kernel_profile_flags(&mut rustflags, &interface, &api_profiles, &profile_hashes);
    let status = Command::new("cargo")
        .current_dir(&project)
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"))
        .arg("check")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .arg("--bin")
        .arg(cargo_name)
        .arg("--no-default-features")
        .arg("--target")
        .arg(target)
        .status()
        .map_err(|err| format!("启动 cargo check 失败: {err}"))?;
    if !status.success() {
        return Err(format!("ELM 检查失败，退出状态 {status}"));
    }
    Ok(())
}

fn cargo_target_directory(project: &Path) -> PathBuf {
    let Some(configured) = std::env::var_os("CARGO_TARGET_DIR") else {
        return project.join("target");
    };
    let configured = PathBuf::from(configured);
    if configured.is_absolute() {
        configured
    } else {
        project.join(configured)
    }
}

fn append_kernel_profile_flags(
    rustflags: &mut Vec<String>,
    interface: &KernelInterfaceManifest,
    api_profiles: &[String],
    profile_hashes: &[String],
) {
    let profile_hash = hex_digest(&interface.interface_hash);
    rustflags.push(format!("--cfg=elm_kernel_api=\"{}\"", interface.profile));
    rustflags.push(check_cfg_values("elm_kernel_api", api_profiles));
    rustflags.push(format!("--cfg=elm_kernel_profile=\"{profile_hash}\""));
    rustflags.push(check_cfg_values("elm_kernel_profile", profile_hashes));
}

fn check_cfg_values(name: &str, values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("--check-cfg=cfg({name},values({values}))")
}

fn kernel_profile_cfg_values(
    manifest: &ElmProjectManifest,
    target: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    let interfaces = selected_kernel_interfaces(manifest, target)?;
    let mut api_profiles = interfaces
        .iter()
        .map(|interface| interface.manifest.profile.clone())
        .collect::<Vec<_>>();
    let mut profile_hashes = interfaces
        .iter()
        .map(|interface| hex_digest(&interface.manifest.interface_hash))
        .collect::<Vec<_>>();
    api_profiles.sort();
    api_profiles.dedup();
    profile_hashes.sort();
    profile_hashes.dedup();
    Ok((api_profiles, profile_hashes))
}

pub fn cargo_test(project: &Path) -> Result<(), String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    ElmProjectManifest::load(&project)?;
    let status = Command::new("cargo")
        .current_dir(&project)
        .arg("test")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .status()
        .map_err(|err| format!("启动开发侧 cargo test 失败: {err}"))?;
    if !status.success() {
        return Err(format!("ELM 开发侧测试失败，退出状态 {status}"));
    }
    Ok(())
}

pub fn diagnose_project(project: &Path) -> Result<String, String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    let manifest = ElmProjectManifest::load(&project)?;
    if !project.join("Cargo.toml").is_file() {
        return Err("工程缺少 Cargo.toml".to_string());
    }
    if !project.join("rust-toolchain.toml").is_file() {
        return Err("工程缺少 rust-toolchain.toml".to_string());
    }
    let mut report = format!(
        "ELM 工程诊断\nname={}\nversion={}\nkind={}\nsource={}\n",
        manifest.name, manifest.version, manifest.kind, manifest.source
    );
    for target in ["riscv64gc-unknown-none-elf", "loongarch64-unknown-none"] {
        match selected_kernel_interfaces(&manifest, target) {
            Ok(interfaces) => {
                for bundle in interfaces {
                    let interface = bundle.manifest;
                    report.push_str(&format!(
                        "target={} profile={} profile_hash={} framework_hash={} symbols={} bridge_abi={} priority={} status=ok\n",
                        target,
                        interface.profile,
                        hex_digest(&interface.interface_hash),
                        hex_digest(&interface.framework_hash),
                        interface.symbols.len(),
                        interface.bridge_abi_version,
                        bundle.priority,
                    ));
                }
            }
            Err(error) => {
                report.push_str(&format!(
                    "target={target} status=unavailable reason={error}\n"
                ));
            }
        }
    }
    Ok(report)
}

fn framework_source_root() -> Result<PathBuf, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize()
        .map_err(|err| format!("定位 ELM 框架源码失败: {err}"))
}

fn packaged_framework_root(manifest: &ElmProjectManifest) -> Result<Option<PathBuf>, String> {
    let mut selected_framework = None;
    let bundle_root = interface_bundle_root()?;
    let targets = if bundle_root.join("manifest.txt").is_file() {
        vec![KernelInterfaceManifest::load(&bundle_root.join("manifest.txt"))?.target]
    } else {
        vec![
            "riscv64gc-unknown-none-elf".to_string(),
            "loongarch64-unknown-none".to_string(),
        ]
    };
    for target in targets {
        let Ok(available) = available_kernel_interfaces(&target) else {
            continue;
        };
        for bundle in available.iter().filter(|bundle| {
            manifest.profiles.is_empty()
                || manifest
                    .profiles
                    .iter()
                    .any(|requested| requested.id == bundle.manifest.profile)
        }) {
            let framework = bundle.root.join("framework");
            if !framework.join("Cargo.toml").is_file()
                || !framework.join("elm/Cargo.toml").is_file()
                || !framework.join("kernel-symbols/Cargo.toml").is_file()
                || !framework.join("allocator/Cargo.toml").is_file()
                || !framework.join("general/Cargo.toml").is_file()
            {
                return Err(format!(
                    "接口包 {} 缺少完整 ELM framework",
                    bundle.root.display()
                ));
            }
            let actual_framework_hash = packaged_framework_hash(&framework)?;
            if actual_framework_hash != bundle.manifest.framework_hash {
                return Err(format!(
                    "接口包 {} 的 ELM framework 摘要不匹配：声明 {}，实际 {}",
                    bundle.root.display(),
                    hex_digest(&bundle.manifest.framework_hash),
                    hex_digest(&actual_framework_hash)
                ));
            }
            match &selected_framework {
                None => {
                    selected_framework = Some((bundle.manifest.framework_hash, framework.clone()));
                }
                Some((hash, _)) if *hash == bundle.manifest.framework_hash => {}
                Some((hash, _)) => {
                    return Err(format!(
                        "所选内核 Profile 混用了不同 ELM framework：{} 与 {}；请统一工具链后重新导出接口包",
                        hex_digest(hash),
                        hex_digest(&bundle.manifest.framework_hash)
                    ));
                }
            }
        }
    }
    Ok(selected_framework.map(|(_, framework)| framework))
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

fn write_metadata_facade(directory: &Path, name: &str, host_alias: &str) -> Result<(), String> {
    fs::create_dir_all(directory.join("src"))
        .map_err(|err| format!("创建 {} 失败: {err}", directory.display()))?;
    fs::write(
        directory.join("Cargo.toml"),
        metadata_facade_manifest(name, host_alias),
    )
    .map_err(|err| format!("写入 {name} façade manifest 失败: {err}"))?;
    fs::write(
        directory.join("src/lib.rs"),
        metadata_facade_source(name, host_alias),
    )
    .map_err(|err| format!("写入 {name} façade 源码失败: {err}"))
}

fn interface_bundle_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("ELM_KERNEL_INTERFACE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    if let Some(home) = std::env::var_os("ELM_HOME") {
        return Ok(PathBuf::from(home).join("interfaces"));
    }
    if let Ok(repository) = framework_source_root() {
        let repository_cache = repository.join("build/elm-interface");
        if repository_cache.exists() {
            return Ok(repository_cache);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache/elm/interfaces"));
    }
    Err("无法定位 ELM 接口仓库；请设置 ELM_KERNEL_INTERFACE_ROOT".to_string())
}

pub fn available_kernel_interfaces(target: &str) -> Result<Vec<KernelInterfaceBundle>, String> {
    let bundle_root = interface_bundle_root()?;
    let root = if bundle_root.join("manifest.txt").is_file()
        && bundle_root.join("framework/Cargo.toml").is_file()
    {
        bundle_root
    } else {
        bundle_root.join(target)
    };
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut manifests = Vec::new();
    collect_interface_manifests(&root, 0, &mut manifests)?;
    manifests.sort();
    let mut seen = BTreeSet::new();
    let mut bundles = Vec::new();
    for path in manifests {
        let manifest = KernelInterfaceManifest::load(&path)?;
        if manifest.target != target {
            return Err(format!(
                "接口包目录目标为 {target}，但 {} 声明目标 {}",
                path.display(),
                manifest.target
            ));
        }
        if !seen.insert((manifest.interface_hash, manifest.kernel_hash)) {
            continue;
        }
        let root = path
            .parent()
            .ok_or_else(|| format!("接口清单没有父目录: {}", path.display()))?
            .canonicalize()
            .map_err(|err| format!("定位接口包 {} 失败: {err}", path.display()))?;
        bundles.push(KernelInterfaceBundle {
            root,
            manifest,
            priority: 0,
        });
    }
    bundles.sort_by(|left, right| {
        left.manifest
            .profile
            .cmp(&right.manifest.profile)
            .then_with(|| {
                left.manifest
                    .interface_hash
                    .cmp(&right.manifest.interface_hash)
            })
            .then_with(|| left.manifest.kernel_hash.cmp(&right.manifest.kernel_hash))
    });
    Ok(bundles)
}

pub fn selected_kernel_interfaces(
    manifest: &ElmProjectManifest,
    target: &str,
) -> Result<Vec<KernelInterfaceBundle>, String> {
    let available = available_kernel_interfaces(target)?;
    if manifest.profiles.is_empty() {
        if available.is_empty() {
            return Err(format!("目标 {target} 没有可用的内核 API Profile"));
        }
        if available.len() > elm::ELM_EKI_MAX_VARIANTS {
            return Err(format!(
                "目标 {target} 的可用 Profile 数量超过 EKI 上限 {}",
                elm::ELM_EKI_MAX_VARIANTS
            ));
        }
        return Ok(available);
    }

    let mut selected = Vec::new();
    for requested in &manifest.profiles {
        let mut matched = 0usize;
        for bundle in &available {
            if bundle.manifest.profile == requested.id {
                let mut bundle = bundle.clone();
                bundle.priority = requested.priority;
                selected.push(bundle);
                matched += 1;
            }
        }
        if matched == 0 {
            return Err(format!(
                "目标 {target} 缺少 Elm.toml 请求的内核 API Profile {}",
                requested.id
            ));
        }
    }
    if selected.len() > elm::ELM_EKI_MAX_VARIANTS {
        return Err(format!(
            "目标 {target} 的所选 Profile 数量超过 EKI 上限 {}",
            elm::ELM_EKI_MAX_VARIANTS
        ));
    }
    Ok(selected)
}

pub fn activate_kernel_interface(
    project: &Path,
    bundle: &KernelInterfaceBundle,
) -> Result<(), String> {
    copy_target_interface(project, &bundle.manifest.target, &bundle.root)?;
    install_lsp_source(project, &bundle.root, &bundle.manifest, true)?;
    let project_manifest = ElmProjectManifest::load(project)?;
    let (api_profiles, profile_hashes) =
        kernel_profile_cfg_values(&project_manifest, &bundle.manifest.target)?;
    fs::write(
        project.join(".cargo/config.toml"),
        elm_cargo_config(Some(&bundle.manifest), &api_profiles, &profile_hashes),
    )
    .map_err(|err| format!("更新活动 Profile 的 Cargo 配置失败: {err}"))
}

fn collect_interface_manifests(
    directory: &Path,
    depth: usize,
    output: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth > 3 {
        return Ok(());
    }
    let manifest = directory.join("manifest.txt");
    if manifest.is_file() {
        output.push(manifest);
    }
    for entry in fs::read_dir(directory)
        .map_err(|err| format!("读取接口仓库 {} 失败: {err}", directory.display()))?
    {
        let entry = entry.map_err(|err| format!("读取接口仓库目录项失败: {err}"))?;
        if matches!(
            entry.file_name().to_str(),
            Some("metadata" | "framework" | "kernel-source" | "target")
        ) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_interface_manifests(&path, depth + 1, output)?;
        }
    }
    Ok(())
}

fn sync_available_target_interfaces(
    project: &Path,
    manifest: &ElmProjectManifest,
) -> Result<(), String> {
    let mut installed_lsp = false;
    for target in ["riscv64gc-unknown-none-elf", "loongarch64-unknown-none"] {
        let Ok(available) = available_kernel_interfaces(target) else {
            // 新建工程不能被仓库中遗留的旧版生成缓存阻断；正式 build/doctor 会对
            // 所选 Profile 返回完整格式错误。
            continue;
        };
        let selected = manifest
            .profiles
            .iter()
            .find_map(|requested| {
                available
                    .iter()
                    .find(|bundle| bundle.manifest.profile == requested.id)
            })
            .or_else(|| available.first());
        let Some(bundle) = selected else { continue };
        copy_target_interface(project, target, &bundle.root)?;
        if !installed_lsp {
            install_lsp_source(project, &bundle.root, &bundle.manifest, true)?;
            installed_lsp = true;
        }
    }
    Ok(())
}

fn prepare_target_interface(project: &Path, target: &str) -> Result<(), String> {
    let destination = project.join(".elm/kernel-interface").join(target);
    if destination.join("manifest.txt").is_file() {
        let manifest = KernelInterfaceManifest::load(&destination.join("manifest.txt"))?;
        if lsp_source_interface_hash(&project.join(".elm/kernel-source"))?
            == Some(manifest.source_hash)
        {
            return Ok(());
        }
        let bundle = available_kernel_interfaces(target)?
            .into_iter()
            .find(|bundle| bundle.manifest.interface_hash == manifest.interface_hash)
            .ok_or_else(|| {
                format!("目标 {target} 已激活的 Profile 已不在接口仓库中；请执行 cargo elm sync")
            })?;
        if bundle.manifest.source_hash != manifest.source_hash {
            return Err(format!(
                "目标 {target} 的工程 Profile 与发布接口包源码摘要不一致；请执行 cargo elm sync"
            ));
        }
        install_lsp_source(project, &bundle.root, &manifest, true)?;
        return Ok(());
    }
    let bundle = available_kernel_interfaces(target)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            format!("缺少目标 {target} 的内核 API Profile；先执行 cargo elm profile-export")
        })?;
    copy_target_interface(project, target, &bundle.root)?;
    install_lsp_source(project, &bundle.root, &bundle.manifest, false)
}

fn copy_target_interface(project: &Path, target: &str, bundle: &Path) -> Result<(), String> {
    let manifest = KernelInterfaceManifest::load(&bundle.join("manifest.txt"))?;
    if manifest.target != target {
        return Err(format!(
            "接口包目标不匹配：目录为 {target}，清单为 {}",
            manifest.target
        ));
    }
    let root = project.join(".elm/kernel-interface");
    fs::create_dir_all(&root).map_err(|err| format!("创建 {} 失败: {err}", root.display()))?;
    let temporary = root.join(format!("{target}.tmp.{}", std::process::id()));
    let destination = root.join(target);
    remove_if_exists(&temporary)?;
    let bundle = bundle
        .canonicalize()
        .map_err(|err| format!("定位接口缓存 {} 失败: {err}", bundle.display()))?;
    let support_library = bundle.join(&manifest.support_library);
    if !support_library.is_file() {
        return Err(format!(
            "接口包缺少 Rust 支持归档: {}",
            support_library.display()
        ));
    }
    let import_library = bundle.join(&manifest.import_library);
    if !import_library.is_file() {
        return Err(format!(
            "接口包缺少内核导入库: {}",
            import_library.display()
        ));
    }
    std::os::unix::fs::symlink(&bundle, &temporary)
        .map_err(|err| format!("建立接口缓存链接失败: {err}"))?;
    remove_if_exists(&destination)?;
    fs::rename(&temporary, &destination).map_err(|err| format!("安装目标接口包失败: {err}"))?;

    let identity = bundle
        .join("framework/kernel-symbols")
        .join(format!("interface.identity.{target}"));
    if !identity.is_file() {
        return Err(format!("接口包缺少身份文件: {}", identity.display()));
    }
    fs::copy(
        &identity,
        project
            .join(".elm/framework/kernel-symbols")
            .join(format!("interface.identity.{target}")),
    )
    .map_err(|err| format!("安装 kernel-symbols 接口身份失败: {err}"))?;
    fs::copy(
        identity,
        project
            .join(".elm/framework/kernel-symbols")
            .join("interface.identity"),
    )
    .map_err(|err| format!("安装 host LSP 接口身份失败: {err}"))?;
    Ok(())
}

fn install_lsp_source(
    project: &Path,
    bundle: &Path,
    manifest: &KernelInterfaceManifest,
    force: bool,
) -> Result<(), String> {
    let source = bundle.join("kernel-source");
    let source_hash = lsp_source_interface_hash(&source)?
        .ok_or_else(|| format!("接口包缺少有效的 LSP 源码投影身份: {}", source.display()))?;
    if source_hash != manifest.source_hash {
        return Err(format!(
            "接口包 LSP 源码投影与清单摘要不一致: {}",
            bundle.display()
        ));
    }
    let destination = project.join(".elm/kernel-source");
    if !force && lsp_source_interface_hash(&destination)? == Some(source_hash) {
        return Ok(());
    }
    let elm_root = project.join(".elm");
    fs::create_dir_all(&elm_root)
        .map_err(|err| format!("创建 {} 失败: {err}", elm_root.display()))?;
    let temporary = elm_root.join(format!("kernel-source.tmp.{}", std::process::id()));
    let backup = elm_root.join(format!("kernel-source.old.{}", std::process::id()));
    remove_if_exists(&temporary)?;
    remove_if_exists(&backup)?;
    let source = source
        .canonicalize()
        .map_err(|err| format!("定位 LSP 源码缓存 {} 失败: {err}", source.display()))?;
    std::os::unix::fs::symlink(&source, &temporary)
        .map_err(|err| format!("建立 LSP 源码缓存链接失败: {err}"))?;
    if destination.exists() {
        fs::rename(&destination, &backup).map_err(|err| {
            format!(
                "备份 LSP 源码投影 {} -> {} 失败: {err}",
                destination.display(),
                backup.display()
            )
        })?;
    }
    if let Err(err) = fs::rename(&temporary, &destination) {
        if backup.exists() {
            let _ = fs::rename(&backup, &destination);
        }
        return Err(format!("原子安装 LSP 源码投影失败: {err}"));
    }
    remove_if_exists(&backup)?;
    Ok(())
}

fn lsp_source_interface_hash(source: &Path) -> Result<Option<[u8; 32]>, String> {
    let identity = source.join(LSP_SOURCE_IDENTITY_FILE);
    if !identity.is_file() {
        return Ok(None);
    }
    let input = fs::read_to_string(&identity)
        .map_err(|err| format!("读取 {} 失败: {err}", identity.display()))?;
    let mut lines = input.lines();
    if lines.next() != Some(LSP_SOURCE_MAGIC) {
        return Err(format!(
            "{} 不是有效的 LSP 源码投影身份",
            identity.display()
        ));
    }
    let mut interface_hash = None;
    let mut packages = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("interface_sha256=") {
            interface_hash = Some(parse_sha256(value)?);
        } else if let Some(value) = line.strip_prefix("packages=") {
            packages = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| format!("{} 的 packages 字段无效", identity.display()))?,
            );
        } else if !line.is_empty() {
            return Err(format!("{} 包含未知字段: {line}", identity.display()));
        }
    }
    if packages == Some(0) || packages.is_none() {
        return Err(format!("{} 缺少有效 packages 字段", identity.display()));
    }
    Ok(Some(interface_hash.ok_or_else(|| {
        format!("{} 缺少 interface_sha256", identity.display())
    })?))
}

fn parse_sha256(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("LSP 源码投影摘要必须包含 64 个十六进制字符".to_string());
    }
    let mut output = [0u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "LSP 源码投影摘要包含非十六进制字符".to_string())?;
    }
    Ok(output)
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        fs::remove_file(path).map_err(|err| format!("删除 {} 失败: {err}", path.display()))?;
    } else if path.is_dir() {
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
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "{name}"
path = "src/main.rs"
test = false
bench = false

[lib]
name = "{crate_name}"
path = "src/main.rs"
test = false
doctest = false
bench = false

[features]
default = ["elm-lsp"]
elm-lsp = ["allocator/lsp", "general/lsp", "log/lsp", "sched/lsp"]
elm-integrated = []

[dependencies]
elm = {{ path = ".elm/framework/elm", default-features = false, features = [{features}] }}
allocator = {{ path = ".elm/framework/allocator", default-features = false }}
general = {{ path = ".elm/framework/general", default-features = false }}
log = {{ path = ".elm/framework/log", default-features = false }}
sched = {{ path = ".elm/framework/sched", default-features = false }}

[profile.release]
panic = "abort"
codegen-units = 1
lto = false
strip = false

[profile.dev]
panic = "abort"
"#,
        crate_name = name.replace('-', "_")
    )
}

fn elm_toml(name: &str, kind: &str, source: &str) -> String {
    format!(
        r#"[elm]
name = "{name}"
version = "0.1.0"
kind = "{kind}"
source = "{source}"
mode = "m"
"#
    )
}

fn module_rs(name: &str) -> String {
    format!(
        r#"#![no_std]
#![no_main]

extern crate alloc;

use alloc::{{boxed::Box, string::String, sync::Arc, vec::Vec}};
use elm::{{ElmModule, HookError, HookResult, LifecycleContext}};

use allocator as _;
use general as _;

struct Module;

#[elm::module]
impl ElmModule for Module {{
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {{
        Ok(Self)
    }}

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {{
        let mut values = Vec::new();
        values.extend_from_slice(&[1_u32, 2, 3]);
        let boxed = Box::new(values.iter().copied().sum::<u32>());
        let shared = Arc::new(String::from("{name}: initialized"));
        core::hint::black_box((&values, &boxed, &shared));
        if *boxed != 6 || Arc::strong_count(&shared) != 1 {{
            return Err(HookError::new(-1));
        }}
        report(6, shared.as_str())?;
        Ok(())
    }}

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {{
        report(6, "{name}: finalized")?;
        Ok(())
    }}
}}

#[cfg(not(feature = "elm-integrated"))]
fn report(level: u32, message: &str) -> HookResult {{
    elm::runtime::log(level, message).map_err(|_| HookError::new(-1))
}}

#[cfg(feature = "elm-integrated")]
fn report(_level: u32, message: &str) -> HookResult {{
    core::hint::black_box(message);
    Ok(())
}}

#[cfg(not(feature = "elm-integrated"))]
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

fn migrate_cargo_manifest(path: &Path, manifest: &ElmProjectManifest) -> Result<(), String> {
    let input =
        fs::read_to_string(path).map_err(|err| format!("读取 {} 失败: {err}", path.display()))?;
    let mut output = remove_retired_standard_manifest_lines(&input);
    output = migrate_standard_root_workspace(&output)?;
    if output.contains("elmmgr") {
        return Err(format!(
            "{} 仍包含定制化 elmmgr 依赖或路径；ELM v1 只允许 elm::runtime 和 elm::management，请手动移除后重试",
            path.display()
        ));
    }
    if output.contains("kernel-api") || output.contains("kernel_api") {
        return Err(format!(
            "{} 仍包含定制化 kernel-api 依赖或路径；ELM v1 已改用 allocator/general 直接符号门面，请手动迁移后重试",
            path.display()
        ));
    }

    let standard_module = "elm = { path = \".elm/framework/elm\", default-features = false, features = [\"module\", \"macros\"] }";
    let standard_manager = "elm = { path = \".elm/framework/elm\", default-features = false, features = [\"module\", \"macros\", \"management\"] }";
    let desired = if manifest.kind == "manager" {
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
    } else if manifest.kind == "manager" && !output.contains("\"management\"") {
        return Err(format!(
            "{} 使用定制化 elm 依赖，但 Manager 工程未启用 management feature",
            path.display()
        ));
    }

    output = ensure_facade_dependency(&output, "allocator")?;
    output = ensure_facade_dependency(&output, "general")?;
    output = ensure_facade_dependency(&output, "log")?;
    output = ensure_facade_dependency(&output, "sched")?;
    for dependency in manifest
        .dependencies
        .iter()
        .filter_map(|dependency| dependency.crate_name.as_deref())
    {
        output = ensure_elm_api_dependency(&output, dependency)?;
    }
    output = ensure_lsp_feature(&output, manifest)?;
    output = ensure_integrated_feature(&output, manifest)?;
    output = ensure_profile_dev_abort(&output);

    if output != input {
        fs::write(path, output).map_err(|err| format!("迁移 {} 失败: {err}", path.display()))?;
    }
    Ok(())
}

fn remove_retired_standard_manifest_lines(input: &str) -> String {
    let trailing_newline = input.ends_with('\n');
    let mut lines = input
        .lines()
        .filter(|line| {
            let line = line.trim();
            !matches!(
                line,
                "\".elm/framework/elmmgr\","
                    | "\".elm/framework/kernel-api\","
                    | "elmmgr = { path = \".elm/framework/elmmgr\" }"
                    | "kernel-api = { path = \".elm/framework/kernel-api\" }"
                    | "kernel-api = { path = \".elm/framework/kernel-api\", default-features = false, features = [\"module\"] }"
                    | "exclude = [\".elm/kernel-source\"]"
                    | "exclude = [\".elm/kernel-source/**\"]"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if trailing_newline {
        lines.push('\n');
    }
    lines
}

fn migrate_standard_root_workspace(input: &str) -> Result<String, String> {
    let trailing_newline = input.ends_with('\n');
    let mut lines = input.lines().map(str::to_string).collect::<Vec<_>>();
    let Some(workspace) = manifest_section_range(&lines, "[workspace]") else {
        if lines.iter().any(|line| {
            let line = line.trim();
            line.starts_with("[workspace.") && line.ends_with(']')
        }) {
            return Err("Cargo.toml 存在脱离根 workspace 的 workspace 子节".to_string());
        }
        return Ok(replace_workspace_package_inheritance(
            input, "0.1.0", "2024",
        ));
    };

    for line in &lines[workspace.0 + 1..workspace.1] {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line == "]" {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            if !matches!(key.trim(), "resolver" | "members" | "exclude") {
                return Err(format!(
                    "ELM 根 Cargo.toml 使用了自定义 workspace 字段 {}；无法自动迁移为独立 package",
                    key.trim()
                ));
            }
            continue;
        }
        if let Some(member) = line
            .strip_prefix('"')
            .and_then(|line| line.split_once('"').map(|(member, _)| member))
        {
            if !matches!(
                member,
                "." | ".elm/framework/elm"
                    | ".elm/framework/elm/macros"
                    | ".elm/framework/kernel-symbols"
                    | ".elm/framework/kernel-symbols/macros"
                    | ".elm/framework/allocator"
                    | ".elm/framework/general"
                    | ".elm/framework/log"
                    | ".elm/framework/sched"
            ) {
                return Err(format!(
                    "ELM 根 Cargo.toml 包含自定义 workspace member {member}；请先把 ELM package 与仓库 workspace 分离"
                ));
            }
            continue;
        }
        return Err(format!("无法识别 ELM 根 workspace 行: {line}"));
    }

    let workspace_package = manifest_section_range(&lines, "[workspace.package]");
    if lines.iter().any(|line| {
        let line = line.trim();
        line.starts_with("[workspace.") && line.ends_with(']') && line != "[workspace.package]"
    }) {
        return Err("ELM 根 Cargo.toml 包含自定义 workspace 子节；无法安全自动迁移".to_string());
    }
    let version = workspace_package
        .and_then(|range| manifest_string_assignment(&lines[range.0 + 1..range.1], "version"))
        .unwrap_or_else(|| "0.1.0".to_string());
    let edition = workspace_package
        .and_then(|range| manifest_string_assignment(&lines[range.0 + 1..range.1], "edition"))
        .unwrap_or_else(|| "2024".to_string());

    let mut ranges = vec![workspace];
    if let Some(range) = workspace_package {
        ranges.push(range);
    }
    ranges.sort_by_key(|range| core::cmp::Reverse(range.0));
    for (start, end) in ranges {
        lines.drain(start..end);
        while lines.get(start).is_some_and(|line| line.trim().is_empty())
            && start > 0
            && lines
                .get(start - 1)
                .is_some_and(|line| line.trim().is_empty())
        {
            lines.remove(start);
        }
    }
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    Ok(replace_workspace_package_inheritance(
        &output, &version, &edition,
    ))
}

fn manifest_section_range(lines: &[String], header: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|line| line.trim() == header)?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            let line = line.trim();
            line.starts_with('[') && line.ends_with(']')
        })
        .map_or(lines.len(), |offset| start + 1 + offset);
    Some((start, end))
}

fn manifest_string_assignment(lines: &[String], key: &str) -> Option<String> {
    for line in lines {
        let Some((candidate, value)) = line.split_once('=') else {
            continue;
        };
        if candidate.trim() != key {
            continue;
        }
        let value = value.trim();
        return value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .map(str::to_string);
    }
    None
}

fn replace_workspace_package_inheritance(input: &str, version: &str, edition: &str) -> String {
    input
        .replace(
            "version.workspace = true",
            &format!("version = {version:?}"),
        )
        .replace(
            "edition.workspace = true",
            &format!("edition = {edition:?}"),
        )
}

fn ensure_facade_dependency(input: &str, name: &str) -> Result<String, String> {
    let path = format!(".elm/framework/{name}");
    let desired = format!("{name} = {{ path = {path:?}, default-features = false }}");
    let trailing_newline = input.ends_with('\n');
    let mut lines = input.lines().map(str::to_string).collect::<Vec<_>>();
    let matches = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            line.split_once('=')
                .filter(|(key, _)| key.trim() == name)
                .map(|_| index)
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(format!("Cargo.toml 重复定义 {name} 依赖"));
    }
    if let Some(index) = matches.first().copied() {
        if !lines[index].contains(&path) {
            return Err(format!(
                "Cargo.toml 使用了定制化 {name} 依赖；ELM 直接符号接口必须来自 {path}"
            ));
        }
        if lines[index].contains("default-features = true") {
            lines[index] =
                lines[index].replace("default-features = true", "default-features = false");
        } else if !lines[index].contains("default-features = false") {
            let line = lines[index].clone();
            let brace = line
                .rfind('}')
                .ok_or_else(|| format!("{name} path dependency 必须使用内联表"))?;
            let prefix = line[..brace].trim_end();
            let suffix = &line[brace..];
            lines[index] = format!("{prefix}, default-features = false {suffix}");
        }
    } else {
        let dependencies = manifest_section_range(&lines, "[dependencies]")
            .ok_or_else(|| "Cargo.toml 缺少 [dependencies]".to_string())?;
        lines.insert(dependencies.0 + 1, desired);
    }
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn ensure_elm_api_dependency(input: &str, name: &str) -> Result<String, String> {
    let path = format!(".elm/dependencies/{name}");
    let desired = format!(
        "{name} = {{ path = {path:?}, default-features = false, features = [\"elm-consumer\"] }}"
    );
    let trailing_newline = input.ends_with('\n');
    let mut lines = input.lines().map(str::to_string).collect::<Vec<_>>();
    let matches = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            line.split_once('=')
                .filter(|(key, _)| key.trim() == name)
                .map(|_| index)
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(format!("Cargo.toml 重复定义 {name} 依赖"));
    }
    if let Some(index) = matches.first().copied() {
        if !lines[index].contains(&path) {
            return Err(format!(
                "Cargo.toml 的 {name} 必须来自构建工具安装的 {path}"
            ));
        }
        lines[index] = desired;
    } else {
        let dependencies = manifest_section_range(&lines, "[dependencies]")
            .ok_or_else(|| "Cargo.toml 缺少 [dependencies]".to_string())?;
        lines.insert(dependencies.0 + 1, desired);
    }
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn ensure_lsp_feature(input: &str, _manifest: &ElmProjectManifest) -> Result<String, String> {
    const LSP_FEATURE: &str =
        "elm-lsp = [\"allocator/lsp\", \"general/lsp\", \"log/lsp\", \"sched/lsp\"]";

    let trailing_newline = input.ends_with('\n');
    let mut lines = input.lines().map(str::to_string).collect::<Vec<_>>();
    let features = if let Some(features) = manifest_section_range(&lines, "[features]") {
        features
    } else {
        let dependencies = manifest_section_range(&lines, "[dependencies]")
            .ok_or_else(|| "Cargo.toml 缺少 [dependencies]".to_string())?;
        lines.splice(
            dependencies.0..dependencies.0,
            [
                "[features]".to_string(),
                "default = [\"elm-lsp\"]".to_string(),
                LSP_FEATURE.to_string(),
                String::new(),
            ],
        );
        let mut output = lines.join("\n");
        if trailing_newline {
            output.push('\n');
        }
        return Ok(output);
    };

    let section = &lines[features.0 + 1..features.1];
    if let Some(line) = section.iter().find(|line| {
        line.split_once('=')
            .is_some_and(|(key, _)| key.trim() == "elm-lsp")
    }) {
        if !line.contains("allocator/lsp")
            || !line.contains("general/lsp")
            || !line.contains("log/lsp")
            || !line.contains("sched/lsp")
        {
            return Err(
                "Cargo.toml 的 elm-lsp feature 未启用全部标准内核元数据 facade".to_string(),
            );
        }
    } else {
        lines.insert(features.0 + 1, LSP_FEATURE.to_string());
    }

    let features = manifest_section_range(&lines, "[features]").unwrap();
    if let Some(index) = lines[features.0 + 1..features.1]
        .iter()
        .position(|line| {
            line.split_once('=')
                .is_some_and(|(key, _)| key.trim() == "default")
        })
        .map(|offset| features.0 + 1 + offset)
    {
        if !lines[index].contains("\"elm-lsp\"") {
            let (key, value) = lines[index]
                .split_once('=')
                .ok_or_else(|| "Cargo.toml 的 default feature 定义无效".to_string())?;
            let value = value.trim();
            let contents = value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .ok_or_else(|| "Cargo.toml 的 default feature 必须使用单行数组".to_string())?
                .trim();
            lines[index] = if contents.is_empty() {
                format!("{} = [\"elm-lsp\"]", key.trim())
            } else {
                format!("{} = [{contents}, \"elm-lsp\"]", key.trim())
            };
        }
    } else {
        lines.insert(features.0 + 1, "default = [\"elm-lsp\"]".to_string());
    }

    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn ensure_integrated_feature(input: &str, manifest: &ElmProjectManifest) -> Result<String, String> {
    let mut propagated = manifest
        .dependencies
        .iter()
        .filter_map(|dependency| dependency.crate_name.as_deref())
        .map(|name| format!("\"{name}/elm-integrated\""))
        .collect::<Vec<_>>();
    if let Some(api) = &manifest.api {
        propagated.push(format!("\"{}/elm-integrated\"", api.crate_name));
    }
    propagated.sort();
    propagated.dedup();
    if propagated.is_empty() {
        return Ok(input.to_string());
    }
    let trailing_newline = input.ends_with('\n');
    let mut lines = input.lines().map(str::to_string).collect::<Vec<_>>();
    let features = manifest_section_range(&lines, "[features]")
        .ok_or_else(|| "Cargo.toml 缺少 [features]".to_string())?;
    let value = format!("elm-integrated = [{}]", propagated.join(", "));
    if let Some(index) = lines[features.0 + 1..features.1]
        .iter()
        .position(|line| {
            line.split_once('=')
                .is_some_and(|(key, _)| key.trim() == "elm-integrated")
        })
        .map(|offset| features.0 + 1 + offset)
    {
        lines[index] = value;
    } else {
        lines.insert(features.0 + 1, value);
    }
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn ensure_profile_dev_abort(input: &str) -> String {
    let trailing_newline = input.ends_with('\n');
    let mut lines = input.lines().map(str::to_string).collect::<Vec<_>>();
    if let Some(profile) = lines.iter().position(|line| line.trim() == "[profile.dev]") {
        let section_end = lines[profile + 1..]
            .iter()
            .position(|line| {
                let line = line.trim();
                line.starts_with('[') && line.ends_with(']')
            })
            .map_or(lines.len(), |offset| profile + 1 + offset);
        if let Some(relative) = lines[profile + 1..section_end]
            .iter()
            .position(|line| line.trim_start().starts_with("panic"))
        {
            lines[profile + 1 + relative] = "panic = \"abort\"".to_string();
        } else {
            lines.insert(profile + 1, "panic = \"abort\"".to_string());
        }
    } else {
        if !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("[profile.dev]".to_string());
        lines.push("panic = \"abort\"".to_string());
    }
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    output
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

fn validate_crate_name(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(format!("{label} 无效: {value}"));
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!("{label} 必须是工程内相对路径: {value}"));
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

fn elm_cargo_config(
    interface: Option<&KernelInterfaceManifest>,
    api_profiles: &[String],
    profile_hashes: &[String],
) -> String {
    let profile_flags = interface.map_or_else(Vec::new, |interface| {
        let profile_hash = hex_digest(&interface.interface_hash);
        let api_profiles = if api_profiles.is_empty() {
            vec![interface.profile.clone()]
        } else {
            api_profiles.to_vec()
        };
        let profile_hashes = if profile_hashes.is_empty() {
            vec![profile_hash.clone()]
        } else {
            profile_hashes.to_vec()
        };
        vec![
            format!("--cfg=elm_kernel_api=\"{}\"", interface.profile),
            check_cfg_values("elm_kernel_api", &api_profiles),
            format!("--cfg=elm_kernel_profile=\"{profile_hash}\""),
            check_cfg_values("elm_kernel_profile", &profile_hashes),
        ]
    });
    let mut output = String::from("[build]\n");
    if !profile_flags.is_empty() {
        output.push_str("rustflags = [\n");
        append_toml_array(&mut output, &profile_flags);
        output.push_str("]\n");
    }
    output
        .push_str("\n[target.riscv64gc-unknown-none-elf]\nlinker = \"rust-lld\"\nrustflags = [\n");
    let mut riscv_flags = elm_link_flags(false);
    riscv_flags.extend(profile_flags.iter().cloned());
    append_toml_array(&mut output, &riscv_flags);
    output
        .push_str("]\n\n[target.loongarch64-unknown-none]\nlinker = \"rust-lld\"\nrustflags = [\n");
    let mut loongarch_flags = elm_link_flags(true);
    loongarch_flags.extend(profile_flags);
    append_toml_array(&mut output, &loongarch_flags);
    output.push_str("]\n");
    output
}

fn elm_link_flags(loongarch: bool) -> Vec<String> {
    let mut flags = vec![
        "-C".to_string(),
        "link-arg=-Telm.ld".to_string(),
        "-C".to_string(),
        "relocation-model=pic".to_string(),
        "-C".to_string(),
        "code-model=small".to_string(),
        "-C".to_string(),
        "link-arg=-pie".to_string(),
        "-C".to_string(),
        "link-arg=-z".to_string(),
        "-C".to_string(),
        "link-arg=notext".to_string(),
        "-C".to_string(),
        "link-arg=--gc-sections".to_string(),
        "-C".to_string(),
        "link-arg=--build-id=none".to_string(),
    ];
    if loongarch {
        flags.push("-A".to_string());
        flags.push("named_asm_labels".to_string());
    }
    flags
}

fn append_toml_array(output: &mut String, values: &[String]) {
    use std::fmt::Write as _;

    for value in values {
        writeln!(output, "    {value:?},").expect("写入 String 不会失败");
    }
}

const ELM_RUST_TOOLCHAIN: &str = r#"[toolchain]
channel = "nightly-2025-05-20"
profile = "minimal"
targets = ["loongarch64-unknown-none", "riscv64gc-unknown-none-elf"]
"#;

const ELM_LINKER_SCRIPT: &str = r#"ENTRY(__elm_module_entry_v1)

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
        KEEP(*(.rodata.elm.module))
        *(.rodata .rodata.* .srodata .srodata.*)
        *(.eh_frame .eh_frame_hdr)
    } :rodata

    .rela.dyn : ALIGN(8)
    {
        KEEP(*(.rela.dyn))
    } :rodata
    .rela.plt : ALIGN(8) { KEEP(*(.rela.plt)) } :rodata

    .dynsym : ALIGN(8) { KEEP(*(.dynsym)) } :rodata
    .dynstr : ALIGN(1) { KEEP(*(.dynstr)) } :rodata
    .hash : ALIGN(8) { KEEP(*(.hash)) } :rodata
    .gnu.hash : ALIGN(8) { KEEP(*(.gnu.hash)) } :rodata

    . = ALIGN(4096);
    .data :
    {
        *(.data .data.* .sdata .sdata.*)
        *(.got .got.*)
        *(.got.plt)
    } :data

    .dynamic : ALIGN(8) { KEEP(*(.dynamic)) } :data

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
                "cargo-elm-{name}-{}-{sequence}",
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

    fn test_manifest(kind: &str) -> ElmProjectManifest {
        ElmProjectManifest {
            name: "test.module".to_string(),
            version: "0.1.0".to_string(),
            kind: kind.to_string(),
            source: "local.test".to_string(),
            mode: ElmBuildMode::Managed,
            api: None,
            menu: None,
            dependencies: Vec::new(),
            profiles: Vec::new(),
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

[[profiles]]
id = "contest-2026"
priority = "100"
"#,
        )
        .unwrap();
        assert_eq!(manifest.name, "demo.echo");
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.profiles.len(), 1);
        assert_eq!(manifest.profiles[0].id, "contest-2026");
        assert_eq!(manifest.profiles[0].priority, 100);
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
        assert!(service_cargo.contains("[lib]\nname = \"demo_service\"\npath = \"src/main.rs\""));
        assert!(service_cargo.contains("[[bin]]\nname = \"demo-service\"\npath = \"src/main.rs\""));
        assert!(!service_cargo.contains("[workspace]"));
        assert!(service_cargo.contains("default = [\"elm-lsp\"]"));
        assert!(service_cargo.contains("\"log/lsp\", \"sched/lsp\""));
        assert!(service_cargo.contains(".elm/framework/allocator"));
        assert!(service_cargo.contains(".elm/framework/general"));
        assert!(service_cargo.contains("[profile.dev]\npanic = \"abort\""));
        assert!(service_cargo.contains(
            "allocator = { path = \".elm/framework/allocator\", default-features = false }"
        ));
        assert!(
            service_cargo.contains(
                "general = { path = \".elm/framework/general\", default-features = false }"
            )
        );
        assert!(!service_cargo.contains("management"));
        assert!(!service_cargo.contains("elmmgr"));
        assert!(service_source.contains("use allocator as _;"));
        assert!(service_source.contains("use general as _;"));
        assert!(service_source.contains("extern crate alloc"));
        assert!(service_source.contains("Vec::new()"));
        assert!(service_source.contains("Box::new"));
        assert!(service_source.contains("Arc::new"));
        assert!(service_source.contains("core::hint::black_box"));
        assert!(service_source.contains("elm::runtime::log"));
        assert!(service_source.contains("impl ElmModule for Module"));
        assert!(
            service_source.contains("#[cfg(not(feature = \"elm-integrated\"))]\n#[panic_handler]")
        );
        assert!(service_source.contains("elm::runtime::abort_panic"));
        assert!(!service.path().join("src/lib.rs").exists());
        assert!(
            service
                .path()
                .join(".elm/framework/elm/Cargo.toml")
                .is_file()
        );
        assert!(
            service
                .path()
                .join(".elm/framework/allocator/Cargo.toml")
                .is_file()
        );
        assert!(
            service
                .path()
                .join(".elm/framework/general/Cargo.toml")
                .is_file()
        );
        assert!(service.path().join(".elm/framework/Cargo.toml").is_file());
        assert!(!service.path().join(".elm/framework/elmmgr").exists());

        let manager = TestDirectory::new("manager-project");
        scaffold_project(manager.path(), "demo.manager", "manager", "local.test").unwrap();
        let manager_cargo = fs::read_to_string(manager.path().join("Cargo.toml")).unwrap();
        assert!(manager_cargo.contains("features = [\"module\", \"macros\", \"management\"]"));
        assert!(!manager_cargo.contains("elmmgr"));
        assert!(!manager.path().join(".elm/framework/elmmgr").exists());
    }

    #[test]
    fn migrates_only_the_retired_standard_framework_layouts() {
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
	    ".elm/framework/kernel-api",
	]

	[dependencies]
	elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros"] }
	elmmgr = { path = ".elm/framework/elmmgr" }
	kernel-api = { path = ".elm/framework/kernel-api", default-features = false, features = ["module"] }
	"#,
        )
        .unwrap();
        migrate_cargo_manifest(&manifest, &test_manifest("manager")).unwrap();
        let migrated = fs::read_to_string(&manifest).unwrap();
        assert!(!migrated.contains("elmmgr"));
        assert!(!migrated.contains("kernel-api"));
        assert!(migrated.contains("features = [\"module\", \"macros\", \"management\"]"));
        assert!(migrated.contains(".elm/framework/allocator"));
        assert!(migrated.contains(".elm/framework/general"));
        assert!(migrated.contains("allocator ="));
        assert!(migrated.contains("general ="));
        assert!(!migrated.contains("[workspace]"));
        assert!(migrated.contains("default = [\"elm-lsp\"]"));
        assert!(migrated.contains("\"log/lsp\", \"sched/lsp\""));
        assert!(migrated.contains("default-features = false"));

        fs::write(
            &manifest,
            r#"[dependencies]
elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros"] }
elmmgr = { path = "custom/elmmgr" }
"#,
        )
        .unwrap();
        let error = migrate_cargo_manifest(&manifest, &test_manifest("service")).unwrap_err();
        assert!(error.contains("定制化 elmmgr"));
        assert!(
            fs::read_to_string(&manifest)
                .unwrap()
                .contains("custom/elmmgr")
        );

        fs::write(
            &manifest,
            r#"[dependencies]
elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros"] }
kernel-api = { path = "custom/kernel-api" }
"#,
        )
        .unwrap();
        let error = migrate_cargo_manifest(&manifest, &test_manifest("service")).unwrap_err();
        assert!(error.contains("定制化 kernel-api"));
        assert!(
            fs::read_to_string(&manifest)
                .unwrap()
                .contains("custom/kernel-api")
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
        migrate_cargo_manifest(&manifest, &test_manifest("service")).unwrap();
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
        let error = migrate_cargo_manifest(&manifest, &test_manifest("manager")).unwrap_err();
        assert!(error.contains("Manager 工程未启用 management feature"));
    }

    #[test]
    fn parses_lsp_source_identity_and_rejects_unknown_fields() {
        let directory = TestDirectory::new("lsp-source-identity");
        let source = directory.path().join("kernel-source");
        fs::create_dir_all(&source).unwrap();
        let digest = [0x5au8; 32];
        fs::write(
            source.join(LSP_SOURCE_IDENTITY_FILE),
            format!(
                "{LSP_SOURCE_MAGIC}\ninterface_sha256={}\npackages=3\n",
                digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        assert_eq!(lsp_source_interface_hash(&source).unwrap(), Some(digest));

        fs::write(
            source.join(LSP_SOURCE_IDENTITY_FILE),
            format!(
                "{LSP_SOURCE_MAGIC}\ninterface_sha256={}\npackages=3\nunknown=1\n",
                digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        assert!(
            lsp_source_interface_hash(&source)
                .unwrap_err()
                .contains("未知字段")
        );
    }

    #[test]
    fn migration_enforces_abort_for_host_lsp_checks() {
        let inserted = ensure_profile_dev_abort("[package]\nname = \"demo\"\n");
        assert!(inserted.contains("[profile.dev]\npanic = \"abort\""));

        let replaced = ensure_profile_dev_abort(
            "[package]\nname = \"demo\"\n\n[profile.dev]\npanic = \"unwind\"\n",
        );
        assert!(replaced.contains("[profile.dev]\npanic = \"abort\""));
        assert!(!replaced.contains("unwind"));
    }

    #[test]
    fn migration_rejects_custom_root_workspace_members() {
        let input = r#"[workspace]
members = [
    ".",
    "helper",
]

[package]
name = "demo"
version = "0.1.0"
edition = "2024"
"#;
        let error = migrate_standard_root_workspace(input).unwrap_err();
        assert!(error.contains("自定义 workspace member helper"));
    }
}
