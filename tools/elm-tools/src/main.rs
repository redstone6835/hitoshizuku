use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};

use elm::{
    ELM_EBI_ABI_VERSION, ELM_EBI_IMPORT_FLAG_EXACT_RUST_API, ELM_EBI_IMPORT_FLAG_KERNEL_STATIC,
    ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL, ELM_EBI_RUST_ABI_VERSION as RUST_ABI,
    ELM_EBI_SEGMENT_FLAG_EXECUTE, ELM_EBI_SEGMENT_FLAG_READ, ELM_EBI_SEGMENT_FLAG_WRITE,
    ELM_EBI_SEGMENT_FLAG_ZERO_FILL, ELM_EBI_SYMBOL_NAME_LEN, ELM_EKI_ABI_FINGERPRINT_BLOCK_SIZE,
    ELM_EKI_BLOCK_DESC_SIZE, ELM_EKI_ELMAPI_BLOCK_SIZE, ELM_EKI_ELMAPI_BLOCK_VERSION,
    ELM_EKI_FORMAT_VERSION, ELM_EKI_HEADER_SIZE, ELM_EKI_IMAGE_HASH_SHA256_SIZE, ELM_EKI_MAGIC,
    ELM_EKI_MANIFEST_NAME_LEN, ELM_EKI_MANIFEST_VERSION_LEN, ELM_EKI_PROOF_ALGORITHM_ED25519,
    ELM_EKI_PROOF_BLOCK_SIZE, ELM_EKI_PROVIDER_PORT_RECORD_SIZE, ELM_MENU_DESCRIPTION_LEN,
    ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ELM_NEXUS_CONTRACT_LEN, ELM_PROOF_ABI_VERSION,
    ELM_PROOF_ED25519_SIGNATURE_LEN, ELM_PROOF_SHA256_LEN, ELM_PROOF_SOURCE_IDENTIFIER_LEN,
    ELM_RUST_ABI_FINGERPRINT_VERSION, ElmEbiArch, ElmEbiProofV1, ElmEbiRelocationKind,
    ElmEbiSegmentKind, ElmEkiBlockKind, ElmKind, ElmModuleDescriptorV1, ElmPanicStrategy,
    ElmRustAbiFingerprintV1, ElmTrustAnchor, ElmTrustStore, canonical_ebi_digest,
    kernel_interface_manifest_v1, parse_eki_image, sha256, sha256_with_zeroed_range,
    sha256_with_zeroed_ranges,
};

mod kernel_interface;
mod project;
mod rust_metadata;

use kernel_interface::{
    KernelInterfaceManifest, emit_kernel_symbol_probe, export_kernel_interface,
};
use project::{
    ElmProjectDependency, ElmProjectManifest, cargo_build, scaffold_project, sync_framework,
};
use rust_metadata::{
    ExportSpec, ExtensionPointSpec, ExtensionSpec, ImportSpec, NativeMetadata, ProviderSpec,
    retain_linked_kernel_symbol_imports,
};

const ELM_TOOL_PAGE_SIZE: u64 = 4096;
const BLOCK_MANIFEST: u32 = ElmEkiBlockKind::Manifest as u32;
const BLOCK_MENU: u32 = ElmEkiBlockKind::Menu as u32;
const BLOCK_SEGMENTS: u32 = ElmEkiBlockKind::Segments as u32;
const BLOCK_CODE: u32 = ElmEkiBlockKind::Code as u32;
const BLOCK_RODATA: u32 = ElmEkiBlockKind::ReadOnlyData as u32;
const BLOCK_DATA: u32 = ElmEkiBlockKind::Data as u32;
const BLOCK_BSS: u32 = ElmEkiBlockKind::Bss as u32;
const BLOCK_IMPORTS: u32 = ElmEkiBlockKind::Imports as u32;
const BLOCK_EXPORTS: u32 = ElmEkiBlockKind::Exports as u32;
const BLOCK_LIFECYCLE_HOOKS: u32 = ElmEkiBlockKind::LifecycleHooks as u32;
const BLOCK_SYMBOL_LOCATIONS: u32 = ElmEkiBlockKind::SymbolLocations as u32;
const BLOCK_RELOCATIONS: u32 = ElmEkiBlockKind::Relocation as u32;
const BLOCK_PROVIDER_PORTS: u32 = ElmEkiBlockKind::ProviderPorts as u32;
const BLOCK_DEPENDENCIES: u32 = ElmEkiBlockKind::Dependencies as u32;
const BLOCK_EXTENSION_POINTS: u32 = ElmEkiBlockKind::ExtensionPoints as u32;
const BLOCK_EXTENSIONS: u32 = ElmEkiBlockKind::Extensions as u32;
const BLOCK_API_COMPATIBILITY: u32 = ElmEkiBlockKind::ApiCompatibility as u32;
const BLOCK_ABI_FINGERPRINT: u32 = ElmEkiBlockKind::AbiFingerprint as u32;
const BLOCK_PROOF: u32 = ElmEkiBlockKind::Signature as u32;
const MENU_KIND_ACTION: u32 = 2;
const HOOK_INITIALIZE: u32 = 1;
const HOOK_FINALIZE: u32 = 2;
const RUST_HOOK_CONTEXT_RESULT: u16 = 1;
const EKI_TABLE_HEADER_SIZE: usize = 8;
const EKI_SEGMENT_RECORD_SIZE: usize = 32;
const EKI_SYMBOL_RECORD_SIZE: usize = 48 + ELM_EBI_SYMBOL_NAME_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_SYMBOL_LOCATION_RECORD_SIZE: usize = 32 + ELM_EBI_SYMBOL_NAME_LEN;
const EKI_RELOCATION_RECORD_SIZE: usize = 32;
const EKI_DEPENDENCY_RECORD_SIZE: usize = 8 + elm::ELM_EBI_NAME_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_EXTENSION_POINT_RECORD_SIZE: usize =
    16 + elm::ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_EXTENSION_RECORD_SIZE: usize =
    24 + elm::ELM_EBI_NAME_LEN + elm::ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN * 2;
const ELF_TYPE_DYN: u16 = 3;
const ELF_SECTION_RELA: u32 = 4;
const ELF_SECTION_REL: u32 = 9;
const ELF_SECTION_FLAG_ALLOC: u64 = 1 << 1;
const ELF_RELOCATION_RELATIVE: u32 = 3;
const ELF_RELOCATION_ABS64: u32 = 2;
const ELF_RELOCATION_JUMP_SLOT: u32 = 5;
const ELF_SYMBOL_TABLE: u32 = 2;
const ELF_SYMBOL_BIND_GLOBAL: u8 = 1;
const ELF_SYMBOL_TYPE_OBJECT: u8 = 1;
const ELF_SYMBOL_TYPE_FUNCTION: u8 = 2;
const ELF_SECTION_INDEX_UNDEFINED: u16 = 0;
const ELF_PROGRAM_FLAG_EXECUTE: u32 = 1;
const ELF_PROGRAM_FLAG_WRITE: u32 = 2;
const ELF_PROGRAM_FLAG_READ: u32 = 4;
const ELF_MAX_PROGRAM_HEADERS: u16 = 64;
const ELF_MAX_SECTION_HEADERS: u16 = 4096;

#[derive(Clone)]
struct PackerBlock {
    kind: u32,
    flags: u32,
    payload: Vec<u8>,
    mem_size: u64,
    align: u64,
}

impl PackerBlock {
    fn new(kind: u32, payload: Vec<u8>) -> Self {
        let mem_size = payload.len() as u64;
        Self {
            kind,
            flags: 0,
            payload,
            mem_size,
            align: 0,
        }
    }

    fn segment(kind: u32, payload: Vec<u8>, mem_size: u64, align: u64) -> Self {
        Self {
            kind,
            flags: 0,
            payload,
            mem_size,
            align,
        }
    }
}

#[derive(Clone)]
struct ElmApiSpec {
    root_import_index: u32,
    versions: Vec<u16>,
    required_features: u64,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("elm-tools: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let Some(command) = args.get(1).map(String::as_str) else {
        usage();
        return Err("missing command".to_string());
    };
    match command {
        "new" => cmd_new(&args[2..]),
        "sync-framework" => cmd_sync_framework(&args[2..]),
        "export-interface" => cmd_export_interface(&args[2..]),
        "emit-symbol-probe" => cmd_emit_symbol_probe(&args[2..]),
        "build" => cmd_build(&args[2..]),
        "pack-metadata" | "pack" => cmd_pack_metadata(&args[2..]),
        "pack-elf" => cmd_pack_elf(&args[2..]),
        "inspect" => cmd_inspect(&args[2..]),
        "hash" => cmd_hash(&args[2..]),
        "keygen" => cmd_keygen(&args[2..]),
        "sign" => cmd_sign(&args[2..]),
        "verify" => cmd_verify(&args[2..]),
        "fingerprint-header" => cmd_fingerprint_header(&args[2..]),
        "help" | "-h" | "--help" => {
            usage();
            Ok(())
        }
        other => Err(format!("unknown command: {other}")),
    }
}

fn usage() {
    eprintln!("elm-tools commands:");
    eprintln!("  new <directory> --name <name> --kind <kind> --source <identifier>");
    eprintln!("  sync-framework <project-directory>");
    eprintln!(
        "  export-interface <kernel-elf> --target <triple> --profile <profile> --output <directory>"
    );
    eprintln!("  emit-symbol-probe <interface-manifest> <output.rs>");
    eprintln!(
        "  build <project-directory> --arch <riscv64|loongarch64|all> --key <seed> --epoch <n>"
    );
    eprintln!("  build <project-directory> --arch <riscv64|loongarch64|all> --unsigned");
    eprintln!(
        "  pack-metadata <out.eki> <name> <version> <kind> [--menu <label> <description> <route>]"
    );
    eprintln!("  pack <out.eki> <name> <version> <kind> [--menu <label> <description> <route>]");
    eprintln!("  pack-elf <project-directory> <image.elf> <out.eki>");
    eprintln!("  inspect <file.eki>");
    eprintln!("  hash <in.eki> <out.eki>");
    eprintln!("  keygen <private-seed.bin> <public-key.bin>");
    eprintln!("  sign <in.eki> <out.eki> <private-seed.bin> <source-id> <release-epoch>");
    eprintln!("  verify <file.eki>");
    eprintln!("  fingerprint-header <target-triple> <output-header>");
}

fn cmd_new(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        usage();
        return Err("new 缺少目标目录".to_string());
    }
    let directory = Path::new(&args[0]);
    let options = parse_named_options(&args[1..], &["--name", "--kind", "--source"])?;
    scaffold_project(
        directory,
        required_option(&options, "--name")?,
        required_option(&options, "--kind")?,
        required_option(&options, "--source")?,
    )?;
    println!("created ELM project: {}", directory.display());
    Ok(())
}

fn cmd_sync_framework(args: &[String]) -> Result<(), String> {
    if args.len() != 1 {
        usage();
        return Err("sync-framework 参数无效".to_string());
    }
    sync_framework(Path::new(&args[0]))?;
    println!("framework synchronized: {}", args[0]);
    Ok(())
}

fn cmd_export_interface(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        usage();
        return Err("export-interface 缺少内核 ELF".to_string());
    }
    let kernel = Path::new(&args[0]);
    let options = parse_named_options(&args[1..], &["--target", "--profile", "--output"])?;
    let target = required_option(&options, "--target")?;
    let profile = required_option(&options, "--profile")?;
    let output = Path::new(required_option(&options, "--output")?);
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .map_err(|error| format!("定位内核仓库失败: {error}"))?;
    let manifest = export_kernel_interface(&repository, target, profile, kernel, output)?;
    println!(
        "exported kernel interface: target={} symbols={} output={}",
        manifest.target,
        manifest.symbols.len(),
        output.display()
    );
    Ok(())
}

fn cmd_emit_symbol_probe(args: &[String]) -> Result<(), String> {
    if args.len() != 2 {
        usage();
        return Err("emit-symbol-probe 参数无效".to_string());
    }
    let count = emit_kernel_symbol_probe(Path::new(&args[0]), Path::new(&args[1]))?;
    println!(
        "generated kernel symbol probe: symbols={count} output={}",
        args[1]
    );
    Ok(())
}

fn cmd_build(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        usage();
        return Err("build 缺少工程目录".to_string());
    }
    let project = Path::new(&args[0]);
    let mut arch = None;
    let mut key = None;
    let mut epoch = None;
    let mut unsigned = false;
    let mut index = 1usize;
    while index < args.len() {
        match args[index].as_str() {
            "--arch" => {
                arch = Some(option_arg(args, index + 1, "--arch")?.to_string());
                index += 2;
            }
            "--key" => {
                key = Some(option_arg(args, index + 1, "--key")?.to_string());
                index += 2;
            }
            "--epoch" => {
                epoch = Some(parse_u64(
                    option_arg(args, index + 1, "--epoch")?,
                    "release epoch",
                )?);
                index += 2;
            }
            "--unsigned" => {
                if unsigned {
                    return Err("--unsigned 只能指定一次".to_string());
                }
                unsigned = true;
                index += 1;
            }
            option => return Err(format!("未知 build 参数: {option}")),
        }
    }
    let arch = arch.ok_or_else(|| "build 必须指定 --arch".to_string())?;
    if unsigned {
        if key.is_some() || epoch.is_some() {
            return Err("--unsigned 不能与 --key/--epoch 同时使用".to_string());
        }
    } else if key.is_none() || epoch.is_none() || epoch == Some(0) {
        return Err("签名构建必须提供 --key 和非零 --epoch".to_string());
    }
    let targets: &[(&str, &str)] = match arch.as_str() {
        "riscv64" => &[("riscv64", "riscv64gc-unknown-none-elf")],
        "loongarch64" => &[("loongarch64", "loongarch64-unknown-none")],
        "all" => &[
            ("riscv64", "riscv64gc-unknown-none-elf"),
            ("loongarch64", "loongarch64-unknown-none"),
        ],
        _ => return Err(format!("未知 build arch: {arch}")),
    };
    let manifest = ElmProjectManifest::load(project)?;
    sync_framework(project)?;
    let output_dir = project.join("dist");
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("创建 {} 失败: {err}", output_dir.display()))?;
    let signing = match key {
        Some(path) => Some(SigningKey::from_bytes(&read_fixed_file::<32>(
            &path,
            "private seed",
        )?)),
        None => None,
    };
    for (arch_name, target) in targets {
        let elf = cargo_build(project, target, &manifest.cargo_name())?;
        let unsigned_path = output_dir.join(format!(".{}-{arch_name}.unsigned.eki", manifest.name));
        let output_path = output_dir.join(format!("{}-{arch_name}.eki", manifest.name));
        pack_elf_project(project, &elf, &unsigned_path)?;
        if let Some(signing) = &signing {
            let bytes = fs::read(&unsigned_path)
                .map_err(|err| format!("读取 {} 失败: {err}", unsigned_path.display()))?;
            let signed = sign_eki_image(
                &bytes,
                signing,
                &manifest.source,
                epoch.expect("已校验签名 epoch"),
            )?;
            fs::write(&output_path, signed)
                .map_err(|err| format!("写入 {} 失败: {err}", output_path.display()))?;
            fs::remove_file(&unsigned_path).map_err(|err| format!("删除临时镜像失败: {err}"))?;
        } else {
            fs::rename(&unsigned_path, &output_path)
                .map_err(|err| format!("写入 {} 失败: {err}", output_path.display()))?;
        }
        println!("built {}", output_path.display());
    }
    Ok(())
}

fn parse_named_options(
    args: &[String],
    allowed: &[&str],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut options = std::collections::BTreeMap::new();
    let mut index = 0usize;
    while index < args.len() {
        let option = args[index].as_str();
        if !allowed.contains(&option) {
            return Err(format!("未知参数: {option}"));
        }
        let value = option_arg(args, index + 1, option)?.to_string();
        if options.insert(option.to_string(), value).is_some() {
            return Err(format!("重复参数: {option}"));
        }
        index += 2;
    }
    Ok(options)
}

fn required_option<'a>(
    options: &'a std::collections::BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, String> {
    options
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| format!("缺少参数 {name}"))
}

fn cmd_fingerprint_header(args: &[String]) -> Result<(), String> {
    if args.len() != 2 {
        usage();
        return Err("bad fingerprint-header arguments".to_string());
    }
    let arch = match args[0].as_str() {
        "riscv64gc-unknown-none-elf" => ElmEbiArch::Riscv64,
        "loongarch64-unknown-none" => ElmEbiArch::LoongArch64,
        _ => return Err(format!("unsupported ELM target triple: {}", args[0])),
    };
    let fingerprint = default_abi_fingerprint(arch);
    let mut header = String::new();
    header.push_str("#ifndef ELM_FINGERPRINT_H\n#define ELM_FINGERPRINT_H\n");
    header.push_str(&format!("#define ELM_FINGERPRINT_ARCH {}\n", arch as u32));
    header.push_str("#define ELM_FINGERPRINT_RUSTC_HASH_BYTES ");
    append_c_byte_list(&mut header, &fingerprint.rustc_commit_hash);
    header.push_str("\n#define ELM_FINGERPRINT_TARGET_HASH_BYTES ");
    append_c_byte_list(&mut header, &fingerprint.target_spec_hash);
    header.push_str("\n#define ELM_FINGERPRINT_KERNEL_INTERFACE_HASH_BYTES ");
    append_c_byte_list(&mut header, &fingerprint.kernel_interface_hash);
    header.push_str("\n#endif\n");
    if let Some(parent) = std::path::Path::new(&args[1]).parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&args[1], header).map_err(|err| format!("write {}: {err}", args[1]))
}

fn append_c_byte_list(out: &mut String, bytes: &[u8]) {
    use std::fmt::Write as _;

    for (index, byte) in bytes.iter().enumerate() {
        if index != 0 {
            out.push_str(", ");
        }
        write!(out, "0x{byte:02x}").unwrap();
    }
}

fn cmd_pack_metadata(args: &[String]) -> Result<(), String> {
    if args.len() != 4 && args.len() != 8 {
        usage();
        return Err("bad pack-metadata arguments".to_string());
    }
    let out = &args[0];
    let name = &args[1];
    let version = &args[2];
    let kind = parse_kind(&args[3])?;
    let mut blocks = vec![
        PackerBlock::new(BLOCK_MANIFEST, manifest_block(name, version, kind)?),
        PackerBlock::new(
            BLOCK_ABI_FINGERPRINT,
            abi_fingerprint_block(&default_abi_fingerprint(ElmEbiArch::Any)),
        ),
        PackerBlock::new(BLOCK_LIFECYCLE_HOOKS, lifecycle_hooks_block()),
    ];
    if args.len() == 8 {
        if args[4] != "--menu" {
            return Err("expected --menu".to_string());
        }
        blocks.insert(
            1,
            PackerBlock::new(BLOCK_MENU, menu_block(&args[5], &args[6], &args[7])?),
        );
    }
    let image = eki_image_with_hash(ElmEbiArch::Any, &blocks);
    fs::write(out, image).map_err(|err| format!("write {out}: {err}"))?;
    Ok(())
}

fn cmd_pack_elf(args: &[String]) -> Result<(), String> {
    if args.len() != 3 {
        usage();
        return Err("bad pack-elf arguments".to_string());
    }
    pack_elf_project(
        Path::new(&args[0]),
        Path::new(&args[1]),
        Path::new(&args[2]),
    )
}

fn pack_elf_project(project: &Path, elf_path: &Path, output: &Path) -> Result<(), String> {
    let manifest = ElmProjectManifest::load(project)?;
    let kind = parse_kind(&manifest.kind)?;
    let elf_bytes =
        fs::read(elf_path).map_err(|err| format!("读取 {} 失败: {err}", elf_path.display()))?;
    let elf = ElfImage::parse(&elf_bytes)?;
    let interface_target = target_triple_for_arch(elf.arch)?;
    let interface_path = project
        .join(".elm/kernel-interface")
        .join(interface_target)
        .join("manifest.txt");
    let interface = KernelInterfaceManifest::load(&interface_path)?;
    if interface.target != interface_target {
        return Err(format!(
            "ELM 目标 {} 与内核接口包 {} 不一致",
            interface_target, interface.target
        ));
    }
    validate_runtime_layout(&elf.load_segments)?;
    let metadata_section = elf.elm_metadata_section(&elf_bytes)?;
    let mut metadata = NativeMetadata::parse(metadata_section)?;
    retain_linked_kernel_symbol_imports(&mut metadata.imports, |symbol| {
        elf.symbols.iter().any(|candidate| candidate.name == symbol)
    });
    validate_native_symbols(&elf, &metadata)?;
    let relocation_records =
        dynamic_runtime_relocations(&elf, &elf_bytes, &interface, &mut metadata.imports)?;
    let relocations = native_relocations_block(&elf, &metadata.imports, relocation_records)?;
    let mut blocks = vec![
        PackerBlock::new(
            BLOCK_MANIFEST,
            manifest_block(&manifest.name, &manifest.version, kind)?,
        ),
        PackerBlock::new(
            BLOCK_ABI_FINGERPRINT,
            abi_fingerprint_block(&default_abi_fingerprint(elf.arch)),
        ),
    ];
    if let Some(menu) = &manifest.menu {
        blocks.push(PackerBlock::new(
            BLOCK_MENU,
            menu_block(&menu.label, &menu.description, &menu.route)?,
        ));
    }
    if !manifest.dependencies.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_DEPENDENCIES,
            dependencies_block(&manifest.dependencies)?,
        ));
    }
    if !metadata.extension_points.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_EXTENSION_POINTS,
            extension_points_block(&metadata.extension_points)?,
        ));
    }
    if !metadata.extensions.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_EXTENSIONS,
            extensions_block(&metadata.extensions)?,
        ));
    }
    blocks.push(PackerBlock::new(
        BLOCK_SEGMENTS,
        segments_block(&elf.load_segments, relocation_segment_len(&relocations)),
    ));
    for segment in &elf.load_segments {
        blocks.push(segment_block(segment, &elf_bytes)?);
    }
    blocks.push(PackerBlock::new(
        BLOCK_IMPORTS,
        import_records_block(&metadata.imports)?,
    ));
    blocks.push(PackerBlock::new(
        BLOCK_API_COMPATIBILITY,
        elmapi_compatibility_block(&ElmApiSpec {
            root_import_index: metadata.api_root_import_index,
            versions: metadata.api_versions.clone(),
            required_features: metadata.api_required_features,
        }),
    ));
    if !metadata.exports.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_EXPORTS,
            export_records_block(&metadata.exports)?,
        ));
    }
    if !metadata.providers.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_PROVIDER_PORTS,
            provider_ports_block(&metadata.providers)?,
        ));
    }
    blocks.push(PackerBlock::new(
        BLOCK_SYMBOL_LOCATIONS,
        symbol_locations_block(&elf, &metadata.symbol_names())?,
    ));
    let relocation_len = relocations.len() as u64;
    blocks.push(PackerBlock::segment(
        BLOCK_RELOCATIONS,
        relocations,
        relocation_len,
        8,
    ));
    let image = eki_image_with_hash(elf.arch, &blocks);
    parse_eki_image(&image).map_err(|status| format!("生成的 EKI 无法通过自校验: {status:?}"))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("创建 {} 失败: {err}", parent.display()))?;
    }
    fs::write(output, image).map_err(|err| format!("写入 {} 失败: {err}", output.display()))
}

fn option_arg<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing argument for {option}"))
}

fn parse_u64(raw: &str, name: &str) -> Result<u64, String> {
    if let Some(hex) = raw.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| format!("bad {name}: {raw}"))
    } else {
        raw.parse::<u64>().map_err(|_| format!("bad {name}: {raw}"))
    }
}

fn cmd_inspect(args: &[String]) -> Result<(), String> {
    if args.len() != 1 {
        usage();
        return Err("bad inspect arguments".to_string());
    }
    let bytes = fs::read(&args[0]).map_err(|err| format!("read {}: {err}", args[0]))?;
    let header = Header::parse(&bytes)?;
    println!("format=EKI");
    println!("version={}", header.format_version);
    println!("ebi_abi={}", header.ebi_abi_version);
    println!("file_size={}", header.file_size);
    println!("arch={}", header.arch);
    println!("blocks={}", header.block_count);
    println!(
        "image_hash={}",
        verify_header_hash(&bytes)?.unwrap_or(HashState::Missing)
    );
    for index in 0..header.block_count as usize {
        let desc = BlockDesc::parse(
            &bytes,
            header.block_table_offset as usize + index * ELM_EKI_BLOCK_DESC_SIZE,
        )?;
        println!(
            "block[{index}] kind={} offset={} file_size={} mem_size={} flags=0x{:x}",
            block_name(desc.kind),
            desc.offset,
            desc.file_size,
            desc.mem_size,
            desc.flags
        );
    }
    let image =
        parse_eki_image(&bytes).map_err(|status| format!("EKI parse failed: {status:?}"))?;
    if let Some(elmapi) = &image.unit.api_compatibility {
        println!("elmapi.root_import_index={}", elmapi.root_import_index);
        println!("elmapi.required_features=0x{:x}", elmapi.required_features);
        println!(
            "elmapi.compatible_versions={:?}",
            elmapi.compatible_versions
        );
    }
    for (index, import) in image.unit.imports.iter().enumerate() {
        println!(
            "import[{index}] name={} contract={} versions={}..={} flags=0x{:x} kernel_symbol={} rust_abi_sha256={}",
            import.name,
            import.contract.as_str(),
            import.min_version,
            import.max_version,
            import.flags,
            import.is_kernel_symbol(),
            hex_digest(&import.rust_abi_hash),
        );
    }
    for (index, export) in image.unit.exports.iter().enumerate() {
        println!(
            "export[{index}] name={} contract={} version={} flags=0x{:x} direct_pinned={} rust_abi_sha256={}",
            export.name,
            export.contract.as_str(),
            export.version,
            export.flags,
            export.is_direct_pinned(),
            hex_digest(&export.rust_abi_hash),
        );
    }
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("写入 String 不会失败");
    }
    output
}

fn cmd_hash(args: &[String]) -> Result<(), String> {
    if args.len() != 2 {
        usage();
        return Err("bad hash arguments".to_string());
    }
    let mut bytes = fs::read(&args[0]).map_err(|err| format!("read {}: {err}", args[0]))?;
    rewrite_header_hash(&mut bytes)?;
    fs::write(&args[1], bytes).map_err(|err| format!("write {}: {err}", args[1]))?;
    Ok(())
}

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    if args.len() != 2 {
        usage();
        return Err("bad keygen arguments".to_string());
    }
    let mut seed = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut seed))
        .map_err(|err| format!("read /dev/urandom: {err}"))?;
    let signing = SigningKey::from_bytes(&seed);
    fs::write(&args[0], seed).map_err(|err| format!("write {}: {err}", args[0]))?;
    fs::write(&args[1], signing.verifying_key().to_bytes())
        .map_err(|err| format!("write {}: {err}", args[1]))?;
    Ok(())
}

fn cmd_sign(args: &[String]) -> Result<(), String> {
    if args.len() != 5 {
        usage();
        return Err("bad sign arguments".to_string());
    }
    let input = fs::read(&args[0]).map_err(|err| format!("read {}: {err}", args[0]))?;
    let seed = read_fixed_file::<32>(&args[2], "private seed")?;
    let release_epoch = args[4]
        .parse::<u64>()
        .map_err(|_| "release epoch must be an unsigned integer".to_string())?;
    if release_epoch == 0 {
        return Err("release epoch must be nonzero".to_string());
    }
    let output = sign_eki_image(
        &input,
        &SigningKey::from_bytes(&seed),
        &args[3],
        release_epoch,
    )?;
    fs::write(&args[1], output).map_err(|err| format!("write {}: {err}", args[1]))?;
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    if args.len() != 1 {
        usage();
        return Err("bad verify arguments".to_string());
    }
    let bytes = fs::read(&args[0]).map_err(|err| format!("read {}: {err}", args[0]))?;
    match verify_header_hash(&bytes)? {
        Some(HashState::Valid) => {}
        Some(HashState::Invalid) => return Err("image hash mismatch".to_string()),
        Some(HashState::Missing) | None => return Err("image hash missing".to_string()),
    }
    let image =
        parse_eki_image(&bytes).map_err(|status| format!("EKI parse failed: {status:?}"))?;
    let proof = image
        .proof
        .as_ref()
        .ok_or_else(|| "EBI proof missing".to_string())?;
    let fingerprint = image
        .abi_fingerprint
        .as_ref()
        .ok_or_else(|| "Rust ABI fingerprint missing".to_string())?;
    let anchor = ElmTrustAnchor::new("embedded", proof.signer_public_key)
        .map_err(|status| format!("invalid signer public key: {status:?}"))?;
    let mut trust = ElmTrustStore::new();
    trust
        .register_anchor(anchor)
        .map_err(|err| format!("register embedded signer: {err:?}"))?;
    trust.seal();
    trust
        .verify(&image, proof, fingerprint)
        .map_err(|err| format!("signature verification failed: {err:?}"))?;
    println!("verify: ok");
    Ok(())
}

fn sign_eki_image(
    bytes: &[u8],
    signing: &SigningKey,
    source_identifier: &str,
    release_epoch: u64,
) -> Result<Vec<u8>, String> {
    if source_identifier.is_empty()
        || source_identifier.len() > ELM_PROOF_SOURCE_IDENTIFIER_LEN
        || source_identifier.as_bytes().contains(&0)
    {
        return Err("invalid source identifier".to_string());
    }
    let image = parse_eki_image(bytes).map_err(|status| format!("EKI parse failed: {status:?}"))?;
    let fingerprint = image
        .abi_fingerprint
        .as_ref()
        .ok_or_else(|| "Rust ABI fingerprint missing".to_string())?;
    let header = Header::parse(bytes)?;
    let arch = ElmEbiArch::from_raw(header.arch).ok_or_else(|| "invalid EKI arch".to_string())?;
    let mut blocks = extract_packer_blocks(bytes, &header)?;
    let proof_index = blocks.iter().position(|block| block.kind == BLOCK_PROOF);
    let placeholder = PackerBlock::new(BLOCK_PROOF, vec![0; ELM_EKI_PROOF_BLOCK_SIZE]);
    match proof_index {
        Some(index) => blocks[index] = placeholder,
        None => blocks.push(placeholder),
    }
    let placeholder_image = eki_image_with_hash(arch, &blocks);
    let placeholder_header = Header::parse(&placeholder_image)?;
    let proof_desc = find_block(&placeholder_image, &placeholder_header, BLOCK_PROOF)?;
    let mut ranges = [
        (
            placeholder_header.image_hash_offset as usize,
            placeholder_header.image_hash_size as usize,
        ),
        (proof_desc.offset as usize, proof_desc.file_size as usize),
    ];
    ranges.sort_unstable_by_key(|range| range.0);
    let source_digest = sha256_with_zeroed_ranges(&placeholder_image, &ranges)
        .ok_or_else(|| "proof source digest range invalid".to_string())?;
    let public_key = signing.verifying_key().to_bytes();
    let mut proof = ElmEbiProofV1 {
        source_identifier: source_identifier.to_string(),
        source_digest,
        subject_digest: canonical_ebi_digest(&image),
        signer_key_id: sha256(&public_key),
        signer_public_key: public_key,
        release_epoch,
        flags: 0,
        signature: [0; ELM_PROOF_ED25519_SIGNATURE_LEN],
    };
    proof.signature = signing
        .sign(&proof.unsigned_message(fingerprint))
        .to_bytes();
    let proof_payload = proof_block(&proof)?;
    let index = blocks
        .iter()
        .position(|block| block.kind == BLOCK_PROOF)
        .ok_or_else(|| "proof block disappeared".to_string())?;
    blocks[index] = PackerBlock::new(BLOCK_PROOF, proof_payload);
    let signed = eki_image_with_hash(arch, &blocks);
    let signed_header = Header::parse(&signed)?;
    let signed_proof = find_block(&signed, &signed_header, BLOCK_PROOF)?;
    let mut signed_ranges = [
        (
            signed_header.image_hash_offset as usize,
            signed_header.image_hash_size as usize,
        ),
        (
            signed_proof.offset as usize,
            signed_proof.file_size as usize,
        ),
    ];
    signed_ranges.sort_unstable_by_key(|range| range.0);
    let signed_source_digest = sha256_with_zeroed_ranges(&signed, &signed_ranges)
        .ok_or_else(|| "signed proof source digest range invalid".to_string())?;
    if signed_source_digest != proof.source_digest {
        return Err("签名后 source digest 自校验失败".to_string());
    }
    parse_eki_image(&signed).map_err(|status| format!("签名后 EKI 自校验失败: {status:?}"))?;
    Ok(signed)
}

fn extract_packer_blocks(bytes: &[u8], header: &Header) -> Result<Vec<PackerBlock>, String> {
    let mut blocks = Vec::new();
    for index in 0..header.block_count as usize {
        let desc = BlockDesc::parse(
            bytes,
            header.block_table_offset as usize + index * ELM_EKI_BLOCK_DESC_SIZE,
        )?;
        let start = desc.offset as usize;
        let end = start
            .checked_add(desc.file_size as usize)
            .ok_or_else(|| "block range overflow".to_string())?;
        let payload = bytes
            .get(start..end)
            .ok_or_else(|| "block range out of file".to_string())?
            .to_vec();
        blocks.push(PackerBlock {
            kind: desc.kind,
            flags: desc.flags,
            payload,
            mem_size: desc.mem_size,
            align: desc.align,
        });
    }
    Ok(blocks)
}

fn find_block(bytes: &[u8], header: &Header, kind: u32) -> Result<BlockDesc, String> {
    for index in 0..header.block_count as usize {
        let desc = BlockDesc::parse(
            bytes,
            header.block_table_offset as usize + index * ELM_EKI_BLOCK_DESC_SIZE,
        )?;
        if desc.kind == kind {
            return Ok(desc);
        }
    }
    Err("required block missing".to_string())
}

fn proof_block(proof: &ElmEbiProofV1) -> Result<Vec<u8>, String> {
    proof
        .validate_shape()
        .map_err(|status| format!("invalid proof: {status:?}"))?;
    let mut out = vec![0; ELM_EKI_PROOF_BLOCK_SIZE];
    write_u16(&mut out, 0, ELM_PROOF_ABI_VERSION);
    write_u16(&mut out, 2, ELM_EKI_PROOF_ALGORITHM_ED25519);
    write_u32(&mut out, 4, proof.flags);
    write_u64(&mut out, 8, proof.release_epoch);
    write_u16(&mut out, 16, proof.source_identifier.len() as u16);
    copy_fixed(&mut out, 24, &proof.source_identifier);
    out[152..184].copy_from_slice(&proof.source_digest);
    out[184..216].copy_from_slice(&proof.subject_digest);
    out[216..248].copy_from_slice(&proof.signer_key_id);
    out[248..280].copy_from_slice(&proof.signer_public_key);
    out[280..344].copy_from_slice(&proof.signature);
    Ok(out)
}

fn read_fixed_file<const N: usize>(path: &str, label: &str) -> Result<[u8; N], String> {
    let bytes = fs::read(path).map_err(|err| format!("read {path}: {err}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("{label} must contain exactly {N} bytes"))
}

#[derive(Clone, Copy)]
struct Header {
    format_version: u16,
    ebi_abi_version: u16,
    file_size: u64,
    block_table_offset: u64,
    image_hash_offset: u64,
    arch: u32,
    block_count: u32,
    image_hash_size: u32,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < ELM_EKI_HEADER_SIZE {
            return Err("file too small".to_string());
        }
        if bytes.get(0..8) != Some(&ELM_EKI_MAGIC) {
            return Err("bad EKI magic".to_string());
        }
        let header = Self {
            format_version: read_u16(bytes, 8)?,
            ebi_abi_version: read_u16(bytes, 10)?,
            file_size: read_u64(bytes, 16)?,
            block_table_offset: read_u64(bytes, 24)?,
            image_hash_offset: read_u64(bytes, 32)?,
            arch: read_u32(bytes, 40)?,
            block_count: read_u32(bytes, 48)?,
            image_hash_size: read_u32(bytes, 52)?,
        };
        if header.file_size as usize != bytes.len() {
            return Err("header file_size mismatch".to_string());
        }
        Ok(header)
    }
}

#[derive(Clone, Copy)]
struct BlockDesc {
    kind: u32,
    flags: u32,
    offset: u64,
    file_size: u64,
    mem_size: u64,
    align: u64,
}

impl BlockDesc {
    fn parse(bytes: &[u8], offset: usize) -> Result<Self, String> {
        Ok(Self {
            kind: read_u32(bytes, offset)?,
            flags: read_u32(bytes, offset + 4)?,
            offset: read_u64(bytes, offset + 8)?,
            file_size: read_u64(bytes, offset + 16)?,
            mem_size: read_u64(bytes, offset + 24)?,
            align: read_u64(bytes, offset + 32)?,
        })
    }
}

#[derive(Clone)]
struct ElfImage {
    arch: ElmEbiArch,
    load_segments: Vec<ElfLoadSegment>,
    sections: Vec<ElfSection>,
    symbols: Vec<ElfSymbol>,
}

#[derive(Clone)]
struct ElfLoadSegment {
    index: u32,
    kind: ElmEbiSegmentKind,
    flags: u32,
    offset: u64,
    vaddr: u64,
    file_size: u64,
    mem_size: u64,
    align: u64,
}

#[derive(Clone)]
struct ElfSymbol {
    name: String,
    value: u64,
    size: u64,
    symbol_type: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EkiRelocationSpec {
    kind: ElmEbiRelocationKind,
    target_segment_index: u32,
    value_index: u32,
    target_offset: u64,
    addend: i64,
}

#[derive(Clone, Copy)]
struct ElfHeader {
    file_type: u16,
    machine: u16,
    phoff: u64,
    shoff: u64,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[derive(Clone)]
struct ElfSection {
    name: String,
    section_type: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    entsize: u64,
}

impl ElfImage {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let header = parse_elf_header(bytes)?;
        if header.file_type != ELF_TYPE_DYN {
            return Err("ELM 原生 ELF 必须是 PIE/ET_DYN；请通过 elm-tools build 构建".to_string());
        }
        let arch = arch_from_machine(header.machine)?;
        let mut load_segments = parse_elf_load_segments(bytes, &header)?;
        if load_segments.is_empty() {
            return Err("ELF has no PT_LOAD segment".to_string());
        }
        load_segments.sort_by_key(|segment| segment.vaddr);
        for (index, segment) in load_segments.iter_mut().enumerate() {
            segment.index = index as u32;
        }
        let sections = parse_elf_sections(bytes, &header)?;
        let symbols = parse_elf_symbols(bytes, &sections)?;
        Ok(Self {
            arch,
            load_segments,
            sections,
            symbols,
        })
    }

    fn symbol(&self, name: &str) -> Result<&ElfSymbol, String> {
        let mut matches = self.symbols.iter().filter(|symbol| symbol.name == name);
        let symbol = matches
            .next()
            .ok_or_else(|| format!("symbol not found in ELF: {name}"))?;
        if matches.next().is_some() {
            return Err(format!("ELF symbol is ambiguous: {name}"));
        }
        Ok(symbol)
    }

    fn symbol_location(&self, name: &str) -> Result<(u32, u64, u64), String> {
        let symbol = self.symbol(name)?;
        if symbol.size == 0 {
            return Err(format!("ELF symbol has zero size: {name}"));
        }
        let segment = self
            .load_segments
            .iter()
            .find(|segment| {
                let end = segment.vaddr.saturating_add(segment.mem_size);
                symbol.value >= segment.vaddr && symbol.value < end
            })
            .ok_or_else(|| format!("symbol is outside PT_LOAD segments: {name}"))?;
        let offset = symbol.value - segment.vaddr;
        let size = symbol.size;
        if offset.saturating_add(size) > segment.mem_size {
            return Err(format!("symbol range is outside segment: {name}"));
        }
        Ok((segment.index, offset, size))
    }

    fn contains_image_address(&self, vaddr: u64) -> bool {
        self.load_segments.iter().any(|segment| {
            segment
                .vaddr
                .checked_add(segment.mem_size)
                .is_some_and(|end| vaddr >= segment.vaddr && vaddr < end)
        })
    }

    fn relocation_target(&self, vaddr: u64, width: u64) -> Result<(u32, u64), String> {
        let end = vaddr
            .checked_add(width)
            .ok_or_else(|| "ELF relocation target overflow".to_string())?;
        let segment = self
            .load_segments
            .iter()
            .find(|segment| {
                vaddr >= segment.vaddr && end <= segment.vaddr.saturating_add(segment.mem_size)
            })
            .ok_or_else(|| format!("ELF relocation target is outside PT_LOAD: 0x{vaddr:x}"))?;
        if matches!(segment.kind, ElmEbiSegmentKind::Code) {
            return Err(format!("ELM 不允许 text relocation: 0x{vaddr:x}"));
        }
        Ok((segment.index, vaddr - segment.vaddr))
    }

    fn elm_metadata_section<'a>(&self, bytes: &'a [u8]) -> Result<&'a [u8], String> {
        let mut sections = self
            .sections
            .iter()
            .filter(|section| section.name == ".elm.meta");
        let section = sections
            .next()
            .ok_or_else(|| "ELF 缺少非装载段 .elm.meta".to_string())?;
        if sections.next().is_some() {
            return Err("ELF 包含多个 .elm.meta section".to_string());
        }
        if section.section_type != 1 || section.flags & 0x2 != 0 || section.size == 0 {
            return Err(".elm.meta 必须是非空、非 SHF_ALLOC 的 PROGBITS section".to_string());
        }
        let metadata_start = section.offset;
        let metadata_end = metadata_start
            .checked_add(section.size)
            .ok_or_else(|| ".elm.meta 文件范围溢出".to_string())?;
        if self.load_segments.iter().any(|segment| {
            let start = segment.offset;
            let end = start.saturating_add(segment.file_size);
            metadata_start < end && start < metadata_end
        }) {
            return Err(".elm.meta 不得落入任何 PT_LOAD 文件范围".to_string());
        }
        checked_slice(bytes, section.offset as usize, section.size as usize)
    }
}

fn parse_elf_header(bytes: &[u8]) -> Result<ElfHeader, String> {
    if bytes.len() < 64 || bytes.get(0..4) != Some(b"\x7fELF") {
        return Err("bad ELF magic".to_string());
    }
    if read_u8(bytes, 4)? != 2 || read_u8(bytes, 5)? != 1 || read_u8(bytes, 6)? != 1 {
        return Err("only ELF64 little-endian v1 is supported".to_string());
    }
    if read_u32(bytes, 20)? != 1 || read_u16(bytes, 52)? != 64 {
        return Err("unsupported ELF64 header version or size".to_string());
    }
    let header = ElfHeader {
        file_type: read_u16(bytes, 16)?,
        machine: read_u16(bytes, 18)?,
        phoff: read_u64(bytes, 32)?,
        shoff: read_u64(bytes, 40)?,
        phentsize: read_u16(bytes, 54)?,
        phnum: read_u16(bytes, 56)?,
        shentsize: read_u16(bytes, 58)?,
        shnum: read_u16(bytes, 60)?,
        shstrndx: read_u16(bytes, 62)?,
    };
    if header.phnum == 0
        || header.phnum > ELF_MAX_PROGRAM_HEADERS
        || header.shnum == 0
        || header.shnum > ELF_MAX_SECTION_HEADERS
    {
        return Err("ELF header table count exceeds ELM limits".to_string());
    }
    let phoff = usize::try_from(header.phoff)
        .map_err(|_| "ELF program header offset exceeds host address space".to_string())?;
    let shoff = usize::try_from(header.shoff)
        .map_err(|_| "ELF section header offset exceeds host address space".to_string())?;
    checked_slice(
        bytes,
        phoff,
        usize::from(header.phnum) * usize::from(header.phentsize),
    )?;
    checked_slice(
        bytes,
        shoff,
        usize::from(header.shnum) * usize::from(header.shentsize),
    )?;
    Ok(header)
}

fn arch_from_machine(machine: u16) -> Result<ElmEbiArch, String> {
    match machine {
        243 => Ok(ElmEbiArch::Riscv64),
        258 => Ok(ElmEbiArch::LoongArch64),
        _ => Err(format!("unsupported ELF machine: {machine}")),
    }
}

fn target_triple_for_arch(arch: ElmEbiArch) -> Result<&'static str, String> {
    match arch {
        ElmEbiArch::Riscv64 => Ok("riscv64gc-unknown-none-elf"),
        ElmEbiArch::LoongArch64 => Ok("loongarch64-unknown-none"),
        ElmEbiArch::Any => Err("原生 ELM 不能使用 Any 架构接口包".to_string()),
    }
}

fn parse_elf_load_segments(
    bytes: &[u8],
    header: &ElfHeader,
) -> Result<Vec<ElfLoadSegment>, String> {
    if header.phentsize as usize != 56 {
        return Err("unsupported ELF program header size".to_string());
    }
    let mut out = Vec::new();
    for index in 0..header.phnum as usize {
        let offset = checked_add(header.phoff as usize, index * header.phentsize as usize)?;
        let p_type = read_u32(bytes, offset)?;
        if p_type != 1 {
            continue;
        }
        let p_flags = read_u32(bytes, offset + 4)?;
        let file_offset = read_u64(bytes, offset + 8)?;
        let vaddr = read_u64(bytes, offset + 16)?;
        let file_size = read_u64(bytes, offset + 32)?;
        let mem_size = read_u64(bytes, offset + 40)?;
        let align = read_u64(bytes, offset + 48)?;
        if mem_size == 0 {
            continue;
        }
        if file_size > mem_size {
            return Err("ELF PT_LOAD file size exceeds memory size".to_string());
        }
        if p_flags != ELF_PROGRAM_FLAG_READ
            && p_flags != (ELF_PROGRAM_FLAG_READ | ELF_PROGRAM_FLAG_EXECUTE)
            && p_flags != (ELF_PROGRAM_FLAG_READ | ELF_PROGRAM_FLAG_WRITE)
        {
            return Err(format!("ELM PT_LOAD 权限无效或包含 W+X: 0x{p_flags:x}"));
        }
        if align < ELM_TOOL_PAGE_SIZE
            || !align.is_power_of_two()
            || file_offset % align != vaddr % align
            || file_offset % ELM_TOOL_PAGE_SIZE != 0
            || vaddr % ELM_TOOL_PAGE_SIZE != 0
        {
            return Err("ELM PT_LOAD 必须满足页对齐及 ELF offset/vaddr 同余约束".to_string());
        }
        vaddr
            .checked_add(mem_size)
            .ok_or_else(|| "ELF PT_LOAD memory range overflow".to_string())?;
        file_offset
            .checked_add(file_size)
            .ok_or_else(|| "ELF PT_LOAD file range overflow".to_string())?;
        checked_slice(bytes, file_offset as usize, file_size as usize)?;
        let kind = if p_flags & ELF_PROGRAM_FLAG_EXECUTE != 0 {
            if file_size == 0 {
                return Err("executable PT_LOAD segment cannot be empty".to_string());
            }
            ElmEbiSegmentKind::Code
        } else if p_flags & ELF_PROGRAM_FLAG_WRITE != 0 {
            if file_size == 0 {
                ElmEbiSegmentKind::Bss
            } else {
                ElmEbiSegmentKind::Data
            }
        } else {
            if file_size == 0 {
                return Err("readonly PT_LOAD segment cannot be empty".to_string());
            }
            ElmEbiSegmentKind::ReadOnlyData
        };
        out.push(ElfLoadSegment {
            index: index as u32,
            kind,
            flags: segment_flags(kind),
            offset: file_offset,
            vaddr,
            file_size,
            mem_size,
            align,
        });
    }
    Ok(out)
}

fn parse_elf_sections(bytes: &[u8], header: &ElfHeader) -> Result<Vec<ElfSection>, String> {
    if header.shentsize as usize != 64 {
        return Err("unsupported ELF section header size".to_string());
    }
    if header.shoff == 0 || header.shnum == 0 {
        return Err("ELF section table is required for symbol extraction".to_string());
    }
    if header.shstrndx as usize >= header.shnum as usize {
        return Err("bad ELF section string table index".to_string());
    }
    #[derive(Clone, Copy)]
    struct RawSection {
        name_offset: u32,
        section_type: u32,
        flags: u64,
        offset: u64,
        size: u64,
        link: u32,
        entsize: u64,
    }

    let mut raw_sections = Vec::new();
    for index in 0..header.shnum as usize {
        let offset = checked_add(header.shoff as usize, index * header.shentsize as usize)?;
        let section = RawSection {
            name_offset: read_u32(bytes, offset)?,
            section_type: read_u32(bytes, offset + 4)?,
            flags: read_u64(bytes, offset + 8)?,
            offset: read_u64(bytes, offset + 24)?,
            size: read_u64(bytes, offset + 32)?,
            link: read_u32(bytes, offset + 40)?,
            entsize: read_u64(bytes, offset + 56)?,
        };
        if section.section_type != 8 {
            checked_slice(bytes, section.offset as usize, section.size as usize)?;
        }
        raw_sections.push(section);
    }
    let string_section = raw_sections
        .get(header.shstrndx as usize)
        .ok_or_else(|| "bad ELF section string table index".to_string())?;
    if string_section.section_type != 3 {
        return Err("ELF section name table is not STRTAB".to_string());
    }
    let strings = checked_slice(
        bytes,
        string_section.offset as usize,
        string_section.size as usize,
    )?;
    let mut out = Vec::new();
    for section in raw_sections {
        out.push(ElfSection {
            name: if section.name_offset == 0 {
                String::new()
            } else {
                read_cstr(strings, section.name_offset as usize)?
            },
            section_type: section.section_type,
            flags: section.flags,
            offset: section.offset,
            size: section.size,
            link: section.link,
            entsize: section.entsize,
        });
    }
    Ok(out)
}

fn parse_elf_symbols(bytes: &[u8], sections: &[ElfSection]) -> Result<Vec<ElfSymbol>, String> {
    let mut tables = sections
        .iter()
        .filter(|section| section.section_type == ELF_SYMBOL_TABLE);
    let table = tables
        .next()
        .ok_or_else(|| "ELF 缺少用于 ELM 元数据绑定的 .symtab".to_string())?;
    if tables.next().is_some() {
        return Err("ELF 包含多个静态符号表".to_string());
    }
    if table.entsize != 24 || table.size % table.entsize != 0 {
        return Err("bad ELF64 symbol table entry size".to_string());
    }
    let strings = sections
        .get(table.link as usize)
        .ok_or_else(|| "bad ELF symbol string table link".to_string())?;
    if strings.section_type != 3 {
        return Err("ELF symbol table link is not STRTAB".to_string());
    }
    let strtab = checked_slice(bytes, strings.offset as usize, strings.size as usize)?;
    let mut out = Vec::new();
    let count = table.size / table.entsize;
    for index in 0..count as usize {
        let offset = checked_add(table.offset as usize, index * table.entsize as usize)?;
        let name_offset = read_u32(bytes, offset)? as usize;
        if name_offset == 0 {
            continue;
        }
        let info = read_u8(bytes, offset + 4)?;
        let section_index = read_u16(bytes, offset + 6)?;
        if section_index == ELF_SECTION_INDEX_UNDEFINED {
            continue;
        }
        let name = read_cstr(strtab, name_offset)?;
        if name.is_empty() {
            continue;
        }
        let binding = info >> 4;
        if binding != ELF_SYMBOL_BIND_GLOBAL && !name.starts_with("__elm_kernel_symbol_") {
            continue;
        }
        out.push(ElfSymbol {
            name,
            symbol_type: info & 0xf,
            value: read_u64(bytes, offset + 8)?,
            size: read_u64(bytes, offset + 16)?,
        });
    }
    if out.is_empty() {
        return Err("ELF has no symbols; build without stripping".to_string());
    }
    Ok(out)
}

fn validate_runtime_layout(segments: &[ElfLoadSegment]) -> Result<(), String> {
    let Some(first) = segments.first() else {
        return Err("ELF has no load segments".to_string());
    };
    if first.vaddr != 0 {
        return Err("ELM PIE 的第一个 PT_LOAD 必须从虚拟地址 0 开始".to_string());
    }
    let mut expected = 0u64;
    for segment in segments {
        if segment.vaddr != expected {
            return Err(format!(
                "ELF PT_LOAD layout is not ELM-compatible at vaddr=0x{:x}; use page-aligned contiguous LOAD segments",
                segment.vaddr
            ));
        }
        let end = segment
            .vaddr
            .checked_add(segment.mem_size)
            .ok_or_else(|| "ELF PT_LOAD memory range overflow".to_string())?;
        expected = align_up_u64(end, ELM_TOOL_PAGE_SIZE)?;
    }
    for (index, left) in segments.iter().enumerate() {
        let left_end = left
            .offset
            .checked_add(left.file_size)
            .ok_or_else(|| "ELF PT_LOAD file range overflow".to_string())?;
        for right in &segments[index + 1..] {
            let right_end = right
                .offset
                .checked_add(right.file_size)
                .ok_or_else(|| "ELF PT_LOAD file range overflow".to_string())?;
            if left.offset < right_end && right.offset < left_end {
                return Err("ELM PT_LOAD 文件范围不得重叠".to_string());
            }
        }
    }
    Ok(())
}

fn validate_native_symbols(elf: &ElfImage, metadata: &NativeMetadata) -> Result<(), String> {
    let descriptor = elf.symbol(&metadata.module_descriptor)?;
    if descriptor.symbol_type != ELF_SYMBOL_TYPE_OBJECT
        || descriptor.size < core::mem::size_of::<ElmModuleDescriptorV1>() as u64
    {
        return Err(format!(
            "统一模块描述符必须是尺寸完整的 OBJECT: {}",
            metadata.module_descriptor
        ));
    }
    let (descriptor_segment, descriptor_offset, _) =
        elf.symbol_location(&metadata.module_descriptor)?;
    if !matches!(
        elf.load_segments[descriptor_segment as usize].kind,
        ElmEbiSegmentKind::ReadOnlyData
    ) || descriptor_offset % core::mem::align_of::<ElmModuleDescriptorV1>() as u64 != 0
    {
        return Err(format!(
            "统一模块描述符必须位于按 ABI 对齐的只读段: {}",
            metadata.module_descriptor
        ));
    }
    for export in &metadata.exports {
        validate_code_symbol(elf, &export.symbol)?;
    }
    for provider in &metadata.providers {
        validate_code_symbol(elf, &provider.handler_symbol)?;
        if let Some(snapshot) = &provider.snapshot_symbol {
            validate_code_symbol(elf, snapshot)?;
        }
    }
    for import in &metadata.imports {
        let Some(slot_symbol) = import.slot_symbol.as_deref() else {
            continue;
        };
        let symbol = elf.symbol(slot_symbol)?;
        if symbol.symbol_type != ELF_SYMBOL_TYPE_OBJECT || symbol.size != 8 {
            return Err(format!(
                "import slot 必须是 8 字节全局 OBJECT: {}",
                slot_symbol
            ));
        }
        let (segment_index, offset, _) = elf.symbol_location(slot_symbol)?;
        let segment = &elf.load_segments[segment_index as usize];
        if !matches!(
            segment.kind,
            ElmEbiSegmentKind::Data | ElmEbiSegmentKind::Bss
        ) || offset & 7 != 0
        {
            return Err(format!(
                "import slot 必须位于 8 字节对齐的可写段: {}",
                slot_symbol
            ));
        }
    }
    Ok(())
}

fn validate_code_symbol(elf: &ElfImage, name: &str) -> Result<(), String> {
    let symbol = elf.symbol(name)?;
    if symbol.symbol_type != ELF_SYMBOL_TYPE_FUNCTION {
        return Err(format!("ELM ABI 函数符号类型不是 FUNC: {name}"));
    }
    let (segment_index, _, _) = elf.symbol_location(name)?;
    if !matches!(
        elf.load_segments[segment_index as usize].kind,
        ElmEbiSegmentKind::Code
    ) {
        return Err(format!("ELM ABI 函数符号不在可执行段: {name}"));
    }
    Ok(())
}

fn checked_add(a: usize, b: usize) -> Result<usize, String> {
    a.checked_add(b)
        .ok_or_else(|| "integer overflow while parsing file".to_string())
}

fn checked_slice(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], String> {
    let end = checked_add(offset, len)?;
    bytes
        .get(offset..end)
        .ok_or_else(|| "file range out of bounds".to_string())
}

fn read_cstr(bytes: &[u8], offset: usize) -> Result<String, String> {
    if offset >= bytes.len() {
        return Err("string table offset out of bounds".to_string());
    }
    let tail = &bytes[offset..];
    let len = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "unterminated string table entry".to_string())?;
    core::str::from_utf8(&tail[..len])
        .map(str::to_string)
        .map_err(|_| "non-utf8 ELF symbol name".to_string())
}

fn align_up_u64(value: u64, align: u64) -> Result<u64, String> {
    if align == 0 || !align.is_power_of_two() {
        return Err("bad alignment".to_string());
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .ok_or_else(|| "alignment overflow".to_string())
}

fn segment_flags(kind: ElmEbiSegmentKind) -> u32 {
    match kind {
        ElmEbiSegmentKind::Code => ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_EXECUTE,
        ElmEbiSegmentKind::ReadOnlyData => ELM_EBI_SEGMENT_FLAG_READ,
        ElmEbiSegmentKind::Data => ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_WRITE,
        ElmEbiSegmentKind::Bss => {
            ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_WRITE | ELM_EBI_SEGMENT_FLAG_ZERO_FILL
        }
        _ => 0,
    }
}

#[derive(Clone, Copy)]
enum HashState {
    Missing,
    Valid,
    Invalid,
}

impl std::fmt::Display for HashState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Valid => f.write_str("valid"),
            Self::Invalid => f.write_str("invalid"),
        }
    }
}

fn verify_header_hash(bytes: &[u8]) -> Result<Option<HashState>, String> {
    let header = Header::parse(bytes)?;
    if header.image_hash_size == 0 {
        return Ok(Some(HashState::Missing));
    }
    if header.image_hash_size != ELM_EKI_IMAGE_HASH_SHA256_SIZE {
        return Ok(Some(HashState::Invalid));
    }
    let offset = header.image_hash_offset as usize;
    let size = header.image_hash_size as usize;
    let expected = bytes
        .get(offset..offset + size)
        .ok_or_else(|| "image hash range out of file".to_string())?;
    let actual = sha256_with_zeroed_range(bytes, offset, size)
        .ok_or_else(|| "image hash range overflow".to_string())?;
    Ok(Some(if expected == actual {
        HashState::Valid
    } else {
        HashState::Invalid
    }))
}

fn rewrite_header_hash(bytes: &mut Vec<u8>) -> Result<(), String> {
    let header = Header::parse(bytes)?;
    let hash_offset = if header.image_hash_size == ELM_EKI_IMAGE_HASH_SHA256_SIZE {
        header.image_hash_offset as usize
    } else if header.image_hash_size == 0 {
        let offset = bytes.len();
        bytes.extend_from_slice(&[0; ELM_PROOF_SHA256_LEN]);
        offset
    } else {
        return Err("unsupported image hash size".to_string());
    };
    let file_size = bytes.len() as u64;
    write_u64(bytes, 16, file_size);
    write_u64(bytes, 32, hash_offset as u64);
    write_u32(bytes, 52, ELM_EKI_IMAGE_HASH_SHA256_SIZE);
    for byte in &mut bytes[hash_offset..hash_offset + ELM_PROOF_SHA256_LEN] {
        *byte = 0;
    }
    let digest = sha256_with_zeroed_range(bytes, hash_offset, ELM_PROOF_SHA256_LEN)
        .ok_or_else(|| "image hash range overflow".to_string())?;
    bytes[hash_offset..hash_offset + ELM_PROOF_SHA256_LEN].copy_from_slice(&digest);
    Ok(())
}

fn eki_image_with_hash(arch: ElmEbiArch, blocks: &[PackerBlock]) -> Vec<u8> {
    let mut image = eki_image(arch, blocks);
    let hash_offset = image.len();
    image.extend_from_slice(&[0; ELM_PROOF_SHA256_LEN]);
    let file_size = image.len() as u64;
    write_u64(&mut image, 16, file_size);
    write_u64(&mut image, 32, hash_offset as u64);
    write_u32(&mut image, 52, ELM_EKI_IMAGE_HASH_SHA256_SIZE);
    let digest = sha256_with_zeroed_range(&image, hash_offset, ELM_PROOF_SHA256_LEN)
        .expect("hash range created by packer");
    image[hash_offset..hash_offset + ELM_PROOF_SHA256_LEN].copy_from_slice(&digest);
    image
}

fn eki_image(arch: ElmEbiArch, blocks: &[PackerBlock]) -> Vec<u8> {
    let mut image = vec![0; ELM_EKI_HEADER_SIZE + blocks.len() * ELM_EKI_BLOCK_DESC_SIZE];
    let mut payload_offset = image.len();
    for (index, block) in blocks.iter().enumerate() {
        let desc = ELM_EKI_HEADER_SIZE + index * ELM_EKI_BLOCK_DESC_SIZE;
        write_u32(&mut image, desc, block.kind);
        write_u32(&mut image, desc + 4, block.flags);
        write_u64(&mut image, desc + 8, payload_offset as u64);
        write_u64(&mut image, desc + 16, block.payload.len() as u64);
        write_u64(&mut image, desc + 24, block.mem_size);
        write_u64(&mut image, desc + 32, block.align);
        image.extend_from_slice(&block.payload);
        payload_offset += block.payload.len();
    }
    image[0..8].copy_from_slice(&ELM_EKI_MAGIC);
    write_u16(&mut image, 8, ELM_EKI_FORMAT_VERSION);
    write_u16(&mut image, 10, ELM_EBI_ABI_VERSION);
    write_u32(&mut image, 12, ELM_EKI_HEADER_SIZE as u32);
    let file_size = image.len() as u64;
    write_u64(&mut image, 16, file_size);
    write_u64(&mut image, 24, ELM_EKI_HEADER_SIZE as u64);
    write_u32(&mut image, 40, arch as u32);
    write_u16(&mut image, 44, 1);
    write_u32(&mut image, 48, blocks.len() as u32);
    image
}

fn default_abi_fingerprint(arch: ElmEbiArch) -> ElmRustAbiFingerprintV1 {
    let rustc = std::process::Command::new(env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            let mut version = output.stdout;
            while matches!(version.last(), Some(b'\n' | b'\r')) {
                version.pop();
            }
            version
        })
        .unwrap_or_else(|| b"rustc-unknown".to_vec());
    let target = match arch {
        ElmEbiArch::Any => b"any".as_slice(),
        ElmEbiArch::Riscv64 => b"riscv64gc-unknown-none-elf".as_slice(),
        ElmEbiArch::LoongArch64 => b"loongarch64-unknown-none".as_slice(),
    };
    ElmRustAbiFingerprintV1::new(
        sha256(&rustc),
        sha256(target),
        sha256(kernel_interface_manifest_v1(arch as u32).as_bytes()),
        1,
        ElmPanicStrategy::AbortThroughRuntime,
        1,
        0,
    )
}

fn abi_fingerprint_block(fingerprint: &ElmRustAbiFingerprintV1) -> Vec<u8> {
    let mut out = vec![0; ELM_EKI_ABI_FINGERPRINT_BLOCK_SIZE];
    write_u16(&mut out, 0, ELM_RUST_ABI_FINGERPRINT_VERSION);
    write_u16(&mut out, 2, fingerprint.elmapi_version);
    out[4] = fingerprint.panic_strategy as u8;
    out[5] = fingerprint.code_model;
    write_u64(&mut out, 8, fingerprint.target_features);
    write_u32(&mut out, 16, fingerprint.flags);
    out[24..56].copy_from_slice(&fingerprint.rustc_commit_hash);
    out[56..88].copy_from_slice(&fingerprint.target_spec_hash);
    out[88..120].copy_from_slice(&fingerprint.kernel_interface_hash);
    out
}

fn manifest_block(name: &str, version: &str, kind: ElmKind) -> Result<Vec<u8>, String> {
    if name.len() > ELM_EKI_MANIFEST_NAME_LEN || version.len() > ELM_EKI_MANIFEST_VERSION_LEN {
        return Err("manifest field too long".to_string());
    }
    let mut out = vec![0; 16 + ELM_EKI_MANIFEST_NAME_LEN + ELM_EKI_MANIFEST_VERSION_LEN];
    write_u32(&mut out, 0, kind.as_raw());
    write_u16(&mut out, 8, name.len() as u16);
    write_u16(&mut out, 10, version.len() as u16);
    copy_fixed(&mut out, 16, name);
    copy_fixed(&mut out, 16 + ELM_EKI_MANIFEST_NAME_LEN, version);
    Ok(out)
}

fn menu_block(label: &str, description: &str, route: &str) -> Result<Vec<u8>, String> {
    if label.len() > ELM_MENU_LABEL_LEN
        || description.len() > ELM_MENU_DESCRIPTION_LEN
        || route.len() > ELM_MENU_ROUTE_LEN
    {
        return Err("menu field too long".to_string());
    }
    let mut out = vec![0; 16 + ELM_MENU_LABEL_LEN + ELM_MENU_DESCRIPTION_LEN + ELM_MENU_ROUTE_LEN];
    write_u32(&mut out, 0, MENU_KIND_ACTION);
    write_u16(&mut out, 8, label.len() as u16);
    write_u16(&mut out, 10, description.len() as u16);
    write_u16(&mut out, 12, route.len() as u16);
    copy_fixed(&mut out, 16, label);
    copy_fixed(&mut out, 16 + ELM_MENU_LABEL_LEN, description);
    copy_fixed(
        &mut out,
        16 + ELM_MENU_LABEL_LEN + ELM_MENU_DESCRIPTION_LEN,
        route,
    );
    Ok(out)
}

fn lifecycle_hooks_block() -> Vec<u8> {
    let record_size = 20 + ELM_EBI_SYMBOL_NAME_LEN;
    let mut out = vec![0; 8 + 2 * record_size];
    write_u32(&mut out, 0, 2);
    lifecycle_hook_record(&mut out, 8, HOOK_INITIALIZE, "on_initialize");
    lifecycle_hook_record(&mut out, 8 + record_size, HOOK_FINALIZE, "on_finalize");
    out
}

fn elmapi_compatibility_block(spec: &ElmApiSpec) -> Vec<u8> {
    let mut out = vec![0u8; ELM_EKI_ELMAPI_BLOCK_SIZE];
    write_u16(&mut out, 0, ELM_EKI_ELMAPI_BLOCK_VERSION);
    write_u16(&mut out, 2, spec.versions.len() as u16);
    write_u32(&mut out, 4, spec.root_import_index);
    write_u64(&mut out, 8, spec.required_features);
    for (index, version) in spec.versions.iter().enumerate() {
        write_u16(&mut out, 16 + index * 2, *version);
    }
    out
}

fn lifecycle_hook_record(out: &mut [u8], offset: usize, kind: u32, symbol: &str) {
    write_u32(out, offset, kind);
    write_u16(out, offset + 8, RUST_ABI);
    write_u16(out, offset + 10, RUST_HOOK_CONTEXT_RESULT);
    write_u16(out, offset + 12, symbol.len() as u16);
    copy_fixed(out, offset + 20, symbol);
}

fn segments_block(segments: &[ElfLoadSegment], relocation_size: Option<u64>) -> Vec<u8> {
    let count = segments.len() + usize::from(relocation_size.is_some());
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + count * EKI_SEGMENT_RECORD_SIZE];
    write_u32(&mut out, 0, count as u32);
    for (index, segment) in segments.iter().enumerate() {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SEGMENT_RECORD_SIZE;
        write_u32(&mut out, offset, segment.kind as u32);
        write_u32(&mut out, offset + 4, segment.flags);
        write_u64(&mut out, offset + 8, segment.file_size);
        write_u64(&mut out, offset + 16, segment.mem_size);
        write_u64(&mut out, offset + 24, segment.align);
    }
    if let Some(size) = relocation_size {
        let offset = EKI_TABLE_HEADER_SIZE + segments.len() * EKI_SEGMENT_RECORD_SIZE;
        write_u32(&mut out, offset, ElmEbiSegmentKind::Relocation as u32);
        write_u64(&mut out, offset + 8, size);
        write_u64(&mut out, offset + 16, size);
        write_u64(&mut out, offset + 24, 8);
    }
    out
}

fn relocation_segment_len(relocations: &[u8]) -> Option<u64> {
    if relocations.len() <= EKI_TABLE_HEADER_SIZE {
        None
    } else {
        Some(relocations.len() as u64)
    }
}

fn segment_block(segment: &ElfLoadSegment, elf_bytes: &[u8]) -> Result<PackerBlock, String> {
    let payload = if segment.file_size == 0 {
        Vec::new()
    } else {
        checked_slice(
            elf_bytes,
            segment.offset as usize,
            segment.file_size as usize,
        )?
        .to_vec()
    };
    let kind = match segment.kind {
        ElmEbiSegmentKind::Code => BLOCK_CODE,
        ElmEbiSegmentKind::ReadOnlyData => BLOCK_RODATA,
        ElmEbiSegmentKind::Data => BLOCK_DATA,
        ElmEbiSegmentKind::Bss => BLOCK_BSS,
        _ => return Err("unsupported ELF segment kind".to_string()),
    };
    Ok(PackerBlock::segment(
        kind,
        payload,
        segment.mem_size,
        segment.align,
    ))
}

fn import_records_block(entries: &[ImportSpec]) -> Result<Vec<u8>, String> {
    let records: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.contract.as_str(),
                entry.min_version,
                entry.max_version,
                entry.flags,
                entry.rust_abi_hash,
            )
        })
        .collect();
    symbol_records_block(&records)
}

fn export_records_block(entries: &[ExportSpec]) -> Result<Vec<u8>, String> {
    let records: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.contract.as_str(),
                entry.version,
                entry.version,
                entry.flags,
                entry.rust_abi_hash,
            )
        })
        .collect();
    symbol_records_block(&records)
}

fn symbol_records_block(
    entries: &[(&str, &str, u32, u32, u32, [u8; 32])],
) -> Result<Vec<u8>, String> {
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + entries.len() * EKI_SYMBOL_RECORD_SIZE];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, (name, contract, min_version, max_version, flags, rust_abi_hash)) in
        entries.iter().enumerate()
    {
        if name.len() > ELM_EBI_SYMBOL_NAME_LEN || contract.len() > ELM_NEXUS_CONTRACT_LEN {
            return Err("native symbol record field too long".to_string());
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SYMBOL_RECORD_SIZE;
        write_u32(&mut out, offset, *min_version);
        write_u32(&mut out, offset + 4, *flags);
        write_u16(&mut out, offset + 8, name.len() as u16);
        write_u16(&mut out, offset + 10, contract.len() as u16);
        write_u32(&mut out, offset + 12, *max_version);
        out[offset + 16..offset + 48].copy_from_slice(rust_abi_hash);
        copy_fixed(&mut out, offset + 48, name);
        copy_fixed(&mut out, offset + 48 + ELM_EBI_SYMBOL_NAME_LEN, contract);
    }
    Ok(out)
}

fn dependencies_block(entries: &[ElmProjectDependency]) -> Result<Vec<u8>, String> {
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + entries.len() * EKI_DEPENDENCY_RECORD_SIZE];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, dependency) in entries.iter().enumerate() {
        if dependency.provider.len() > elm::ELM_EBI_NAME_LEN
            || dependency.contract.len() > ELM_NEXUS_CONTRACT_LEN
        {
            return Err("dependency 字段超过 EKI v1 上限".to_string());
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_DEPENDENCY_RECORD_SIZE;
        write_u16(&mut out, offset, dependency.provider.len() as u16);
        write_u16(&mut out, offset + 2, dependency.contract.len() as u16);
        copy_fixed(&mut out, offset + 8, &dependency.provider);
        copy_fixed(
            &mut out,
            offset + 8 + elm::ELM_EBI_NAME_LEN,
            &dependency.contract,
        );
    }
    Ok(out)
}

fn extension_points_block(entries: &[ExtensionPointSpec]) -> Result<Vec<u8>, String> {
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + entries.len() * EKI_EXTENSION_POINT_RECORD_SIZE];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, point) in entries.iter().enumerate() {
        if point.point.len() > elm::ELM_MGR_RELATION_POINT_LEN
            || point.contract.len() > ELM_NEXUS_CONTRACT_LEN
        {
            return Err("mixin point 字段超过 EKI v1 上限".to_string());
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_EXTENSION_POINT_RECORD_SIZE;
        write_u16(&mut out, offset, point.point.len() as u16);
        write_u16(&mut out, offset + 2, point.contract.len() as u16);
        write_u32(&mut out, offset + 4, point.mode as u32);
        copy_fixed(&mut out, offset + 16, &point.point);
        copy_fixed(
            &mut out,
            offset + 16 + elm::ELM_MGR_RELATION_POINT_LEN,
            &point.contract,
        );
    }
    Ok(out)
}

fn extensions_block(entries: &[ExtensionSpec]) -> Result<Vec<u8>, String> {
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + entries.len() * EKI_EXTENSION_RECORD_SIZE];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, extension) in entries.iter().enumerate() {
        if extension.target.len() > elm::ELM_EBI_NAME_LEN
            || extension.point.len() > elm::ELM_MGR_RELATION_POINT_LEN
            || extension.contract.len() > ELM_NEXUS_CONTRACT_LEN
            || extension.handler_contract.len() > ELM_NEXUS_CONTRACT_LEN
        {
            return Err("mixin 字段超过 EKI v1 上限".to_string());
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_EXTENSION_RECORD_SIZE;
        write_u16(&mut out, offset, extension.target.len() as u16);
        write_u16(&mut out, offset + 2, extension.point.len() as u16);
        write_u16(&mut out, offset + 4, extension.contract.len() as u16);
        write_u16(
            &mut out,
            offset + 6,
            extension.handler_contract.len() as u16,
        );
        write_u32(&mut out, offset + 8, extension.priority as u32);
        let target_start = offset + 24;
        let point_start = target_start + elm::ELM_EBI_NAME_LEN;
        let contract_start = point_start + elm::ELM_MGR_RELATION_POINT_LEN;
        let handler_start = contract_start + ELM_NEXUS_CONTRACT_LEN;
        copy_fixed(&mut out, target_start, &extension.target);
        copy_fixed(&mut out, point_start, &extension.point);
        copy_fixed(&mut out, contract_start, &extension.contract);
        copy_fixed(&mut out, handler_start, &extension.handler_contract);
    }
    Ok(out)
}

fn provider_ports_block(providers: &[ProviderSpec]) -> Result<Vec<u8>, String> {
    let mut out =
        vec![0; EKI_TABLE_HEADER_SIZE + providers.len() * ELM_EKI_PROVIDER_PORT_RECORD_SIZE];
    write_u32(&mut out, 0, providers.len() as u32);
    for (index, provider) in providers.iter().enumerate() {
        if provider.contract.len() > ELM_NEXUS_CONTRACT_LEN
            || provider.handler_symbol.len() > ELM_EBI_SYMBOL_NAME_LEN
            || provider
                .snapshot_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.len() > ELM_EBI_SYMBOL_NAME_LEN)
        {
            return Err("provider field too long".to_string());
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * ELM_EKI_PROVIDER_PORT_RECORD_SIZE;
        write_u32(&mut out, offset, provider.access as u32);
        write_u32(&mut out, offset + 4, provider.direction as u32);
        write_u32(&mut out, offset + 8, provider.mode as u32);
        write_u32(&mut out, offset + 12, provider.flags);
        write_u16(&mut out, offset + 16, provider.contract.len() as u16);
        write_u16(&mut out, offset + 18, provider.handler_symbol.len() as u16);
        let snapshot_len = provider
            .snapshot_symbol
            .as_ref()
            .map(|symbol| symbol.len())
            .unwrap_or(0);
        write_u16(&mut out, offset + 20, snapshot_len as u16);
        let contract_start = offset + 24;
        let handler_start = contract_start + ELM_NEXUS_CONTRACT_LEN;
        let snapshot_start = handler_start + ELM_EBI_SYMBOL_NAME_LEN;
        copy_fixed(&mut out, contract_start, &provider.contract);
        copy_fixed(&mut out, handler_start, &provider.handler_symbol);
        if let Some(snapshot) = &provider.snapshot_symbol {
            copy_fixed(&mut out, snapshot_start, snapshot);
        }
    }
    Ok(out)
}

fn symbol_locations_block(elf: &ElfImage, symbol_names: &[String]) -> Result<Vec<u8>, String> {
    let mut out =
        vec![0; EKI_TABLE_HEADER_SIZE + symbol_names.len() * EKI_SYMBOL_LOCATION_RECORD_SIZE];
    write_u32(&mut out, 0, symbol_names.len() as u32);
    for (index, name) in symbol_names.iter().enumerate() {
        if name.len() > ELM_EBI_SYMBOL_NAME_LEN {
            return Err(format!("symbol name too long: {name}"));
        }
        let (segment_index, offset_in_segment, size) = elf.symbol_location(name)?;
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SYMBOL_LOCATION_RECORD_SIZE;
        write_u16(&mut out, offset, name.len() as u16);
        write_u32(&mut out, offset + 8, segment_index);
        write_u64(&mut out, offset + 16, offset_in_segment);
        write_u64(&mut out, offset + 24, size);
        copy_fixed(&mut out, offset + 32, name);
    }
    Ok(out)
}

fn native_relocations_block(
    elf: &ElfImage,
    slots: &[ImportSpec],
    mut records: Vec<EkiRelocationSpec>,
) -> Result<Vec<u8>, String> {
    for (index, slot) in slots.iter().enumerate() {
        let Some(slot_symbol) = slot.slot_symbol.as_deref() else {
            continue;
        };
        let (segment_index, offset_in_segment, size) = elf.symbol_location(slot_symbol)?;
        if size != 8 {
            return Err(format!(
                "import slot must be exactly 8 bytes: {}",
                slot_symbol
            ));
        }
        records.push(EkiRelocationSpec {
            kind: ElmEbiRelocationKind::ImportAbs64,
            target_segment_index: segment_index,
            value_index: index as u32,
            target_offset: offset_in_segment,
            addend: 0,
        });
    }
    records.sort_by_key(|record| (record.target_segment_index, record.target_offset));
    if records.windows(2).any(|items| {
        items[0].target_segment_index == items[1].target_segment_index
            && items[0].target_offset == items[1].target_offset
    }) {
        return Err("ELF 包含指向同一槽位的重复运行时重定位".to_string());
    }
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + records.len() * EKI_RELOCATION_RECORD_SIZE];
    write_u32(&mut out, 0, records.len() as u32);
    for (index, record) in records.iter().enumerate() {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_RELOCATION_RECORD_SIZE;
        write_u32(&mut out, offset, record.kind as u32);
        write_u32(&mut out, offset + 8, record.target_segment_index);
        write_u32(&mut out, offset + 12, record.value_index);
        write_u64(&mut out, offset + 16, record.target_offset);
        write_u64(&mut out, offset + 24, record.addend as u64);
    }
    Ok(out)
}

fn dynamic_runtime_relocations(
    elf: &ElfImage,
    bytes: &[u8],
    interface: &KernelInterfaceManifest,
    imports: &mut Vec<ImportSpec>,
) -> Result<Vec<EkiRelocationSpec>, String> {
    let mut records = Vec::new();
    for section in &elf.sections {
        if section.flags & ELF_SECTION_FLAG_ALLOC == 0
            || !matches!(section.section_type, ELF_SECTION_RELA | ELF_SECTION_REL)
        {
            continue;
        }
        if !matches!(section.name.as_str(), ".rela.dyn" | ".rela.plt")
            || section.section_type != ELF_SECTION_RELA
        {
            return Err(format!(
                "ELM PIE 包含不支持的动态重定位 section: {}",
                section.name
            ));
        }
        if section.entsize != 24 || section.size % section.entsize != 0 {
            return Err(format!("{} entry size 无效", section.name));
        }
        for index in 0..(section.size / section.entsize) as usize {
            let offset = checked_add(section.offset as usize, index * section.entsize as usize)?;
            let target_vaddr = read_u64(bytes, offset)?;
            let info = read_u64(bytes, offset + 8)?;
            let addend = read_i64(bytes, offset + 16)?;
            let relocation_type = info as u32;
            let symbol_index = (info >> 32) as u32;
            let (target_segment_index, target_offset) = elf.relocation_target(target_vaddr, 8)?;
            if relocation_type == ELF_RELOCATION_RELATIVE && symbol_index == 0 {
                if target_vaddr & 7 != 0 || addend < 0 || !elf.contains_image_address(addend as u64)
                {
                    return Err(format!(
                        "ELM PIE relative relocation 范围无效: target=0x{target_vaddr:x} addend={addend}"
                    ));
                }
                records.push(EkiRelocationSpec {
                    kind: ElmEbiRelocationKind::ImageBase64,
                    target_segment_index,
                    value_index: 0,
                    target_offset,
                    addend,
                });
                continue;
            }
            if !matches!(
                relocation_type,
                ELF_RELOCATION_ABS64 | ELF_RELOCATION_JUMP_SLOT
            ) || symbol_index == 0
            {
                return Err(format!(
                    "ELM PIE 包含不支持的动态重定位: section={} type={} symbol={}",
                    section.name, relocation_type, symbol_index
                ));
            }
            let link_name = dynamic_symbol_name(bytes, &elf.sections, section.link, symbol_index)?;
            let symbol = interface
                .symbol_by_link_name(&link_name)
                .ok_or_else(|| format!("ELM 引用了未审核或未导出的内核符号: {link_name}"))?;
            let import_index = match imports.iter().position(|import| {
                import.name == symbol.api_path
                    && import.contract == symbol.contract
                    && import.min_version <= symbol.version
                    && import.max_version >= symbol.version
                    && import.rust_abi_hash == symbol.rust_abi_hash
            }) {
                Some(index) => index,
                None => {
                    let mut flags =
                        ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL | ELM_EBI_IMPORT_FLAG_EXACT_RUST_API;
                    if symbol.kind == kernel_symbols::KERNEL_SYMBOL_KIND_STATIC {
                        flags |= ELM_EBI_IMPORT_FLAG_KERNEL_STATIC;
                    }
                    imports.push(ImportSpec {
                        slot_symbol: None,
                        name: symbol.api_path.clone(),
                        contract: symbol.contract.clone(),
                        min_version: symbol.version,
                        max_version: symbol.version,
                        flags,
                        rust_abi_hash: symbol.rust_abi_hash,
                    });
                    imports.len() - 1
                }
            };
            records.push(EkiRelocationSpec {
                kind: ElmEbiRelocationKind::ImportAbs64,
                target_segment_index,
                value_index: import_index as u32,
                target_offset,
                addend,
            });
        }
    }
    Ok(records)
}

fn dynamic_symbol_name(
    bytes: &[u8],
    sections: &[ElfSection],
    symbol_table_index: u32,
    symbol_index: u32,
) -> Result<String, String> {
    let table = sections
        .get(symbol_table_index as usize)
        .ok_or_else(|| "动态重定位引用了无效符号表".to_string())?;
    if table.entsize != 24 || table.size % table.entsize != 0 {
        return Err("动态符号表记录尺寸无效".to_string());
    }
    if u64::from(symbol_index) >= table.size / table.entsize {
        return Err("动态重定位符号索引越界".to_string());
    }
    let strings = sections
        .get(table.link as usize)
        .ok_or_else(|| "动态符号表字符串表索引无效".to_string())?;
    let strtab = checked_slice(bytes, strings.offset as usize, strings.size as usize)?;
    let offset = table.offset as usize + symbol_index as usize * table.entsize as usize;
    if read_u16(bytes, offset + 6)? != ELF_SECTION_INDEX_UNDEFINED {
        return Err("内核导入动态符号必须保持未定义状态".to_string());
    }
    let name = read_cstr(strtab, read_u32(bytes, offset)? as usize)?;
    if name.is_empty() {
        return Err("动态重定位引用了空符号名".to_string());
    }
    Ok(name)
}

#[cfg(test)]
fn dynamic_relative_relocations(
    elf: &ElfImage,
    bytes: &[u8],
) -> Result<Vec<EkiRelocationSpec>, String> {
    let mut records = Vec::new();
    let mut found_dynamic_relocations = false;
    for section in &elf.sections {
        if section.flags & ELF_SECTION_FLAG_ALLOC == 0
            || !matches!(section.section_type, ELF_SECTION_RELA | ELF_SECTION_REL)
        {
            continue;
        }
        if section.name != ".rela.dyn" || section.section_type != ELF_SECTION_RELA {
            return Err(format!(
                "ELM PIE 包含不支持的动态重定位 section: {}",
                section.name
            ));
        }
        if found_dynamic_relocations {
            return Err("ELM PIE 包含重复 .rela.dyn section".to_string());
        }
        found_dynamic_relocations = true;
        if section.entsize != 24 || section.size % section.entsize != 0 {
            return Err("ELM PIE 的 .rela.dyn entry size 无效".to_string());
        }
        for index in 0..(section.size / section.entsize) as usize {
            let offset = checked_add(section.offset as usize, index * section.entsize as usize)?;
            let target_vaddr = read_u64(bytes, offset)?;
            let info = read_u64(bytes, offset + 8)?;
            let addend = read_i64(bytes, offset + 16)?;
            let relocation_type = info as u32;
            let symbol_index = (info >> 32) as u32;
            if relocation_type != ELF_RELOCATION_RELATIVE || symbol_index != 0 {
                return Err(format!(
                    "ELM PIE 只允许无符号 R_*_RELATIVE，发现 type={relocation_type} symbol={symbol_index}"
                ));
            }
            if target_vaddr & 7 != 0 || addend < 0 || !elf.contains_image_address(addend as u64) {
                return Err(format!(
                    "ELM PIE relative relocation 范围无效: target=0x{target_vaddr:x} addend={addend}"
                ));
            }
            let (target_segment_index, target_offset) = elf.relocation_target(target_vaddr, 8)?;
            records.push(EkiRelocationSpec {
                kind: ElmEbiRelocationKind::ImageBase64,
                target_segment_index,
                value_index: 0,
                target_offset,
                addend,
            });
        }
    }
    Ok(records)
}

fn parse_kind(raw: &str) -> Result<ElmKind, String> {
    match raw {
        "manager" => Ok(ElmKind::Manager),
        "service" => Ok(ElmKind::Service),
        "driver" => Ok(ElmKind::Driver),
        "extension" => Ok(ElmKind::Extension),
        "filesystem" => Ok(ElmKind::Filesystem),
        "network" => Ok(ElmKind::Network),
        "debug" => Ok(ElmKind::Debug),
        "other" => Ok(ElmKind::Other),
        _ => Err(format!("unknown ELM kind: {raw}")),
    }
}

fn block_name(kind: u32) -> String {
    match ElmEkiBlockKind::from_raw(kind) {
        Some(kind) => format!("{kind:?}"),
        None => format!("unknown({kind})"),
    }
}

fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, String> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| "u8 out of range".to_string())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "u16 out of range".to_string())?;
    Ok(u16::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "u32 out of range".to_string())?;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "u64 out of range".to_string())?;
    Ok(u64::from_le_bytes(raw.try_into().unwrap()))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, String> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "i64 out of range".to_string())?;
    Ok(i64::from_le_bytes(raw.try_into().unwrap()))
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn copy_fixed(out: &mut [u8], offset: usize, value: &str) {
    out[offset..offset + value.len()].copy_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relocation_fixture(relocation_type: u32) -> (ElfImage, Vec<u8>) {
        let elf = ElfImage {
            arch: ElmEbiArch::LoongArch64,
            load_segments: vec![
                ElfLoadSegment {
                    index: 0,
                    kind: ElmEbiSegmentKind::Code,
                    flags: ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_EXECUTE,
                    offset: 0,
                    vaddr: 0,
                    file_size: 0x100,
                    mem_size: 0x100,
                    align: 0x1000,
                },
                ElfLoadSegment {
                    index: 1,
                    kind: ElmEbiSegmentKind::Data,
                    flags: ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_WRITE,
                    offset: 0x100,
                    vaddr: 0x1000,
                    file_size: 0x10,
                    mem_size: 0x10,
                    align: 0x1000,
                },
            ],
            sections: vec![ElfSection {
                name: ".rela.dyn".to_string(),
                section_type: ELF_SECTION_RELA,
                flags: ELF_SECTION_FLAG_ALLOC,
                offset: 0,
                size: 24,
                link: 0,
                entsize: 24,
            }],
            symbols: Vec::new(),
        };
        let mut bytes = vec![0; 24];
        write_u64(&mut bytes, 0, 0x1008);
        write_u64(&mut bytes, 8, u64::from(relocation_type));
        write_u64(&mut bytes, 16, 0x20);
        (elf, bytes)
    }

    #[test]
    fn converts_elf_relative_relocation_to_image_base() {
        let (elf, bytes) = relocation_fixture(ELF_RELOCATION_RELATIVE);
        let records = dynamic_relative_relocations(&elf, &bytes).unwrap();

        assert_eq!(
            records,
            vec![EkiRelocationSpec {
                kind: ElmEbiRelocationKind::ImageBase64,
                target_segment_index: 1,
                value_index: 0,
                target_offset: 8,
                addend: 0x20,
            }]
        );
    }

    #[test]
    fn rejects_non_relative_dynamic_relocation() {
        let (elf, bytes) = relocation_fixture(4);
        assert!(dynamic_relative_relocations(&elf, &bytes).is_err());
    }

    #[test]
    fn rejects_relative_relocation_into_segment_gap() {
        let (elf, mut bytes) = relocation_fixture(ELF_RELOCATION_RELATIVE);
        write_u64(&mut bytes, 16, 0x800);

        assert!(dynamic_relative_relocations(&elf, &bytes).is_err());
    }

    #[test]
    fn formats_full_abi_digest_as_lowercase_hex() {
        let mut digest = [0u8; 32];
        digest[0] = 0xab;
        digest[31] = 0xcd;

        let encoded = hex_digest(&digest);

        assert_eq!(encoded.len(), 64);
        assert!(encoded.starts_with("ab00"));
        assert!(encoded.ends_with("00cd"));
    }
}
