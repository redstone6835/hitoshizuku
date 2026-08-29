OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* KERNEL_PHYS_BASE 与 KERNEL_VIRT_BASE 由 kernel/build.rs 注入。 */
LOAD_OFFSET = KERNEL_VIRT_BASE - KERNEL_PHYS_BASE;
X86_64_HIGHER_HALF_OFFSET = 0xffffffff80000000;

PHDRS
{
    apboot PT_LOAD FLAGS(5); /* low physical INIT/SIPI trampoline */
    text   PT_LOAD FLAGS(5); /* R E */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* RW  */
}

SECTIONS
{
    /* Intel SIPI vectors encode a physical 4 KiB page below 1 MiB.  Keep the
       trampoline and its serialized handoff parameters in a dedicated low
       segment; its high VMA is never entered until paging is active. */
    .ap_trampoline (KERNEL_VIRT_BASE + 0x8000 - KERNEL_PHYS_BASE) : AT(0x8000) {
        KEEP(*(.text.ap_trampoline))
        . = ALIGN(8);
        KEEP(*(.data.ap_trampoline))
    } :apboot

    . = KERNEL_VIRT_BASE;
    skernel = .;

    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text) - LOAD_OFFSET) {
        /* _start is the first byte; the Multiboot2 header follows it and is
           still kept in the first 32 KiB of the image. */
        KEEP(*(.text.entry.boot))
        KEEP(*(.multiboot2.header))
        KEEP(*(.text.entry .text.entry.*))
        KEEP(*(.text.efi.entry))
        . = ALIGN(4K);
        KEEP(*(.text.trap .text.trap.*))
        *(.text .text.*)
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
        /* Startup snapshots live before sbss so clear_bss() preserves them. */
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

/* Physical aliases are consumed by the 32-bit Multiboot2 trampoline before
   paging is enabled.  Keep them as absolute symbols so assembly cannot
   accidentally encode a high-half address in a 32-bit relocation. */
__x86_start_phys = _start - LOAD_OFFSET;
__x86_kernel_end_phys = ekernel - LOAD_OFFSET;
__x86_early_page_tables_phys = EARLY_PAGE_TABLES - LOAD_OFFSET;
__x86_boot_magic_phys = __x86_boot_magic - LOAD_OFFSET;
__x86_boot_info_phys = __x86_boot_info - LOAD_OFFSET;
__x86_gdt_phys = __x86_gdt - LOAD_OFFSET;
__x86_gdt_ptr_phys = __x86_gdt_ptr - LOAD_OFFSET;
__x86_long_mode_phys = __x86_long_mode_low - LOAD_OFFSET;
__x86_mb2_header_phys = __x86_mb2_header - LOAD_OFFSET;
__x86_ap_gdt_phys = __x86_ap_gdt - LOAD_OFFSET;
__x86_ap_gdt_ptr_phys = __x86_ap_gdt_ptr - LOAD_OFFSET;
__x86_ap_params_phys = __x86_ap_trampoline_params - LOAD_OFFSET;
__x86_ap_protected_phys = __x86_ap_protected - LOAD_OFFSET;
__x86_ap_long_mode_phys = __x86_ap_long_mode - LOAD_OFFSET;

ASSERT((KERNEL_PHYS_BASE & 0xfff) == 0,
       "x86_64 kernel physical base must be 4 KiB aligned")
ASSERT((KERNEL_VIRT_BASE & 0xfff) == 0,
       "x86_64 kernel virtual base must be 4 KiB aligned")
ASSERT(KERNEL_VIRT_BASE - KERNEL_PHYS_BASE == X86_64_HIGHER_HALF_OFFSET,
       "x86_64 kernel VMA/LMA offset does not match the higher-half mapping")
ASSERT(_start == KERNEL_VIRT_BASE,
       "x86_64 boot entry must be the first byte of the load image")
ASSERT(LOADADDR(.text) == KERNEL_PHYS_BASE,
       "x86_64 first load segment must start at the platform physical base")
ASSERT((ekernel - KERNEL_VIRT_BASE) < 0x80000000,
       "x86_64 early identity/direct map cannot cover the kernel image")
ASSERT(__x86_start_phys < 0x100000000 &&
       __x86_kernel_end_phys < 0x100000000 &&
       __x86_early_page_tables_phys < 0x100000000,
       "x86_64 Multiboot2 physical bootstrap symbols must fit below 4 GiB")
ASSERT((__x86_mb2_header & 7) == 0 &&
       (__x86_mb2_header - KERNEL_VIRT_BASE) < 0x8000,
       "x86_64 Multiboot2 header must be 8-byte aligned in the first 32 KiB")
ASSERT(LOADADDR(.ap_trampoline) == 0x8000 &&
       SIZEOF(.ap_trampoline) <= 0x1000,
       "x86_64 AP trampoline must fit in its SIPI page")
ASSERT(stext <= etext && etext == srodata && srodata <= erodata &&
       erodata == sdata && sdata <= edata && edata <= ebss && ebss <= ekernel,
       "x86_64 kernel sections are out of order")
ASSERT(DEFINED(HITOSHIZUKU_PLATFORM_TAG),
       "x86_64 kernel is missing its platform provenance tag")
