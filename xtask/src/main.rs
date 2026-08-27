use std::env;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use object::endian::LittleEndian;
use object::read::elf::{ElfFile64, ProgramHeader as _};
use object::{Object, ObjectSymbol, SymbolSection};
use xtask::{CATALOG_RELATIVE_PATH, ImageFormat, PlatformCatalog, PlatformSpec};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PLATFORM_IDENTITY_SYMBOL: &str = "HITOSHIZUKU_PLATFORM_TAG";

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
    let catalog = PlatformCatalog::load(root.join(CATALOG_RELATIVE_PATH))
        .map_err(|error| error.to_string())?;
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    let rest = args.collect::<Vec<_>>();

    match command.as_str() {
        "config" | "oldconfig" | "defconfig" => configure(&root, &command),
        "modules" => build_modules(&root, &catalog, &rest),
        "build" | "kernel" => build_kernel(&root, &catalog, &rest),
        "image" => build_image(&root, &catalog, &rest),
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

fn build_modules(root: &Path, catalog: &PlatformCatalog, args: &[String]) -> Result<(), String> {
    let options = BuildOptions::parse(args)?;
    if options.reuse_modules {
        return Err("--reuse-modules is only valid for build and image".to_string());
    }
    let context = BuildContext::resolve(&options, catalog)?;
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
    if let Some(initramfs) = options.initramfs.as_deref() {
        kernel_environment.push(("INITRAMFS", initramfs.into()));
    }
    context.append_platform_environment(&mut kernel_environment);
    cargo_with_env(
        root,
        kernel_cargo_command(
            &context.target,
            options.features.as_deref(),
            options.initramfs.is_some(),
        ),
        &kernel_environment,
    )?;

    let kernel = cargo_target.join(&context.target).join("release/kernel");
    let interface = root.join(&context.interface_dir);
    let mut profile_environment = vec![("CARGO_TARGET_DIR", cargo_target.as_os_str().to_owned())];
    context.append_platform_environment(&mut profile_environment);
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
        &profile_environment,
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
    let mut module_environment = vec![
        ("CARGO_TARGET_DIR", cargo_target.as_os_str().to_owned()),
        (
            "ELM_KERNEL_INTERFACE_ROOT",
            interface.as_os_str().to_owned(),
        ),
    ];
    context.append_platform_environment(&mut module_environment);
    cargo_elm_with_env(root, command, &module_environment)
}

fn build_kernel(root: &Path, catalog: &PlatformCatalog, args: &[String]) -> Result<(), String> {
    let options = BuildOptions::parse(args)?;
    let context = BuildContext::resolve(&options, catalog)?;
    ensure_config(root, &context.config)?;

    let module_output = options
        .modules
        .clone()
        .unwrap_or_else(|| context.default_module_output());
    if options.refresh_modules() {
        build_modules_to(root, &options, &context, &module_output)?;
    }

    let module_artifacts = ModuleArtifacts::load(root, &module_output)?;
    let embedded_initramfs = options.initramfs.is_some();
    let mut environment = vec![
        (
            "ELM_BUILD_BOUND_MANIFEST",
            module_artifacts.manifest.into_os_string(),
        ),
        ("ELM_INTEGRATED_ARCHIVES", module_artifacts.archives),
    ];
    if let Some(initramfs) = options.initramfs.as_deref() {
        environment.push(("INITRAMFS", initramfs.into()));
    }
    environment.push((
        "CARGO_TARGET_DIR",
        root.join(&context.target_dir).into_os_string(),
    ));
    context.append_platform_environment(&mut environment);

    let command = kernel_cargo_command(
        &context.target,
        options.features.as_deref(),
        embedded_initramfs,
    );
    cargo_with_env(root, command, &environment)
}

fn kernel_cargo_command(
    target: &str,
    features: Option<&str>,
    embedded_initramfs: bool,
) -> Vec<OsString> {
    let mut command: Vec<OsString> = vec![
        "build".into(),
        "-p".into(),
        "kernel".into(),
        "--target".into(),
        target.into(),
        "--release".into(),
    ];
    if let Some(features) = features {
        command.push("--features".into());
        command.push(features.into());
    }
    if embedded_initramfs {
        command.push("--features".into());
        command.push("embedded-initramfs".into());
    }
    command
}

fn build_image(root: &Path, catalog: &PlatformCatalog, args: &[String]) -> Result<(), String> {
    let options = ImageOptions::parse(args)?;
    let build_options = BuildOptions::parse(&options.build_args)?;
    let context = BuildContext::resolve(&build_options, catalog)?;
    if !options.no_build {
        build_kernel(root, catalog, &options.build_args)?;
    }

    let source = root
        .join(&context.target_dir)
        .join(&context.target)
        .join("release/kernel");
    if !source.is_file() {
        return Err(format!(
            "kernel ELF {} does not exist; run without --no-build or build this platform first",
            source.display()
        ));
    }
    validate_kernel_elf(&source, &context.platform)?;

    let formats = options.formats(&context.platform)?;
    let output_dir = root.join(&context.platform.image.output_dir);
    std::fs::create_dir_all(&output_dir)
        .map_err(|error| format!("create {}: {error}", output_dir.display()))?;

    let needs_raw = formats.contains(&ImageFormat::Raw) || formats.contains(&ImageFormat::Uimage);
    let mut raw = needs_raw
        .then(|| TempOutput::create(&output_dir, "kernel.bin"))
        .transpose()?;
    if let Some(raw) = raw.as_ref() {
        let mut command = Command::new(&options.objcopy);
        command
            .current_dir(root)
            .arg("-O")
            .arg("binary")
            .arg(&source)
            .arg(raw.path());
        run_command(command).map_err(|error| {
            format!(
                "create raw kernel with {} (override with --objcopy): {error}",
                options.objcopy.display()
            )
        })?;
        validate_raw_image(raw.path())?;
    }

    let mut uimage = formats
        .contains(&ImageFormat::Uimage)
        .then(|| TempOutput::create(&output_dir, "uImage"))
        .transpose()?;
    if let Some(output) = uimage.as_ref() {
        let settings =
            context.platform.image.uimage.as_ref().ok_or_else(|| {
                format!("platform {:?} has no uImage recipe", context.platform.id)
            })?;
        let raw = raw.as_ref().expect("uImage always requires a raw payload");
        let mut command = Command::new(&options.mkimage);
        command.current_dir(root).args(mkimage_args(
            settings,
            context.platform.link.physical_base.get(),
            raw.path(),
            output.path(),
        ));
        run_command(command).map_err(|error| {
            format!(
                "create uImage with {} (the platform may require a board-specific mkimage; override with --mkimage): {error}",
                options.mkimage.display()
            )
        })?;
        validate_uimage(
            output.path(),
            raw.path(),
            context.platform.link.physical_base.get(),
            settings,
        )?;
    }

    if formats.contains(&ImageFormat::Elf) {
        let mut elf = TempOutput::create(&output_dir, "kernel.elf")?;
        std::fs::copy(&source, elf.path()).map_err(|error| {
            format!(
                "copy {} to {}: {error}",
                source.display(),
                elf.path().display()
            )
        })?;
        validate_kernel_elf(elf.path(), &context.platform)?;
        elf.commit(&output_dir.join(ImageFormat::Elf.file_name()))?;
    }
    if formats.contains(&ImageFormat::Raw) {
        raw.as_mut()
            .expect("raw output was generated")
            .commit(&output_dir.join(ImageFormat::Raw.file_name()))?;
    }
    if formats.contains(&ImageFormat::Uimage) {
        uimage
            .as_mut()
            .expect("uImage output was generated")
            .commit(&output_dir.join(ImageFormat::Uimage.file_name()))?;
    }

    println!(
        "published {} image(s) for {} in {}",
        formats.len(),
        context.platform.id,
        output_dir.display()
    );
    Ok(())
}

struct ImageOptions {
    build_args: Vec<String>,
    no_build: bool,
    format: Option<ImageFormatRequest>,
    objcopy: PathBuf,
    mkimage: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImageFormatRequest {
    All,
    One(ImageFormat),
}

impl ImageOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            build_args: Vec::new(),
            no_build: false,
            format: None,
            objcopy: PathBuf::from("llvm-objcopy"),
            mkimage: PathBuf::from("mkimage"),
        };
        let mut index = 0;
        while index < args.len() {
            let key = args[index].as_str();
            if key == "--no-build" {
                if options.no_build {
                    return Err("--no-build was specified more than once".to_string());
                }
                options.no_build = true;
                index += 1;
                continue;
            }
            if key == "--reuse-modules" {
                options.build_args.push(key.to_string());
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("{key} requires a value"))?;
            match key {
                "--format" => {
                    if options.format.is_some() {
                        return Err("--format was specified more than once".to_string());
                    }
                    options.format = Some(if value == "all" {
                        ImageFormatRequest::All
                    } else {
                        ImageFormatRequest::One(
                            value
                                .parse::<ImageFormat>()
                                .map_err(|error| error.to_string())?,
                        )
                    });
                }
                "--objcopy" => options.objcopy = value.into(),
                "--mkimage" => options.mkimage = value.into(),
                _ => {
                    options.build_args.push(args[index].clone());
                    options.build_args.push(value.clone());
                }
            }
            index += 2;
        }
        Ok(options)
    }

    fn formats(&self, platform: &PlatformSpec) -> Result<Vec<ImageFormat>, String> {
        let formats = match self.format {
            None => platform.image.default_formats.clone(),
            Some(ImageFormatRequest::All) => platform.image.allowed_formats.clone(),
            Some(ImageFormatRequest::One(format)) => vec![format],
        };
        for format in &formats {
            if !platform.image.allowed_formats.contains(format) {
                return Err(format!(
                    "platform {:?} does not support {:?} images; allowed formats: {:?}",
                    platform.id, format, platform.image.allowed_formats
                ));
            }
        }
        Ok(formats)
    }
}

fn validate_kernel_elf(path: &Path, platform: &PlatformSpec) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let file = object::File::parse(bytes.as_slice())
        .map_err(|error| format!("parse {} as ELF: {error}", path.display()))?;
    if file.format() != object::BinaryFormat::Elf
        || file.kind() != object::ObjectKind::Executable
        || !file.is_64()
        || !file.is_little_endian()
    {
        return Err(format!(
            "{} must be a little-endian ELF64 ET_EXEC kernel",
            path.display()
        ));
    }
    let expected_arch = match platform.link.layout {
        xtask::LinkLayout::Loongarch64Dmw1 => object::Architecture::LoongArch64,
        xtask::LinkLayout::Riscv64Sv48 => object::Architecture::Riscv64,
    };
    if file.architecture() != expected_arch {
        return Err(format!(
            "{} architecture {:?} does not match platform {:?} ({expected_arch:?})",
            path.display(),
            file.architecture(),
            platform.id
        ));
    }
    let entry = file.entry();
    if entry != platform.link.virtual_base.get() {
        return Err(format!(
            "{} entry {entry:#018x} does not match platform virtual base {}",
            path.display(),
            platform.link.virtual_base
        ));
    }
    let start = file
        .symbols()
        .find_map(|symbol| (symbol.name().ok() == Some("_start")).then_some(symbol.address()))
        .ok_or_else(|| format!("{} has no _start symbol", path.display()))?;
    if start != entry {
        return Err(format!(
            "{} _start {start:#018x} does not match entry {entry:#018x}",
            path.display()
        ));
    }
    let platform_tag = file
        .symbols()
        .find_map(|symbol| {
            (symbol.section() == SymbolSection::Absolute
                && symbol.name().ok() == Some(PLATFORM_IDENTITY_SYMBOL))
            .then_some(symbol.address())
        })
        .ok_or_else(|| {
            format!(
                "{} has no {PLATFORM_IDENTITY_SYMBOL} provenance symbol",
                path.display()
            )
        })?;
    if platform_tag != platform.identity_tag() {
        return Err(format!(
            "{} was built for another platform (tag {platform_tag:#018x}); expected {:?} ({:#018x})",
            path.display(),
            platform.id,
            platform.identity_tag()
        ));
    }

    let elf = ElfFile64::<LittleEndian>::parse(bytes.as_slice())
        .map_err(|error| format!("parse {} program headers: {error}", path.display()))?;
    let endian = elf.endian();
    let mut loads = Vec::new();
    for header in elf.elf_program_headers() {
        if header.p_type(endian) != object::elf::PT_LOAD || header.p_memsz(endian) == 0 {
            continue;
        }
        let vaddr = header.p_vaddr(endian);
        let paddr = header.p_paddr(endian);
        let filesz = header.p_filesz(endian);
        let memsz = header.p_memsz(endian);
        let offset = header.p_offset(endian);
        let alignment = header.p_align(endian);
        let flags = header.p_flags(endian);
        if filesz > memsz
            || offset
                .checked_add(filesz)
                .is_none_or(|end| end > bytes.len() as u64)
            || alignment != 0 && !alignment.is_power_of_two()
        {
            return Err(format!("{} contains an invalid PT_LOAD", path.display()));
        }
        if flags & object::elf::PF_W != 0 && flags & object::elf::PF_X != 0 {
            return Err(format!(
                "{} contains a writable and executable PT_LOAD",
                path.display()
            ));
        }
        let expected_offset = platform
            .link
            .virtual_base
            .get()
            .wrapping_sub(platform.link.physical_base.get());
        if vaddr.wrapping_sub(paddr) != expected_offset {
            return Err(format!(
                "{} PT_LOAD VMA {vaddr:#018x} and PADDR {paddr:#018x} violate platform mapping",
                path.display()
            ));
        }
        loads.push((vaddr, paddr, memsz, flags));
    }
    if loads.is_empty() {
        return Err(format!("{} contains no PT_LOAD segments", path.display()));
    }
    let first = loads
        .iter()
        .min_by_key(|(_, paddr, _, _)| *paddr)
        .expect("nonempty PT_LOAD list");
    if first.1 != platform.link.physical_base.get() || first.0 != platform.link.virtual_base.get() {
        return Err(format!(
            "{} first PT_LOAD is VMA {:#018x}/PADDR {:#018x}, expected {}/{}",
            path.display(),
            first.0,
            first.1,
            platform.link.virtual_base,
            platform.link.physical_base
        ));
    }
    if !loads.iter().any(|(vaddr, _, memsz, flags)| {
        *flags & object::elf::PF_X != 0 && entry >= *vaddr && entry < vaddr.saturating_add(*memsz)
    }) {
        return Err(format!(
            "{} entry is not covered by an executable PT_LOAD",
            path.display()
        ));
    }
    validate_nonoverlapping_loads(path, &loads, true)?;
    validate_nonoverlapping_loads(path, &loads, false)
}

fn validate_nonoverlapping_loads(
    path: &Path,
    loads: &[(u64, u64, u64, u32)],
    virtual_addresses: bool,
) -> Result<(), String> {
    let mut ranges = loads
        .iter()
        .map(|(vaddr, paddr, size, _)| {
            let start = if virtual_addresses { *vaddr } else { *paddr };
            let end = start
                .checked_add(*size)
                .ok_or_else(|| format!("{} PT_LOAD address range overflows", path.display()))?;
            Ok((start, end))
        })
        .collect::<Result<Vec<_>, String>>()?;
    ranges.sort_unstable();
    if ranges.windows(2).any(|range| range[0].1 > range[1].0) {
        return Err(format!(
            "{} contains overlapping PT_LOAD {} ranges",
            path.display(),
            if virtual_addresses {
                "virtual"
            } else {
                "physical"
            }
        ));
    }
    Ok(())
}

fn validate_raw_image(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("raw image {} is empty", path.display()));
    }
    if bytes.starts_with(b"\x7fELF") {
        return Err(format!(
            "raw image {} still contains an ELF header",
            path.display()
        ));
    }
    Ok(())
}

fn mkimage_args(
    spec: &xtask::UImageSpec,
    load_address: u64,
    raw: &Path,
    output: &Path,
) -> Vec<OsString> {
    [
        "-A".into(),
        spec.architecture.clone().into(),
        "-O".into(),
        spec.os.clone().into(),
        "-T".into(),
        spec.image_type.clone().into(),
        "-C".into(),
        spec.compression.clone().into(),
        "-a".into(),
        format!("0x{load_address:x}").into(),
        "-e".into(),
        format!("0x{load_address:x}").into(),
        "-n".into(),
        spec.name.clone().into(),
        "-d".into(),
        raw.as_os_str().to_owned(),
        output.as_os_str().to_owned(),
    ]
    .into()
}

fn validate_uimage(
    path: &Path,
    raw: &Path,
    load_address: u64,
    spec: &xtask::UImageSpec,
) -> Result<(), String> {
    let image = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let payload = std::fs::read(raw).map_err(|error| format!("read {}: {error}", raw.display()))?;
    if image.len() < 64
        || u32::from_be_bytes(image[0..4].try_into().expect("fixed slice")) != 0x2705_1956
    {
        return Err(format!("{} is not a legacy uImage", path.display()));
    }
    let declared_size = u32::from_be_bytes(image[12..16].try_into().expect("fixed slice")) as usize;
    let declared_load = u32::from_be_bytes(image[16..20].try_into().expect("fixed slice")) as u64;
    let declared_entry = u32::from_be_bytes(image[20..24].try_into().expect("fixed slice")) as u64;
    if declared_size != payload.len()
        || image.len() != 64 + payload.len()
        || &image[64..] != payload.as_slice()
        || declared_load != load_address
        || declared_entry != load_address
    {
        return Err(format!(
            "{} header or payload does not match the raw kernel and platform addresses",
            path.display()
        ));
    }
    let expected_ids = spec
        .legacy_header_ids()
        .map_err(|error| error.to_string())?;
    let mut expected_name = [0u8; 32];
    expected_name[..spec.name.len()].copy_from_slice(spec.name.as_bytes());
    if image[28..32] != expected_ids || image[32..64] != expected_name {
        return Err(format!(
            "{} uImage metadata does not match its platform recipe",
            path.display()
        ));
    }
    let header_crc = u32::from_be_bytes(image[4..8].try_into().expect("fixed slice"));
    let mut header = image[..64].to_vec();
    header[4..8].fill(0);
    let data_crc = u32::from_be_bytes(image[24..28].try_into().expect("fixed slice"));
    if crc32(&header) != header_crc || crc32(&payload) != data_crc {
        return Err(format!("{} has an invalid uImage CRC", path.display()));
    }
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

struct TempOutput {
    path: PathBuf,
    committed: bool,
}

impl TempOutput {
    fn create(directory: &Path, name: &str) -> Result<Self, String> {
        for _ in 0..100 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => {
                    return Ok(Self {
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create {}: {error}", path.display())),
            }
        }
        Err(format!(
            "cannot allocate a temporary output in {}",
            directory.display()
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(&mut self, destination: &Path) -> Result<(), String> {
        std::fs::rename(&self.path, destination).map_err(|error| {
            format!(
                "publish {} as {}: {error}",
                self.path.display(),
                destination.display()
            )
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
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

struct BuildOptions {
    platform: Option<String>,
    board: Option<String>,
    target: Option<String>,
    config: Option<String>,
    output: Option<String>,
    modules: Option<String>,
    reuse_modules: bool,
    target_dir: Option<String>,
    features: Option<String>,
    initramfs: Option<String>,
}

impl BuildOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            platform: None,
            board: None,
            target: None,
            config: None,
            output: None,
            modules: None,
            reuse_modules: false,
            target_dir: None,
            features: None,
            initramfs: None,
        };
        let mut index = 0;
        while index < args.len() {
            let key = args[index].as_str();
            if key == "--reuse-modules" {
                if options.reuse_modules {
                    return Err("--reuse-modules was specified more than once".to_string());
                }
                options.reuse_modules = true;
                index += 1;
                continue;
            }
            let value = || {
                args.get(index + 1)
                    .cloned()
                    .ok_or_else(|| format!("{key} requires a value"))
            };
            match key {
                "--platform" => options.platform = Some(value()?),
                "--board" => options.board = Some(value()?),
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
        if options.platform.is_some() && (options.board.is_some() || options.target.is_some()) {
            return Err("--platform cannot be combined with --board or --target".to_string());
        }
        Ok(options)
    }

    fn refresh_modules(&self) -> bool {
        !self.reuse_modules
    }
}

struct ModuleArtifacts {
    manifest: PathBuf,
    archives: OsString,
}

impl ModuleArtifacts {
    fn load(root: &Path, output: &str) -> Result<Self, String> {
        let output = root.join(output);
        let manifest = output.join("modules.manifest");
        if !manifest.is_file() {
            return Err(format!(
                "module manifest {} does not exist; rebuild without --reuse-modules",
                manifest.display()
            ));
        }
        if manifest
            .metadata()
            .map_err(|error| format!("inspect {}: {error}", manifest.display()))?
            .len()
            == 0
        {
            return Err(format!("module manifest {} is empty", manifest.display()));
        }

        let archive_list = output.join("integrated.archives");
        let archive_paths = std::fs::read_to_string(&archive_list)
            .map_err(|error| format!("read {}: {error}", archive_list.display()))?
            .lines()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            })
            .collect::<Vec<_>>();
        if archive_paths.is_empty() {
            return Err(format!(
                "integrated archive list {} is empty",
                archive_list.display()
            ));
        }
        for archive in &archive_paths {
            if !archive.is_file() {
                return Err(format!(
                    "integrated archive {} does not exist; rebuild without --reuse-modules",
                    archive.display()
                ));
            }
        }
        let archives = env::join_paths(archive_paths)
            .map_err(|error| format!("encode integrated archive paths: {error}"))?;
        Ok(Self { manifest, archives })
    }
}

struct BuildContext {
    platform: PlatformSpec,
    target: String,
    config: String,
    target_dir: String,
    interface_dir: String,
}

impl BuildContext {
    fn resolve(options: &BuildOptions, catalog: &PlatformCatalog) -> Result<Self, String> {
        let inherited_platform =
            if options.platform.is_none() && options.board.is_none() && options.target.is_none() {
                env::var("HITOSHIZUKU_PLATFORM")
                    .ok()
                    .filter(|value| !value.is_empty())
            } else {
                None
            };
        Self::resolve_with_platform_env(options, catalog, inherited_platform.as_deref())
    }

    fn resolve_with_platform_env(
        options: &BuildOptions,
        catalog: &PlatformCatalog,
        inherited_platform: Option<&str>,
    ) -> Result<Self, String> {
        let platform_id = options.platform.as_deref().or(inherited_platform);
        let platform = catalog
            .select(
                platform_id,
                options.board.as_deref(),
                options.target.as_deref(),
            )
            .map_err(|error| error.to_string())?
            .clone();
        let target_dir = options
            .target_dir
            .clone()
            .unwrap_or_else(|| platform.target_dir.clone());
        Ok(Self {
            target: platform.target.clone(),
            config: options
                .config
                .clone()
                .unwrap_or_else(|| platform.config.clone()),
            target_dir,
            interface_dir: platform.interface_dir.clone(),
            platform,
        })
    }

    fn default_module_output(&self) -> String {
        format!("{}/modules", self.platform.image.output_dir)
    }

    fn append_platform_environment(&self, environment: &mut Vec<(&'static str, OsString)>) {
        environment.push(("HITOSHIZUKU_PLATFORM", self.platform.id.clone().into()));
    }
}

fn clear_inherited_build_environment(command: &mut Command) {
    command.env_remove("HITOSHIZUKU_PLATFORM");
    command.env_remove("MYGO_LA_BOARD");
    command.env_remove("MYGO_LA_DEBUG_LINKER");
    command.env_remove("MYGO_RV_DEBUG_LINKER");
    command.env_remove("ELM_BUILD_BOUND_MANIFEST");
    command.env_remove("ELM_INTEGRATED_ARCHIVES");
}

fn cargo<I, S>(root: &Path, args: I, env_var: Option<(&str, OsString)>) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("cargo");
    command.current_dir(root).args(args);
    clear_inherited_build_environment(&mut command);
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
    clear_inherited_build_environment(&mut command);
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
    clear_inherited_build_environment(&mut command);
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
    clear_inherited_build_environment(&mut command);
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
  modules [--platform <id> | --board <qemu|ls2k1000|visionfive2> [--target <triple>]] [--config <path>] [--output <dir>]\n\
  build [--platform <id> | --board <qemu|ls2k1000|visionfive2> [--target <triple>]] [--config <path>] [--modules <dir>] [--reuse-modules] [--features <a,b>] [--initramfs <cpio>]\n\
  image [build options] [--reuse-modules] [--no-build] [--format <elf|raw|uimage|all>] [--objcopy <path>] [--mkimage <path>]\n\
  clean\n\n\
Platform definitions select the target, link layout, config, and output paths. QEMU defaults to qemu-loongarch64; use --target or --platform qemu-riscv64 for RISC-V.\n\
Build and image refresh the ELM profile and modules by default; --reuse-modules opts into validated existing module artifacts.\n\
The image command publishes a canonical ELF for QEMU and ELF/raw/uImage outputs for physical boards."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_SOURCE: &str = include_str!("../../configs/platforms.toml");

    fn catalog() -> PlatformCatalog {
        PlatformCatalog::parse(CATALOG_SOURCE).expect("valid platform catalog")
    }

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
        let context = BuildContext::resolve_with_platform_env(&options(&[]), &catalog(), None)
            .expect("resolve QEMU defaults");
        assert_eq!(context.platform.id, "qemu-loongarch64");
        assert_eq!(context.target, "loongarch64-unknown-none");
        assert_eq!(context.config, ".config");
        assert_eq!(context.target_dir, "target/loongarch64");
        assert_eq!(context.interface_dir, "build/elm-interface/loongarch64");
        assert_eq!(context.default_module_output(), "build/loongarch64/modules");
    }

    #[test]
    fn physical_board_defaults_are_isolated() {
        let context = BuildContext::resolve_with_platform_env(
            &options(&["--board", "visionfive2"]),
            &catalog(),
            None,
        )
        .expect("resolve VisionFive 2 defaults");
        assert_eq!(context.platform.id, "visionfive2");
        assert_eq!(context.target, "riscv64gc-unknown-none-elf");
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
        let context = BuildContext::resolve_with_platform_env(
            &options(&[
                "--board",
                "ls2k1000",
                "--config",
                "local.config",
                "--target-dir",
                "target/custom",
            ]),
            &catalog(),
            None,
        )
        .expect("resolve path overrides");
        assert_eq!(context.config, "local.config");
        assert_eq!(context.target_dir, "target/custom");
    }

    #[test]
    fn ls2k1000_exports_exact_platform_for_kernel_builds() {
        let context = BuildContext::resolve_with_platform_env(
            &options(&["--board", "ls2k1000"]),
            &catalog(),
            None,
        )
        .expect("resolve LS2K1000 defaults");
        let mut environment = Vec::new();
        context.append_platform_environment(&mut environment);
        assert_eq!(
            environment,
            vec![("HITOSHIZUKU_PLATFORM", OsString::from("ls2k1000"))]
        );
    }

    #[test]
    fn inherited_platform_is_used_without_cli_selection() {
        let context = BuildContext::resolve_with_platform_env(
            &options(&[]),
            &catalog(),
            Some("qemu-riscv64"),
        )
        .expect("resolve inherited platform");
        assert_eq!(context.platform.id, "qemu-riscv64");
    }

    #[test]
    fn board_target_mismatch_is_rejected() {
        let error = BuildContext::resolve_with_platform_env(
            &options(&[
                "--board",
                "visionfive2",
                "--target",
                "loongarch64-unknown-none",
            ]),
            &catalog(),
            None,
        )
        .err()
        .expect("mismatched target must fail");
        assert!(error.contains("visionfive2"));
        assert!(error.contains("loongarch64-unknown-none"));
    }

    #[test]
    fn unknown_board_is_rejected() {
        let error = BuildContext::resolve_with_platform_env(
            &options(&["--board", "unknown"]),
            &catalog(),
            None,
        )
        .err()
        .expect("unknown board must fail");
        assert!(error.contains("unknown board"));
    }

    #[test]
    fn explicit_platform_cannot_mix_with_legacy_selectors() {
        let error = BuildOptions::parse(&[
            "--platform".into(),
            "ls2k1000".into(),
            "--board".into(),
            "ls2k1000".into(),
        ])
        .err()
        .expect("mixed selectors must fail");
        assert!(error.contains("cannot be combined"));
    }

    #[test]
    fn build_refreshes_modules_unless_reuse_is_explicit() {
        let default = options(&[]);
        assert!(default.refresh_modules());

        let reuse = options(&["--reuse-modules"]);
        assert!(reuse.reuse_modules);
        assert!(!reuse.refresh_modules());

        let error = BuildOptions::parse(&["--reuse-modules".into(), "--reuse-modules".into()])
            .err()
            .expect("duplicate reuse flag must fail");
        assert!(error.contains("more than once"));
    }

    #[test]
    fn image_forwards_reuse_modules_as_a_flag() {
        let options = ImageOptions::parse(&[
            "--platform".into(),
            "qemu-loongarch64".into(),
            "--reuse-modules".into(),
            "--format".into(),
            "elf".into(),
        ])
        .expect("valid image options");
        assert_eq!(
            options.build_args,
            vec![
                "--platform".to_string(),
                "qemu-loongarch64".to_string(),
                "--reuse-modules".to_string(),
            ]
        );
        assert!(
            !BuildOptions::parse(&options.build_args)
                .expect("forwarded build options")
                .refresh_modules()
        );
    }

    #[test]
    fn reused_module_artifacts_must_be_complete() {
        let directory = std::env::temp_dir().join(format!(
            "hitoshizuku-xtask-modules-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let output = directory.join("modules");
        std::fs::create_dir_all(&output).expect("create module output");

        let missing_manifest = ModuleArtifacts::load(&directory, "modules")
            .err()
            .expect("missing manifest must fail");
        assert!(missing_manifest.contains("rebuild without --reuse-modules"));

        std::fs::write(output.join("modules.manifest"), "ELM-BUILD-MODULES-V1\n")
            .expect("write manifest");
        std::fs::write(output.join("integrated.archives"), "\n").expect("write empty archive list");
        let empty_archives = ModuleArtifacts::load(&directory, "modules")
            .err()
            .expect("empty archive list must fail");
        assert!(empty_archives.contains("is empty"));

        let archive = output.join("libintegrated.a");
        std::fs::write(&archive, b"archive").expect("write archive");
        std::fs::write(
            output.join("integrated.archives"),
            format!("{}\n", archive.display()),
        )
        .expect("write archive list");
        let artifacts =
            ModuleArtifacts::load(&directory, "modules").expect("complete artifacts load");
        assert_eq!(artifacts.manifest, output.join("modules.manifest"));
        assert_eq!(
            env::split_paths(&artifacts.archives).collect::<Vec<_>>(),
            vec![archive]
        );

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn image_defaults_follow_platform_recipe() {
        let options =
            ImageOptions::parse(&["--platform".into(), "ls2k1000".into(), "--no-build".into()])
                .expect("valid image options");
        let platform = catalog().get("ls2k1000").expect("LS2K1000").clone();
        assert!(options.no_build);
        assert_eq!(
            options.formats(&platform).expect("default formats"),
            vec![ImageFormat::Elf, ImageFormat::Raw, ImageFormat::Uimage]
        );
        assert_eq!(
            options.build_args,
            vec!["--platform".to_string(), "ls2k1000".to_string()]
        );
    }

    #[test]
    fn qemu_rejects_non_elf_image_formats() {
        let options = ImageOptions::parse(&[
            "--platform".into(),
            "qemu-riscv64".into(),
            "--format".into(),
            "raw".into(),
        ])
        .expect("valid image options");
        let platform = catalog().get("qemu-riscv64").expect("RISC-V QEMU").clone();
        assert!(options.formats(&platform).is_err());
    }

    #[test]
    fn mkimage_arguments_use_physical_load_and_entry() {
        let catalog = catalog();
        let platform = catalog.get("visionfive2").expect("VisionFive 2");
        let args = mkimage_args(
            platform.image.uimage.as_ref().expect("uImage recipe"),
            platform.link.physical_base.get(),
            Path::new("kernel.bin"),
            Path::new("uImage"),
        );
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args[0..4], ["-A", "riscv", "-O", "linux"]);
        assert!(args.windows(2).any(|pair| pair == ["-a", "0x80200000"]));
        assert!(args.windows(2).any(|pair| pair == ["-e", "0x80200000"]));

        let ls2k = catalog.get("ls2k1000").expect("LS2K1000");
        let args = mkimage_args(
            ls2k.image.uimage.as_ref().expect("uImage recipe"),
            ls2k.link.physical_base.get(),
            Path::new("kernel.bin"),
            Path::new("uImage"),
        );
        assert_eq!(args[1], "loongarch");
    }

    #[test]
    fn crc32_matches_the_standard_test_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn validates_legacy_uimage_header_payload_and_crc() {
        let directory = std::env::temp_dir().join(format!(
            "hitoshizuku-xtask-uimage-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).expect("create test directory");
        let raw = directory.join("kernel.bin");
        let image = directory.join("uImage");
        let payload = b"test kernel payload";
        std::fs::write(&raw, payload).expect("write raw payload");

        let catalog = catalog();
        let spec = catalog
            .get("visionfive2")
            .expect("VisionFive 2")
            .image
            .uimage
            .as_ref()
            .expect("uImage recipe");
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(&0x2705_1956u32.to_be_bytes());
        bytes[12..16].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes[16..20].copy_from_slice(&0x8020_0000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&0x8020_0000u32.to_be_bytes());
        bytes[24..28].copy_from_slice(&crc32(payload).to_be_bytes());
        bytes[28..32].copy_from_slice(&spec.legacy_header_ids().expect("header ids"));
        bytes[32..32 + spec.name.len()].copy_from_slice(spec.name.as_bytes());
        let header_crc = crc32(&bytes);
        bytes[4..8].copy_from_slice(&header_crc.to_be_bytes());
        bytes.extend_from_slice(payload);
        std::fs::write(&image, &bytes).expect("write uImage");

        validate_uimage(&image, &raw, 0x8020_0000, spec).expect("valid uImage");

        for index in [28, 29, 30, 31, 32] {
            let mut corrupted = bytes.clone();
            corrupted[index] ^= 1;
            corrupted[4..8].fill(0);
            let header_crc = crc32(&corrupted[..64]);
            corrupted[4..8].copy_from_slice(&header_crc.to_be_bytes());
            std::fs::write(&image, &corrupted).expect("write invalid uImage metadata");
            let error = validate_uimage(&image, &raw, 0x8020_0000, spec)
                .expect_err("invalid metadata must fail");
            assert!(error.contains("metadata"));
        }

        bytes[64] ^= 1;
        std::fs::write(&image, &bytes).expect("corrupt uImage payload");
        assert!(validate_uimage(&image, &raw, 0x8020_0000, spec).is_err());

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }
}
