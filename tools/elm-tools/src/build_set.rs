use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use elm::{
    ELM_EKI_PROJECTION_SOURCE_ID, ElmEkiSelector, canonical_ebi_digest, parse_eki_image_for, sha256,
};

use crate::kernel_interface::{KernelInterfaceManifest, hex_digest};
use crate::project::{
    ElmBuildMode, ElmProjectManifest, available_kernel_interfaces, publish_project_api,
};

const MODULE_SET_MAGIC: &str = "ELM-MODULE-SET-V1";
const BUILD_MANIFEST_MAGIC: &str = "ELM-BUILD-MODULES-V1";

#[derive(Debug, Clone)]
struct ModuleSpec {
    name: String,
    path: PathBuf,
    config: String,
    depends: Vec<String>,
    after: Vec<String>,
    targets: Vec<String>,
    features: Vec<String>,
    prompt: String,
    default: ElmBuildMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigMode {
    Config,
    OldConfig,
    DefConfig,
}

#[derive(Debug, Clone)]
struct BuiltManagedModule {
    order: usize,
    name: String,
    file_name: String,
    eki_hash: [u8; 32],
    ebi_hash: [u8; 32],
    capabilities: u64,
}

pub fn build_set(
    set_path: &Path,
    config_path: &Path,
    target: &str,
    output: &Path,
    extra_features: &[String],
) -> Result<(), String> {
    let set_path = set_path
        .canonicalize()
        .map_err(|err| format!("定位模块集合 {} 失败: {err}", set_path.display()))?;
    let set_root = set_path
        .parent()
        .ok_or_else(|| "模块集合路径没有父目录".to_string())?;
    let modules = active_modules(parse_module_set(&set_path)?, target)?;
    let config = parse_config(config_path)?;
    let ordered = topological_order(&modules)?;
    let modes = resolve_modes(&ordered, &config)?;
    validate_enabled_dependencies(&ordered, &modes)?;

    if output.exists() {
        fs::remove_dir_all(output)
            .map_err(|err| format!("清理模块输出目录 {} 失败: {err}", output.display()))?;
    }
    fs::create_dir_all(output)
        .map_err(|err| format!("创建模块输出目录 {} 失败: {err}", output.display()))?;
    let output = output
        .canonicalize()
        .map_err(|err| format!("规范化模块输出目录 {} 失败: {err}", output.display()))?;
    let api_repository = output.join(".apis");
    fs::create_dir_all(&api_repository).map_err(|err| format!("创建 API 仓库失败: {err}"))?;
    let cargo_cache = output.join(".cargo-cache");
    fs::create_dir_all(&cargo_cache)
        .map_err(|err| format!("创建共享 Cargo 缓存目录失败: {err}"))?;
    let shared_cargo_lock = cargo_cache.join("Cargo.lock");

    let interface = selected_interface(target)?;
    // 所有模块共享同一个接口包根。除了避免重复复制接口文件，这个稳定的
    // 绝对路径还会进入 Cargo 的 rustflags/fingerprint，必须在整个 build-set
    // 生命周期内保持一致，Cargo 才能复用公共 facade 和内核 crate。
    let interface_root = interface_repository_root(&interface)?;
    let framework_root = interface_root.join("framework");
    let arch = target_arch_name(target)?;
    let executable =
        std::env::current_exe().map_err(|err| format!("定位 cargo-elm 可执行文件失败: {err}"))?;
    let mut managed = Vec::new();
    let mut integrated = Vec::new();

    for (order, module) in ordered.iter().enumerate() {
        let mode = modes[&module.name];
        if mode == ElmBuildMode::Disabled {
            continue;
        }
        let project = set_root.join(&module.path);
        let manifest = ElmProjectManifest::load(&project)?;
        if manifest.name != module.name {
            return Err(format!(
                "模块集合名称 {} 与 {} 中的名称 {} 不一致",
                module.name,
                project.join("Elm.toml").display(),
                manifest.name
            ));
        }
        let mut command = Command::new(&executable);
        command
            .arg("elm")
            .arg("build")
            .arg(&project)
            .arg("--arch")
            .arg(arch)
            .env("ELM_BUILD_MODE_OVERRIDE", mode.as_str())
            .env("ELM_DEPENDENCY_API_ROOT", &api_repository)
            .env("ELM_KERNEL_INTERFACE_ROOT", &interface_root)
            .env("ELM_SHARED_FRAMEWORK_ROOT", &framework_root)
            .env("ELM_SHARED_CARGO_LOCK", &shared_cargo_lock);
        if mode == ElmBuildMode::Managed {
            command.arg("--unsigned");
        }
        let module_features = module_build_features(&ordered, module, extra_features);
        if !module_features.is_empty() {
            command.arg("--features").arg(module_features.join(","));
        }
        let status = command
            .status()
            .map_err(|err| format!("启动 {} 构建失败: {err}", module.name))?;
        if !status.success() {
            return Err(format!("模块 {} 构建失败，退出状态 {status}", module.name));
        }

        if manifest.api.is_some() {
            // Safety: 构建工具单线程执行，发布 API 时临时覆盖的模式只影响本次清单读取。
            unsafe { std::env::set_var("ELM_BUILD_MODE_OVERRIDE", mode.as_str()) };
            let published = publish_project_api(&project, &api_repository);
            // Safety: 与上面的单线程临时设置成对恢复。
            unsafe { std::env::remove_var("ELM_BUILD_MODE_OVERRIDE") };
            published?;
        }

        match mode {
            ElmBuildMode::Managed => {
                let source = project
                    .join("dist")
                    .join(format!("{}-{arch}.eki", manifest.name));
                if !source.is_file() {
                    return Err(format!("模块构建未生成 {}", source.display()));
                }
                let file_name = format!("{}.eki", manifest.name);
                let destination = output.join(&file_name);
                fs::copy(&source, &destination).map_err(|err| {
                    format!(
                        "安装模块镜像 {} -> {} 失败: {err}",
                        source.display(),
                        destination.display()
                    )
                })?;
                managed.push(inspect_managed_module(
                    order,
                    &manifest.name,
                    file_name,
                    &destination,
                    &interface.manifest,
                )?);
            }
            ElmBuildMode::Integrated => {
                let source = project
                    .join("dist")
                    .join(format!("{}-{target}.integrated.a", manifest.cargo_name()));
                if !source.is_file() {
                    return Err(format!("集成构建未生成 {}", source.display()));
                }
                let destination = output.join(format!("{}.integrated.a", manifest.name));
                fs::copy(&source, &destination).map_err(|err| {
                    format!(
                        "安装集成归档 {} -> {} 失败: {err}",
                        source.display(),
                        destination.display()
                    )
                })?;
                integrated.push(destination);
            }
            ElmBuildMode::Disabled => unreachable!(),
        }
    }

    write_build_manifest(&output, target, &interface.manifest, &managed)?;
    write_integrated_archives(&output, &integrated)?;
    Ok(())
}

fn parse_module_set(path: &Path) -> Result<Vec<ModuleSpec>, String> {
    let input = fs::read_to_string(path)
        .map_err(|err| format!("读取模块集合 {} 失败: {err}", path.display()))?;
    let mut lines = input.lines();
    let version = lines.next().map(str::trim);
    if version != Some("format = \"ELM-MODULE-SET-V1\"") {
        return Err(format!("{} 缺少 {MODULE_SET_MAGIC} 版本头", path.display()));
    }
    let mut records = Vec::<BTreeMap<String, String>>::new();
    let mut current = None;
    for (offset, raw) in lines.enumerate() {
        let line_number = offset + 2;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[modules]]" {
            records.push(BTreeMap::new());
            current = Some(records.len() - 1);
            continue;
        }
        let index = current
            .ok_or_else(|| format!("Modules.toml 第 {line_number} 行位于 [[modules]] 之外"))?;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("Modules.toml 第 {line_number} 行缺少 '='"))?;
        let key = key.trim();
        if !matches!(
            key,
            "name"
                | "path"
                | "config"
                | "depends"
                | "after"
                | "targets"
                | "features"
                | "prompt"
                | "default"
        ) {
            return Err(format!(
                "Modules.toml 第 {line_number} 行包含未知字段 {key}"
            ));
        }
        let value = parse_string(value.trim(), line_number)?;
        if records[index].insert(key.to_string(), value).is_some() {
            return Err(format!("Modules.toml 第 {line_number} 行重复定义 {key}"));
        }
    }
    if records.is_empty() {
        return Err("Modules.toml 没有模块记录".to_string());
    }
    let mut names = BTreeSet::new();
    records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            let required = |key: &str| {
                record
                    .get(key)
                    .cloned()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("[[modules]] #{} 缺少 {key}", index + 1))
            };
            let name = required("name")?;
            if !names.insert(name.clone()) {
                return Err(format!("Modules.toml 重复定义模块 {name}"));
            }
            let path = PathBuf::from(required("path")?);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(format!("模块 {name} 的 path 必须位于集合目录内"));
            }
            let depends = record
                .get("depends")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let after = split_list(record.get("after"));
            let targets = split_list(record.get("targets"));
            let features = split_list(record.get("features"));
            let default = match record.get("default").map(String::as_str).unwrap_or("n") {
                "y" => ElmBuildMode::Integrated,
                "m" => ElmBuildMode::Managed,
                "n" => ElmBuildMode::Disabled,
                value => return Err(format!("模块 {name} 的 default={value} 不是 y/m/n")),
            };
            Ok(ModuleSpec {
                name,
                path,
                config: required("config")?,
                depends,
                after,
                targets,
                features,
                prompt: required("prompt")?,
                default,
            })
        })
        .collect()
}

/// 解析集合级附加特性在当前模块上的有效子集。
///
/// 一旦某个特性由至少一个模块显式声明，该特性只传给声明它的模块；没有任何
/// 声明的特性继续维持旧版全局透传语义，避免破坏现有外部模块集合。
fn module_build_features(
    modules: &[ModuleSpec],
    module: &ModuleSpec,
    requested: &[String],
) -> Vec<String> {
    requested
        .iter()
        .filter(|feature| {
            let scoped = modules
                .iter()
                .any(|candidate| candidate.features.contains(feature));
            !scoped || module.features.contains(feature)
        })
        .cloned()
        .collect()
}

fn split_list(value: Option<&String>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn active_modules(modules: Vec<ModuleSpec>, target: &str) -> Result<Vec<ModuleSpec>, String> {
    let active_names = modules
        .iter()
        .filter(|module| {
            module.targets.is_empty() || module.targets.iter().any(|item| item == target)
        })
        .map(|module| module.name.clone())
        .collect::<BTreeSet<_>>();
    let all_names = modules
        .iter()
        .map(|module| module.name.clone())
        .collect::<BTreeSet<_>>();
    let mut active = Vec::new();
    for mut module in modules {
        if !active_names.contains(&module.name) {
            continue;
        }
        for dependency in &module.depends {
            if !all_names.contains(dependency) {
                return Err(format!("模块 {} 依赖未知模块 {dependency}", module.name));
            }
            if !active_names.contains(dependency) {
                return Err(format!(
                    "模块 {} 在目标 {target} 上依赖未启用的模块 {dependency}",
                    module.name
                ));
            }
        }
        module
            .after
            .retain(|dependency| active_names.contains(dependency));
        active.push(module);
    }
    Ok(active)
}

fn parse_config(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let input = fs::read_to_string(path)
        .map_err(|err| format!("读取配置 {} 失败: {err}", path.display()))?;
    let mut values = BTreeMap::new();
    for (index, raw) in input.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("配置第 {} 行缺少 '='", index + 1))?;
        if !key.starts_with("CONFIG_") || !matches!(value, "y" | "m" | "n") {
            return Err(format!("配置第 {} 行不是 CONFIG_*=y/m/n", index + 1));
        }
        if values.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!("配置重复定义 {key}"));
        }
    }
    Ok(values)
}

fn topological_order(modules: &[ModuleSpec]) -> Result<Vec<ModuleSpec>, String> {
    let by_name = modules
        .iter()
        .map(|module| (module.name.as_str(), module))
        .collect::<BTreeMap<_, _>>();
    for module in modules {
        for dependency in module.depends.iter().chain(&module.after) {
            if !by_name.contains_key(dependency.as_str()) {
                return Err(format!("模块 {} 依赖未知模块 {dependency}", module.name));
            }
        }
    }
    let mut pending = modules.iter().collect::<Vec<_>>();
    let mut emitted = BTreeSet::new();
    let mut output = Vec::new();
    while !pending.is_empty() {
        let before = pending.len();
        let mut index = 0;
        while index < pending.len() {
            if pending[index]
                .depends
                .iter()
                .chain(&pending[index].after)
                .all(|dependency| emitted.contains(dependency))
            {
                let module = pending.remove(index);
                emitted.insert(module.name.clone());
                output.push(module.clone());
            } else {
                index += 1;
            }
        }
        if pending.len() == before {
            return Err("模块集合依赖图包含环".to_string());
        }
    }
    Ok(output)
}

fn resolve_modes(
    modules: &[ModuleSpec],
    config: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ElmBuildMode>, String> {
    let mut modes = BTreeMap::new();
    for module in modules {
        let raw = config
            .get(&module.config)
            .map(String::as_str)
            .unwrap_or(module.default.as_str());
        let mode = match raw {
            "y" => ElmBuildMode::Integrated,
            "m" => ElmBuildMode::Managed,
            "n" => ElmBuildMode::Disabled,
            _ => unreachable!(),
        };
        modes.insert(module.name.clone(), mode);
    }
    Ok(modes)
}

pub fn configure_set(set_path: &Path, config_path: &Path, mode: ConfigMode) -> Result<(), String> {
    let modules = parse_module_set(set_path)?;
    let known = modules
        .iter()
        .map(|module| module.config.clone())
        .collect::<BTreeSet<_>>();
    let existing = if config_path.is_file() {
        parse_config(config_path)?
    } else {
        BTreeMap::new()
    };
    for key in existing.keys() {
        if !known.contains(key) {
            return Err(format!("配置包含 Modules.toml 未声明的键 {key}"));
        }
    }

    let mut selected = BTreeMap::new();
    let mut input = String::new();
    for module in &modules {
        let fallback = if mode == ConfigMode::DefConfig {
            module.default.as_str()
        } else {
            existing
                .get(&module.config)
                .map(String::as_str)
                .unwrap_or(module.default.as_str())
        };
        let value = match mode {
            ConfigMode::DefConfig => fallback,
            ConfigMode::OldConfig if existing.contains_key(&module.config) => fallback,
            ConfigMode::Config | ConfigMode::OldConfig => loop {
                print!(
                    "{} ({}) [y/m/n] ({}): ",
                    module.prompt, module.config, fallback
                );
                io::stdout()
                    .flush()
                    .map_err(|err| format!("刷新配置提示失败: {err}"))?;
                input.clear();
                io::stdin()
                    .read_line(&mut input)
                    .map_err(|err| format!("读取配置输入失败: {err}"))?;
                let answer = input.trim();
                let answer = if answer.is_empty() { fallback } else { answer };
                if matches!(answer, "y" | "m" | "n") {
                    break answer;
                }
                eprintln!("请输入 y、m 或 n");
            },
        };
        selected.insert(module.config.clone(), value.to_string());
    }

    for target in ["loongarch64-unknown-none", "riscv64gc-unknown-none-elf"] {
        let active = active_modules(modules.clone(), target)?;
        let ordered = topological_order(&active)?;
        let modes = resolve_modes(&ordered, &selected)?;
        validate_enabled_dependencies(&ordered, &modes)?;
    }

    let mut output = String::from("# 此文件由 cargo xtask config 生成，请勿提交构建产物。\n");
    for module in modules {
        output.push_str(&module.config);
        output.push('=');
        output.push_str(&selected[&module.config]);
        output.push('\n');
    }
    fs::write(config_path, output)
        .map_err(|err| format!("写入配置 {} 失败: {err}", config_path.display()))
}

fn validate_enabled_dependencies(
    modules: &[ModuleSpec],
    modes: &BTreeMap<String, ElmBuildMode>,
) -> Result<(), String> {
    for module in modules {
        let mode = modes[&module.name];
        if mode == ElmBuildMode::Disabled {
            continue;
        }
        for dependency in &module.depends {
            let dependency_mode = modes[dependency];
            if dependency_mode == ElmBuildMode::Disabled {
                return Err(format!(
                    "模块 {} 已启用，但依赖 {dependency} 已禁用",
                    module.name
                ));
            }
            if dependency_mode != mode {
                return Err(format!(
                    "模块 {} 的模式 {} 与依赖 {dependency} 的模式 {} 不一致",
                    module.name,
                    mode.as_str(),
                    dependency_mode.as_str()
                ));
            }
        }
    }
    Ok(())
}

fn selected_interface(target: &str) -> Result<crate::project::KernelInterfaceBundle, String> {
    let available = available_kernel_interfaces(target)?;
    if available.len() != 1 {
        return Err(format!(
            "build-set 要求目标 {target} 的接口仓库精确包含一个 Profile，当前为 {}",
            available.len()
        ));
    }
    Ok(available.into_iter().next().expect("长度已经校验"))
}

fn interface_repository_root(
    interface: &crate::project::KernelInterfaceBundle,
) -> Result<PathBuf, String> {
    let root = interface
        .root
        .canonicalize()
        .map_err(|err| format!("定位接口包 {} 失败: {err}", interface.root.display()))?;
    if !root.join("manifest.txt").is_file() || !root.join("framework/Cargo.toml").is_file() {
        return Err("接口包目录缺少 manifest.txt 或 framework/Cargo.toml".to_string());
    }
    Ok(root)
}

fn inspect_managed_module(
    order: usize,
    name: &str,
    file_name: String,
    path: &Path,
    interface: &KernelInterfaceManifest,
) -> Result<BuiltManagedModule, String> {
    let bytes = fs::read(path).map_err(|err| format!("读取 {} 失败: {err}", path.display()))?;
    let image = parse_eki_image_for(
        &bytes,
        ElmEkiSelector {
            arch: target_ebi_arch(&interface.target)?,
            profile_hash: interface.interface_hash,
            bridge_abi_version: interface.bridge_abi_version,
        },
    )
    .map_err(|status| format!("解析构建模块 {name} 失败: {status:?}"))?;
    if image.unit.manifest.name.as_str() != name {
        return Err(format!("构建模块名称不匹配: 预期 {name}"));
    }
    let mut capabilities = 0u64;
    for import in image
        .unit
        .imports
        .iter()
        .filter(|import| import.is_kernel_symbol())
    {
        let symbol = interface
            .symbols
            .iter()
            .filter(|symbol| {
                symbol.api_path == import.name
                    && symbol.contract == import.contract.as_str()
                    && import.min_version <= symbol.version
                    && symbol.version <= import.max_version
                    && symbol.rust_abi_hash == import.rust_abi_hash
            })
            .max_by_key(|symbol| symbol.version)
            .ok_or_else(|| format!("模块 {name} 的内核符号导入 {} 无法映射", import.name))?;
        capabilities |= symbol.capabilities;
    }
    Ok(BuiltManagedModule {
        order,
        name: name.to_string(),
        file_name,
        eki_hash: sha256(&bytes),
        ebi_hash: canonical_ebi_digest(&image),
        capabilities,
    })
}

fn write_build_manifest(
    output: &Path,
    target: &str,
    interface: &KernelInterfaceManifest,
    modules: &[BuiltManagedModule],
) -> Result<(), String> {
    let mut text = format!(
        "{BUILD_MANIFEST_MAGIC}\ntarget={target}\nprofile={}\nprofile_sha256={}\nmodule_count={}\n",
        interface.profile,
        hex_digest(&interface.interface_hash),
        modules.len()
    );
    for module in modules {
        text.push_str(&format!(
            "module\t{}\t{}\t{}\t{}\t{}\t{}\t0x{:016x}\n",
            module.order,
            module.name,
            module.file_name,
            ELM_EKI_PROJECTION_SOURCE_ID,
            hex_digest(&module.eki_hash),
            hex_digest(&module.ebi_hash),
            module.capabilities
        ));
    }
    fs::write(output.join("modules.manifest"), text)
        .map_err(|err| format!("写入 BuildBound 模块清单失败: {err}"))
}

fn write_integrated_archives(output: &Path, archives: &[PathBuf]) -> Result<(), String> {
    let mut text = String::new();
    for archive in archives {
        let canonical = archive
            .canonicalize()
            .map_err(|err| format!("定位集成归档 {} 失败: {err}", archive.display()))?;
        text.push_str(&canonical.to_string_lossy());
        text.push('\n');
    }
    fs::write(output.join("integrated.archives"), text)
        .map_err(|err| format!("写入集成归档清单失败: {err}"))
}

fn target_arch_name(target: &str) -> Result<&'static str, String> {
    if target.starts_with("riscv64") {
        Ok("riscv64")
    } else if target.starts_with("loongarch64") {
        Ok("loongarch64")
    } else {
        Err(format!("不支持的模块集合目标: {target}"))
    }
}

fn target_ebi_arch(target: &str) -> Result<elm::ElmEbiArch, String> {
    if target.starts_with("riscv64") {
        Ok(elm::ElmEbiArch::Riscv64)
    } else if target.starts_with("loongarch64") {
        Ok(elm::ElmEbiArch::LoongArch64)
    } else {
        Err(format!("不支持的 EBI 目标: {target}"))
    }
}

fn parse_string(value: &str, line: usize) -> Result<String, String> {
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| format!("Modules.toml 第 {line} 行必须使用双引号字符串"))?;
    if value.contains('\\') || value.contains('"') || value.contains('\0') {
        return Err(format!("Modules.toml 第 {line} 行包含不支持的转义"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, config: &str, depends: &[&str], after: &[&str]) -> ModuleSpec {
        ModuleSpec {
            name: name.to_string(),
            path: PathBuf::from(name),
            config: config.to_string(),
            depends: depends.iter().map(|value| (*value).to_string()).collect(),
            after: after.iter().map(|value| (*value).to_string()).collect(),
            targets: Vec::new(),
            features: Vec::new(),
            prompt: name.to_string(),
            default: ElmBuildMode::Disabled,
        }
    }

    fn virtio_modules() -> Vec<ModuleSpec> {
        vec![
            module("virtio.framework", "CONFIG_VIRTIO", &[], &[]),
            module(
                "virtio.block",
                "CONFIG_VIRTIO_BLK",
                &["virtio.framework"],
                &[],
            ),
        ]
    }

    #[test]
    fn hard_dependency_requires_matching_mode() {
        let modules = topological_order(&virtio_modules()).expect("依赖图应当有效");
        for (parent, expected) in [
            ("y", ElmBuildMode::Integrated),
            ("m", ElmBuildMode::Managed),
        ] {
            let config = BTreeMap::from([
                ("CONFIG_VIRTIO".to_string(), parent.to_string()),
                ("CONFIG_VIRTIO_BLK".to_string(), parent.to_string()),
            ]);
            let modes = resolve_modes(&modules, &config).expect("模式解析应当成功");
            assert_eq!(modes["virtio.framework"], expected);
            assert_eq!(modes["virtio.block"], expected);
            validate_enabled_dependencies(&modules, &modes).expect("依赖模式应当一致");
        }
    }

    #[test]
    fn dependent_module_can_be_disabled_independently() {
        let modules = topological_order(&virtio_modules()).expect("依赖图应当有效");
        let config = BTreeMap::from([
            ("CONFIG_VIRTIO".to_string(), "m".to_string()),
            ("CONFIG_VIRTIO_BLK".to_string(), "n".to_string()),
        ]);
        let modes = resolve_modes(&modules, &config).expect("禁用子模块应当成功");
        assert_eq!(modes["virtio.framework"], ElmBuildMode::Managed);
        assert_eq!(modes["virtio.block"], ElmBuildMode::Disabled);
        validate_enabled_dependencies(&modules, &modes).expect("禁用子模块不破坏依赖");
    }

    #[test]
    fn enabled_module_rejects_disabled_dependency() {
        let modules = topological_order(&virtio_modules()).expect("依赖图应当有效");
        let config = BTreeMap::from([
            ("CONFIG_VIRTIO".to_string(), "n".to_string()),
            ("CONFIG_VIRTIO_BLK".to_string(), "y".to_string()),
        ]);
        let modes = resolve_modes(&modules, &config).expect("模式解析应当成功");
        let error = validate_enabled_dependencies(&modules, &modes)
            .expect_err("禁用依赖必须拒绝启用子模块");
        assert!(error.contains("依赖 virtio.framework 已禁用"));
    }

    #[test]
    fn declared_build_feature_is_only_passed_to_its_owner() {
        let mut modules = virtio_modules();
        modules[1].features.push("block-profile".to_string());
        let requested = vec!["block-profile".to_string()];

        assert!(module_build_features(&modules, &modules[0], &requested).is_empty());
        assert_eq!(
            module_build_features(&modules, &modules[1], &requested),
            requested
        );
    }

    #[test]
    fn undeclared_build_feature_keeps_global_compatibility() {
        let modules = virtio_modules();
        let requested = vec!["diagnostic".to_string()];

        for module in &modules {
            assert_eq!(
                module_build_features(&modules, module, &requested),
                requested
            );
        }
    }
}
