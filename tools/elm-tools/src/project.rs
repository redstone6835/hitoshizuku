use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::kernel_interface::{
    KernelInterfaceManifest, LSP_SOURCE_IDENTITY_FILE, LSP_SOURCE_MAGIC,
    framework_workspace_manifest, metadata_facade_manifest, metadata_facade_source,
};

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
    write_new(&directory.join("rust-toolchain.toml"), ELM_RUST_TOOLCHAIN)?;
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
    copy_tree(&kernel_symbols_source, &temporary.join("kernel-symbols"))?;
    write_metadata_facade(
        &temporary.join("allocator"),
        "allocator",
        "__elm_host_allocator",
    )?;
    write_metadata_facade(&temporary.join("general"), "general", "__elm_host_general")?;
    fs::write(temporary.join("Cargo.toml"), framework_workspace_manifest())
        .map_err(|err| format!("写入框架 workspace manifest 失败: {err}"))?;
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
    fs::write(project.join(".cargo/config.toml"), ELM_CARGO_CONFIG)
        .map_err(|err| format!("更新 ELM Cargo 配置失败: {err}"))?;
    sync_available_target_interfaces(project)?;
    Ok(())
}

pub fn cargo_build(project: &Path, target: &str, cargo_name: &str) -> Result<PathBuf, String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("定位 {} 失败: {err}", project.display()))?;
    prepare_target_interface(&project, target)?;
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
    if target == "loongarch64-unknown-none" {
        rustflags.push("-Anamed_asm_labels".to_string());
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
        .arg("--no-default-features")
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
    Ok(framework_source_root()?.join("build/elm-interface"))
}

fn sync_available_target_interfaces(project: &Path) -> Result<(), String> {
    let root = interface_bundle_root()?;
    let mut installed_source_hash = None;
    for target in ["riscv64gc-unknown-none-elf", "loongarch64-unknown-none"] {
        let bundle = root.join(target);
        if bundle.join("manifest.txt").is_file() {
            let manifest = KernelInterfaceManifest::load(&bundle.join("manifest.txt"))?;
            if installed_source_hash.is_some_and(|hash| hash != manifest.interface_hash) {
                return Err("不同目标接口包的规范接口摘要不一致".to_string());
            }
            if installed_source_hash.is_none() {
                install_lsp_source(project, &bundle, &manifest, true)?;
                installed_source_hash = Some(manifest.interface_hash);
            }
            copy_target_interface(project, target, &bundle, false)?;
        }
    }
    Ok(())
}

fn prepare_target_interface(project: &Path, target: &str) -> Result<(), String> {
    let destination = project.join(".elm/kernel-interface").join(target);
    if destination.join("manifest.txt").is_file() {
        let manifest = KernelInterfaceManifest::load(&destination.join("manifest.txt"))?;
        if lsp_source_interface_hash(&project.join(".elm/kernel-source"))?
            == Some(manifest.interface_hash)
        {
            return Ok(());
        }
        let bundle = interface_bundle_root()?.join(target);
        if !bundle.join("manifest.txt").is_file() {
            return Err(format!(
                "目标 {target} 已有接口包，但缺少与其匹配的 LSP 源码投影；请执行 elm-tools sync-framework"
            ));
        }
        let bundle_manifest = KernelInterfaceManifest::load(&bundle.join("manifest.txt"))?;
        if bundle_manifest.interface_hash != manifest.interface_hash {
            return Err(format!(
                "目标 {target} 的工程接口包与发布接口包摘要不一致；请执行 elm-tools sync-framework"
            ));
        }
        install_lsp_source(project, &bundle, &manifest, true)?;
        return Ok(());
    }
    let bundle = interface_bundle_root()?.join(target);
    if !bundle.join("manifest.txt").is_file() {
        return Err(format!(
            "缺少目标 {target} 的精确内核接口包；先对对应内核执行 elm-tools export-interface"
        ));
    }
    let manifest = KernelInterfaceManifest::load(&bundle.join("manifest.txt"))?;
    copy_target_interface(project, target, &bundle, true)?;
    install_lsp_source(project, &bundle, &manifest, false)
}

fn copy_target_interface(
    project: &Path,
    target: &str,
    bundle: &Path,
    enforce_existing_coherence: bool,
) -> Result<(), String> {
    let manifest = KernelInterfaceManifest::load(&bundle.join("manifest.txt"))?;
    if manifest.target != target {
        return Err(format!(
            "接口包目标不匹配：目录为 {target}，清单为 {}",
            manifest.target
        ));
    }
    if enforce_existing_coherence {
        ensure_existing_interface_coherence(project, target, manifest.interface_hash)?;
    }
    let root = project.join(".elm/kernel-interface");
    fs::create_dir_all(&root).map_err(|err| format!("创建 {} 失败: {err}", root.display()))?;
    let temporary = root.join(format!("{target}.tmp.{}", std::process::id()));
    let destination = root.join(target);
    remove_if_exists(&temporary)?;
    copy_tree(&bundle.join("metadata"), &temporary.join("metadata"))?;
    let support_library = bundle.join(&manifest.support_library);
    if !support_library.is_file() {
        return Err(format!(
            "接口包缺少 Rust 支持归档: {}",
            support_library.display()
        ));
    }
    fs::copy(&support_library, temporary.join(&manifest.support_library))
        .map_err(|err| format!("复制目标 Rust 支持归档失败: {err}"))?;
    let import_library = bundle.join(&manifest.import_library);
    if !import_library.is_file() {
        return Err(format!(
            "接口包缺少内核导入库: {}",
            import_library.display()
        ));
    }
    fs::copy(&import_library, temporary.join(&manifest.import_library))
        .map_err(|err| format!("复制目标内核导入库失败: {err}"))?;
    fs::copy(bundle.join("manifest.txt"), temporary.join("manifest.txt"))
        .map_err(|err| format!("复制目标接口清单失败: {err}"))?;
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

fn ensure_existing_interface_coherence(
    project: &Path,
    target: &str,
    interface_hash: [u8; 32],
) -> Result<(), String> {
    let root = project.join(".elm/kernel-interface");
    if !root.is_dir() {
        return Ok(());
    }
    for entry in
        fs::read_dir(&root).map_err(|err| format!("读取 {} 失败: {err}", root.display()))?
    {
        let entry = entry.map_err(|err| format!("读取目标接口目录项失败: {err}"))?;
        if entry.file_name() == target || !entry.path().is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.txt");
        if !manifest_path.is_file() {
            continue;
        }
        let existing = KernelInterfaceManifest::load(&manifest_path)?;
        if existing.interface_hash != interface_hash {
            return Err(format!(
                "目标 {target} 与已安装目标 {} 的规范接口摘要不一致；必须整体同步接口包",
                existing.target
            ));
        }
    }
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
    if source_hash != manifest.interface_hash {
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
    copy_tree(&source, &temporary)?;
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
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "{name}"
path = "src/main.rs"
test = false
bench = false

[features]
default = ["elm-lsp"]
elm-lsp = ["allocator/lsp", "general/lsp"]

[dependencies]
elm = {{ path = ".elm/framework/elm", default-features = false, features = [{features}] }}
allocator = {{ path = ".elm/framework/allocator", default-features = false }}
general = {{ path = ".elm/framework/general", default-features = false }}

[profile.release]
panic = "abort"
codegen-units = 1
lto = false
strip = false

[profile.dev]
panic = "abort"
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
        elm::runtime::log(6, shared.as_str()).map_err(|_| HookError::new(-1))?;
        Ok(())
    }}

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {{
        elm::runtime::log(6, "{name}: finalized").map_err(|_| HookError::new(-1))?;
        Ok(())
    }}
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

    output = ensure_facade_dependency(&output, "allocator")?;
    output = ensure_facade_dependency(&output, "general")?;
    output = ensure_lsp_feature(&output)?;
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

fn ensure_lsp_feature(input: &str) -> Result<String, String> {
    const LSP_FEATURE: &str = "elm-lsp = [\"allocator/lsp\", \"general/lsp\"]";

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
        if !line.contains("allocator/lsp") || !line.contains("general/lsp") {
            return Err(
                "Cargo.toml 的 elm-lsp feature 未同时启用 allocator/lsp 与 general/lsp".to_string(),
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
        assert!(!service_cargo.contains("[workspace]"));
        assert!(service_cargo.contains("default = [\"elm-lsp\"]"));
        assert!(service_cargo.contains("elm-lsp = [\"allocator/lsp\", \"general/lsp\"]"));
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
        assert!(service_source.contains("elm::runtime::abort_panic"));
        assert!(service_source.contains("impl ElmModule for Module"));
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
        migrate_cargo_manifest(&manifest, "manager").unwrap();
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
        assert!(migrated.contains("elm-lsp = [\"allocator/lsp\", \"general/lsp\"]"));
        assert!(migrated.contains("default-features = false"));

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

        fs::write(
            &manifest,
            r#"[dependencies]
elm = { path = ".elm/framework/elm", default-features = false, features = ["module", "macros"] }
kernel-api = { path = "custom/kernel-api" }
"#,
        )
        .unwrap();
        let error = migrate_cargo_manifest(&manifest, "service").unwrap_err();
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
