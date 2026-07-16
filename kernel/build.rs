//! 内核构建脚本：校验并打包当前目标架构的 initramfs。
//!
//! 用户态 rootfs 的生成由顶层 Makefile 编排；这里仅把 Cargo 当前目标需要的
//! rootfs 打包为架构专属 CPIO，并把路径暴露给内核源码的 `include_bytes!`。

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

fn main() {
    let target = std::env::var("TARGET").expect("Cargo 未设置 TARGET 环境变量");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo 未设置 OUT_DIR"));

    generate_elm_trust_anchors(&root, &out_dir);
    generate_elm_build_bound(&root, &out_dir, &target);
    link_integrated_components();

    let initramfs_root = env_path("INITRAMFS_ROOT", &root).unwrap_or_else(|| {
        root.join(
            default_rootfs_dir(&target)
                .unwrap_or_else(|| panic!("不支持的目标 {target}，必须显式设置 INITRAMFS_ROOT")),
        )
    });
    let initramfs_cpio = env_path("INITRAMFS_CPIO", &root).unwrap_or_else(|| {
        root.join(
            default_initramfs_cpio(&target)
                .unwrap_or_else(|| panic!("不支持的目标 {target}，必须显式设置 INITRAMFS_CPIO")),
        )
    });

    pack_initramfs(&initramfs_root, &initramfs_cpio);

    println!("cargo:rerun-if-env-changed=INITRAMFS_ROOT");
    println!("cargo:rerun-if-env-changed=INITRAMFS_CPIO");
    println!("cargo:rerun-if-env-changed=ELM_TRUST_ANCHORS_FILE");
    println!("cargo:rerun-if-env-changed=ELM_BUILD_BOUND_MANIFEST");
    emit_initramfs_rerun_inputs(&initramfs_root);
    println!(
        "cargo:rustc-env=MYGO_INITRAMFS_CPIO={}",
        initramfs_cpio.display()
    );
    println!("cargo:rustc-env=ELM_TARGET_TRIPLE={target}");
    println!("cargo:rustc-env=ELM_RUSTC_VERSION={}", rustc_version_line());
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

fn default_rootfs_dir(target: &str) -> Option<&'static str> {
    if target.starts_with("loongarch64") {
        Some("userland/rootfs-la")
    } else if target.starts_with("riscv64") {
        Some("userland/rootfs-rv")
    } else {
        None
    }
}

fn default_initramfs_cpio(target: &str) -> Option<&'static str> {
    if target.starts_with("loongarch64") {
        Some("build/initramfs-la.cpio")
    } else if target.starts_with("riscv64") {
        Some("build/initramfs-rv.cpio")
    } else {
        None
    }
}

fn pack_initramfs(src: &Path, out_cpio: &Path) {
    if !src.is_dir() {
        panic!("initramfs 根目录不存在或不是目录：{src:?}");
    }
    let parent = out_cpio
        .parent()
        .unwrap_or_else(|| panic!("initramfs 输出路径没有父目录：{out_cpio:?}"));
    std::fs::create_dir_all(parent)
        .unwrap_or_else(|err| panic!("创建 initramfs 输出目录 {parent:?} 失败：{err}"));

    run_cmd(Command::new("sh").arg("-c").arg(&format!(
        "cd {} && find . -print0 | cpio --quiet -o -0 -H newc > {}",
        shell_quote(src),
        shell_quote(out_cpio)
    )));
}

fn emit_initramfs_rerun_inputs(root: &Path) {
    // Cargo 对目录的 rerun-if-changed 不是递归语义。initramfs 里 rcS、
    // busybox applet 链接等任一文件变更都必须触发重新打包，否则内嵌
    // cpio 会继续使用旧内容，启动行为和源码工作区不一致。
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.is_dir() {
            continue;
        }
        let mut entries = std::fs::read_dir(&path)
            .unwrap_or_else(|err| panic!("读取 initramfs 目录 {path:?} 失败：{err}"))
            .map(|entry| {
                entry.unwrap_or_else(|err| panic!("读取 initramfs 目录项 {path:?} 失败：{err}"))
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries.into_iter().rev() {
            stack.push(entry.path());
        }
    }
}

fn shell_quote(path: &Path) -> String {
    let raw = path.as_os_str().to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn run_cmd(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("启动命令 {cmd:?} 失败：{e}"));
    assert!(status.success(), "命令 {cmd:?} 退出状态异常：{status:?}");
}
