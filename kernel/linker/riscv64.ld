OUTPUT_FORMAT("elf64-littleriscv")
OUTPUT_ARCH(riscv)
ENTRY(_start)

/* KERNEL_PHYS_BASE 与 KERNEL_VIRT_BASE 由 kernel/build.rs 按平台注入。 */
LOAD_OFFSET = KERNEL_VIRT_BASE - KERNEL_PHYS_BASE;
KERNEL_DIRECT_MAP_OFFSET = 0xFFFFFF8000000000;

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
        *(.text.trampoline_user)
        *(.text.trampoline_kernel)
    } :text

    . = ALIGN(0x200000);
    etext = .;
    srodata = .;
    .rodata ALIGN(4K) : AT(ADDR(.rodata) - LOAD_OFFSET) {
        INCLUDE common-rodata.ld
    } :rodata

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
        *(.got .got.*)
    } :data

    edata = .;
    .bss (NOLOAD) : AT(ADDR(.bss) - LOAD_OFFSET) {
        /* MMU 开启前复制的 DTB；位于 sbss 前，避免 clear_bss() 擦除。 */
        *(.bss.prepage .bss.prepage.*)
        . = ALIGN(4K);
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
    . = ALIGN(4K);
    ekernel = .;

    INCLUDE common-debug.ld
}

ASSERT((KERNEL_PHYS_BASE & 0xfff) == 0,
       "RISC-V kernel physical base must be 4 KiB aligned")
ASSERT((KERNEL_VIRT_BASE & 0xfff) == 0,
       "RISC-V kernel virtual base must be 4 KiB aligned")
ASSERT(KERNEL_VIRT_BASE - KERNEL_PHYS_BASE == KERNEL_DIRECT_MAP_OFFSET,
       "RISC-V kernel VMA/LMA offset does not match the direct map")
ASSERT(_start == KERNEL_VIRT_BASE,
       "RISC-V boot entry must be the first byte of the load image")
ASSERT(LOADADDR(.text) == KERNEL_PHYS_BASE,
       "RISC-V first load segment must start at the platform physical base")
ASSERT(stext <= etext && etext == srodata && srodata <= erodata &&
       erodata == sdata && sdata <= edata && edata <= ebss && ebss <= ekernel,
       "RISC-V kernel sections are out of order")
ASSERT(DEFINED(HITOSHIZUKU_PLATFORM_TAG),
       "RISC-V kernel is missing its platform provenance tag")
