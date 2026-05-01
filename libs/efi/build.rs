fn main() {
    println!("cargo:rerun-if-changed=src/c/efi_types.h");
    println!("cargo:rerun-if-changed=src/c/efi_global.h");
    println!("cargo:rerun-if-changed=src/c/efi_global.c");
    println!("cargo:rerun-if-changed=src/c/efi_boot.h");
    println!("cargo:rerun-if-changed=src/c/efi_boot.c");
    println!("cargo:rerun-if-changed=src/c/efi_guids.c");

    cc::Build::new()
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
        .flag("-Wall")
        .compile("efi_c");
}
