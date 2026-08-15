#![cfg(feature = "alloc")]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use fdt::{Fdt, OwnedTree, Tree};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mygo-fdt-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SemanticTree {
    reservations: Vec<(u64, u64)>,
    nodes: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
}

fn semantic_tree(blob: &[u8]) -> SemanticTree {
    let tree = Tree::parse(blob).unwrap();
    let nodes = tree
        .node_ids()
        .map(|node_id| {
            let path = tree.path(node_id).unwrap();
            let properties = tree
                .node(node_id)
                .unwrap()
                .properties()
                .map(|property| (property.name().to_string(), property.value().to_vec()))
                .collect();
            (path, properties)
        })
        .collect();
    let reservations = tree
        .fdt()
        .reservations()
        .map(|entry| (entry.address, entry.size))
        .collect();
    SemanticTree {
        reservations,
        nodes,
    }
}

fn command_available(program: &str, version_argument: &str) -> bool {
    Command::new(program)
        .arg(version_argument)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn compile_dts(source: &[u8], symbols: bool) -> Vec<u8> {
    let mut command = Command::new("dtc");
    command.args(["-q", "-I", "dts", "-O", "dtb", "-o", "-", "-"]);
    if symbols {
        command.arg("-@");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(source).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "dtc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn write_blob(path: &Path, blob: &[u8]) {
    fs::write(path, blob).unwrap();
}

fn build_libfdt_checker(directory: &TempDirectory) -> Option<PathBuf> {
    if !command_available("cc", "--version") {
        return None;
    }
    let source = directory.path("check.c");
    let executable = directory.path("check");
    fs::write(
        &source,
        br#"#include <libfdt.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    FILE *file;
    long length;
    void *blob;
    int result;

    if (argc != 2 || !(file = fopen(argv[1], "rb")))
        return 2;
    if (fseek(file, 0, SEEK_END) || (length = ftell(file)) < 0 ||
        fseek(file, 0, SEEK_SET))
        return 2;
    blob = malloc((size_t)length);
    if (!blob || fread(blob, 1, (size_t)length, file) != (size_t)length)
        return 2;
    fclose(file);
    result = fdt_check_full(blob, (size_t)length);
    free(blob);
    return result == 0 ? 0 : 1;
}
"#,
    )
    .unwrap();
    let output = Command::new("cc")
        .arg(&source)
        .args(["-lfdt", "-o"])
        .arg(&executable)
        .output()
        .ok()?;
    output.status.success().then_some(executable)
}

fn libfdt_accepts(checker: &Path, directory: &TempDirectory, name: &str, blob: &[u8]) -> bool {
    let path = directory.path(name);
    write_blob(&path, blob);
    Command::new(checker).arg(path).status().unwrap().success()
}

#[test]
fn canonical_owned_output_round_trips_through_dtc_and_libfdt() {
    if !command_available("dtc", "--version") {
        return;
    }
    let directory = TempDirectory::new("owned");
    let checker = build_libfdt_checker(&directory);
    let input = compile_dts(
        br#"/dts-v1/;
/memreserve/ 0x12340000 0x2000;

/ {
    compatible = "test,conformance", "test,fallback";
    #address-cells = <2>;
    #size-cells = <2>;

    bus@0 {
        compatible = "simple-bus";
        #address-cells = <1>;
        #size-cells = <1>;
        ranges = <0 0 0x10000000 0x10000>;

        device@20 {
            reg = <0x20 0x10>;
            bytes = [00 ff 7f 80];
            empty;
        };
    };
};
"#,
        false,
    );
    let encoded = OwnedTree::parse(&input).unwrap().to_dtb().unwrap();

    assert_eq!(Fdt::parse(&encoded).unwrap().header().version, 17);
    assert_eq!(semantic_tree(&encoded), semantic_tree(&input));
    if let Some(checker) = checker.as_ref() {
        assert!(libfdt_accepts(
            checker,
            &directory,
            "canonical.dtb",
            &encoded
        ));
    }

    let encoded_path = directory.path("canonical-dtc.dtb");
    write_blob(&encoded_path, &encoded);
    let output = Command::new("dtc")
        .args(["-q", "-I", "dtb", "-O", "dts", "-o", "-"])
        .arg(encoded_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "dtc rejected OwnedTree output: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn overlay_result_matches_fdtoverlay_semantics() {
    if !command_available("dtc", "--version") || !command_available("fdtoverlay", "--version") {
        return;
    }
    let directory = TempDirectory::new("overlay");
    let base = compile_dts(
        br#"/dts-v1/;
/memreserve/ 0x1000 0x100;

/ {
    compatible = "test,overlay-base";
    #address-cells = <1>;
    #size-cells = <1>;

    clock0: clock@1000 {
        compatible = "fixed-clock";
        #clock-cells = <1>;
        reg = <0x1000 0x100>;
    };

    bus0: bus@2000 {
        compatible = "simple-bus";
        #address-cells = <1>;
        #size-cells = <1>;
        ranges = <0 0x2000 0x100>;
        status = "disabled";
    };
};
"#,
        true,
    );
    let overlay = compile_dts(
        br#"/dts-v1/;
/plugin/;
/memreserve/ 0x2000 0x100;

/ {
    fragment@0 {
        target = <&bus0>;
        __overlay__ {
            status = "okay";

            producer: device@10 {
                reg = <0x10 0x10>;
                clocks = <&clock0 3>;
            };

            consumer@20 {
                reg = <0x20 0x10>;
                peer = <&producer>;
            };
        };
    };

    fragment@1 {
        target-path = "/";
        __overlay__ {
            top@3000 {
                compatible = "test,top";
                reg = <0x3000 0x100>;
                link = <&producer>;
            };
        };
    };
};
"#,
        true,
    );

    let base_path = directory.path("base.dtb");
    let overlay_path = directory.path("overlay.dtbo");
    let expected_path = directory.path("expected.dtb");
    write_blob(&base_path, &base);
    write_blob(&overlay_path, &overlay);
    let output = Command::new("fdtoverlay")
        .args(["-i"])
        .arg(&base_path)
        .args(["-o"])
        .arg(&expected_path)
        .arg(&overlay_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fdtoverlay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fs::read(expected_path).unwrap();

    let mut actual = OwnedTree::parse(&base).unwrap();
    actual.apply_overlay_blob(&overlay).unwrap();
    let actual = actual.to_dtb().unwrap();
    assert_eq!(semantic_tree(&actual), semantic_tree(&expected));
}

#[test]
fn malformed_layouts_agree_with_libfdt_full_validation() {
    if !command_available("dtc", "--version") {
        return;
    }
    let directory = TempDirectory::new("malformed");
    let Some(checker) = build_libfdt_checker(&directory) else {
        return;
    };
    let valid = compile_dts(
        br#"/dts-v1/;
/ {
    compatible = "test,malformed";
    child@0 { reg = <0>; };
};
"#,
        false,
    );
    assert!(Fdt::parse(&valid).is_ok());
    assert!(libfdt_accepts(&checker, &directory, "valid.dtb", &valid));

    let mut cases = Vec::new();

    let mut oversized = valid.clone();
    let declared = u32::from_be_bytes(oversized[4..8].try_into().unwrap());
    oversized[4..8].copy_from_slice(&declared.saturating_add(4).to_be_bytes());
    cases.push(("oversized.dtb", oversized));

    // DTSpec 要求 reservation block 8 字节对齐；libfdt 在允许非对齐访问的
    // 主机上有意保持宽松，因此这里只断言规范解析器拒绝，不把它列入差分集合。
    let mut misaligned_reservations = valid.clone();
    let reservations = u32::from_be_bytes(misaligned_reservations[16..20].try_into().unwrap());
    misaligned_reservations[16..20].copy_from_slice(&(reservations + 1).to_be_bytes());
    assert!(Fdt::parse(&misaligned_reservations).is_err());

    let mut bad_structure_token = valid.clone();
    let structure = u32::from_be_bytes(bad_structure_token[8..12].try_into().unwrap()) as usize;
    bad_structure_token[structure..structure + 4].copy_from_slice(&8u32.to_be_bytes());
    cases.push(("bad-token.dtb", bad_structure_token));

    let mut unterminated_reservations = valid.clone();
    let reservations =
        u32::from_be_bytes(unterminated_reservations[16..20].try_into().unwrap()) as usize;
    unterminated_reservations[reservations..reservations + 16].fill(0xff);
    cases.push(("unterminated-reservations.dtb", unterminated_reservations));

    for (name, blob) in cases {
        assert!(Fdt::parse(&blob).is_err(), "parser accepted {name}");
        assert!(
            !libfdt_accepts(&checker, &directory, name, &blob),
            "libfdt accepted {name}"
        );
    }
}
