OUTPUT_FORMAT("elf64-loongarch")
OUTPUT_ARCH(loongarch64)
ENTRY(_start)

/* KERNEL_PHYS_BASE 与 KERNEL_VIRT_BASE 由 kernel/build.rs 按平台注入。 */
LOAD_OFFSET = KERNEL_VIRT_BASE - KERNEL_PHYS_BASE;
DMW1_CACHED_BASE = 0x9000000000000000;

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R E */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* RW  */
}

SECTIONS
{
    . = KERNEL_VIRT_BASE;
    skernel = .;

    /* 固件跳到物理装载基址；DMW1 建立后，同一入口使用 cached 虚拟别名。 */
    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text) - LOAD_OFFSET) {
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
        *(.text.trampoline_user)
        *(.text.trampoline_kernel)
    } :text

    . = ALIGN(0x10000);
    etext = .;
    srodata = .;
    .rodata ALIGN(4K) : AT(ADDR(.rodata) - LOAD_OFFSET) {
        INCLUDE common-rodata.ld
    } :rodata

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
        *(.got .got.*)
        . = ALIGN(4K);
    } :data

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
    } :data

    . = ALIGN(4K);
    ebss = .;
    . = ALIGN(0x10000);
    ekernel = .;

    INCLUDE common-debug.ld
}

ASSERT((KERNEL_PHYS_BASE & 0xfff) == 0,
       "LoongArch64 kernel physical base must be 4 KiB aligned")
ASSERT((KERNEL_VIRT_BASE & 0xfff) == 0,
       "LoongArch64 kernel virtual base must be 4 KiB aligned")
ASSERT(KERNEL_VIRT_BASE == DMW1_CACHED_BASE + KERNEL_PHYS_BASE,
       "LoongArch64 kernel VMA must be the DMW1 cached alias of its LMA")
ASSERT(_start == KERNEL_VIRT_BASE,
       "LoongArch64 boot entry must be the first byte of the load image")
ASSERT(LOADADDR(.text) == KERNEL_PHYS_BASE,
       "LoongArch64 first load segment must start at the platform physical base")
ASSERT(stext <= etext && etext == srodata && srodata <= erodata &&
       erodata == sdata && sdata <= edata && edata <= ebss && ebss <= ekernel,
       "LoongArch64 kernel sections are out of order")
ASSERT(DEFINED(HITOSHIZUKU_PLATFORM_TAG),
       "LoongArch64 kernel is missing its platform provenance tag")
