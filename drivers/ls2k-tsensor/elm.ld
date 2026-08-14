ENTRY(__elm_module_entry_v1)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0;
    .text : ALIGN(4096)
    {
        KEEP(*(.text.elm.abi))
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata :
    {
        KEEP(*(.rodata.elm.module))
        *(.rodata .rodata.* .srodata .srodata.*)
        *(.eh_frame .eh_frame_hdr)
    } :rodata

    .rela.dyn : ALIGN(8)
    {
        KEEP(*(.rela.dyn))
    } :rodata
    .rela.plt : ALIGN(8) { KEEP(*(.rela.plt)) } :rodata

    .dynsym : ALIGN(8) { KEEP(*(.dynsym)) } :rodata
    .dynstr : ALIGN(1) { KEEP(*(.dynstr)) } :rodata
    .hash : ALIGN(8) { KEEP(*(.hash)) } :rodata
    .gnu.hash : ALIGN(8) { KEEP(*(.gnu.hash)) } :rodata

    . = ALIGN(4096);
    .data :
    {
        *(.data .data.* .sdata .sdata.*)
        *(.got .got.*)
        *(.got.plt)
    } :data

    .dynamic : ALIGN(8) { KEEP(*(.dynamic)) } :data

    .bss (NOLOAD) :
    {
        *(.bss .bss.* .sbss .sbss.*)
        *(COMMON)
    } :data

    .elm.meta 0 (INFO) :
    {
        KEEP(*(.elm.meta))
    }

    /DISCARD/ :
    {
        *(.comment)
        *(.note.gnu.build-id)
        *(.gnu_debuglink)
        *(.interp)
    }
}
