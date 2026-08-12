fn main() {
    println!("cargo:rerun-if-changed=src/c/efi_types.h");
    println!("cargo:rerun-if-changed=src/c/efi_global.h");
    println!("cargo:rerun-if-changed=src/c/efi_global.c");
    println!("cargo:rerun-if-changed=src/c/efi_boot.h");
    println!("cargo:rerun-if-changed=src/c/efi_boot.c");
    println!("cargo:rerun-if-changed=src/c/efi_guids.c");

    let mut build = cc::Build::new();
    build
        .file("src/c/efi_global.c")
        .file("src/c/efi_boot.c")
        .file("src/c/efi_guids.c")
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-PIC")
        .flag("-fno-builtin")
        .flag("-fno-tree-vectorize")
        .flag_if_supported("-mno-lsx")
        .flag_if_supported("-mno-lasx")
        .opt_level(2)
        .warnings(false);

    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("riscv64") {
        // Rust 的 riscv64gc target 使用双精度浮点 ABI，C 对象必须携带相同 ELF 标志。
        build
            .flag("-march=rv64gc")
            .flag("-mabi=lp64d")
            .flag("-mcmodel=medany");
    }

    build.compile("efi_c");
}
