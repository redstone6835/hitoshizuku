use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

const SCALARS: &[&str] = &[
    "HEADER_SIZE",
    "DIRECTORY_ENTRY_SIZE",
    "IMAGE_SEGMENT_SIZE",
    "ABI_IMPORT_SIZE",
    "CAPABILITY_REQUIREMENT_SIZE",
    "RELOCATION_SIZE",
    "RUNTIME_INFO_SIZE",
];

const MODULES: &[(&str, &[&str])] = &[
    (
        "header",
        &[
            "MAGIC",
            "FORMAT_VERSION",
            "HEADER_SIZE",
            "ARTIFACT_KIND",
            "TARGET_ARCH",
            "ENDIAN",
            "POINTER_WIDTH",
            "ABI_FAMILY",
            "ABI_EPOCH",
            "HASH_ALGORITHM",
            "FLAGS",
            "REQUIRED_FEATURES",
            "OPTIONAL_FEATURES",
            "ENTRY_OFFSET",
            "TABLE_OFFSET",
            "TABLE_COUNT",
            "TABLE_ENTRY_SIZE",
            "RESERVED0",
            "FILE_SIZE",
            "IMAGE_VIRTUAL_SIZE",
            "BUILD_ID",
            "CONTENT_HASH",
            "RESERVED1",
        ],
    ),
    (
        "directory",
        &[
            "TABLE_TYPE",
            "FLAGS",
            "ENTRY_SIZE",
            "ENTRY_COUNT",
            "RESERVED0",
            "FILE_OFFSET",
            "FILE_SIZE",
            "ALIGNMENT",
            "RESERVED1",
        ],
    ),
    (
        "image_segment",
        &[
            "KIND",
            "PERMISSIONS",
            "FLAGS",
            "VIRTUAL_OFFSET",
            "FILE_OFFSET",
            "FILE_SIZE",
            "MEMORY_SIZE",
            "ALIGNMENT",
            "RESERVED0",
            "RESERVED1",
        ],
    ),
    (
        "abi_import",
        &[
            "SLOT",
            "OPERATION_ID",
            "FLAGS",
            "DIAGNOSTIC_NAME_OFFSET",
            "SIGNATURE_HASH",
            "RESERVED",
        ],
    ),
    (
        "capability_requirement",
        &[
            "REQUIREMENT_ID",
            "OBJECT_INTERFACE",
            "FLAGS",
            "REQUIRED_RIGHTS",
            "DIAGNOSTIC_NAME_OFFSET",
            "RESERVED0",
            "RESERVED1",
        ],
    ),
    (
        "relocation",
        &[
            "KIND",
            "FLAGS",
            "TARGET_SEGMENT_INDEX",
            "TARGET_OFFSET",
            "SOURCE_SEGMENT_INDEX",
            "RESERVED0",
            "ADDEND",
            "RESERVED1",
            "RESERVED2",
        ],
    ),
    (
        "runtime_info",
        &[
            "STACK_SIZE",
            "STACK_GUARD_SIZE",
            "RUNTIME_FLAGS",
            "INIT_ARRAY_OFFSET",
            "INIT_ARRAY_COUNT",
            "INIT_ARRAY_ENTRY_SIZE",
            "RESERVED0",
            "FINI_ARRAY_OFFSET",
            "FINI_ARRAY_COUNT",
            "FINI_ARRAY_ENTRY_SIZE",
            "RESERVED1",
            "STACK_ALIGNMENT",
            "START_INFO_MAX_SIZE",
            "RESERVED2",
        ],
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=wire-abi.registry");
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let registry_path = manifest_dir.join("wire-abi.registry");
    let source = fs::read_to_string(&registry_path).expect("读取 SOYO Wire registry 失败");
    let values = parse_registry(&source);
    let generation = value(&values, "meta", "generation");

    let mut generated = String::new();
    writeln!(
        generated,
        "pub const WIRE_ABI_GENERATION: u32 = {generation};"
    )
    .unwrap();
    for name in SCALARS {
        writeln!(
            generated,
            "pub const {name}: usize = {};",
            value(&values, "scalar", name)
        )
        .unwrap();
    }
    for (module, fields) in MODULES {
        writeln!(generated, "pub mod {module} {{").unwrap();
        for field in *fields {
            writeln!(
                generated,
                "    pub const {field}: usize = {};",
                value(&values, module, field)
            )
            .unwrap();
        }
        writeln!(generated, "}}").unwrap();
    }

    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("wire_generated.rs");
    fs::write(output, generated).expect("写入生成的 SOYO Wire 常量失败");
}

fn parse_registry(source: &str) -> BTreeMap<(String, String), u64> {
    let mut section = String::new();
    let mut values = BTreeMap::new();
    for (line_number, raw_line) in source.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = name.trim().to_owned();
            continue;
        }
        let (key, raw_value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("SOYO Wire registry 第 {} 行缺少 '='", line_number + 1));
        let key = key.trim().to_owned();
        let raw_value = raw_value.trim();
        let value = if let Some(hex) = raw_value.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
        } else {
            raw_value.parse()
        }
        .unwrap_or_else(|_| panic!("SOYO Wire registry 第 {} 行数值无效", line_number + 1));
        if values.insert((section.clone(), key), value).is_some() {
            panic!("SOYO Wire registry 第 {} 行重复定义", line_number + 1);
        }
    }
    values
}

fn value(values: &BTreeMap<(String, String), u64>, section: &str, key: &str) -> u64 {
    *values
        .get(&(section.to_owned(), key.to_owned()))
        .unwrap_or_else(|| panic!("SOYO Wire registry 缺少 [{section}] {key}"))
}
