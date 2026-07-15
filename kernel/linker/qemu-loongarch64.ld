OUTPUT_FORMAT(binary)
OUTPUT_ARCH(loongarch64)
ENTRY(_start)

BASE_ADDRESS = 0x90000000;
VIRTUAL_BASE_ADDRESS = 0x9000000090000000;
LOAD_OFFSET = VIRTUAL_BASE_ADDRESS - BASE_ADDRESS;

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
    . = VIRTUAL_BASE_ADDRESS;
    skernel = .;
    _head = .;

    .head : AT(BASE_ADDRESS) {
        efi_header_begin = .;

        SHORT(0x5a4d)                         /* "MZ" */
        . = efi_header_begin + 0x8;
        QUAD(ABSOLUTE(_start - LOAD_OFFSET))  /* kernel_entry */
        QUAD(ABSOLUTE(ekernel - skernel))     /* kernel_asize */
        QUAD(BASE_ADDRESS)                    /* text_offset */
        . = efi_header_begin + 0x38;
        LONG(LINUX_PE_MAGIC)
        LONG(pe_header - efi_header_begin)

        pe_header = .;
        LONG(IMAGE_NT_SIGNATURE)
        SHORT(IMAGE_FILE_MACHINE_LOONGARCH64)
        SHORT(2)                              /* NumberOfSections */
        LONG(0)                               /* TimeDateStamp */
        LONG(0)                               /* PointerToSymbolTable */
        LONG(0)                               /* NumberOfSymbols */
        SHORT(section_table - optional_header)
        SHORT(IMAGE_FILE_DEBUG_STRIPPED | IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_LINE_NUMS_STRIPPED)

        optional_header = .;
        SHORT(IMAGE_NT_OPTIONAL_HDR64_MAGIC)
        BYTE(0x02)
        BYTE(0x14)
        LONG(etext - pecoff_header_end)       /* SizeOfCode */
        LONG(edata - etext)                   /* SizeOfInitializedData */
        LONG(ebss - edata)                   /* SizeOfUninitializedData */
        LONG(efi_pe_entry_trampoline - skernel) /* AddressOfEntryPoint */
        LONG(pecoff_header_end - skernel)     /* BaseOfCode */

        QUAD(BASE_ADDRESS)                    /* ImageBase */
        LONG(PECOFF_SEGMENT_ALIGN)            /* SectionAlignment */
        LONG(PECOFF_FILE_ALIGN)               /* FileAlignment */
        SHORT(0)                              /* MajorOperatingSystemVersion */
        SHORT(0)                              /* MinorOperatingSystemVersion */
        SHORT(LINUX_EFISTUB_MAJOR_VERSION)
        SHORT(LINUX_EFISTUB_MINOR_VERSION)
        SHORT(0)                              /* MajorSubsystemVersion */
        SHORT(0)                              /* MinorSubsystemVersion */
        LONG(0)                               /* Win32VersionValue */
        LONG(ekernel - skernel)               /* SizeOfImage */
        LONG(pecoff_header_end - skernel)     /* SizeOfHeaders */
        LONG(0)                               /* CheckSum */
        SHORT(IMAGE_SUBSYSTEM_EFI_APPLICATION)
        SHORT(0)                              /* DllCharacteristics */
        QUAD(0)                               /* SizeOfStackReserve */
        QUAD(0)                               /* SizeOfStackCommit */
        QUAD(0)                               /* SizeOfHeapReserve */
        QUAD(0)                               /* SizeOfHeapCommit */
        LONG(0)                               /* LoaderFlags */
        LONG(6)                               /* NumberOfRvaAndSizes */

        QUAD(0)                               /* ExportTable */
        QUAD(0)                               /* ImportTable */
        QUAD(0)                               /* ResourceTable */
        QUAD(0)                               /* ExceptionTable */
        QUAD(0)                               /* CertificationTable */
        QUAD(0)                               /* BaseRelocationTable */

        section_table = .;
        LONG(0x7865742e)                      /* ".text\0\0\0" */
        LONG(0x00000074)
        LONG(etext - pecoff_header_end)       /* VirtualSize */
        LONG(pecoff_header_end - skernel)     /* VirtualAddress */
        LONG(etext - pecoff_header_end)       /* SizeOfRawData */
        LONG(pecoff_header_end - skernel)     /* PointerToRawData */
        LONG(0)                               /* PointerToRelocations */
        LONG(0)                               /* PointerToLineNumbers */
        SHORT(0)                              /* NumberOfRelocations */
        SHORT(0)                              /* NumberOfLineNumbers */
        LONG(IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_EXECUTE)

        LONG(0x7461642e)                      /* ".data\0\0\0" */
        LONG(0x00000061)
        LONG(ebss - etext)                    /* VirtualSize */
        LONG(etext - skernel)                 /* VirtualAddress */
        LONG(edata - etext)                   /* SizeOfRawData */
        LONG(etext - skernel)                 /* PointerToRawData */
        LONG(0)                               /* PointerToRelocations */
        LONG(0)                               /* PointerToLineNumbers */
        SHORT(0)                              /* NumberOfRelocations */
        SHORT(0)                              /* NumberOfLineNumbers */
        LONG(IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE)

        . = efi_header_begin + PECOFF_SEGMENT_ALIGN;
        pecoff_header_end = .;
    }

    stext = .;
    .text ALIGN(4K) : AT(ADDR(.text) - LOAD_OFFSET) {
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
        . = ALIGN(PECOFF_FILE_ALIGN);
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
    . = ALIGN(PECOFF_SEGMENT_ALIGN);
    ekernel = .;

    . = ALIGN(8);

    .debug_line : {
        KEEP(*(.debug_line))
    }

    . = ALIGN(8);
}

ASSERT((pecoff_header_end - skernel) == PECOFF_SEGMENT_ALIGN, "PE/COFF header must be 64 KiB")
ASSERT(((etext - skernel) % PECOFF_SEGMENT_ALIGN) == 0, "PE/COFF .data section must be 64 KiB aligned")
ASSERT(((edata - etext) % PECOFF_FILE_ALIGN) == 0, "PE/COFF .data raw size must be file aligned")
