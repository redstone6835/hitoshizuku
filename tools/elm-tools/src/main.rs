use std::env;
use std::fs;
use std::io::Read;

use ed25519_dalek::{Signer, SigningKey};

use elm::{
    ELM_API_MAX_COMPATIBLE_VERSIONS, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_API_VERSION_V1,
    ELM_EBI_ABI_VERSION, ELM_EBI_RUST_ABI_VERSION as RUST_ABI, ELM_EBI_SEGMENT_FLAG_EXECUTE,
    ELM_EBI_SEGMENT_FLAG_READ, ELM_EBI_SEGMENT_FLAG_WRITE, ELM_EBI_SEGMENT_FLAG_ZERO_FILL,
    ELM_EBI_SYMBOL_NAME_LEN, ELM_EKI_BLOCK_DESC_SIZE, ELM_EKI_FORMAT_VERSION,
    ELM_EKI_ABI_FINGERPRINT_BLOCK_SIZE, ELM_EKI_ELMAPI_BLOCK_SIZE,
    ELM_EKI_ELMAPI_BLOCK_VERSION, ELM_EKI_HEADER_SIZE,
    ELM_EKI_IMAGE_HASH_SHA256_SIZE, ELM_EKI_MAGIC,
    ELM_EKI_MANIFEST_NAME_LEN, ELM_EKI_MANIFEST_VERSION_LEN, ELM_EKI_PROVIDER_PORT_RECORD_SIZE,
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ELM_NEXUS_CONTRACT_LEN,
    ELM_EKI_PROOF_ALGORITHM_ED25519, ELM_EKI_PROOF_BLOCK_SIZE, ELM_PROOF_ABI_VERSION,
    ELM_PROOF_ED25519_SIGNATURE_LEN, ELM_PROOF_SHA256_LEN,
    ELM_PROOF_SOURCE_IDENTIFIER_LEN, ELM_RUNTIME_LOG_EXPORT_CONTRACT, ELM_RUNTIME_LOG_EXPORT_NAME,
    ELM_RUNTIME_LOG_EXPORT_VERSION, ElmEbiArch, ElmEbiRelocationKind, ElmEbiSegmentKind,
    ELM_RUST_ABI_FINGERPRINT_VERSION, ElmEbiProofV1, ElmEkiBlockKind, ElmKind,
    ElmPanicStrategy, ElmPortAccessPolicy, ElmRustAbiFingerprintV1, ElmTrustAnchor, ElmTrustStore,
    FlowDirection, FlowMode, canonical_ebi_digest, kernel_api_manifest_v1, parse_eki_image, sha256,
    sha256_with_zeroed_range, sha256_with_zeroed_ranges,
};

const ELM_TOOL_PAGE_SIZE: u64 = 4096;
const BLOCK_MANIFEST: u32 = ElmEkiBlockKind::Manifest as u32;
const BLOCK_MENU: u32 = ElmEkiBlockKind::Menu as u32;
const BLOCK_ENTRY: u32 = ElmEkiBlockKind::Entry as u32;
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
const BLOCK_API_COMPATIBILITY: u32 = ElmEkiBlockKind::ApiCompatibility as u32;
const BLOCK_ABI_FINGERPRINT: u32 = ElmEkiBlockKind::AbiFingerprint as u32;
const BLOCK_PROOF: u32 = ElmEkiBlockKind::Signature as u32;
const MENU_KIND_ACTION: u32 = 2;
const HOOK_INITIALIZE: u32 = 1;
const HOOK_FINALIZE: u32 = 2;
const RUST_HOOK_CONTEXT_RESULT: u16 = 1;
const EKI_TABLE_HEADER_SIZE: usize = 8;
const EKI_SEGMENT_RECORD_SIZE: usize = 32;
const EKI_SYMBOL_RECORD_SIZE: usize = 16 + ELM_EBI_SYMBOL_NAME_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_SYMBOL_LOCATION_RECORD_SIZE: usize = 32 + ELM_EBI_SYMBOL_NAME_LEN;
const EKI_RELOCATION_RECORD_SIZE: usize = 32;

const GENERATED_ELMMGR_CARGO_TOML: &str = r#"[package]
name = "elmmgr"
version = "0.1.0"
edition = "2024"
license = "GPL-3.0-only"

[lib]
path = "src/lib.rs"

[workspace]
"#;

const GENERATED_ELMMGR_LIB_RS: &str = r#"#![no_std]

//! 由 `elm-tools generate-elmmgr` 生成的独立 ELM API 包。
//! 本包只描述稳定 API 根，不依赖内核源码或工作区 crate。

pub mod api {
    pub const VERSION: u16 = 1;
    pub const ROOT_MAGIC: u64 = u64::from_le_bytes(*b"ELMAPI1\0");
    pub const FEATURE_DISPATCH: u64 = 1 << 0;
    pub const FEATURE_CONTEXT: u64 = 1 << 1;
    pub const FEATURE_NAMESPACE_QUERY: u64 = 1 << 2;
    pub const FEATURE_LOG: u64 = 1 << 3;
    pub const FEATURE_ABORT: u64 = 1 << 4;
    pub const FEATURE_MANAGED_CALL: u64 = 1 << 5;
    pub const ABORT_REASON_PANIC: u32 = 4;
    pub const STATUS_BUFFER_TOO_SMALL: i32 = -4;
    pub const FRAME_PAYLOAD_LEN: usize = 256;

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CallFrame {
        pub binding_id: u64,
        pub call_id: u64,
        pub opcode: u32,
        pub flags: u32,
        pub payload_len: u16,
        pub reserved0: u16,
        pub reserved1: u32,
        pub payload: [u8; FRAME_PAYLOAD_LEN],
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ReplyFrame {
        pub binding_id: u64,
        pub call_id: u64,
        pub status: i32,
        pub flags: u32,
        pub payload_len: u16,
        pub reserved0: u16,
        pub reserved1: u32,
        pub payload: [u8; FRAME_PAYLOAD_LEN],
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Context {
        pub struct_size: u32,
        pub flags: u32,
        pub cell_id: u64,
        pub parent_id: u64,
        pub generation: u64,
        pub state: u32,
        pub phase: u32,
        pub allowed_actions: u32,
        pub reserved: u32,
    }

    impl Context {
        pub const fn empty() -> Self {
            Self {
                struct_size: core::mem::size_of::<Self>() as u32,
                flags: 0,
                cell_id: 0,
                parent_id: 0,
                generation: 0,
                state: 0,
                phase: 0,
                allowed_actions: 0,
                reserved: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Namespace {
        pub struct_size: u32,
        pub flags: u32,
        pub selected_version: u16,
        pub reserved0: u16,
        pub table_size: u32,
        pub table_address: usize,
        pub generation: u64,
        pub capabilities: u64,
    }

    impl Namespace {
        pub const fn empty() -> Self {
            Self {
                struct_size: core::mem::size_of::<Self>() as u32,
                flags: 0,
                selected_version: 0,
                reserved0: 0,
                table_size: 0,
                table_address: 0,
                generation: 0,
                capabilities: 0,
            }
        }
    }

    type DispatchFn = extern "C" fn(u32, *const u8, usize, *mut u8, usize, *mut usize) -> i32;
    type ContextFn = extern "C" fn(*mut Context) -> i32;
    type LogFn = extern "C" fn(u32, *const u8, usize) -> i32;
    type AbortFn = extern "C" fn(u32) -> !;
    type InvokeManagedFn = extern "C" fn(u64, *const CallFrame, *mut ReplyFrame) -> i32;
    type QueryNamespaceFn =
        extern "C" fn(*const u8, usize, *const u16, usize, *mut Namespace) -> i32;

    #[repr(C)]
    struct RuntimeTable {
        struct_size: u32,
        abi_version: u16,
        reserved0: u16,
        features: u64,
        dispatch: DispatchFn,
        current_context: ContextFn,
        log: LogFn,
        abort_current: AbortFn,
        invoke_managed: InvokeManagedFn,
    }

    #[repr(C)]
    struct Root {
        magic: u64,
        struct_size: u32,
        abi_version: u16,
        selected_version: u16,
        features: u64,
        runtime_table: *const RuntimeTable,
        runtime_table_size: u32,
        reserved0: u32,
        query_namespace: QueryNamespaceFn,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        RootUnavailable,
        IncompatibleRoot,
        RuntimeUnavailable,
        BufferTooSmall(usize),
        Status(i32),
    }

    #[unsafe(no_mangle)]
    #[unsafe(link_section = ".data.elm_imports")]
    #[used]
    pub static mut __ELM_API_ROOT_SLOT: usize = 0;

    fn root() -> Result<&'static Root, Error> {
        let address = unsafe { core::ptr::read_volatile(&raw const __ELM_API_ROOT_SLOT) };
        if address == 0 {
            return Err(Error::RootUnavailable);
        }
        let root = unsafe { &*(address as *const Root) };
        if root.magic != ROOT_MAGIC
            || root.abi_version != VERSION
            || root.selected_version != VERSION
            || root.struct_size < core::mem::size_of::<Root>() as u32
        {
            return Err(Error::IncompatibleRoot);
        }
        Ok(root)
    }

    fn runtime() -> Result<&'static RuntimeTable, Error> {
        let root = root()?;
        if root.runtime_table.is_null()
            || root.runtime_table_size < core::mem::size_of::<RuntimeTable>() as u32
        {
            return Err(Error::RuntimeUnavailable);
        }
        let runtime = unsafe { &*root.runtime_table };
        if runtime.abi_version != VERSION
            || runtime.struct_size < core::mem::size_of::<RuntimeTable>() as u32
        {
            return Err(Error::RuntimeUnavailable);
        }
        Ok(runtime)
    }

    pub fn features() -> Result<u64, Error> {
        Ok(root()?.features)
    }

    pub fn log(level: u32, message: &str) -> Result<(), Error> {
        let status = (runtime()?.log)(level, message.as_ptr(), message.len());
        if status == 0 { Ok(()) } else { Err(Error::Status(status)) }
    }

    pub fn abort_current(reason: u32) -> ! {
        (runtime().unwrap_or_else(|_| loop { core::hint::spin_loop() }).abort_current)(reason)
    }

    pub fn invoke_managed(import_handle: u64, request: &CallFrame) -> Result<ReplyFrame, Error> {
        let mut reply = ReplyFrame {
            binding_id: request.binding_id,
            call_id: request.call_id,
            status: -4098,
            flags: 0,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            payload: [0; FRAME_PAYLOAD_LEN],
        };
        let status = (runtime()?.invoke_managed)(import_handle, request, &mut reply);
        if status == 0 { Ok(reply) } else { Err(Error::Status(status)) }
    }

    pub fn current_context() -> Result<Context, Error> {
        let mut output = Context::empty();
        let status = (runtime()?.current_context)(&mut output);
        if status == 0 { Ok(output) } else { Err(Error::Status(status)) }
    }

    pub fn dispatch(kind: u32, input: &[u8], output: &mut [u8]) -> Result<(i32, usize), Error> {
        let mut output_len = 0usize;
        let status = (runtime()?.dispatch)(
            kind,
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &mut output_len,
        );
        if status == STATUS_BUFFER_TOO_SMALL {
            Err(Error::BufferTooSmall(output_len))
        } else {
            Ok((status, output_len))
        }
    }

    pub fn query_namespace(identifier: &str, versions: &[u16]) -> Result<Namespace, Error> {
        let mut output = Namespace::empty();
        let status = (root()?.query_namespace)(
            identifier.as_ptr(),
            identifier.len(),
            versions.as_ptr(),
            versions.len(),
            &mut output,
        );
        if status == 0 { Ok(output) } else { Err(Error::Status(status)) }
    }
}
"#;

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
struct ImportSlotSpec {
    slot_symbol: String,
    import_name: String,
    contract: String,
    version: u32,
}

#[derive(Clone)]
struct ExportSpec {
    symbol: String,
    contract: String,
    version: u32,
}

#[derive(Clone)]
struct ProviderSpec {
    contract: String,
    access: ElmPortAccessPolicy,
    direction: FlowDirection,
    mode: FlowMode,
    handler_symbol: String,
    snapshot_symbol: Option<String>,
}

#[derive(Clone)]
struct ElmApiSpec {
    root_import_index: u32,
    versions: Vec<u16>,
    required_features: u64,
}

#[derive(Clone)]
struct NativePackOptions {
    out: String,
    elf: String,
    name: String,
    version: String,
    kind: ElmKind,
    arch: Option<ElmEbiArch>,
    entry: Option<String>,
    menu: Option<(String, String, String)>,
    import_slots: Vec<ImportSlotSpec>,
    exports: Vec<ExportSpec>,
    providers: Vec<ProviderSpec>,
    elmapi: Option<ElmApiSpec>,
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
        "pack-metadata" | "pack" => cmd_pack_metadata(&args[2..]),
        "pack-elf" => cmd_pack_elf(&args[2..]),
        "inspect" => cmd_inspect(&args[2..]),
        "hash" => cmd_hash(&args[2..]),
        "keygen" => cmd_keygen(&args[2..]),
        "sign" => cmd_sign(&args[2..]),
        "verify" => cmd_verify(&args[2..]),
        "generate-elmmgr" => cmd_generate_elmmgr(&args[2..]),
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
    eprintln!(
        "  pack-metadata <out.eki> <name> <version> <kind> [--menu <label> <description> <route>]"
    );
    eprintln!("  pack <out.eki> <name> <version> <kind> [--menu <label> <description> <route>]");
    eprintln!(
        "  pack-elf <out.eki> <image.elf> <name> <version> <kind> [--arch any|riscv64|loongarch64] [--entry <symbol>] [--menu <label> <description> <route>]"
    );
    eprintln!(
        "           [--runtime-log-slot <slot-symbol>] [--import-slot <slot-symbol> <import-name> <contract> <version>]"
    );
    eprintln!(
        "           [--elmapi-root-slot <slot-symbol> <versions-comma> <required-features>]"
    );
    eprintln!(
        "           [--export <symbol> <contract> <version>] [--provider <contract> <access> <direction> <mode> <handler-symbol> <snapshot-symbol|->]"
    );
    eprintln!("  inspect <file.eki>");
    eprintln!("  hash <in.eki> <out.eki>");
    eprintln!("  keygen <private-seed.bin> <public-key.bin>");
    eprintln!("  sign <in.eki> <out.eki> <private-seed.bin> <source-id> <release-epoch>");
    eprintln!("  verify <file.eki>");
    eprintln!("  generate-elmmgr <output-directory>");
    eprintln!("  fingerprint-header <target-triple> <output-header>");
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
    header.push_str("\n#define ELM_FINGERPRINT_KERNEL_API_HASH_BYTES ");
    append_c_byte_list(&mut header, &fingerprint.kernel_api_hash);
    header.push_str("\n#endif\n");
    if let Some(parent) = std::path::Path::new(&args[1]).parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("create {}: {err}", parent.display()))?;
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

fn cmd_generate_elmmgr(args: &[String]) -> Result<(), String> {
    if args.len() != 1 {
        usage();
        return Err("bad generate-elmmgr arguments".to_string());
    }
    let root = std::path::Path::new(&args[0]);
    let src = root.join("src");
    fs::create_dir_all(&src).map_err(|err| format!("create {}: {err}", src.display()))?;
    fs::write(root.join("Cargo.toml"), GENERATED_ELMMGR_CARGO_TOML)
        .map_err(|err| format!("write generated Cargo.toml: {err}"))?;
    fs::write(src.join("lib.rs"), GENERATED_ELMMGR_LIB_RS)
        .map_err(|err| format!("write generated lib.rs: {err}"))?;
    Ok(())
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
        blocks.insert(1, PackerBlock::new(BLOCK_MENU, menu_block(&args[5], &args[6], &args[7])?));
    }
    let image = eki_image_with_hash(ElmEbiArch::Any, &blocks);
    fs::write(out, image).map_err(|err| format!("write {out}: {err}"))?;
    Ok(())
}

fn cmd_pack_elf(args: &[String]) -> Result<(), String> {
    let options = parse_native_pack_options(args)?;
    let elf_bytes = fs::read(&options.elf).map_err(|err| format!("read {}: {err}", options.elf))?;
    let elf = ElfImage::parse(&elf_bytes)?;
    let arch = options.arch.unwrap_or_else(|| elf.arch);
    if arch != ElmEbiArch::Any && elf.arch != ElmEbiArch::Any && arch != elf.arch {
        return Err("requested --arch does not match ELF machine".to_string());
    }
    validate_runtime_layout(&elf.load_segments)?;

    let mut blocks = Vec::new();
    blocks.push(PackerBlock::new(
        BLOCK_MANIFEST,
        manifest_block(&options.name, &options.version, options.kind)?,
    ));
    blocks.push(PackerBlock::new(
        BLOCK_ABI_FINGERPRINT,
        abi_fingerprint_block(&default_abi_fingerprint(arch)),
    ));
    if let Some((label, description, route)) = &options.menu {
        blocks.push(PackerBlock::new(
            BLOCK_MENU,
            menu_block(label, description, route)?,
        ));
    }
    if let Some(entry) = &options.entry {
        blocks.push(PackerBlock::new(BLOCK_ENTRY, entry_block(entry)?));
    }
    let relocations = import_slot_relocations_block(&elf, &options.import_slots)?;
    blocks.push(PackerBlock::new(
        BLOCK_SEGMENTS,
        segments_block(&elf.load_segments, relocation_segment_len(&relocations)),
    ));
    for segment in &elf.load_segments {
        blocks.push(segment_block(segment, &elf_bytes)?);
    }
    if !options.import_slots.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_IMPORTS,
            symbol_records_block(
                &options
                    .import_slots
                    .iter()
                    .map(|slot| {
                        (
                            slot.import_name.as_str(),
                            slot.contract.as_str(),
                            slot.version,
                        )
                    })
                    .collect::<Vec<_>>(),
            )?,
        ));
    }
    if let Some(elmapi) = &options.elmapi {
        blocks.push(PackerBlock::new(
            BLOCK_API_COMPATIBILITY,
            elmapi_compatibility_block(elmapi),
        ));
    }
    if !options.exports.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_EXPORTS,
            symbol_records_block(
                &options
                    .exports
                    .iter()
                    .map(|export| (export.symbol.as_str(), export.contract.as_str(), export.version))
                    .collect::<Vec<_>>(),
            )?,
        ));
    }
    if !options.providers.is_empty() {
        blocks.push(PackerBlock::new(
            BLOCK_PROVIDER_PORTS,
            provider_ports_block(&options.providers)?,
        ));
    }
    blocks.push(PackerBlock::new(
        BLOCK_LIFECYCLE_HOOKS,
        lifecycle_hooks_block(),
    ));

    let mut symbol_names = vec!["on_initialize".to_string(), "on_finalize".to_string()];
    if let Some(entry) = &options.entry {
        symbol_names.push(entry.clone());
    }
    for slot in &options.import_slots {
        symbol_names.push(slot.slot_symbol.clone());
    }
    for export in &options.exports {
        symbol_names.push(export.symbol.clone());
    }
    for provider in &options.providers {
        symbol_names.push(provider.handler_symbol.clone());
        if let Some(snapshot) = &provider.snapshot_symbol {
            symbol_names.push(snapshot.clone());
        }
    }
    symbol_names.sort();
    symbol_names.dedup();
    let symbol_locations = symbol_locations_block(&elf, &symbol_names)?;
    blocks.push(PackerBlock::new(BLOCK_SYMBOL_LOCATIONS, symbol_locations));

    if !options.import_slots.is_empty() {
        let len = relocations.len() as u64;
        blocks.push(PackerBlock::segment(BLOCK_RELOCATIONS, relocations, len, 8));
    }

    let image = eki_image_with_hash(arch, &blocks);
    fs::write(&options.out, image).map_err(|err| format!("write {}: {err}", options.out))?;
    Ok(())
}

fn parse_native_pack_options(args: &[String]) -> Result<NativePackOptions, String> {
    if args.len() < 5 {
        usage();
        return Err("bad pack-elf arguments".to_string());
    }
    let mut options = NativePackOptions {
        out: args[0].clone(),
        elf: args[1].clone(),
        name: args[2].clone(),
        version: args[3].clone(),
        kind: parse_kind(&args[4])?,
        arch: None,
        entry: None,
        menu: None,
        import_slots: Vec::new(),
        exports: Vec::new(),
        providers: Vec::new(),
        elmapi: None,
    };
    let mut index = 5;
    while index < args.len() {
        match args[index].as_str() {
            "--arch" => {
                let value = option_arg(args, index + 1, "--arch")?;
                options.arch = Some(parse_arch(value)?);
                index += 2;
            }
            "--entry" => {
                options.entry = Some(option_arg(args, index + 1, "--entry")?.to_string());
                index += 2;
            }
            "--menu" => {
                let label = option_arg(args, index + 1, "--menu")?.to_string();
                let description = option_arg(args, index + 2, "--menu")?.to_string();
                let route = option_arg(args, index + 3, "--menu")?.to_string();
                options.menu = Some((label, description, route));
                index += 4;
            }
            "--runtime-log-slot" => {
                let slot_symbol = option_arg(args, index + 1, "--runtime-log-slot")?.to_string();
                options.import_slots.push(ImportSlotSpec {
                    slot_symbol,
                    import_name: ELM_RUNTIME_LOG_EXPORT_NAME.to_string(),
                    contract: ELM_RUNTIME_LOG_EXPORT_CONTRACT.to_string(),
                    version: ELM_RUNTIME_LOG_EXPORT_VERSION,
                });
                index += 2;
            }
            "--import-slot" => {
                let slot_symbol = option_arg(args, index + 1, "--import-slot")?.to_string();
                let import_name = option_arg(args, index + 2, "--import-slot")?.to_string();
                let contract = option_arg(args, index + 3, "--import-slot")?.to_string();
                let version = parse_u32(option_arg(args, index + 4, "--import-slot")?, "version")?;
                options.import_slots.push(ImportSlotSpec {
                    slot_symbol,
                    import_name,
                    contract,
                    version,
                });
                index += 5;
            }
            "--elmapi-root-slot" => {
                if options.elmapi.is_some() {
                    return Err("--elmapi-root-slot may only be specified once".to_string());
                }
                let slot_symbol = option_arg(args, index + 1, "--elmapi-root-slot")?.to_string();
                let versions = parse_elmapi_versions(option_arg(
                    args,
                    index + 2,
                    "--elmapi-root-slot",
                )?)?;
                let required_features = parse_u64(
                    option_arg(args, index + 3, "--elmapi-root-slot")?,
                    "required features",
                )?;
                let root_import_index = options.import_slots.len() as u32;
                options.import_slots.push(ImportSlotSpec {
                    slot_symbol,
                    import_name: ELM_API_ROOT_IMPORT_NAME.to_string(),
                    contract: ELM_API_ROOT_IMPORT_CONTRACT.to_string(),
                    version: 0,
                });
                options.elmapi = Some(ElmApiSpec {
                    root_import_index,
                    versions,
                    required_features,
                });
                index += 4;
            }
            "--export" => {
                let symbol = option_arg(args, index + 1, "--export")?.to_string();
                let contract = option_arg(args, index + 2, "--export")?.to_string();
                let version = parse_u32(option_arg(args, index + 3, "--export")?, "version")?;
                options.exports.push(ExportSpec {
                    symbol,
                    contract,
                    version,
                });
                index += 4;
            }
            "--provider" => {
                let contract = option_arg(args, index + 1, "--provider")?.to_string();
                let access = parse_access(option_arg(args, index + 2, "--provider")?)?;
                let direction = parse_direction(option_arg(args, index + 3, "--provider")?)?;
                let mode = parse_mode(option_arg(args, index + 4, "--provider")?)?;
                let handler_symbol = option_arg(args, index + 5, "--provider")?.to_string();
                let snapshot = option_arg(args, index + 6, "--provider")?;
                options.providers.push(ProviderSpec {
                    contract,
                    access,
                    direction,
                    mode,
                    handler_symbol,
                    snapshot_symbol: if snapshot == "-" {
                        None
                    } else {
                        Some(snapshot.to_string())
                    },
                });
                index += 7;
            }
            other => return Err(format!("unknown pack-elf option: {other}")),
        }
    }
    Ok(options)
}

fn option_arg<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing argument for {option}"))
}

fn parse_u32(raw: &str, name: &str) -> Result<u32, String> {
    raw.parse::<u32>()
        .map_err(|_| format!("bad {name}: {raw}"))
}

fn parse_u64(raw: &str, name: &str) -> Result<u64, String> {
    if let Some(hex) = raw.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| format!("bad {name}: {raw}"))
    } else {
        raw.parse::<u64>()
            .map_err(|_| format!("bad {name}: {raw}"))
    }
}

fn parse_elmapi_versions(raw: &str) -> Result<Vec<u16>, String> {
    let mut versions = Vec::new();
    for value in raw.split(',') {
        let version = value
            .parse::<u16>()
            .map_err(|_| format!("bad elmapi version: {value}"))?;
        if version == 0 || versions.contains(&version) {
            return Err(format!("bad or duplicate elmapi version: {value}"));
        }
        versions.push(version);
    }
    versions.sort_unstable();
    if versions.is_empty() || versions.len() > ELM_API_MAX_COMPATIBLE_VERSIONS {
        return Err(format!(
            "elmapi compatibility set must contain 1..={ELM_API_MAX_COMPATIBLE_VERSIONS} versions"
        ));
    }
    if versions.as_slice() != [ELM_API_VERSION_V1] {
        return Err(format!(
            "unpublished elmapi only accepts version {ELM_API_VERSION_V1}"
        ));
    }
    Ok(versions)
}

fn parse_arch(raw: &str) -> Result<ElmEbiArch, String> {
    match raw {
        "any" => Ok(ElmEbiArch::Any),
        "riscv64" => Ok(ElmEbiArch::Riscv64),
        "loongarch64" => Ok(ElmEbiArch::LoongArch64),
        _ => Err(format!("unknown arch: {raw}")),
    }
}

fn parse_access(raw: &str) -> Result<ElmPortAccessPolicy, String> {
    match raw {
        "internal" => Ok(ElmPortAccessPolicy::Internal),
        "public" => Ok(ElmPortAccessPolicy::Public),
        "extension-only" => Ok(ElmPortAccessPolicy::ExtensionOnly),
        _ => Err(format!("unknown provider access: {raw}")),
    }
}

fn parse_direction(raw: &str) -> Result<FlowDirection, String> {
    match raw {
        "source" => Ok(FlowDirection::Source),
        "sink" => Ok(FlowDirection::Sink),
        "duplex" => Ok(FlowDirection::Duplex),
        "control" => Ok(FlowDirection::Control),
        _ => Err(format!("unknown provider direction: {raw}")),
    }
}

fn parse_mode(raw: &str) -> Result<FlowMode, String> {
    match raw {
        "exclusive" => Ok(FlowMode::Exclusive),
        "shared" => Ok(FlowMode::Shared),
        "ordered" => Ok(FlowMode::Ordered),
        "pipeline" => Ok(FlowMode::Pipeline),
        "broadcast" => Ok(FlowMode::Broadcast),
        _ => Err(format!("unknown provider mode: {raw}")),
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
    let image = parse_eki_image(&bytes).map_err(|status| format!("EKI parse failed: {status:?}"))?;
    if let Some(elmapi) = &image.unit.api_compatibility {
        println!("elmapi.root_import_index={}", elmapi.root_import_index);
        println!("elmapi.required_features=0x{:x}", elmapi.required_features);
        println!("elmapi.compatible_versions={:?}", elmapi.compatible_versions);
    }
    Ok(())
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
    let output = sign_eki_image(&input, &SigningKey::from_bytes(&seed), &args[3], release_epoch)?;
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
    let image = parse_eki_image(&bytes).map_err(|status| format!("EKI parse failed: {status:?}"))?;
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
    proof.signature = signing.sign(&proof.unsigned_message(fingerprint)).to_bytes();
    let proof_payload = proof_block(&proof)?;
    let index = blocks
        .iter()
        .position(|block| block.kind == BLOCK_PROOF)
        .ok_or_else(|| "proof block disappeared".to_string())?;
    blocks[index] = PackerBlock::new(BLOCK_PROOF, proof_payload);
    Ok(eki_image_with_hash(arch, &blocks))
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
}

#[derive(Clone, Copy)]
struct ElfHeader {
    machine: u16,
    phoff: u64,
    shoff: u64,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[derive(Clone, Copy)]
struct ElfSection {
    section_type: u32,
    offset: u64,
    size: u64,
    link: u32,
    entsize: u64,
}

impl ElfImage {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let header = parse_elf_header(bytes)?;
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
            symbols,
        })
    }

    fn symbol(&self, name: &str) -> Result<&ElfSymbol, String> {
        self.symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .ok_or_else(|| format!("symbol not found in ELF: {name}"))
    }

    fn symbol_location(&self, name: &str) -> Result<(u32, u64, u64), String> {
        let symbol = self.symbol(name)?;
        let segment = self
            .load_segments
            .iter()
            .find(|segment| {
                let end = segment.vaddr.saturating_add(segment.mem_size);
                symbol.value >= segment.vaddr && symbol.value < end
            })
            .ok_or_else(|| format!("symbol is outside PT_LOAD segments: {name}"))?;
        let offset = symbol.value - segment.vaddr;
        let size = symbol.size.max(1);
        if offset.saturating_add(size) > segment.mem_size {
            return Err(format!("symbol range is outside segment: {name}"));
        }
        Ok((segment.index, offset, size))
    }
}

fn parse_elf_header(bytes: &[u8]) -> Result<ElfHeader, String> {
    if bytes.len() < 64 || bytes.get(0..4) != Some(b"\x7fELF") {
        return Err("bad ELF magic".to_string());
    }
    if read_u8(bytes, 4)? != 2 || read_u8(bytes, 5)? != 1 || read_u8(bytes, 6)? != 1 {
        return Err("only ELF64 little-endian v1 is supported".to_string());
    }
    Ok(ElfHeader {
        machine: read_u16(bytes, 18)?,
        phoff: read_u64(bytes, 32)?,
        shoff: read_u64(bytes, 40)?,
        phentsize: read_u16(bytes, 54)?,
        phnum: read_u16(bytes, 56)?,
        shentsize: read_u16(bytes, 58)?,
        shnum: read_u16(bytes, 60)?,
        shstrndx: read_u16(bytes, 62)?,
    })
}

fn arch_from_machine(machine: u16) -> Result<ElmEbiArch, String> {
    match machine {
        243 => Ok(ElmEbiArch::Riscv64),
        258 => Ok(ElmEbiArch::LoongArch64),
        _ => Err(format!("unsupported ELF machine: {machine}")),
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
        checked_slice(bytes, file_offset as usize, file_size as usize)?;
        let kind = if p_flags & 1 != 0 {
            if file_size == 0 {
                return Err("executable PT_LOAD segment cannot be empty".to_string());
            }
            ElmEbiSegmentKind::Code
        } else if p_flags & 2 != 0 {
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
    let mut out = Vec::new();
    for index in 0..header.shnum as usize {
        let offset = checked_add(header.shoff as usize, index * header.shentsize as usize)?;
        let section = ElfSection {
            section_type: read_u32(bytes, offset + 4)?,
            offset: read_u64(bytes, offset + 24)?,
            size: read_u64(bytes, offset + 32)?,
            link: read_u32(bytes, offset + 40)?,
            entsize: read_u64(bytes, offset + 56)?,
        };
        checked_slice(bytes, section.offset as usize, section.size as usize)?;
        out.push(section);
    }
    Ok(out)
}

fn parse_elf_symbols(bytes: &[u8], sections: &[ElfSection]) -> Result<Vec<ElfSymbol>, String> {
    let mut out = Vec::new();
    for section in sections {
        if section.section_type != 2 && section.section_type != 11 {
            continue;
        }
        if section.entsize == 0 || section.size % section.entsize != 0 {
            return Err("bad ELF symbol table entry size".to_string());
        }
        let strings = sections
            .get(section.link as usize)
            .ok_or_else(|| "bad ELF symbol string table link".to_string())?;
        if strings.section_type != 3 {
            return Err("ELF symbol table link is not STRTAB".to_string());
        }
        let strtab = checked_slice(bytes, strings.offset as usize, strings.size as usize)?;
        let count = section.size / section.entsize;
        for index in 0..count as usize {
            let offset = checked_add(section.offset as usize, index * section.entsize as usize)?;
            let name_offset = read_u32(bytes, offset)? as usize;
            if name_offset == 0 {
                continue;
            }
            let name = read_cstr(strtab, name_offset)?;
            if name.is_empty() {
                continue;
            }
            let value = read_u64(bytes, offset + 8)?;
            let size = read_u64(bytes, offset + 16)?;
            out.push(ElfSymbol { name, value, size });
        }
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
    let base = first.vaddr;
    let mut expected = 0u64;
    for segment in segments {
        if segment.vaddr != base.saturating_add(expected) {
            return Err(format!(
                "ELF PT_LOAD layout is not ELM-compatible at vaddr=0x{:x}; use page-aligned contiguous LOAD segments",
                segment.vaddr
            ));
        }
        expected = align_up_u64(expected.saturating_add(segment.mem_size), ELM_TOOL_PAGE_SIZE)?;
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
        sha256(kernel_api_manifest_v1(arch as u32).as_bytes()),
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
    out[88..120].copy_from_slice(&fingerprint.kernel_api_hash);
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

fn entry_block(symbol: &str) -> Result<Vec<u8>, String> {
    if symbol.len() > ELM_EBI_SYMBOL_NAME_LEN {
        return Err("entry symbol too long".to_string());
    }
    let mut out = vec![0; 8 + ELM_EBI_SYMBOL_NAME_LEN];
    write_u16(&mut out, 0, symbol.len() as u16);
    copy_fixed(&mut out, 8, symbol);
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

fn symbol_records_block(entries: &[(&str, &str, u32)]) -> Result<Vec<u8>, String> {
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + entries.len() * EKI_SYMBOL_RECORD_SIZE];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, (name, contract, version)) in entries.iter().enumerate() {
        if name.len() > ELM_EBI_SYMBOL_NAME_LEN || contract.len() > ELM_NEXUS_CONTRACT_LEN {
            return Err("native symbol record field too long".to_string());
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SYMBOL_RECORD_SIZE;
        let (min_version, max_version) = if *version == 0 {
            (1, u32::MAX)
        } else {
            (*version, *version)
        };
        write_u32(&mut out, offset, min_version);
        write_u16(&mut out, offset + 8, name.len() as u16);
        write_u16(&mut out, offset + 10, contract.len() as u16);
        write_u32(&mut out, offset + 12, max_version);
        copy_fixed(&mut out, offset + 16, name);
        copy_fixed(&mut out, offset + 16 + ELM_EBI_SYMBOL_NAME_LEN, contract);
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
        write_u16(&mut out, offset + 16, provider.contract.len() as u16);
        write_u16(
            &mut out,
            offset + 18,
            provider.handler_symbol.len() as u16,
        );
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

fn import_slot_relocations_block(
    elf: &ElfImage,
    slots: &[ImportSlotSpec],
) -> Result<Vec<u8>, String> {
    let mut out = vec![0; EKI_TABLE_HEADER_SIZE + slots.len() * EKI_RELOCATION_RECORD_SIZE];
    write_u32(&mut out, 0, slots.len() as u32);
    for (index, slot) in slots.iter().enumerate() {
        let (segment_index, offset_in_segment, size) = elf.symbol_location(&slot.slot_symbol)?;
        if size < 8 {
            return Err(format!(
                "import slot must be at least 8 bytes: {}",
                slot.slot_symbol
            ));
        }
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_RELOCATION_RECORD_SIZE;
        write_u32(&mut out, offset, ElmEbiRelocationKind::ImportAbs64 as u32);
        write_u32(&mut out, offset + 8, segment_index);
        write_u32(&mut out, offset + 12, index as u32);
        write_u64(&mut out, offset + 16, offset_in_segment);
    }
    Ok(out)
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
