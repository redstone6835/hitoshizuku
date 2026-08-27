//! 内核构建脚本。
//!
//! 本脚本只校验并嵌入调用方已经准备好的 initramfs，不负责构建任何用户态内容。

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use xtask::{CATALOG_RELATIVE_PATH, LinkLayout, PlatformCatalog, PlatformSpec};

const HITOSHIZUKU_PLATFORM_ENV: &str = "HITOSHIZUKU_PLATFORM";

fn main() {
    let target = std::env::var("TARGET").expect("Cargo 未设置 TARGET 环境变量");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo 未设置 OUT_DIR"));

    generate_elm_trust_anchors(&root, &out_dir);
    generate_elm_build_bound(&root, &out_dir, &target);
    generate_soyo_trust_policy(&root, &out_dir);
    link_integrated_components();
    configure_kernel_linker(&root, &target);

    println!("cargo:rerun-if-env-changed=INITRAMFS");
    println!("cargo:rerun-if-env-changed=ELM_TRUST_ANCHORS_FILE");
    println!("cargo:rerun-if-env-changed=ELM_BUILD_BOUND_MANIFEST");
    println!("cargo:rerun-if-env-changed=SOYO_TRUST_POLICY_FILE");
    if std::env::var_os("CARGO_FEATURE_EMBEDDED_INITRAMFS").is_some() {
        let initramfs = env_path("INITRAMFS", &root)
            .unwrap_or_else(|| panic!("启用 embedded-initramfs 时必须设置 INITRAMFS"));
        validate_initramfs(&initramfs);
        println!("cargo:rerun-if-changed={}", initramfs.display());
        println!(
            "cargo:rustc-env=MYGO_INITRAMFS_CPIO={}",
            initramfs.display()
        );
    }
    println!("cargo:rustc-env=ELM_TARGET_TRIPLE={target}");
    println!("cargo:rustc-env=ELM_RUSTC_VERSION={}", rustc_version_line());
}

/// 选择架构级规范链接脚本，并从平台目录注入该板卡的地址布局。
///
/// Cargo 产物始终是带符号 ELF；raw/uImage 由 xtask 在链接完成后派生。链接脚本不再
/// 包含板卡地址，也不再生成 debug 或板级变体。
fn configure_kernel_linker(root: &Path, target: &str) {
    let expected_layout = match target {
        "loongarch64-unknown-none" => LinkLayout::Loongarch64Dmw1,
        "riscv64gc-unknown-none-elf" => LinkLayout::Riscv64Sv48,
        _ => return,
    };

    println!("cargo:rerun-if-env-changed={HITOSHIZUKU_PLATFORM_ENV}");
    let catalog_path = root.join(CATALOG_RELATIVE_PATH);
    println!("cargo:rerun-if-changed={}", catalog_path.display());
    let catalog = PlatformCatalog::load(&catalog_path)
        .unwrap_or_else(|error| panic!("加载平台目录 {} 失败：{error}", catalog_path.display()));
    let platform_id = std::env::var_os(HITOSHIZUKU_PLATFORM_ENV).map(|value| {
        value
            .into_string()
            .unwrap_or_else(|_| panic!("{HITOSHIZUKU_PLATFORM_ENV} 必须是有效的 UTF-8 平台标识"))
    });
    let platform = catalog
        .select_for_build(platform_id.as_deref(), target)
        .unwrap_or_else(|error| panic!("选择内核链接平台失败：{error}"));
    validate_link_platform(platform, target, expected_layout);

    let linker_dir = root.join("kernel/linker");
    let script = match expected_layout {
        LinkLayout::Loongarch64Dmw1 => linker_dir.join("loongarch64.ld"),
        LinkLayout::Riscv64Sv48 => linker_dir.join("riscv64.ld"),
    };
    for source in [
        script.clone(),
        linker_dir.join("common-rodata.ld"),
        linker_dir.join("common-debug.ld"),
    ] {
        println!("cargo:rerun-if-changed={}", source.display());
    }

    println!("cargo:rustc-link-arg-bin=kernel=-L{}", linker_dir.display());
    println!(
        "cargo:rustc-link-arg-bin=kernel=--defsym=KERNEL_PHYS_BASE={:#x}",
        platform.link.physical_base.get()
    );
    println!(
        "cargo:rustc-link-arg-bin=kernel=--defsym=KERNEL_VIRT_BASE={:#x}",
        platform.link.virtual_base.get()
    );
    println!(
        "cargo:rustc-link-arg-bin=kernel=--defsym=HITOSHIZUKU_PLATFORM_TAG={:#x}",
        platform.identity_tag()
    );
    println!("cargo:rustc-link-arg-bin=kernel=-T{}", script.display());
}

fn validate_link_platform(platform: &PlatformSpec, target: &str, expected_layout: LinkLayout) {
    assert_eq!(
        platform.target, target,
        "平台 {} 的目标与 Cargo TARGET 不一致",
        platform.id
    );
    assert_eq!(
        platform.link.layout, expected_layout,
        "平台 {} 的链接布局与目标架构不一致",
        platform.id
    );

    let physical = platform.link.physical_base.get();
    let virtual_address = platform.link.virtual_base.get();
    let alignment = platform.link.alignment.get();
    assert!(
        alignment >= 0x1000 && alignment.is_power_of_two(),
        "平台 {} 的链接对齐必须是至少 4 KiB 的 2 的幂",
        platform.id
    );
    assert_eq!(
        physical % alignment,
        0,
        "平台 {} 的物理基址未满足链接对齐",
        platform.id
    );
    assert_eq!(
        virtual_address % alignment,
        0,
        "平台 {} 的虚拟基址未满足链接对齐",
        platform.id
    );

    let expected_virtual = match expected_layout {
        LinkLayout::Loongarch64Dmw1 => {
            assert!(
                physical < (1 << 60),
                "平台 {} 的物理基址超出 LoongArch DMW1",
                platform.id
            );
            0x9000_0000_0000_0000 | physical
        }
        LinkLayout::Riscv64Sv48 => {
            assert!(
                physical < 0x80_0000_0000,
                "平台 {} 的物理基址超出 RISC-V Sv48 内核窗口",
                platform.id
            );
            0xffff_ff80_0000_0000 | physical
        }
    };
    assert_eq!(
        virtual_address, expected_virtual,
        "平台 {} 的虚拟基址不是物理基址的规范内核映射",
        platform.id
    );
}

struct ConfiguredBuildBoundModule {
    order: u32,
    name: String,
    file_name: String,
    provider_id: u64,
    eki_hash: [u8; 32],
    ebi_hash: [u8; 32],
    capabilities: u64,
}

fn generate_elm_build_bound(root: &Path, out_dir: &Path, target: &str) {
    let configured_path = std::env::var_os("ELM_BUILD_BOUND_MANIFEST")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        });
    let (manifest_hash, profile_hash, modules) = match configured_path {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            parse_elm_build_bound_manifest(&path, target)
        }
        None => ([0; 32], [0; 32], Vec::new()),
    };

    let mut generated = String::new();
    generated.push_str("pub(super) const CONFIGURED_ELM_BUILD_MANIFEST_SHA256: [u8; 32] = ");
    write_rust_byte_array(&mut generated, &manifest_hash);
    generated.push_str(";\n");
    generated.push_str("pub(super) const CONFIGURED_ELM_BUILD_PROFILE_SHA256: [u8; 32] = ");
    write_rust_byte_array(&mut generated, &profile_hash);
    generated.push_str(";\n");
    generated.push_str(
        "pub(super) const CONFIGURED_ELM_BUILD_BOUND_MODULES: &[ElmBuildBoundRecord] = &[\n",
    );
    for module in modules {
        write!(
            generated,
            "    ElmBuildBoundRecord::new({}, {:?}, {:?}, {}, ",
            module.order, module.name, module.file_name, module.provider_id
        )
        .unwrap();
        write_rust_byte_array(&mut generated, &module.eki_hash);
        generated.push_str(", ");
        write_rust_byte_array(&mut generated, &module.ebi_hash);
        writeln!(generated, ", 0x{:016x}),", module.capabilities).unwrap();
    }
    generated.push_str("];\n");
    std::fs::write(out_dir.join("elm_build_bound.rs"), generated)
        .unwrap_or_else(|err| panic!("写入 ELM BuildBound 生成文件失败：{err}"));
}

fn parse_elm_build_bound_manifest(
    path: &Path,
    expected_target: &str,
) -> ([u8; 32], [u8; 32], Vec<ConfiguredBuildBoundModule>) {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|err| panic!("读取 ELM BuildBound 清单 {path:?} 失败：{err}"));
    let input = std::str::from_utf8(&bytes)
        .unwrap_or_else(|_| panic!("ELM BuildBound 清单 {path:?} 不是 UTF-8"));
    let manifest_hash: [u8; 32] = Sha256::digest(&bytes).into();
    let mut lines = input.lines();
    assert_eq!(
        lines.next(),
        Some("ELM-BUILD-MODULES-V1"),
        "ELM BuildBound 清单版本错误"
    );
    let target = lines
        .next()
        .and_then(|line| line.strip_prefix("target="))
        .unwrap_or_else(|| panic!("ELM BuildBound 清单缺少 target"));
    assert_eq!(target, expected_target, "ELM BuildBound 清单目标不匹配");
    let _profile = lines
        .next()
        .and_then(|line| line.strip_prefix("profile="))
        .unwrap_or_else(|| panic!("ELM BuildBound 清单缺少 profile"));
    let profile_hash = lines
        .next()
        .and_then(|line| line.strip_prefix("profile_sha256="))
        .and_then(parse_hex32)
        .unwrap_or_else(|| panic!("ELM BuildBound 清单 profile_sha256 无效"));
    let module_count = lines
        .next()
        .and_then(|line| line.strip_prefix("module_count="))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("ELM BuildBound 清单 module_count 无效"));
    let mut modules = Vec::new();
    let mut names = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut last_order = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 8, "ELM BuildBound module 记录字段数错误");
        assert_eq!(fields[0], "module", "ELM BuildBound 包含未知记录");
        let order = fields[1]
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("ELM BuildBound order 无效"));
        assert!(
            last_order.is_none_or(|previous| order > previous),
            "ELM BuildBound module 顺序必须严格递增"
        );
        last_order = Some(order);
        let name = fields[2];
        let file_name = fields[3];
        assert!(
            valid_build_identifier(name),
            "ELM BuildBound module 名称无效"
        );
        assert!(
            valid_build_file_name(file_name),
            "ELM BuildBound module 文件名无效"
        );
        assert!(
            names.insert(name.to_string()),
            "ELM BuildBound module 名称重复"
        );
        assert!(
            files.insert(file_name.to_string()),
            "ELM BuildBound 文件名重复"
        );
        let provider_id = fields[4]
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("ELM BuildBound provider id 无效"));
        assert_ne!(provider_id, 0, "ELM BuildBound provider id 不能为零");
        let eki_hash =
            parse_hex32(fields[5]).unwrap_or_else(|| panic!("ELM BuildBound EKI 摘要无效"));
        let ebi_hash =
            parse_hex32(fields[6]).unwrap_or_else(|| panic!("ELM BuildBound EBI 摘要无效"));
        let capabilities = fields[7]
            .strip_prefix("0x")
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .unwrap_or_else(|| panic!("ELM BuildBound capability mask 无效"));
        modules.push(ConfiguredBuildBoundModule {
            order,
            name: name.to_string(),
            file_name: file_name.to_string(),
            provider_id,
            eki_hash,
            ebi_hash,
            capabilities,
        });
    }
    assert_eq!(
        modules.len(),
        module_count,
        "ELM BuildBound module_count 不匹配"
    );
    (manifest_hash, profile_hash, modules)
}

fn valid_build_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn valid_build_file_name(value: &str) -> bool {
    valid_build_identifier(value) && value.ends_with(".eki")
}

fn parse_hex32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut output = [0; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(output)
}

fn write_rust_byte_array(output: &mut String, bytes: &[u8; 32]) {
    output.push('[');
    for (index, byte) in bytes.iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        write!(output, "0x{byte:02x}").unwrap();
    }
    output.push(']');
}

fn generate_soyo_trust_policy(root: &Path, out_dir: &Path) {
    let configured_path = std::env::var_os("SOYO_TRUST_POLICY_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        });
    let (allow_unsigned, trusted, revoked, rejected) = match configured_path {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            parse_soyo_trust_policy(&path)
        }
        None => (true, Vec::new(), Vec::new(), Vec::new()),
    };

    let mut generated = String::new();
    writeln!(
        generated,
        "pub(super) const CONFIGURED_SOYO_ALLOW_UNSIGNED: bool = {allow_unsigned};"
    )
    .unwrap();
    generated.push_str(
        "pub(super) const CONFIGURED_SOYO_TRUSTED_KEYS: &[soyo::TrustedPublicKey] = &[\n",
    );
    for public_key in trusted {
        let key_id: [u8; 32] = Sha256::digest(public_key).into();
        generated.push_str("    soyo::TrustedPublicKey { key_id: ");
        write_rust_byte_array(&mut generated, &key_id);
        generated.push_str(", public_key: ");
        write_rust_byte_array(&mut generated, &public_key);
        generated.push_str(" },\n");
    }
    generated.push_str("];\n");
    generated.push_str("pub(super) const CONFIGURED_SOYO_REVOKED_KEYS: &[[u8; 32]] = &[\n");
    for key_id in revoked {
        generated.push_str("    ");
        write_rust_byte_array(&mut generated, &key_id);
        generated.push_str(",\n");
    }
    generated.push_str("];\n");
    generated.push_str("pub(super) const CONFIGURED_SOYO_REJECTED_HASHES: &[[u8; 32]] = &[\n");
    for content_hash in rejected {
        generated.push_str("    ");
        write_rust_byte_array(&mut generated, &content_hash);
        generated.push_str(",\n");
    }
    generated.push_str("];\n");
    std::fs::write(out_dir.join("soyo_trust_policy.rs"), generated)
        .unwrap_or_else(|error| panic!("写入 SOYO 信任策略失败：{error}"));
}

fn parse_soyo_trust_policy(path: &Path) -> (bool, Vec<[u8; 32]>, Vec<[u8; 32]>, Vec<[u8; 32]>) {
    let input = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("读取 SOYO 信任策略 {path:?} 失败：{error}"));
    let mut allow_unsigned = None;
    let mut trusted = BTreeSet::new();
    let mut revoked = BTreeSet::new();
    let mut rejected = BTreeSet::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(
            fields.len(),
            2,
            "SOYO 信任策略 {path:?} 第 {line_number} 行必须包含动作和值"
        );
        match fields[0] {
            "allow-unsigned" => {
                let value = match fields[1] {
                    "true" => true,
                    "false" => false,
                    _ => panic!(
                        "SOYO 信任策略 {path:?} 第 {line_number} 行的 allow-unsigned 必须是 true 或 false"
                    ),
                };
                assert!(
                    allow_unsigned.replace(value).is_none(),
                    "SOYO 信任策略 {path:?} 重复声明 allow-unsigned"
                );
            }
            "key" => {
                let public_key = parse_hex32(fields[1]).unwrap_or_else(|| {
                    panic!("SOYO 信任策略 {path:?} 第 {line_number} 行的公钥不是 64 位 hex")
                });
                ed25519_dalek::VerifyingKey::from_bytes(&public_key).unwrap_or_else(|_| {
                    panic!("SOYO 信任策略 {path:?} 第 {line_number} 行不是有效 Ed25519 公钥")
                });
                assert!(
                    trusted.insert(public_key),
                    "SOYO 信任策略 {path:?} 第 {line_number} 行包含重复公钥"
                );
            }
            "revoke" => {
                let key_id = parse_hex32(fields[1]).unwrap_or_else(|| {
                    panic!("SOYO 信任策略 {path:?} 第 {line_number} 行的 key id 不是 64 位 hex")
                });
                assert!(
                    revoked.insert(key_id),
                    "SOYO 信任策略 {path:?} 第 {line_number} 行包含重复撤销项"
                );
            }
            "reject" => {
                let content_hash = parse_hex32(fields[1]).unwrap_or_else(|| {
                    panic!("SOYO 信任策略 {path:?} 第 {line_number} 行的内容摘要不是 64 位 hex")
                });
                assert!(
                    rejected.insert(content_hash),
                    "SOYO 信任策略 {path:?} 第 {line_number} 行包含重复回滚拒绝项"
                );
            }
            action => panic!("SOYO 信任策略 {path:?} 第 {line_number} 行包含未知动作 {action:?}"),
        }
    }
    (
        allow_unsigned.unwrap_or(true),
        trusted.into_iter().collect(),
        revoked.into_iter().collect(),
        rejected.into_iter().collect(),
    )
}

fn link_integrated_components() {
    println!("cargo:rerun-if-env-changed=ELM_INTEGRATED_ARCHIVES");
    let Some(value) = std::env::var_os("ELM_INTEGRATED_ARCHIVES") else {
        return;
    };
    let archives = std::env::split_paths(&value).collect::<Vec<_>>();
    assert!(
        !archives.is_empty(),
        "ELM_INTEGRATED_ARCHIVES 不得是空路径列表"
    );
    println!("cargo:rustc-link-arg-bin=kernel=--whole-archive");
    for archive in archives {
        let archive = archive
            .canonicalize()
            .unwrap_or_else(|error| panic!("定位集成组件归档 {archive:?} 失败：{error}"));
        assert!(archive.is_file(), "集成组件归档不是普通文件：{archive:?}");
        println!("cargo:rerun-if-changed={}", archive.display());
        println!("cargo:rustc-link-arg-bin=kernel={}", archive.display());
    }
    println!("cargo:rustc-link-arg-bin=kernel=--no-whole-archive");
}

struct ConfiguredTrustAnchor {
    identifier: String,
    rollback_authority_identifier: String,
    public_key: [u8; 32],
}

fn generate_elm_trust_anchors(root: &Path, out_dir: &Path) {
    let configured_path = std::env::var_os("ELM_TRUST_ANCHORS_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        });
    let anchors = match configured_path {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            parse_elm_trust_anchors(&path)
        }
        None => Vec::new(),
    };

    let mut generated = String::new();
    writeln!(
        generated,
        "pub(crate) const CONFIGURED_ELM_TRUST_ANCHORS: &[(&str, &str, [u8; 32])] = &["
    )
    .unwrap();
    for anchor in anchors {
        write!(
            generated,
            "    ({:?}, {:?}, [",
            anchor.identifier, anchor.rollback_authority_identifier
        )
        .unwrap();
        for (index, byte) in anchor.public_key.iter().enumerate() {
            if index != 0 {
                generated.push_str(", ");
            }
            write!(generated, "0x{byte:02x}").unwrap();
        }
        generated.push_str("]),\n");
    }
    generated.push_str("];\n");
    std::fs::write(out_dir.join("elm_trust_anchors.rs"), generated)
        .unwrap_or_else(|err| panic!("写入 ELM 信任根生成文件失败：{err}"));
}

fn parse_elm_trust_anchors(path: &Path) -> Vec<ConfiguredTrustAnchor> {
    let input = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("读取 ELM 信任根文件 {path:?} 失败：{err}"));
    let mut identifiers = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut anchors = Vec::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert!(
            matches!(fields.len(), 2 | 3),
            "ELM 信任根文件 {path:?} 第 {line_number} 行必须是 `<identifier> [rollback-authority-identifier] <ed25519-public-key-hex>`"
        );
        let identifier = fields[0];
        assert!(
            !identifier.is_empty()
                && identifier.len() <= 128
                && !identifier.as_bytes().contains(&0),
            "ELM 信任根文件 {path:?} 第 {line_number} 行的 identifier 无效"
        );
        assert!(
            identifiers.insert(identifier.to_string()),
            "ELM 信任根文件 {path:?} 第 {line_number} 行存在重复 identifier"
        );
        let rollback_authority_identifier = if fields.len() == 3 {
            fields[1]
        } else {
            identifier
        };
        assert!(
            !rollback_authority_identifier.is_empty()
                && rollback_authority_identifier.len() <= 128
                && !rollback_authority_identifier.as_bytes().contains(&0),
            "ELM 信任根文件 {path:?} 第 {line_number} 行的 rollback authority identifier 无效"
        );
        let public_key_field = fields[fields.len() - 1];
        let public_key = decode_public_key(public_key_field).unwrap_or_else(|| {
            panic!("ELM 信任根文件 {path:?} 第 {line_number} 行的公钥必须是 64 位十六进制")
        });
        ed25519_dalek::VerifyingKey::from_bytes(&public_key).unwrap_or_else(|_| {
            panic!("ELM 信任根文件 {path:?} 第 {line_number} 行不是有效 Ed25519 公钥")
        });
        assert!(
            keys.insert(public_key),
            "ELM 信任根文件 {path:?} 第 {line_number} 行存在重复公钥"
        );
        anchors.push(ConfiguredTrustAnchor {
            identifier: identifier.to_string(),
            rollback_authority_identifier: rollback_authority_identifier.to_string(),
            public_key,
        });
    }
    anchors
}

fn decode_public_key(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16).ok()?;
    }
    Some(out)
}

fn rustc_version_line() -> String {
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--version")
        .output()
        .unwrap_or_else(|err| panic!("读取 rustc 版本失败：{err}"));
    assert!(output.status.success(), "rustc --version 执行失败");
    String::from_utf8(output.stdout)
        .unwrap_or_else(|err| panic!("rustc 版本不是 UTF-8：{err}"))
        .trim()
        .to_string()
}

fn env_path(name: &str, root: &Path) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn validate_initramfs(path: &Path) {
    let bytes =
        std::fs::read(path).unwrap_or_else(|err| panic!("读取 initramfs {path:?} 失败：{err}"));
    assert!(!bytes.is_empty(), "initramfs 不能为空：{path:?}");
    assert!(
        bytes.starts_with(b"070701") || bytes.starts_with(b"070702"),
        "initramfs 不是 newc CPIO：{path:?}"
    );
    assert!(
        bytes
            .windows(b"TRAILER!!!".len())
            .any(|window| window == b"TRAILER!!!"),
        "initramfs 缺少 newc 结束记录：{path:?}"
    );
}
