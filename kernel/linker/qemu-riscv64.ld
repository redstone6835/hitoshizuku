OUTPUT_FORMAT("elf64-littleriscv")
OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;
VIRTUAL_BASE_ADDRESS = 0xFFFFFF8080200000;
LOAD_OFFSET = VIRTUAL_BASE_ADDRESS - BASE_ADDRESS;

SECTIONS
{
    . = VIRTUAL_BASE_ADDRESS;
    skernel = .;

    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text) - LOAD_OFFSET) {
        /* OpenSBI 跳转到物理装载基址，主入口必须严格占据首地址。 */
        KEEP(*(.text.entry.boot))
        KEEP(*(.text.entry .text.entry.*))
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

    . = ALIGN(0x200000);
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
        PROVIDE(__kernel_integrated_managed_exports_start = .);
        KEEP(*(.kernel.integrated_managed_exports .kernel.integrated_managed_exports.*))
        PROVIDE(__kernel_integrated_managed_exports_end = .);
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
    . = ALIGN(4K);
    ekernel = .;

    . = ALIGN(8);

    .debug_line : {
        KEEP(*(.debug_line))
    }

    . = ALIGN(8);
}

ASSERT(_start == VIRTUAL_BASE_ADDRESS,
       "RISC-V boot entry must be the first byte of the load image")
