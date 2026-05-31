OUTPUT_FORMAT(elf64-loongarch)
OUTPUT_ARCH(loongarch64)
ENTRY(_start)

BASE_ADDRESS = 0x90000000;

PECOFF_FILE_ALIGN = 0x200;
PECOFF_SEGMENT_ALIGN = 0x10000;

IMAGE_NT_SIGNATURE = 0x00004550;
IMAGE_FILE_MACHINE_LOONGARCH64 = 0x6264;
IMAGE_FILE_EXECUTABLE_IMAGE = 0x0002;
IMAGE_FILE_LINE_NUMS_STRIPPED = 0x0004;
IMAGE_FILE_DEBUG_STRIPPED = 0x0200;
IMAGE_NT_OPTIONAL_HDR64_MAGIC = 0x020b;
IMAGE_SUBSYSTEM_EFI_APPLICATION = 0x0a;
IMAGE_SCN_CNT_CODE = 0x00000020;
IMAGE_SCN_CNT_INITIALIZED_DATA = 0x00000040;
IMAGE_SCN_MEM_EXECUTE = 0x20000000;
IMAGE_SCN_MEM_READ = 0x40000000;
IMAGE_SCN_MEM_WRITE = 0x80000000;
LINUX_EFISTUB_MAJOR_VERSION = 0x3;
LINUX_EFISTUB_MINOR_VERSION = 0x0;
LINUX_PE_MAGIC = 0x818223cd;

SECTIONS
{
    . = BASE_ADDRESS;
    skernel = .;
    _head = .;

    .head : AT(BASE_ADDRESS) {
        efi_header_begin = .;

        SHORT(0x5a4d)
        . = efi_header_begin + 0x8;
        QUAD(ABSOLUTE(_start))
        QUAD(ABSOLUTE(ekernel - skernel))
        QUAD(BASE_ADDRESS)
        . = efi_header_begin + 0x38;
        LONG(LINUX_PE_MAGIC)
        LONG(pe_header - efi_header_begin)

        pe_header = .;
        LONG(IMAGE_NT_SIGNATURE)
        SHORT(IMAGE_FILE_MACHINE_LOONGARCH64)
        SHORT(2)
        LONG(0)
        LONG(0)
        LONG(0)
        SHORT(section_table - optional_header)
        SHORT(IMAGE_FILE_DEBUG_STRIPPED | IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_LINE_NUMS_STRIPPED)

        optional_header = .;
        SHORT(IMAGE_NT_OPTIONAL_HDR64_MAGIC)
        BYTE(0x02)
        BYTE(0x14)
        LONG(etext - pecoff_header_end)
        LONG(edata - etext)
        LONG(ebss - edata)
        LONG(efi_pe_entry_trampoline - skernel)
        LONG(pecoff_header_end - skernel)

        QUAD(BASE_ADDRESS)
        LONG(PECOFF_SEGMENT_ALIGN)
        LONG(PECOFF_FILE_ALIGN)
        SHORT(0)
        SHORT(0)
        SHORT(LINUX_EFISTUB_MAJOR_VERSION)
        SHORT(LINUX_EFISTUB_MINOR_VERSION)
        SHORT(0)
        SHORT(0)
        LONG(0)
        LONG(ekernel - skernel)
        LONG(pecoff_header_end - skernel)
        LONG(0)
        SHORT(IMAGE_SUBSYSTEM_EFI_APPLICATION)
        SHORT(0)
        QUAD(0)
        QUAD(0)
        QUAD(0)
        QUAD(0)
        LONG(0)
        LONG(6)

        QUAD(0)
        QUAD(0)
        QUAD(0)
        QUAD(0)
        QUAD(0)
        QUAD(0)

        section_table = .;
        LONG(0x7865742e)
        LONG(0x00000074)
        LONG(etext - pecoff_header_end)
        LONG(pecoff_header_end - skernel)
        LONG(etext - pecoff_header_end)
        LONG(pecoff_header_end - skernel)
        LONG(0)
        LONG(0)
        SHORT(0)
        SHORT(0)
        LONG(IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_EXECUTE)

        LONG(0x7461642e)
        LONG(0x00000061)
        LONG(ebss - etext)
        LONG(etext - skernel)
        LONG(edata - etext)
        LONG(etext - skernel)
        LONG(0)
        LONG(0)
        SHORT(0)
        SHORT(0)
        LONG(IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE)

        . = efi_header_begin + PECOFF_SEGMENT_ALIGN;
        pecoff_header_end = .;
    }

    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text)) {
        KEEP(*(.text.entry))
        KEEP(*(.text.efi.entry))
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

    . = ALIGN(PECOFF_SEGMENT_ALIGN);
    etext = .;
    srodata = .;
    .rodata ALIGN(4K) : AT(ADDR(.rodata)) {
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
        *(.eh_frame_hdr)
        *(.eh_frame)
    }

    . = ALIGN(4K);
    erodata = .;
    . = ALIGN(8);

    sdata = .;
    .data ALIGN(4K) : AT(ADDR(.data)) {
        . = ALIGN(4K);
        *(.data.prepage .data.prepage.*)
        . = ALIGN(4K);
        *(.data .data.*)
        *(.sdata .sdata.*)
        . = ALIGN(PECOFF_FILE_ALIGN);
    }

    edata = .;
    .bss (NOLOAD) : AT(ADDR(.bss)) {
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

    .debug_info       0 : { *(.debug_info) }
    .debug_abbrev     0 : { *(.debug_abbrev) }
    .debug_str        0 : { *(.debug_str) }
    .debug_line       0 : { *(.debug_line) }
    .debug_line_str   0 : { *(.debug_line_str) }
    .debug_ranges     0 : { *(.debug_ranges) }
    .debug_rnglists   0 : { *(.debug_rnglists) }
    .debug_loc        0 : { *(.debug_loc) }
    .debug_loclists   0 : { *(.debug_loclists) }
    .debug_frame      0 : { *(.debug_frame) }
    .debug_aranges    0 : { *(.debug_aranges) }
    .debug_macro      0 : { *(.debug_macro) }
    .debug_pubnames   0 : { *(.debug_pubnames) }
    .debug_pubtypes   0 : { *(.debug_pubtypes) }
}
