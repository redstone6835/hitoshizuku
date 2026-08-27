/* 非装载调试信息。显式置于 VMA 0，objcopy 导出 raw 镜像时不会包含这些段。 */
.debug_info        0 : { KEEP(*(.debug_info .debug_info.*)) } :NONE
.debug_abbrev      0 : { KEEP(*(.debug_abbrev .debug_abbrev.*)) } :NONE
.debug_line        0 : { KEEP(*(.debug_line .debug_line.*)) } :NONE
.debug_line_str    0 : { KEEP(*(.debug_line_str .debug_line_str.*)) } :NONE
.debug_str         0 : { KEEP(*(.debug_str .debug_str.*)) } :NONE
.debug_str_offsets 0 : { KEEP(*(.debug_str_offsets .debug_str_offsets.*)) } :NONE
.debug_addr        0 : { KEEP(*(.debug_addr .debug_addr.*)) } :NONE
.debug_ranges      0 : { KEEP(*(.debug_ranges .debug_ranges.*)) } :NONE
.debug_rnglists    0 : { KEEP(*(.debug_rnglists .debug_rnglists.*)) } :NONE
.debug_loc         0 : { KEEP(*(.debug_loc .debug_loc.*)) } :NONE
.debug_loclists    0 : { KEEP(*(.debug_loclists .debug_loclists.*)) } :NONE
.debug_aranges     0 : { KEEP(*(.debug_aranges .debug_aranges.*)) } :NONE
.debug_frame       0 : { KEEP(*(.debug_frame .debug_frame.*)) } :NONE
.debug_names       0 : { KEEP(*(.debug_names .debug_names.*)) } :NONE
.debug_pubnames    0 : { KEEP(*(.debug_pubnames .debug_pubnames.*)) } :NONE
.debug_pubtypes    0 : { KEEP(*(.debug_pubtypes .debug_pubtypes.*)) } :NONE
.debug_types       0 : { KEEP(*(.debug_types .debug_types.*)) } :NONE
.debug_macro       0 : { KEEP(*(.debug_macro .debug_macro.*)) } :NONE
.debug_macinfo     0 : { KEEP(*(.debug_macinfo .debug_macinfo.*)) } :NONE
.debug_sup         0 : { KEEP(*(.debug_sup .debug_sup.*)) } :NONE
.debug_cu_index    0 : { KEEP(*(.debug_cu_index)) } :NONE
.debug_tu_index    0 : { KEEP(*(.debug_tu_index)) } :NONE
.gdb_index         0 : { KEEP(*(.gdb_index)) } :NONE
.riscv.attributes  0 : { KEEP(*(.riscv.attributes)) } :NONE
.loongarch.attributes 0 : { KEEP(*(.loongarch.attributes)) } :NONE
.comment           0 : { KEEP(*(.comment)) } :NONE

/DISCARD/ : {
    *(.note.GNU-stack)
}
