/* 两个架构共同的只读内核元数据。此文件只能在 .rodata 输出段内 INCLUDE。 */
*(.rodata .rodata.*)
*(.srodata .srodata.*)
*(.data.rel.ro .data.rel.ro.*)

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
