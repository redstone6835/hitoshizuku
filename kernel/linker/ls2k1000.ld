OUTPUT_FORMAT(binary)
OUTPUT_ARCH(loongarch64)
ENTRY(_start)

/* 龙芯 2K1000LA 开发板（LS2K1000-DP-FACTORY / BPI1001）fork U-Boot 装载基址。
   板载 U-Boot 2022.04（Loongson fork）经 bootm 传统镜像把内核解压到物理
   0x200000 后跳转（实测 "Load Address: 00200000"）。虚拟基址落在 DMW1
   直映窗口（0x9000_0000_0000_0000 起，映射物理 0 起）。 */
BASE_ADDRESS = 0x200000;
VIRTUAL_BASE_ADDRESS = 0x9000000002000000;
LOAD_OFFSET = VIRTUAL_BASE_ADDRESS - BASE_ADDRESS;

SECTIONS
{
    . = VIRTUAL_BASE_ADDRESS;
    skernel = .;

    /* 无 EFI/PE 头部：fork U-Boot 跳到装载基址，_start 必须是镜像首字节。
       启动 ABI 遵循 LoongArch Linux 协议：
       a0=efi_boot（U-Boot 直启为 0）、a1=cmdline 或 DTB、a2=EFI system table 或 DTB。 */
    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text) - LOAD_OFFSET) {
        KEEP(*(.text.entry.boot))
        KEEP(*(.text.entry))
        . = ALIGN(4K);
        KEEP(*(.text.trap.eentry))
        . = ALIGN(4K);
        KEEP(*(.text.trap.tlbr))
        . = ALIGN(4K);
        KEEP(*(.text.trap.merr))

        *(.text .text.*)
        . = ALIGN(4K);
        strampoline = .;
        *(.text.trampoline_user);
        *(.text.trampoline_kernel);
    }

    . = ALIGN(0x10000);
    etext = .;
    srodata = .;
    .rodata ALIGN(4K) : AT(ADDR(.rodata) - LOAD_OFFSET) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(8);
        PROVIDE(__ex_table_start = .);
        KEEP(*(__ex_table))
        PROVIDE(__ex_table_end = .);
        . = ALIGN(8);
        PROVIDE(__start_ktest = .);
        KEEP(*(.ktest .ktest.*))
        PROVIDE(__stop_ktest = .);
        . = ALIGN(8);
        PROVIDE(__elm_kernel_symbols_start = .);
        KEEP(*(.elm.kernel_symbols .elm.kernel_symbols.*))
        PROVIDE(__elm_kernel_symbols_end = .);
        . = ALIGN(8);
        PROVIDE(__elm_kernel_mixin_sites_start = .);
        KEEP(*(.elm.kernel_mixin_sites .elm.kernel_mixin_sites.*))
        PROVIDE(__elm_kernel_mixin_sites_end = .);
        . = ALIGN(8);
        PROVIDE(__kernel_integrated_components_start = .);
        KEEP(*(.kernel.integrated_components .kernel.integrated_components.*))
        PROVIDE(__kernel_integrated_components_end = .);
        . = ALIGN(8);
        *(.eh_frame_hdr)
        *(.eh_frame)
    }

    . = ALIGN(4K);
    erodata = .;
    . = ALIGN(8);

    sdata = .;
    .data ALIGN(4K) : AT(ADDR(.data) - LOAD_OFFSET) {
        . = ALIGN(4K);
        *(.data.prepage .data.prepage.*)
        . = ALIGN(4K);
        *(.data .data.*)
        *(.sdata .sdata.*)
        . = ALIGN(4K);
    }

    edata = .;
    .bss (NOLOAD) : AT(ADDR(.bss) - LOAD_OFFSET) {
        PROVIDE(sheap = .);
        *(.bss.heap)
        . += 0x04000000;
        PROVIDE(eheap = .);
        *(.bss.stack)
        PROVIDE(__tmp_stack_bottom = .);
        . = ALIGN(4K);
        . += 0x00180000;
        . = ALIGN(4K);
        PROVIDE(__tmp_stack_top = .);
        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
    }

    . = ALIGN(4K);
    ebss = .;
    . = ALIGN(0x10000);
    ekernel = .;

    . = ALIGN(8);

    .debug_line : {
        KEEP(*(.debug_line))
    }

    . = ALIGN(8);
}

ASSERT(_start == VIRTUAL_BASE_ADDRESS,
       "LoongArch64 boot entry must be the first byte of the load image")
