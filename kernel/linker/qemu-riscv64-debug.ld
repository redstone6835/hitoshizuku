/* 调试版链接脚本 — 输出 ELF 格式，支持 GDB / objdump / addr2line
 * 与 qemu-riscv64.ld 布局一致，但不含 PE/COFF header。
 * 使用方法：cargo-config/config.toml 中将 -Tkernel/linker/qemu-riscv64.ld
 *          替换为 -Tkernel/linker/qemu-riscv64-debug.ld
 */
OUTPUT_FORMAT("elf64-littleriscv")
OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;
VIRTUAL_BASE_ADDRESS = 0xFFFFFF8080200000;
LOAD_OFFSET = VIRTUAL_BASE_ADDRESS - BASE_ADDRESS;

PECOFF_SEGMENT_ALIGN = 0x10000;

SECTIONS
{
    . = VIRTUAL_BASE_ADDRESS;
    skernel = .;

    /* 保留 0x10000 偏移量与 release 版布局一致 */
    . += PECOFF_SEGMENT_ALIGN;
    pecoff_header_end = .;

    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text) - LOAD_OFFSET) {
        KEEP(*(.text.entry))
        KEEP(*(.text.efi.entry))
        . = ALIGN(4K);
        KEEP(*(.text.trap.eentry))
        *(.text .text.*)
    }

    . = ALIGN(0x200000);
    etext = .;
    srodata = .;
    .rodata ALIGN(4K) : AT(ADDR(.rodata) - LOAD_OFFSET) {
        KEEP(*(.rodata.vdso))
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
        PROVIDE(__kernel_integrated_components_start = .);
        KEEP(*(.kernel.integrated_components .kernel.integrated_components.*))
        PROVIDE(__kernel_integrated_components_end = .);
        . = ALIGN(8);
        *(.eh_frame_hdr)
        *(.eh_frame)
    }

    . = ALIGN(0x200000);
    erodata = .;
    . = ALIGN(8);

    sdata = .;
    .data ALIGN(4K) : AT(ADDR(.data) - LOAD_OFFSET) {
        . = ALIGN(4K);
        *(.data.prepage .data.prepage.*)
        . = ALIGN(4K);
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    edata = .;
    /* BSS 布局：
     *   [sheap, eheap)       — 内核堆（64MiB）
     *   [stack_bottom, top)  — boot 栈（1.5MiB）
     *   [sbss, ebss)         — BSS，由 clear_bss() 清零
     */
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
    . = ALIGN(PECOFF_SEGMENT_ALIGN);
    ekernel = .;

    /* DWARF 调试信息 — 不加载到内存 */
    .debug_info     0 : { *(.debug_info) }
    .debug_abbrev   0 : { *(.debug_abbrev) }
    .debug_line     0 : { *(.debug_line) }
    .debug_str      0 : { *(.debug_str) }
    .debug_ranges   0 : { *(.debug_ranges) }
    .debug_aranges  0 : { *(.debug_aranges) }
    .debug_frame    0 : { *(.debug_frame) }
    .debug_loc      0 : { *(.debug_loc) }

    /DISCARD/ : {
        *(.comment)
        *(.note*)
    }
}
