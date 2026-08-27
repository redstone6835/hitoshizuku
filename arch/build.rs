//! 架构 crate 的平台构建选择。

use std::collections::BTreeSet;
use std::path::Path;

use xtask::{CATALOG_RELATIVE_PATH, PlatformCatalog};

const PLATFORM_ENV: &str = "HITOSHIZUKU_PLATFORM";

fn main() {
    println!("cargo:rerun-if-env-changed=MYGO_BOARD_DEBUG_UART");
    println!("cargo:rerun-if-env-changed={PLATFORM_ENV}");
    println!("cargo::rustc-check-cfg=cfg(mygo_board_debug_uart)");

    if std::env::var_os("MYGO_BOARD_DEBUG_UART").is_some_and(|value| value == "1") {
        println!("cargo::rustc-cfg=mygo_board_debug_uart");
    }

    let manifest_dir =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo did not set CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest_dir)
        .parent()
        .expect("arch crate must be inside the repository root");
    let catalog_path = root.join(CATALOG_RELATIVE_PATH);
    println!("cargo:rerun-if-changed={}", catalog_path.display());
    let catalog = PlatformCatalog::load(&catalog_path).unwrap_or_else(|error| {
        panic!(
            "load platform catalog {} for arch crate: {error}",
            catalog_path.display()
        )
    });

    let known_cfgs = catalog
        .platforms()
        .iter()
        .flat_map(|platform| platform.rust_cfg.iter())
        .collect::<BTreeSet<_>>();
    for cfg in known_cfgs {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }

    let target = std::env::var("TARGET").expect("Cargo did not set TARGET");
    let platform_id = std::env::var(PLATFORM_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    let target_is_supported = catalog
        .platforms()
        .iter()
        .any(|platform| platform.target == target);
    if !target_is_supported && platform_id.is_none() {
        return;
    }
    let platform = catalog
        .select_for_build(platform_id.as_deref(), &target)
        .unwrap_or_else(|error| panic!("select platform for arch crate: {error}"));
    for cfg in &platform.rust_cfg {
        println!("cargo::rustc-cfg={cfg}");
    }
}
