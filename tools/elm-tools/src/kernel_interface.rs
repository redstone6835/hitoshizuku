use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use elm::{Sha256, sha256};
use kernel_symbols::{
    KERNEL_SYMBOL_KIND_FUNCTION, KERNEL_SYMBOL_KIND_METHOD, KERNEL_SYMBOL_KIND_STATIC,
};
use quote::{ToTokens, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, Item, LitInt, LitStr, ReturnType, Signature, Token, Type};

const INTERFACE_MAGIC: &str = "ELM-KERNEL-INTERFACE-V1";
const KERNEL_API_PROFILE_DOMAIN: &[u8] = b"ELM-KERNEL-API-PROFILE-V1\0";
const FRAMEWORK_DISTRIBUTION_DOMAIN: &[u8] = b"ELM-FRAMEWORK-DISTRIBUTION-V1\0";
pub(crate) const KERNEL_API_BRIDGE_ABI_V1: u16 = 1;
pub(crate) const KERNEL_API_MODE_EXACT_RUST: &str = "exact-rust";
pub(crate) const LSP_SOURCE_IDENTITY_FILE: &str = "interface.identity";
pub(crate) const LSP_SOURCE_MAGIC: &str = "ELM-KERNEL-LSP-SOURCE-V1";

#[derive(Debug, Clone)]
pub struct KernelInterfaceSymbol {
    pub kind: u8,
    pub flags: u32,
    pub version: u32,
    pub capabilities: u64,
    pub retained_argument_mask: u64,
    pub interface_hash: [u8; 32],
    pub api_path: String,
    pub item_path: String,
    pub link_name: String,
    pub contract: String,
    pub rust_abi: String,
    pub rust_abi_hash: [u8; 32],
    pub abi_mode: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KernelInterfaceManifest {
    pub target: String,
    pub profile: String,
    pub bridge_abi_version: u16,
    pub kernel_hash: [u8; 32],
    pub interface_hash: [u8; 32],
    pub source_hash: [u8; 32],
    pub framework_hash: [u8; 32],
    pub source_file_count: usize,
    pub allocator_metadata: String,
    pub general_metadata: String,
    pub support_library: String,
    pub import_library: String,
    pub symbols: Vec<KernelInterfaceSymbol>,
}

impl KernelInterfaceManifest {
    pub fn load(path: &Path) -> Result<Self, String> {
        let input = fs::read_to_string(path)
            .map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
        let mut lines = input.lines();
        if lines.next() != Some(INTERFACE_MAGIC) {
            return Err(format!("{} 不是 ELM 内核接口清单", path.display()));
        }
        let mut target = None;
        let mut profile = None;
        let mut bridge_abi_version = None;
        let mut kernel_hash = None;
        let mut interface_hash = None;
        let mut source_hash = None;
        let mut framework_hash = None;
        let mut source_file_count = None;
        let mut allocator_metadata = None;
        let mut general_metadata = None;
        let mut support_library = None;
        let mut import_library = None;
        let mut symbols = Vec::new();
        for line in lines {
            if let Some(value) = line.strip_prefix("target=") {
                target = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("profile=") {
                profile = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("bridge_abi=") {
                bridge_abi_version = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| "接口清单 bridge_abi 无效".to_string())?,
                );
            } else if let Some(value) = line.strip_prefix("kernel_sha256=") {
                kernel_hash = Some(parse_digest(value)?);
            } else if let Some(value) = line.strip_prefix("interface_sha256=") {
                interface_hash = Some(parse_digest(value)?);
            } else if let Some(value) = line.strip_prefix("source_sha256=") {
                source_hash = Some(parse_digest(value)?);
            } else if let Some(value) = line.strip_prefix("framework_sha256=") {
                framework_hash = Some(parse_digest(value)?);
            } else if let Some(value) = line.strip_prefix("source_files=") {
                source_file_count = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| "接口清单 source_files 无效".to_string())?,
                );
            } else if let Some(value) = line.strip_prefix("allocator_metadata=") {
                allocator_metadata = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("general_metadata=") {
                general_metadata = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("support_library=") {
                support_library = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("import_library=") {
                import_library = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("symbol\t") {
                symbols.push(parse_symbol_record(value)?);
            } else if line.starts_with("symbol_count=") || line.is_empty() {
                continue;
            } else {
                return Err(format!("{} 包含未知接口清单记录: {line}", path.display()));
            }
        }
        let interface_hash =
            interface_hash.ok_or_else(|| "接口清单缺少 interface hash".to_string())?;
        for symbol in &mut symbols {
            symbol.interface_hash = interface_hash;
            symbol.rust_abi_hash = sha256(symbol.rust_abi.as_bytes());
        }
        let manifest = Self {
            target: target.ok_or_else(|| "接口清单缺少 target".to_string())?,
            profile: profile.ok_or_else(|| "接口清单缺少 profile".to_string())?,
            bridge_abi_version: bridge_abi_version
                .ok_or_else(|| "接口清单缺少 bridge_abi".to_string())?,
            kernel_hash: kernel_hash.ok_or_else(|| "接口清单缺少 kernel hash".to_string())?,
            interface_hash,
            source_hash: source_hash.ok_or_else(|| "接口清单缺少 source hash".to_string())?,
            framework_hash: framework_hash
                .ok_or_else(|| "接口清单缺少 framework hash".to_string())?,
            source_file_count: source_file_count
                .ok_or_else(|| "接口清单缺少 source_files".to_string())?,
            allocator_metadata: allocator_metadata
                .ok_or_else(|| "接口清单缺少 allocator metadata".to_string())?,
            general_metadata: general_metadata
                .ok_or_else(|| "接口清单缺少 general metadata".to_string())?,
            support_library: support_library
                .ok_or_else(|| "接口清单缺少 support library".to_string())?,
            import_library: import_library
                .ok_or_else(|| "接口清单缺少 import library".to_string())?,
            symbols,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.target.is_empty()
            || self.profile.is_empty()
            || self.bridge_abi_version != KERNEL_API_BRIDGE_ABI_V1
            || self.interface_hash == [0; 32]
            || self.source_hash == [0; 32]
            || self.framework_hash == [0; 32]
            || self.kernel_hash == [0; 32]
            || self.source_file_count == 0
            || self.support_library.is_empty()
            || self.import_library.is_empty()
            || self.symbols.is_empty()
        {
            return Err("内核接口清单缺少必需身份信息".to_string());
        }
        if self.profile.len() > 64
            || !self.profile.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'-' | b'_')
            })
        {
            return Err(format!(
                "内核 API Profile identifier 无效: {}",
                self.profile
            ));
        }
        if kernel_api_profile_hash(&self.target, &self.profile, &self.symbols)
            != self.interface_hash
        {
            return Err("内核 API Profile 摘要与符号清单不一致".to_string());
        }
        let mut api_paths = BTreeSet::new();
        let mut link_names = BTreeMap::new();
        for symbol in &self.symbols {
            if symbol.interface_hash != self.interface_hash
                || symbol.rust_abi_hash != sha256(symbol.rust_abi.as_bytes())
            {
                return Err(format!("内核接口符号摘要错误: {}", symbol.api_path));
            }
            if symbol.abi_mode != KERNEL_API_MODE_EXACT_RUST {
                return Err(format!(
                    "内核接口 {} 声明了尚未实现的 ABI 模式 {}",
                    symbol.api_path, symbol.abi_mode
                ));
            }
            if !api_paths.insert(symbol.api_path.as_str()) {
                return Err(format!("内核接口 API 路径冲突: {}", symbol.api_path));
            }
            let api_leaf = symbol.api_path.rsplit('.').next().unwrap_or_default();
            let item_leaf = symbol.item_path.rsplit("::").next().unwrap_or_default();
            if symbol.kind == KERNEL_SYMBOL_KIND_FUNCTION
                && api_leaf != item_leaf
                && symbol.aliases.is_empty()
                && !symbol.api_path.starts_with("allocator.GlobalAlloc.")
            {
                return Err(format!(
                    "内核接口 {} 既没有同名源码入口，也没有经过审核的 Rust 方法别名",
                    symbol.api_path
                ));
            }
            for link_name in std::iter::once(&symbol.link_name).chain(&symbol.aliases) {
                if link_name.is_empty() {
                    return Err(format!("内核接口包含空链接符号: {}", symbol.api_path));
                }
                if let Some(previous) = link_names.insert(link_name.as_str(), &symbol.api_path) {
                    return Err(format!(
                        "内核接口链接符号冲突: {link_name} 同时属于 {previous} 和 {}",
                        symbol.api_path
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn symbol_by_link_name(&self, name: &str) -> Option<&KernelInterfaceSymbol> {
        let mut matches = self.symbols.iter().filter(|symbol| {
            symbol.link_name == name
                || symbol.aliases.iter().any(|alias| alias == name)
                || name.starts_with(&legacy_mangled_prefix(&symbol.api_path))
        });
        let symbol = matches.next()?;
        matches.next().is_none().then_some(symbol)
    }

    fn encode(&self) -> String {
        let mut output = String::new();
        output.push_str(INTERFACE_MAGIC);
        output.push('\n');
        output.push_str("target=");
        output.push_str(&self.target);
        output.push('\n');
        output.push_str("profile=");
        output.push_str(&self.profile);
        output.push('\n');
        output.push_str(&format!("bridge_abi={}\n", self.bridge_abi_version));
        output.push_str("kernel_sha256=");
        output.push_str(&hex_digest(&self.kernel_hash));
        output.push('\n');
        output.push_str("interface_sha256=");
        output.push_str(&hex_digest(&self.interface_hash));
        output.push('\n');
        output.push_str("source_sha256=");
        output.push_str(&hex_digest(&self.source_hash));
        output.push('\n');
        output.push_str("framework_sha256=");
        output.push_str(&hex_digest(&self.framework_hash));
        output.push('\n');
        output.push_str(&format!("source_files={}\n", self.source_file_count));
        output.push_str("allocator_metadata=");
        output.push_str(&self.allocator_metadata);
        output.push('\n');
        output.push_str("general_metadata=");
        output.push_str(&self.general_metadata);
        output.push('\n');
        output.push_str("support_library=");
        output.push_str(&self.support_library);
        output.push('\n');
        output.push_str("import_library=");
        output.push_str(&self.import_library);
        output.push('\n');
        output.push_str(&format!("symbol_count={}\n", self.symbols.len()));
        for symbol in &self.symbols {
            output.push_str("symbol\t");
            output.push_str(&symbol.kind.to_string());
            output.push('\t');
            output.push_str(&symbol.flags.to_string());
            output.push('\t');
            output.push_str(&symbol.version.to_string());
            output.push('\t');
            output.push_str(&symbol.capabilities.to_string());
            output.push('\t');
            output.push_str(&symbol.retained_argument_mask.to_string());
            output.push('\t');
            output.push_str(&symbol.api_path);
            output.push('\t');
            output.push_str(&symbol.item_path);
            output.push('\t');
            output.push_str(&symbol.link_name);
            output.push('\t');
            output.push_str(&symbol.contract);
            output.push('\t');
            output.push_str(&hex_bytes(symbol.rust_abi.as_bytes()));
            output.push('\t');
            output.push_str(&symbol.abi_mode);
            output.push('\t');
            output.push_str(&symbol.aliases.join(","));
            output.push('\n');
        }
        output
    }
}

pub fn export_kernel_interface(
    repository: &Path,
    target: &str,
    profile: &str,
    cargo_profile: &str,
    kernel: &Path,
    output: &Path,
) -> Result<KernelInterfaceManifest, String> {
    let kernel_bytes = fs::read(kernel)
        .map_err(|error| format!("读取内核镜像 {} 失败: {error}", kernel.display()))?;
    let (source_hash, source_file_count) = repository_interface_hash(repository)?;
    let framework_hash = framework_distribution_hash(repository)?;
    let mut symbols = scan_repository_exports(repository, [0; 32])?;

    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                repository.join(path)
            }
        })
        .unwrap_or_else(|| repository.join("target"));
    let deps = target_root.join(target).join(cargo_profile).join("deps");
    let general_path = newest_matching_file(&deps, "libgeneral-", ".rmeta")?;
    let general_file = general_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "general rmeta 文件名不是 UTF-8".to_string())?
        .replace(".rmeta", ".rlib");
    let root = rustc_crate_root(&general_path)?;
    let allocator_metadata = dependency_metadata_name(&root, "allocator")?;
    let allocator_file = format!("lib{allocator_metadata}.rlib");
    if !deps.join(&allocator_file).is_file() || !deps.join(&general_file).is_file() {
        return Err("kernel rmeta 引用的 allocator/general 精确元数据不存在".to_string());
    }
    let interface_rlibs = vec![deps.join(&allocator_file), deps.join(&general_file)];
    let allocator_root = rustc_crate_root(&deps.join(&allocator_file))?;
    let metadata_rlibs = exact_dependency_rlibs(
        &deps,
        &[&root, &allocator_root],
        &[deps.join(&general_file), deps.join(&allocator_file)],
    )?;
    let public_api_abis = scan_repository_api_abis(repository, &symbols)?;
    populate_link_aliases(target, &interface_rlibs, &public_api_abis, &mut symbols)?;
    let interface_hash = kernel_api_profile_hash(target, profile, &symbols);
    for symbol in &mut symbols {
        symbol.interface_hash = interface_hash;
        symbol.rust_abi_hash = sha256(symbol.rust_abi.as_bytes());
    }

    let manifest = KernelInterfaceManifest {
        target: target.to_string(),
        profile: profile.to_string(),
        bridge_abi_version: KERNEL_API_BRIDGE_ABI_V1,
        kernel_hash: sha256(&kernel_bytes),
        interface_hash,
        source_hash,
        framework_hash,
        source_file_count,
        allocator_metadata: allocator_file,
        general_metadata: general_file,
        support_library: "libelm-rust-support.a".to_string(),
        import_library: "libelm-kernel-imports.so".to_string(),
        symbols,
    };
    manifest.validate()?;

    let temporary = output.with_extension(format!("tmp.{}", std::process::id()));
    remove_path(&temporary)?;
    fs::create_dir_all(temporary.join("metadata"))
        .map_err(|error| format!("创建接口包临时目录失败: {error}"))?;
    copy_metadata_rlibs(&metadata_rlibs, &temporary.join("metadata"))?;
    copy_proc_macro_directory(
        &target_root.join("release/deps"),
        &temporary.join("metadata"),
    )?;
    build_rust_support_library(
        target,
        &metadata_rlibs,
        &temporary.join(&manifest.support_library),
    )?;
    build_import_library(target, &manifest, &temporary.join(&manifest.import_library))?;
    copy_lsp_source_snapshot(
        repository,
        &temporary.join("kernel-source"),
        manifest.source_hash,
    )?;
    copy_framework(repository, &temporary.join("framework"), target, &manifest)?;
    fs::write(temporary.join("manifest.txt"), manifest.encode())
        .map_err(|error| format!("写入接口清单失败: {error}"))?;
    remove_path(output)?;
    fs::rename(&temporary, output)
        .map_err(|error| format!("安装内核接口包 {} 失败: {error}", output.display()))?;
    Ok(manifest)
}

pub fn emit_kernel_symbol_probe(manifest_path: &Path, output: &Path) -> Result<usize, String> {
    let manifest = KernelInterfaceManifest::load(manifest_path)?;
    let mut source = String::from(
        "//! 由 `cargo elm` 按精确内核接口清单生成。\n\
         //! 本文件只取得符号地址，不调用具有副作用或硬件约束的入口。\n\n",
    );
    writeln!(
        source,
        "/// 生成探针覆盖的内核直接符号数量。\npub const EXPORTED_SYMBOL_COUNT: usize = {};\n",
        manifest.symbols.len()
    )
    .map_err(|_| "生成内核符号探针失败".to_string())?;
    source.push_str("unsafe extern \"Rust\" {\n");
    for (index, symbol) in manifest.symbols.iter().enumerate() {
        writeln!(source, "    /// `{}`。", symbol.api_path)
            .map_err(|_| "生成内核符号探针失败".to_string())?;
        writeln!(source, "    #[link_name = \"{}\"]", symbol.link_name)
            .map_err(|_| "生成内核符号探针失败".to_string())?;
        if symbol.kind == KERNEL_SYMBOL_KIND_STATIC {
            writeln!(source, "    static ELM_KERNEL_SYMBOL_{index:04}: u8;")
                .map_err(|_| "生成内核符号探针失败".to_string())?;
        } else {
            writeln!(source, "    fn ELM_KERNEL_SYMBOL_{index:04}();")
                .map_err(|_| "生成内核符号探针失败".to_string())?;
        }
    }
    source.push_str("}\n\n");
    source.push_str(
        "/// 强制链接当前清单中的全部内核直接符号。\n\
         ///\n\
         /// 函数不会调用这些入口；实际功能验证由 demo 的生命周期实现负责。\n\
         #[inline(never)]\n\
         pub fn touch_all() -> usize {\n\
             let mut count = 0usize;\n",
    );
    for (index, symbol) in manifest.symbols.iter().enumerate() {
        if symbol.kind == KERNEL_SYMBOL_KIND_STATIC {
            writeln!(
                source,
                "    core::hint::black_box(core::ptr::addr_of!(ELM_KERNEL_SYMBOL_{index:04}));"
            )
            .map_err(|_| "生成内核符号探针失败".to_string())?;
        } else {
            writeln!(
                source,
                "    core::hint::black_box(ELM_KERNEL_SYMBOL_{index:04} as *const ());"
            )
            .map_err(|_| "生成内核符号探针失败".to_string())?;
        }
        source.push_str("    count += 1;\n");
    }
    source.push_str("    count\n}\n");
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建符号探针输出目录失败: {error}"))?;
    }
    fs::write(output, source)
        .map_err(|error| format!("写入内核符号探针 {} 失败: {error}", output.display()))?;
    Ok(manifest.symbols.len())
}

fn repository_interface_hash(repository: &Path) -> Result<([u8; 32], usize), String> {
    let mut files = Vec::new();
    collect_rust_sources(
        &repository.join("libs/allocator/src"),
        "allocator/src",
        &mut files,
    )?;
    collect_rust_sources(
        &repository.join("general/src/dev"),
        "general/src/dev",
        &mut files,
    )?;
    files.push((
        "allocator/Cargo.toml".to_string(),
        repository.join("libs/allocator/Cargo.toml"),
    ));
    files.push((
        "general/Cargo.toml".to_string(),
        repository.join("general/Cargo.toml"),
    ));
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut input = Vec::new();
    input.extend_from_slice(b"ELM-KERNEL-EXACT-INTERFACE-V1\0RUST-MONOMORPHIZATION=LOCAL\0");
    for (logical, physical) in &files {
        let contents = fs::read(physical)
            .map_err(|error| format!("读取接口源码 {} 失败: {error}", physical.display()))?;
        input.extend_from_slice(&(logical.len() as u64).to_le_bytes());
        input.extend_from_slice(logical.as_bytes());
        input.extend_from_slice(&(contents.len() as u64).to_le_bytes());
        input.extend_from_slice(&contents);
    }
    Ok((sha256(&input), files.len()))
}

fn framework_distribution_hash(repository: &Path) -> Result<[u8; 32], String> {
    let mut files = Vec::new();
    collect_framework_files(&repository.join("libs/elm"), "elm", &mut files)?;
    collect_framework_files(
        &repository.join("libs/kernel-symbols"),
        "kernel-symbols",
        &mut files,
    )?;
    files.push((
        "allocator/Cargo.toml".to_string(),
        metadata_facade_manifest("allocator", "__elm_host_allocator").into_bytes(),
    ));
    files.push((
        "allocator/src/lib.rs".to_string(),
        metadata_facade_source("allocator", "__elm_host_allocator").into_bytes(),
    ));
    files.push((
        "general/Cargo.toml".to_string(),
        metadata_facade_manifest("general", "__elm_host_general").into_bytes(),
    ));
    files.push((
        "general/src/lib.rs".to_string(),
        metadata_facade_source("general", "__elm_host_general").into_bytes(),
    ));
    files.push((
        "Cargo.toml".to_string(),
        framework_workspace_manifest().as_bytes().to_vec(),
    ));
    Ok(hash_framework_distribution(files))
}

pub(crate) fn packaged_framework_hash(framework: &Path) -> Result<[u8; 32], String> {
    let mut files = Vec::new();
    collect_framework_files(framework, "", &mut files)?;
    Ok(hash_framework_distribution(files))
}

fn hash_framework_distribution(mut files: Vec<(String, Vec<u8>)>) -> [u8; 32] {
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hash = Sha256::new();
    hash.update(FRAMEWORK_DISTRIBUTION_DOMAIN);
    hash.update(&(files.len() as u64).to_le_bytes());
    for (logical, contents) in files {
        hash_field(&mut hash, logical.as_bytes());
        hash_field(&mut hash, &contents);
    }
    hash.finish()
}

fn collect_framework_files(
    directory: &Path,
    prefix: &str,
    output: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("读取 framework 目录 {} 失败: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 framework 目录项失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if matches!(entry.file_name().to_str(), Some("target" | ".git")) {
            continue;
        }
        if prefix == "kernel-symbols"
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("interface.identity"))
        {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 framework 文件类型失败: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let logical = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if file_type.is_dir() {
            collect_framework_files(&path, &logical, output)?;
        } else if file_type.is_file() {
            let contents = fs::read(&path)
                .map_err(|error| format!("读取 framework 文件 {} 失败: {error}", path.display()))?;
            output.push((logical, contents));
        } else {
            return Err(format!(
                "framework 源包含不支持的文件类型: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn collect_rust_sources(
    directory: &Path,
    prefix: &str,
    output: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("读取 {} 失败: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取接口源码目录项失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let logical = format!("{prefix}/{name}");
        if path.is_dir() {
            collect_rust_sources(&path, &logical, output)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push((logical, path));
        }
    }
    Ok(())
}

fn scan_repository_exports(
    repository: &Path,
    interface_hash: [u8; 32],
) -> Result<Vec<KernelInterfaceSymbol>, String> {
    let mut sources = vec![
        (
            repository.join("libs/allocator/src/lib.rs"),
            "allocator".to_string(),
        ),
        (
            repository.join("libs/allocator/src/kernel_symbols.rs"),
            "allocator::direct_symbols".to_string(),
        ),
    ];
    collect_export_sources(
        &repository.join("general/src/dev"),
        "general::dev",
        &mut sources,
    )?;
    let mut symbols = Vec::new();
    for (path, module_path) in sources {
        let input = fs::read_to_string(&path)
            .map_err(|error| format!("读取导出源码 {} 失败: {error}", path.display()))?;
        if !input.contains("kernel_symbols::export") {
            continue;
        }
        let syntax = syn::parse_file(&input)
            .map_err(|error| format!("解析导出源码 {} 失败: {error}", path.display()))?;
        for item in syntax.items {
            match item {
                Item::Fn(function) => {
                    let Some(args) = export_args(&function.attrs)? else {
                        continue;
                    };
                    let mut flags =
                        args.flags.as_ref().map(eval_u64).transpose()?.unwrap_or(0) as u32;
                    if function.sig.unsafety.is_some() {
                        flags |= kernel_symbols::KERNEL_SYMBOL_FLAG_UNSAFE;
                    }
                    let retained = args
                        .retained_args
                        .as_ref()
                        .map(eval_u64)
                        .transpose()?
                        .unwrap_or(0);
                    if retained != 0 {
                        flags |= kernel_symbols::KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE;
                    }
                    let abi = canonical_function_abi(&function.sig)?;
                    symbols.push(make_symbol(
                        KERNEL_SYMBOL_KIND_FUNCTION,
                        flags,
                        &args,
                        retained,
                        interface_hash,
                        format!("{module_path}::{}", function.sig.ident),
                        abi,
                    )?);
                }
                Item::Static(item) => {
                    let Some(args) = export_args(&item.attrs)? else {
                        continue;
                    };
                    if args.retained_args.is_some() {
                        return Err(format!("静态导出 {} 不能声明 retained_args", item.ident));
                    }
                    let ty = item.ty.as_ref();
                    symbols.push(make_symbol(
                        KERNEL_SYMBOL_KIND_STATIC,
                        args.flags.as_ref().map(eval_u64).transpose()?.unwrap_or(0) as u32,
                        &args,
                        0,
                        interface_hash,
                        format!("{module_path}::{}", item.ident),
                        normalize_abi_tokens(quote!(static #ty)),
                    )?);
                }
                Item::Impl(item) => {
                    let self_ty = item.self_ty.as_ref();
                    for implementation_item in item.items {
                        let syn::ImplItem::Fn(method) = implementation_item else {
                            continue;
                        };
                        let Some(args) = export_args(&method.attrs)? else {
                            continue;
                        };
                        let retained = args
                            .retained_args
                            .as_ref()
                            .map(eval_u64)
                            .transpose()?
                            .unwrap_or(0);
                        let mut flags =
                            args.flags.as_ref().map(eval_u64).transpose()?.unwrap_or(0) as u32;
                        if retained != 0 {
                            flags |= kernel_symbols::KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE;
                        }
                        if method.sig.unsafety.is_some() {
                            flags |= kernel_symbols::KERNEL_SYMBOL_FLAG_UNSAFE;
                        }
                        symbols.push(make_symbol(
                            KERNEL_SYMBOL_KIND_METHOD,
                            flags,
                            &args,
                            retained,
                            interface_hash,
                            format!(
                                "{module_path}::{}::{}",
                                normalize_abi_tokens(self_ty.to_token_stream()),
                                method.sig.ident
                            ),
                            canonical_method_abi(&method.sig, self_ty)?,
                        )?);
                    }
                }
                _ => {}
            }
        }
    }
    symbols.sort_by(|left, right| left.api_path.cmp(&right.api_path));
    if symbols.is_empty() {
        return Err("allocator/general 没有任何内核符号导出".to_string());
    }
    Ok(symbols)
}

fn scan_repository_api_abis(
    repository: &Path,
    symbols: &[KernelInterfaceSymbol],
) -> Result<BTreeMap<String, String>, String> {
    let expected = symbols
        .iter()
        .map(|symbol| symbol.api_path.as_str())
        .collect::<BTreeSet<_>>();
    let mut sources = vec![(
        repository.join("libs/allocator/src/lib.rs"),
        "allocator".to_string(),
    )];
    collect_export_sources(
        &repository.join("general/src/dev"),
        "general::dev",
        &mut sources,
    )?;
    let mut methods = BTreeMap::new();
    for (path, module_path) in sources {
        let input = fs::read_to_string(&path)
            .map_err(|error| format!("读取公开 API 源码 {} 失败: {error}", path.display()))?;
        let syntax = syn::parse_file(&input)
            .map_err(|error| format!("解析公开 API 源码 {} 失败: {error}", path.display()))?;
        for item in syntax.items {
            let Item::Impl(item) = item else {
                continue;
            };
            if item.trait_.is_some() {
                continue;
            }
            let Type::Path(self_path) = item.self_ty.as_ref() else {
                continue;
            };
            let Some(self_name) = self_path.path.segments.last().map(|segment| &segment.ident)
            else {
                continue;
            };
            for implementation_item in item.items {
                let syn::ImplItem::Fn(method) = implementation_item else {
                    continue;
                };
                if !matches!(method.vis, syn::Visibility::Public(_)) {
                    continue;
                }
                let api_path = format!(
                    "{}.{}.{}",
                    module_path.replace("::", "."),
                    self_name,
                    method.sig.ident
                );
                if !expected.contains(api_path.as_str()) {
                    continue;
                }
                let abi = canonical_method_abi(&method.sig, item.self_ty.as_ref())?;
                if let Some(previous) = methods.insert(api_path.clone(), abi.clone())
                    && previous != abi
                {
                    return Err(format!("公开 API {api_path} 存在冲突签名"));
                }
            }
        }
    }
    Ok(methods)
}

fn collect_export_sources(
    directory: &Path,
    module_prefix: &str,
    output: &mut Vec<(PathBuf, String)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("读取 {} 失败: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取导出源码目录项失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        if path.is_dir() {
            collect_export_sources(&path, &format!("{module_prefix}::{stem}"), output)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") && stem != "mod" {
            output.push((path, format!("{module_prefix}::{stem}")));
        }
    }
    Ok(())
}

struct SourceExportArgs {
    name: LitStr,
    contract: LitStr,
    version: u32,
    capabilities: Expr,
    flags: Option<Expr>,
    retained_args: Option<Expr>,
}

impl Parse for SourceExportArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut contract = None;
        let mut version = None;
        let mut capabilities = None;
        let mut flags = None;
        let mut retained_args = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "contract" => contract = Some(input.parse()?),
                "version" => {
                    let value: LitInt = input.parse()?;
                    version = Some(value.base10_parse()?);
                }
                "capabilities" => capabilities = Some(input.parse()?),
                "flags" => flags = Some(input.parse()?),
                "retained_args" => retained_args = Some(input.parse()?),
                _ => return Err(syn::Error::new_spanned(key, "未知内核符号导出参数")),
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("缺少 name"))?,
            contract: contract.ok_or_else(|| input.error("缺少 contract"))?,
            version: version.ok_or_else(|| input.error("缺少 version"))?,
            capabilities: capabilities.ok_or_else(|| input.error("缺少 capabilities"))?,
            flags,
            retained_args,
        })
    }
}

fn export_args(attributes: &[syn::Attribute]) -> Result<Option<SourceExportArgs>, String> {
    let Some(attribute) = attributes.iter().find(|attribute| {
        let segments = &attribute.path().segments;
        segments.len() == 2
            && segments[0].ident == "kernel_symbols"
            && segments[1].ident == "export"
    }) else {
        return Ok(None);
    };
    let syn::Meta::List(list) = &attribute.meta else {
        return Ok(None);
    };
    syn::parse2(list.tokens.clone())
        .map(Some)
        .map_err(|error| format!("解析 kernel_symbols::export 失败: {error}"))
}

fn make_symbol(
    kind: u8,
    flags: u32,
    args: &SourceExportArgs,
    retained_argument_mask: u64,
    interface_hash: [u8; 32],
    item_path: String,
    rust_abi: String,
) -> Result<KernelInterfaceSymbol, String> {
    let capabilities = eval_u64(&args.capabilities)?;
    let api_path = args.name.value();
    let link_name = stable_link_name(&api_path);
    Ok(KernelInterfaceSymbol {
        kind,
        flags,
        version: args.version,
        capabilities,
        retained_argument_mask,
        interface_hash,
        rust_abi_hash: sha256(rust_abi.as_bytes()),
        abi_mode: KERNEL_API_MODE_EXACT_RUST.to_string(),
        api_path,
        item_path,
        link_name,
        contract: args.contract.value(),
        rust_abi,
        aliases: Vec::new(),
    })
}

fn eval_u64(expression: &Expr) -> Result<u64, String> {
    match expression {
        Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(value) => value
                .base10_parse()
                .map_err(|_| "整数导出参数无效".to_string()),
            _ => Err("导出参数必须是整数表达式".to_string()),
        },
        Expr::Binary(binary) if matches!(binary.op, syn::BinOp::BitOr(_)) => {
            Ok(eval_u64(&binary.left)? | eval_u64(&binary.right)?)
        }
        Expr::Paren(parenthesized) => eval_u64(&parenthesized.expr),
        Expr::Group(grouped) => eval_u64(&grouped.expr),
        Expr::Path(path) => {
            let name = path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
                .ok_or_else(|| "空导出常量路径".to_string())?;
            exported_constant(&name).ok_or_else(|| format!("未知导出常量: {name}"))
        }
        _ => Err(format!(
            "不支持的导出整数表达式: {}",
            expression.to_token_stream()
        )),
    }
}

fn kernel_api_profile_hash(
    _target: &str,
    _profile: &str,
    symbols: &[KernelInterfaceSymbol],
) -> [u8; 32] {
    let mut ordered = symbols.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.api_path
            .cmp(&right.api_path)
            .then_with(|| left.contract.cmp(&right.contract))
            .then_with(|| left.version.cmp(&right.version))
    });
    let mut hash = Sha256::new();
    hash.update(KERNEL_API_PROFILE_DOMAIN);
    hash.update(&KERNEL_API_BRIDGE_ABI_V1.to_le_bytes());
    hash.update(&(ordered.len() as u64).to_le_bytes());
    for symbol in ordered {
        hash.update(&[symbol.kind]);
        hash.update(&symbol.flags.to_le_bytes());
        hash.update(&symbol.version.to_le_bytes());
        hash.update(&symbol.capabilities.to_le_bytes());
        hash.update(&symbol.retained_argument_mask.to_le_bytes());
        hash_field(&mut hash, symbol.api_path.as_bytes());
        hash_field(&mut hash, symbol.item_path.as_bytes());
        hash_field(&mut hash, symbol.contract.as_bytes());
        hash_field(&mut hash, symbol.rust_abi.as_bytes());
        hash_field(&mut hash, symbol.abi_mode.as_bytes());
    }
    hash.finish()
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn exported_constant(name: &str) -> Option<u64> {
    let value = match name {
        "CORE_SAFE" => kernel_symbols::capability::CORE_SAFE,
        "ALLOCATOR_MEMORY" => kernel_symbols::capability::ALLOCATOR_MEMORY,
        "ALLOCATOR_DIAGNOSTIC" => kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC,
        "ALLOCATOR_PHYSICAL" => kernel_symbols::capability::ALLOCATOR_PHYSICAL,
        "ALLOCATOR_MANAGED" => kernel_symbols::capability::ALLOCATOR_MANAGED,
        "ALLOCATOR_ADMIN" => kernel_symbols::capability::ALLOCATOR_ADMIN,
        "DEVICE_DISCOVERY" => kernel_symbols::capability::DEVICE_DISCOVERY,
        "DEVICE_DRIVER" => kernel_symbols::capability::DEVICE_DRIVER,
        "DEVICE_RESOURCE" => kernel_symbols::capability::DEVICE_RESOURCE,
        "DEVICE_DMA" => kernel_symbols::capability::DEVICE_DMA,
        "DEVICE_INTERRUPT" => kernel_symbols::capability::DEVICE_INTERRUPT,
        "DEVICE_BUS" => kernel_symbols::capability::DEVICE_BUS,
        "DEVICE_ADMIN" => kernel_symbols::capability::DEVICE_ADMIN,
        "KERNEL_SYMBOL_FLAG_MUTATES_STATE" => {
            u64::from(kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)
        }
        "KERNEL_SYMBOL_FLAG_UNSAFE" => u64::from(kernel_symbols::KERNEL_SYMBOL_FLAG_UNSAFE),
        "KERNEL_SYMBOL_FLAG_RETURNS_OWNED" => {
            u64::from(kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)
        }
        "KERNEL_SYMBOL_FLAG_DIAGNOSTIC" => u64::from(kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC),
        "KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE" => {
            u64::from(kernel_symbols::KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE)
        }
        "KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW" => {
            u64::from(kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW)
        }
        _ => return None,
    };
    Some(value)
}

fn canonical_function_abi(signature: &Signature) -> Result<String, String> {
    if signature.constness.is_some()
        || signature.asyncness.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
    {
        return Err("直接内核符号签名不满足 Rust ABI 约束".to_string());
    }
    let mut arguments = Vec::new();
    for argument in &signature.inputs {
        let syn::FnArg::Typed(argument) = argument else {
            return Err("自由函数导出不能包含 self".to_string());
        };
        let ty = argument.ty.as_ref();
        arguments.push(quote!(#ty));
    }
    let unsafety = &signature.unsafety;
    let result: Type = match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, result) => (**result).clone(),
    };
    Ok(normalize_abi_tokens(
        quote!(#unsafety fn(#(#arguments),*) -> #result),
    ))
}

fn canonical_method_abi(signature: &Signature, self_ty: &Type) -> Result<String, String> {
    if signature.constness.is_some()
        || signature.asyncness.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
    {
        return Err("直接内核方法签名不满足 Rust ABI 约束".to_string());
    }
    let mut arguments = Vec::new();
    for argument in &signature.inputs {
        match argument {
            syn::FnArg::Typed(argument) => {
                let ty = argument.ty.as_ref();
                arguments.push(quote!(#ty));
            }
            syn::FnArg::Receiver(receiver) => {
                if receiver.colon_token.is_some() {
                    let ty = receiver.ty.as_ref();
                    arguments.push(quote!(#ty));
                } else {
                    let mutability = &receiver.mutability;
                    if let Some((and_token, lifetime)) = &receiver.reference {
                        arguments.push(quote!(#and_token #lifetime #mutability #self_ty));
                    } else {
                        arguments.push(quote!(#self_ty));
                    }
                }
            }
        }
    }
    let unsafety = &signature.unsafety;
    let result: Type = match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, result) => (**result).clone(),
    };
    let abi = normalize_abi_tokens(quote!(#unsafety fn(#(#arguments),*) -> #result));
    Ok(replace_self_type(&abi, &normalize_abi_tokens(self_ty)))
}

fn replace_self_type(input: &str, self_type: &str) -> String {
    let mut output = String::with_capacity(input.len() + self_type.len());
    let bytes = input.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let is_self = bytes[index..].starts_with(b"Self")
            && (index == 0 || !is_rust_ident_byte(bytes[index - 1]))
            && (index + 4 == bytes.len() || !is_rust_ident_byte(bytes[index + 4]));
        if is_self {
            output.push_str(self_type);
            index += 4;
        } else {
            output.push(bytes[index] as char);
            index += 1;
        }
    }
    output
}

const fn is_rust_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn normalize_abi_tokens(tokens: impl ToTokens) -> String {
    tokens
        .into_token_stream()
        .to_string()
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
}

fn stable_link_name(api_path: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in api_path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("__elm_kernel_api_{hash:016x}")
}

fn legacy_mangled_prefix(api_path: &str) -> String {
    let mut output = String::from("_ZN");
    for component in api_path.split('.') {
        output.push_str(&component.len().to_string());
        output.push_str(component);
    }
    output
}

fn populate_link_aliases(
    target: &str,
    rlibs: &[PathBuf],
    public_api_abis: &BTreeMap<String, String>,
    symbols: &mut [KernelInterfaceSymbol],
) -> Result<(), String> {
    let nm = target_nm(target)?;
    let mut defined = BTreeSet::new();
    for rlib in rlibs {
        let output = Command::new(nm)
            .arg("-j")
            .arg("-g")
            .arg("--defined-only")
            .arg(rlib)
            .output()
            .map_err(|error| format!("执行 {nm} 读取 {} 失败: {error}", rlib.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{nm} 无法读取 {}: {}",
                rlib.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| format!("{nm} 对 {} 的输出不是 UTF-8", rlib.display()))?;
        defined.extend(
            stdout
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
        );
    }

    for symbol in symbols {
        let prefix = legacy_mangled_prefix(&symbol.api_path);
        let matches = defined
            .iter()
            .filter(|name| {
                name.strip_prefix(&prefix)
                    .is_some_and(is_legacy_rust_hash_suffix)
            })
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => {}
            [alias] if alias != &symbol.link_name => {
                let actual_abi = public_api_abis.get(&symbol.api_path).ok_or_else(|| {
                    format!(
                        "内核 API {} 存在 Rust 链接别名，但找不到对应公开方法签名",
                        symbol.api_path
                    )
                })?;
                if actual_abi != &symbol.rust_abi {
                    return Err(format!(
                        "内核 API {} 的导出 ABI 与公开方法不一致：export={} public={}",
                        symbol.api_path, symbol.rust_abi, actual_abi
                    ));
                }
                symbol.aliases.push(alias.clone());
            }
            [_] => {}
            _ => {
                return Err(format!(
                    "内核 API {} 对应多个 Rust 链接符号: {}",
                    symbol.api_path,
                    matches.join(", ")
                ));
            }
        }
    }
    Ok(())
}

fn is_legacy_rust_hash_suffix(suffix: &str) -> bool {
    let Some(hash) = suffix
        .strip_prefix("17h")
        .and_then(|value| value.strip_suffix('E'))
    else {
        return false;
    };
    hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn build_import_library(
    target: &str,
    manifest: &KernelInterfaceManifest,
    output: &Path,
) -> Result<(), String> {
    let compiler = target_cc(target)?;
    let return_instruction = match target {
        "loongarch64-unknown-none" => "jirl $zero, $ra, 0",
        "riscv64gc-unknown-none-elf" => "ret",
        _ => return Err(format!("不支持为目标 {target} 生成内核导入库")),
    };
    let mut entries = BTreeMap::new();
    for symbol in &manifest.symbols {
        for name in std::iter::once(&symbol.link_name).chain(&symbol.aliases) {
            validate_asm_symbol(name)?;
            if let Some(previous) = entries.insert(name.clone(), symbol.kind) {
                if previous != symbol.kind {
                    return Err(format!("导入符号 {name} 同时具有函数和对象类型"));
                }
            }
        }
    }

    let mut assembly =
        String::from(".section .text.elm_kernel_imports,\"ax\",@progbits\n.p2align 2\n");
    for (name, kind) in &entries {
        if *kind == KERNEL_SYMBOL_KIND_STATIC {
            continue;
        }
        let name = quoted_asm_symbol(name);
        assembly.push_str(&format!(
            ".globl {name}\n.type {name}, @function\n{name}:\n    {return_instruction}\n.size {name}, .-{name}\n"
        ));
    }
    assembly.push_str(".section .data.elm_kernel_imports,\"aw\",@progbits\n.p2align 3\n");
    for (name, kind) in &entries {
        if *kind != KERNEL_SYMBOL_KIND_STATIC {
            continue;
        }
        let name = quoted_asm_symbol(name);
        assembly.push_str(&format!(
            ".globl {name}\n.type {name}, @object\n{name}:\n    .quad 0\n.size {name}, 8\n"
        ));
    }

    let source = output.with_extension("S");
    fs::write(&source, assembly)
        .map_err(|error| format!("写入导入库汇编 {} 失败: {error}", source.display()))?;
    let result = Command::new(compiler)
        .arg("-shared")
        .arg("-nostdlib")
        .arg("-fPIC")
        .arg(format!("-Wl,-soname,{}", manifest.import_library))
        .arg("-Wl,--no-undefined")
        .arg(&source)
        .arg("-o")
        .arg(output)
        .output()
        .map_err(|error| format!("执行 {compiler} 生成内核导入库失败: {error}"));
    let remove_result = fs::remove_file(&source);
    let result = result?;
    if !result.status.success() {
        return Err(format!(
            "{compiler} 生成内核导入库失败: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    remove_result.map_err(|error| format!("删除临时汇编 {} 失败: {error}", source.display()))?;
    if !output.is_file() {
        return Err(format!("{compiler} 未生成内核导入库 {}", output.display()));
    }
    Ok(())
}

fn build_rust_support_library(
    target: &str,
    rlibs: &[PathBuf],
    output: &Path,
) -> Result<(), String> {
    let nm = target_nm(target)?;
    let objcopy = target_objcopy(target)?;
    let linker = target_ld(target)?;
    let temporary = output.with_extension(format!("support.tmp.{}", std::process::id()));
    remove_path(&temporary)?;
    fs::create_dir_all(&temporary)
        .map_err(|error| format!("创建 Rust 支持归档临时目录失败: {error}"))?;
    let result = (|| {
        let mut global_definitions = BTreeSet::new();
        let mut input_objects = Vec::new();
        for (rlib_index, rlib) in rlibs.iter().enumerate() {
            let source = rlib
                .canonicalize()
                .map_err(|error| format!("定位 {} 失败: {error}", rlib.display()))?;
            let extract = temporary.join(format!("archive-{rlib_index:04}"));
            fs::create_dir_all(&extract)
                .map_err(|error| format!("创建 rlib 解包目录失败: {error}"))?;
            let unpack = Command::new("llvm-ar")
                .current_dir(&extract)
                .arg("x")
                .arg(&source)
                .output()
                .map_err(|error| format!("解包 {} 失败: {error}", source.display()))?;
            if !unpack.status.success() {
                return Err(format!(
                    "llvm-ar 无法解包 {}: {}",
                    source.display(),
                    String::from_utf8_lossy(&unpack.stderr)
                ));
            }
            let mut objects = fs::read_dir(&extract)
                .map_err(|error| format!("读取 {} 失败: {error}", extract.display()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("读取 rlib 对象目录项失败: {error}"))?;
            objects.sort_by_key(|entry| entry.file_name());
            for (object_index, entry) in objects.into_iter().enumerate() {
                let object = entry.path();
                if object.extension().is_none_or(|extension| extension != "o") {
                    continue;
                }
                let defined = defined_global_symbols(nm, &object)?;
                if defined.is_empty() {
                    continue;
                }
                let localize = defined
                    .into_iter()
                    .filter(|name| !global_definitions.insert(name.clone()))
                    .collect::<Vec<_>>();
                let localize_file = extract.join(format!("localize-{object_index:04}.txt"));
                fs::write(&localize_file, localize.join("\n"))
                    .map_err(|error| format!("写入符号局部化清单失败: {error}"))?;
                let filtered = temporary.join(format!("input-{rlib_index:04}-{object_index:04}.o"));
                let mut command = Command::new(objcopy);
                if !localize.is_empty() {
                    command.arg(format!("--localize-symbols={}", localize_file.display()));
                }
                let filter = command
                    .arg("--strip-debug")
                    .arg(&object)
                    .arg(&filtered)
                    .output()
                    .map_err(|error| {
                        format!("执行 {objcopy} 过滤 {} 失败: {error}", object.display())
                    })?;
                if !filter.status.success() {
                    return Err(format!(
                        "{objcopy} 无法过滤 {}（状态 {}）: {}{}",
                        object.display(),
                        filter.status,
                        String::from_utf8_lossy(&filter.stdout),
                        String::from_utf8_lossy(&filter.stderr)
                    ));
                }
                input_objects.push(filtered);
            }
        }
        if input_objects.is_empty() {
            return Err("精确依赖闭包没有可用的目标对象".to_string());
        }
        let combined = temporary.join("rust-support-combined.o");
        let link = Command::new(linker)
            .arg("-r")
            .arg("-o")
            .arg(&combined)
            .args(&input_objects)
            .output()
            .map_err(|error| format!("执行 {linker} 合并 Rust 支持对象失败: {error}"))?;
        if !link.status.success() {
            return Err(format!(
                "{linker} 合并 Rust 支持对象失败: {}{}",
                String::from_utf8_lossy(&link.stdout),
                String::from_utf8_lossy(&link.stderr)
            ));
        }
        let combined_symbols = defined_global_symbols(nm, &combined)?;
        let published = combined_symbols
            .iter()
            .filter(|name| is_rust_support_symbol(name))
            .cloned()
            .collect::<BTreeSet<_>>();
        if published.is_empty() {
            return Err("精确依赖闭包没有 core/alloc 编译器支持符号".to_string());
        }
        let localize = combined_symbols
            .difference(&published)
            .cloned()
            .collect::<Vec<_>>();
        let localize_file = temporary.join("localize-final.txt");
        fs::write(&localize_file, localize.join("\n"))
            .map_err(|error| format!("写入最终符号局部化清单失败: {error}"))?;
        let filtered = temporary.join("rust-support.o");
        let filter = Command::new(objcopy)
            .arg(format!("--localize-symbols={}", localize_file.display()))
            .arg("--strip-debug")
            .arg(&combined)
            .arg(&filtered)
            .output()
            .map_err(|error| format!("执行 {objcopy} 收口 Rust 支持对象失败: {error}"))?;
        if !filter.status.success() {
            return Err(format!(
                "{objcopy} 收口 Rust 支持对象失败（状态 {}）: {}{}",
                filter.status,
                String::from_utf8_lossy(&filter.stdout),
                String::from_utf8_lossy(&filter.stderr)
            ));
        }
        let archive = Command::new("llvm-ar")
            .arg("crs")
            .arg(output)
            .arg(&filtered)
            .output()
            .map_err(|error| format!("生成 Rust 支持归档失败: {error}"))?;
        if !archive.status.success() {
            return Err(format!(
                "llvm-ar 生成 Rust 支持归档失败: {}",
                String::from_utf8_lossy(&archive.stderr)
            ));
        }
        let archived = defined_global_symbols(nm, output)?;
        if archived != published {
            return Err("Rust 支持归档最终全局符号集合与审计结果不一致".to_string());
        }
        Ok(())
    })();
    let cleanup = fs::remove_dir_all(&temporary)
        .map_err(|error| format!("删除 Rust 支持归档临时目录失败: {error}"));
    result?;
    cleanup?;
    Ok(())
}

fn defined_global_symbols(nm: &str, input: &Path) -> Result<BTreeSet<String>, String> {
    let output = Command::new(nm)
        .arg("-j")
        .arg("-g")
        .arg("--defined-only")
        .arg(input)
        .output()
        .map_err(|error| format!("执行 {nm} 读取 {} 失败: {error}", input.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{nm} 无法读取 {}: {}",
            input.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("{nm} 对 {} 的输出不是 UTF-8", input.display()))?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

fn is_rust_support_symbol(name: &str) -> bool {
    name.starts_with("_ZN4core") || name.starts_with("_ZN5alloc")
}

fn target_nm(target: &str) -> Result<&'static str, String> {
    match target {
        "loongarch64-unknown-none" => Ok("loongarch64-linux-gnu-nm"),
        "riscv64gc-unknown-none-elf" => Ok("riscv64-linux-gnu-nm"),
        _ => Err(format!("不支持读取目标 {target} 的内核符号")),
    }
}

fn target_cc(target: &str) -> Result<&'static str, String> {
    match target {
        "loongarch64-unknown-none" => Ok("loongarch64-linux-gnu-gcc"),
        "riscv64gc-unknown-none-elf" => Ok("riscv64-linux-gnu-gcc"),
        _ => Err(format!("不支持为目标 {target} 生成导入库")),
    }
}

fn target_objcopy(target: &str) -> Result<&'static str, String> {
    match target {
        "loongarch64-unknown-none" | "riscv64gc-unknown-none-elf" => Ok("llvm-objcopy"),
        _ => Err(format!("不支持为目标 {target} 过滤 Rust 支持对象")),
    }
}

fn target_ld(target: &str) -> Result<&'static str, String> {
    match target {
        "loongarch64-unknown-none" => Ok("loongarch64-linux-gnu-ld"),
        "riscv64gc-unknown-none-elf" => Ok("riscv64-linux-gnu-ld"),
        _ => Err(format!("不支持为目标 {target} 合并 Rust 支持对象")),
    }
}

fn validate_asm_symbol(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$'))
    {
        return Err(format!("内核导入链接符号无法安全写入汇编: {name:?}"));
    }
    Ok(())
}

fn quoted_asm_symbol(name: &str) -> String {
    format!("\"{name}\"")
}

fn newest_matching_file(directory: &Path, prefix: &str, suffix: &str) -> Result<PathBuf, String> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("读取 {} 失败: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("读取构建产物失败: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(prefix) || !name.ends_with(suffix) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .is_none_or(|(current, _)| modified > *current)
        {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path).ok_or_else(|| {
        format!(
            "{} 中没有匹配 {prefix}*{suffix} 的构建产物",
            directory.display()
        )
    })
}

fn rustc_crate_root(metadata: &Path) -> Result<String, String> {
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-Z")
        .arg("ls=root")
        .arg(metadata)
        .output()
        .map_err(|error| format!("读取 Rust 元数据失败: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "rustc 无法读取 {}: {}",
            metadata.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "rustc 元数据输出不是 UTF-8".to_string())
}

fn dependency_metadata_name(root: &str, crate_name: &str) -> Result<String, String> {
    root.lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let _index = fields.next()?;
            let name = fields.next()?;
            name.starts_with(&format!("{crate_name}-"))
                .then(|| name.to_string())
        })
        .next()
        .ok_or_else(|| format!("kernel 元数据没有直接依赖 {crate_name}"))
}

fn exact_dependency_rlibs(
    directory: &Path,
    roots: &[&str],
    root_rlibs: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut files = root_rlibs.iter().cloned().collect::<BTreeSet<_>>();
    for root in roots {
        let mut dependencies = false;
        for line in root.lines() {
            if line == "=External Dependencies=" {
                dependencies = true;
                continue;
            }
            if !dependencies {
                continue;
            }
            if line.is_empty() {
                break;
            }
            let mut fields = line.split_ascii_whitespace();
            let Some(index) = fields.next() else {
                continue;
            };
            if index.parse::<usize>().is_err() {
                continue;
            }
            let Some(name) = fields.next() else {
                return Err("Rust 元数据依赖记录缺少 crate 名称".to_string());
            };
            let path = directory.join(format!("lib{name}.rlib"));
            if path.is_file() {
                files.insert(path);
            }
        }
    }
    if files.iter().any(|path| !path.is_file()) {
        return Err("精确 Rust 元数据依赖闭包包含不存在的 rlib".to_string());
    }
    Ok(files.into_iter().collect())
}

fn copy_metadata_rlibs(sources: &[PathBuf], destination: &Path) -> Result<(), String> {
    for source in sources {
        let name = source
            .file_name()
            .ok_or_else(|| format!("rlib 路径没有文件名: {}", source.display()))?;
        write_metadata_only_rlib(source, &destination.join(name))?;
    }
    Ok(())
}

fn write_metadata_only_rlib(source: &Path, destination: &Path) -> Result<(), String> {
    let output = Command::new("llvm-ar")
        .arg("p")
        .arg(source)
        .arg("lib.rmeta")
        .output()
        .map_err(|error| format!("读取 rlib 元数据 {} 失败: {error}", source.display()))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(format!(
            "rlib {} 不包含可用的 lib.rmeta: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut archive = Vec::with_capacity(output.stdout.len() + 70);
    archive.extend_from_slice(b"!<arch>\n");
    let header = format!(
        "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
        "lib.rmeta/",
        0,
        0,
        0,
        0o100644,
        output.stdout.len()
    );
    if header.len() != 60 {
        return Err("内部 ar member header 尺寸错误".to_string());
    }
    archive.extend_from_slice(header.as_bytes());
    archive.extend_from_slice(&output.stdout);
    if output.stdout.len() & 1 != 0 {
        archive.push(b'\n');
    }
    fs::write(destination, archive)
        .map_err(|error| format!("写入元数据 rlib {} 失败: {error}", destination.display()))
}

fn copy_proc_macro_directory(source: &Path, destination: &Path) -> Result<(), String> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in
        fs::read_dir(source).map_err(|error| format!("读取 {} 失败: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| format!("读取 proc-macro 目录项失败: {error}"))?;
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "so") {
            fs::copy(&path, destination.join(entry.file_name()))
                .map_err(|error| format!("复制 proc-macro {} 失败: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn copy_framework(
    repository: &Path,
    destination: &Path,
    target: &str,
    manifest: &KernelInterfaceManifest,
) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| format!("创建框架目录失败: {error}"))?;
    copy_tree(&repository.join("libs/elm"), &destination.join("elm"))?;
    copy_tree(
        &repository.join("libs/kernel-symbols"),
        &destination.join("kernel-symbols"),
    )?;
    fs::write(
        destination
            .join("kernel-symbols")
            .join(format!("interface.identity.{target}")),
        format!(
            "sha256={}\nfiles={}\n",
            hex_digest(&manifest.interface_hash),
            manifest.source_file_count
        ),
    )
    .map_err(|error| format!("写入接口身份失败: {error}"))?;
    fs::write(
        destination
            .join("kernel-symbols")
            .join("interface.identity"),
        format!(
            "sha256={}\nfiles={}\n",
            hex_digest(&manifest.interface_hash),
            manifest.source_file_count
        ),
    )
    .map_err(|error| format!("写入 host LSP 接口身份失败: {error}"))?;
    write_facade(
        &destination.join("allocator"),
        "allocator",
        "__elm_host_allocator",
    )?;
    write_facade(
        &destination.join("general"),
        "general",
        "__elm_host_general",
    )?;
    fs::write(
        destination.join("Cargo.toml"),
        framework_workspace_manifest(),
    )
    .map_err(|error| format!("写入框架 workspace manifest 失败: {error}"))
}

fn write_facade(directory: &Path, name: &str, host_alias: &str) -> Result<(), String> {
    fs::create_dir_all(directory.join("src"))
        .map_err(|error| format!("创建 façade {name} 失败: {error}"))?;
    fs::write(
        directory.join("Cargo.toml"),
        metadata_facade_manifest(name, host_alias),
    )
    .map_err(|error| format!("写入 façade manifest 失败: {error}"))?;
    fs::write(
        directory.join("src/lib.rs"),
        metadata_facade_source(name, host_alias),
    )
    .map_err(|error| format!("写入 façade 源码失败: {error}"))
}

pub(crate) fn metadata_facade_manifest(name: &str, host_alias: &str) -> String {
    let source_path = match name {
        "allocator" => "../../kernel-source/libs/allocator",
        "general" => "../../kernel-source/general",
        _ => panic!("不支持的内核元数据 facade: {name}"),
    };
    let lsp_alias = format!("__elm_lsp_{name}");
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\ntest = false\nbench = false\n\n[features]\ndefault = [\"lsp\"]\nlsp = [\"dep:{lsp_alias}\"]\n\n[dependencies]\n{lsp_alias} = {{ package = \"elm-lsp-{name}\", path = \"{source_path}\", optional = true }}\n\n[package.metadata.elm]\nformal-extern = \"{host_alias}\"\n"
    )
}

pub(crate) fn metadata_facade_source(name: &str, host_alias: &str) -> String {
    let lsp_alias = format!("__elm_lsp_{name}");
    if name != "allocator" {
        return format!(
            "#![no_std]\n#![warn(missing_docs)]\n\n//! 由 elmtools 从目标内核精确元数据生成的只读接口。\n\n#[cfg(feature = \"lsp\")]\nextern crate {lsp_alias} as __elm_backend;\n#[cfg(not(feature = \"lsp\"))]\nextern crate {host_alias} as __elm_backend;\n\npub use __elm_backend::*;\n"
        );
    }
    let alloc = stable_link_name("allocator.GlobalAlloc.alloc");
    let dealloc = stable_link_name("allocator.GlobalAlloc.dealloc");
    let realloc = stable_link_name("allocator.GlobalAlloc.realloc");
    let alloc_zeroed = stable_link_name("allocator.GlobalAlloc.alloc_zeroed");
    format!(
        r#"#![no_std]
#![warn(missing_docs)]

//! 由 elmtools 从目标内核精确元数据生成的 allocator 接口。

#[cfg(feature = "lsp")]
extern crate {lsp_alias} as __elm_backend;
#[cfg(not(feature = "lsp"))]
extern crate {host_alias} as __elm_backend;

use core::alloc::{{GlobalAlloc, Layout}};

pub use __elm_backend::*;

unsafe extern "Rust" {{
    #[link_name = "{alloc}"]
    fn elm_kernel_alloc(layout: Layout) -> *mut u8;
    #[link_name = "{dealloc}"]
    fn elm_kernel_dealloc(pointer: *mut u8, layout: Layout);
    #[link_name = "{realloc}"]
    fn elm_kernel_realloc(pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8;
    #[link_name = "{alloc_zeroed}"]
    fn elm_kernel_alloc_zeroed(layout: Layout) -> *mut u8;
}}

struct ElmGlobalAllocator;

unsafe impl GlobalAlloc for ElmGlobalAllocator {{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {{
        // Safety: 入口由 ELM 装载器绑定到同构内核分配器，layout 来自 GlobalAlloc。
        unsafe {{ elm_kernel_alloc(layout) }}
    }}

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {{
        // Safety: pointer/layout 必须来自本 ELM 的同一个全局分配器。
        unsafe {{ elm_kernel_dealloc(pointer, layout) }}
    }}

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {{
        // Safety: 调用方遵守 GlobalAlloc::realloc 的所有权和布局约束。
        unsafe {{ elm_kernel_realloc(pointer, layout, new_size) }}
    }}

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {{
        // Safety: 入口由 ELM 装载器绑定，layout 来自 GlobalAlloc。
        unsafe {{ elm_kernel_alloc_zeroed(layout) }}
    }}
}}

#[global_allocator]
static ELM_GLOBAL_ALLOCATOR: ElmGlobalAllocator = ElmGlobalAllocator;
"#
    )
}

pub(crate) fn framework_workspace_manifest() -> &'static str {
    r#"[workspace]
resolver = "2"
members = [
    "elm",
    "elm/macros",
    "kernel-symbols",
    "kernel-symbols/macros",
    "allocator",
    "general",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
"#
}

fn copy_lsp_source_snapshot(
    repository: &Path,
    destination: &Path,
    interface_hash: [u8; 32],
) -> Result<(), String> {
    copy_tree(&repository.join("general"), &destination.join("general"))?;
    copy_tree(&repository.join("libs"), &destination.join("libs"))?;

    let packages = discover_lsp_source_packages(repository)?;
    project_lsp_package_manifests(destination, &packages)?;
    let mut workspace = String::from("[workspace]\nresolver = \"2\"\nmembers = [\n");
    for package in &packages {
        writeln!(workspace, "    {package:?},")
            .map_err(|_| "生成 LSP 源码 workspace 失败".to_string())?;
    }
    workspace.push_str("]\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    fs::write(destination.join("Cargo.toml"), workspace)
        .map_err(|error| format!("写入 LSP 源码 workspace 失败: {error}"))?;
    if repository.join("Cargo.lock").is_file() {
        fs::copy(
            repository.join("Cargo.lock"),
            destination.join("Cargo.lock"),
        )
        .map_err(|error| format!("复制 LSP 源码依赖锁失败: {error}"))?;
    }
    fs::write(
        destination.join(LSP_SOURCE_IDENTITY_FILE),
        format!(
            "{LSP_SOURCE_MAGIC}\ninterface_sha256={}\npackages={}\n",
            hex_digest(&interface_hash),
            packages.len()
        ),
    )
    .map_err(|error| format!("写入 LSP 源码身份失败: {error}"))?;
    fs::write(
        destination.join("README.md"),
        "# ELM 内核源码投影\n\n该目录由 `cargo elm` 按接口包生成，只用于 rust-analyzer 和宿主 `cargo check` 建立精确源码导航。正式 ELM 构建不会编译这里的实现，而是使用目标内核发布的 `.rlib/.rmeta` 和直接符号目录。\n",
    )
    .map_err(|error| format!("写入 LSP 源码说明失败: {error}"))?;
    Ok(())
}

fn discover_lsp_source_packages(repository: &Path) -> Result<Vec<String>, String> {
    let mut packages = Vec::new();
    collect_lsp_source_packages(repository, &repository.join("general"), &mut packages)?;
    collect_lsp_source_packages(repository, &repository.join("libs"), &mut packages)?;
    packages.sort();
    packages.dedup();
    if packages.is_empty() {
        return Err("LSP 源码投影没有发现任何 Cargo package".to_string());
    }
    Ok(packages)
}

fn project_lsp_package_manifests(destination: &Path, packages: &[String]) -> Result<(), String> {
    let mut projected_names = BTreeMap::new();
    for package in packages {
        let directory = destination.join(package);
        let manifest = directory.join("Cargo.toml");
        let input = fs::read_to_string(&manifest)
            .map_err(|error| format!("读取 {} 失败: {error}", manifest.display()))?;
        let name = manifest_package_name(&input)
            .ok_or_else(|| format!("{} 缺少 package name", manifest.display()))?;
        let canonical = directory
            .canonicalize()
            .map_err(|error| format!("定位 {} 失败: {error}", directory.display()))?;
        let projected = format!("elm-lsp-{name}");
        if projected_names.insert(canonical, projected).is_some() {
            return Err(format!("LSP 源码投影重复登记 package: {package}"));
        }
    }
    for package in packages {
        let directory = destination.join(package);
        let manifest = directory.join("Cargo.toml");
        rewrite_lsp_package_manifest(&manifest, &projected_names)?;
        suppress_lsp_source_warnings(&directory)?;
    }
    Ok(())
}

fn manifest_package_name(input: &str) -> Option<String> {
    let mut in_package = false;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some((_, _, value)) = toml_string_field(line, "name") {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn rewrite_lsp_package_manifest(
    manifest: &Path,
    projected_names: &BTreeMap<PathBuf, String>,
) -> Result<(), String> {
    let input = fs::read_to_string(manifest)
        .map_err(|error| format!("读取 {} 失败: {error}", manifest.display()))?;
    let trailing_newline = input.ends_with('\n');
    let package_name = manifest_package_name(&input)
        .ok_or_else(|| format!("{} 缺少 package name", manifest.display()))?;
    let projected_name = format!("elm-lsp-{package_name}");
    let mut section = String::new();
    let mut skip_section = false;
    let mut renamed_package = false;
    let mut output = Vec::new();
    for raw_line in input.lines() {
        let mut line = raw_line.to_string();
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            skip_section = trimmed.starts_with("[profile.")
                || matches!(
                    trimmed,
                    "[[bin]]" | "[[example]]" | "[[test]]" | "[[bench]]"
                );
            section.clear();
            section.push_str(trimmed);
            if !skip_section {
                output.push(line);
            }
            continue;
        }
        if skip_section {
            continue;
        }
        if section == "[package]" && !renamed_package {
            if let Some((start, end, _)) = toml_string_field(&line, "name") {
                line.replace_range(start..end, &projected_name);
                renamed_package = true;
                output.push(line);
                output.push("autobins = false".to_string());
                output.push("autoexamples = false".to_string());
                output.push("autotests = false".to_string());
                output.push("autobenches = false".to_string());
                continue;
            }
        } else if section == "[package]"
            && matches!(
                line.trim_start().split_once('=').map(|(key, _)| key.trim()),
                Some("autobins" | "autoexamples" | "autotests" | "autobenches")
            )
        {
            continue;
        } else if is_dependency_section(&section) {
            if let Some((_, _, path)) = toml_string_field(&line, "path") {
                let dependency = manifest
                    .parent()
                    .unwrap()
                    .join(path)
                    .canonicalize()
                    .map_err(|error| {
                        format!(
                            "定位 {} 中的 path dependency {path} 失败: {error}",
                            manifest.display()
                        )
                    })?;
                if let Some(projected) = projected_names.get(&dependency) {
                    if let Some((start, end, _)) = toml_string_field(&line, "package") {
                        line.replace_range(start..end, projected);
                    } else if let Some(brace) = line.find('{') {
                        line.insert_str(brace + 1, &format!(" package = {projected:?},"));
                    } else {
                        return Err(format!(
                            "{} 使用了暂不支持的非内联 path dependency: {line}",
                            manifest.display()
                        ));
                    }
                }
            }
        } else if package_name == "smoltcp"
            && section == "[features]"
            && line.trim_start().starts_with("default = [")
        {
            let indentation = &line[..line.len() - line.trim_start().len()];
            output.push(format!("{indentation}default = []"));
            line = line.replacen("default", "_elm_lsp_original_default", 1);
        }
        output.push(line);
    }
    if !renamed_package {
        return Err(format!("{} 未能改写 package name", manifest.display()));
    }
    let mut output = output.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    fs::write(manifest, output)
        .map_err(|error| format!("写入 LSP package {} 失败: {error}", manifest.display()))
}

fn suppress_lsp_source_warnings(directory: &Path) -> Result<(), String> {
    let root = directory.join("src/lib.rs");
    if !root.is_file() {
        return Ok(());
    }
    let input = fs::read_to_string(&root)
        .map_err(|error| format!("读取 {} 失败: {error}", root.display()))?;
    if input.starts_with("#![allow(warnings)]") {
        return Ok(());
    }
    fs::write(&root, format!("#![allow(warnings)]\n{input}"))
        .map_err(|error| format!("写入 LSP 诊断抑制 {} 失败: {error}", root.display()))
}

fn is_dependency_section(section: &str) -> bool {
    section == "[dependencies]"
        || section == "[dev-dependencies]"
        || section == "[build-dependencies]"
        || section.starts_with("[target.") && section.ends_with(".dependencies]")
}

fn toml_string_field<'a>(line: &'a str, key: &str) -> Option<(usize, usize, &'a str)> {
    let key_start = line.find(key)?;
    if key_start != 0 && line.as_bytes()[key_start - 1].is_ascii_alphanumeric() {
        return None;
    }
    let equal = line[key_start + key.len()..].find('=')? + key_start + key.len();
    let quote = line[equal + 1..].find('"')? + equal + 1;
    let end = line[quote + 1..].find('"')? + quote + 1;
    Some((quote + 1, end, &line[quote + 1..end]))
}

fn collect_lsp_source_packages(
    repository: &Path,
    directory: &Path,
    output: &mut Vec<String>,
) -> Result<(), String> {
    let manifest = directory.join("Cargo.toml");
    if manifest.is_file() {
        let contents = fs::read_to_string(&manifest)
            .map_err(|error| format!("读取 {} 失败: {error}", manifest.display()))?;
        if contents.lines().any(|line| line.trim() == "[package]") {
            let relative = directory
                .strip_prefix(repository)
                .map_err(|_| format!("{} 不位于仓库中", directory.display()))?;
            output.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("读取 {} 失败: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 LSP 源码目录项失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if matches!(entry.file_name().to_str(), Some("target" | ".git" | "fuzz")) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_lsp_source_packages(repository, &path, output)?;
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("创建 {} 失败: {error}", destination.display()))?;
    for entry in
        fs::read_dir(source).map_err(|error| format!("读取 {} 失败: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| format!("读取目录项失败: {error}"))?;
        let path = entry.path();
        if matches!(entry.file_name().to_str(), Some("target" | ".git")) {
            continue;
        }
        let target = destination.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else if path.is_file() {
            fs::copy(&path, &target)
                .map_err(|error| format!("复制 {} 失败: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        fs::remove_file(path).map_err(|error| format!("删除 {} 失败: {error}", path.display()))
    } else if path.is_dir() {
        fs::remove_dir_all(path).map_err(|error| format!("删除 {} 失败: {error}", path.display()))
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| format!("删除 {} 失败: {error}", path.display()))
    } else {
        Ok(())
    }
}

fn parse_symbol_record(value: &str) -> Result<KernelInterfaceSymbol, String> {
    let fields = value.split('\t').collect::<Vec<_>>();
    if fields.len() != 12 {
        return Err("内核接口 symbol 记录字段数量错误".to_string());
    }
    let interface_hash = [0; 32];
    let rust_abi = String::from_utf8(parse_hex_bytes(fields[9])?)
        .map_err(|_| "内核接口 rust_abi 不是 UTF-8".to_string())?;
    Ok(KernelInterfaceSymbol {
        kind: fields[0]
            .parse()
            .map_err(|_| "symbol kind 无效".to_string())?,
        flags: fields[1]
            .parse()
            .map_err(|_| "symbol flags 无效".to_string())?,
        version: fields[2]
            .parse()
            .map_err(|_| "symbol version 无效".to_string())?,
        capabilities: fields[3]
            .parse()
            .map_err(|_| "symbol capabilities 无效".to_string())?,
        retained_argument_mask: fields[4]
            .parse()
            .map_err(|_| "symbol retained mask 无效".to_string())?,
        api_path: fields[5].to_string(),
        item_path: fields[6].to_string(),
        link_name: fields[7].to_string(),
        contract: fields[8].to_string(),
        rust_abi,
        rust_abi_hash: [0; 32],
        interface_hash,
        abi_mode: fields[10].to_string(),
        aliases: if fields[11].is_empty() {
            Vec::new()
        } else {
            fields[11].split(',').map(str::to_string).collect()
        },
    })
}

fn parse_digest(value: &str) -> Result<[u8; 32], String> {
    parse_hex_bytes(value)?
        .try_into()
        .map_err(|_| "摘要长度必须为 32 字节".to_string())
}

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err("十六进制字符串长度必须为偶数".to_string());
    }
    let mut output = Vec::with_capacity(value.len() / 2);
    for index in (0..value.len()).step_by(2) {
        output.push(
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| "十六进制字符串包含非法字符".to_string())?,
        );
    }
    Ok(output)
}

pub(crate) fn hex_digest(value: &[u8; 32]) -> String {
    hex_bytes(value)
}

fn hex_bytes(value: &[u8]) -> String {
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_facades_select_lsp_source_with_an_explicit_feature() {
        let allocator = metadata_facade_manifest("allocator", "__elm_host_allocator");
        assert!(allocator.contains("default = [\"lsp\"]"));
        assert!(allocator.contains("lsp = [\"dep:__elm_lsp_allocator\"]"));
        assert!(allocator.contains("package = \"elm-lsp-allocator\""));
        assert!(allocator.contains("../../kernel-source/libs/allocator"));
        assert!(allocator.contains("formal-extern = \"__elm_host_allocator\""));
        let allocator_source = metadata_facade_source("allocator", "__elm_host_allocator");
        assert!(allocator_source.contains("cfg(feature = \"lsp\")"));
        assert!(allocator_source.contains("extern crate __elm_lsp_allocator as __elm_backend"));
        assert!(allocator_source.contains("extern crate __elm_host_allocator as __elm_backend"));

        let general = metadata_facade_manifest("general", "__elm_host_general");
        assert!(general.contains("lsp = [\"dep:__elm_lsp_general\"]"));
        assert!(general.contains("package = \"elm-lsp-general\""));
        assert!(general.contains("../../kernel-source/general"));
    }

    #[test]
    fn framework_is_an_independent_nested_workspace() {
        let manifest = framework_workspace_manifest();
        assert!(manifest.contains("\"elm\""));
        assert!(manifest.contains("\"kernel-symbols\""));
        assert!(manifest.contains("\"allocator\""));
        assert!(manifest.contains("\"general\""));
        assert!(manifest.contains("[workspace.package]"));
    }

    #[test]
    fn packaged_framework_hash_covers_the_complete_distribution() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root =
            std::env::temp_dir().join(format!("cargo-elm-framework-hash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let manifest = KernelInterfaceManifest {
            target: "test-target".to_string(),
            profile: "test-profile".to_string(),
            bridge_abi_version: KERNEL_API_BRIDGE_ABI_V1,
            kernel_hash: [1; 32],
            interface_hash: [2; 32],
            source_hash: [3; 32],
            framework_hash: framework_distribution_hash(&repository).unwrap(),
            source_file_count: 1,
            allocator_metadata: "allocator.rlib".to_string(),
            general_metadata: "general.rlib".to_string(),
            support_library: "support.a".to_string(),
            import_library: "imports.so".to_string(),
            symbols: Vec::new(),
        };
        copy_framework(&repository, &root, &manifest.target, &manifest).unwrap();
        assert_eq!(
            packaged_framework_hash(&root).unwrap(),
            manifest.framework_hash
        );

        fs::write(root.join("unexpected.cfg"), b"tampered").unwrap();
        assert_ne!(
            packaged_framework_hash(&root).unwrap(),
            manifest.framework_hash
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lsp_projection_namespaces_packages_and_path_dependencies() {
        let root =
            std::env::temp_dir().join(format!("cargo-elm-lsp-projection-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("alpha/src")).unwrap();
        fs::create_dir_all(root.join("beta/src")).unwrap();
        fs::write(
            root.join("alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nbeta = { path = \"../beta\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("beta/Cargo.toml"),
            "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("alpha/src/lib.rs"), "#![no_std]\n").unwrap();
        fs::write(root.join("beta/src/lib.rs"), "#![no_std]\n").unwrap();

        project_lsp_package_manifests(&root, &["alpha".to_string(), "beta".to_string()]).unwrap();
        let alpha = fs::read_to_string(root.join("alpha/Cargo.toml")).unwrap();
        assert!(alpha.contains("name = \"elm-lsp-alpha\""));
        assert!(alpha.contains("package = \"elm-lsp-beta\""));
        assert!(alpha.contains("autotests = false"));
        assert!(
            fs::read_to_string(root.join("alpha/src/lib.rs"))
                .unwrap()
                .starts_with("#![allow(warnings)]")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
