.section .rodata.vdso, "a"
.balign 4

.global __mygo_vdso_blob_start
__mygo_vdso_blob_start:

.global __mygo_vdso_rt_sigreturn
__mygo_vdso_rt_sigreturn:
    li.w $a7, 139
    syscall 0

.global __mygo_vdso_clock_gettime
__mygo_vdso_clock_gettime:
    li.w $a7, 113
    syscall 0
    ret

.global __mygo_vdso_gettimeofday
__mygo_vdso_gettimeofday:
    li.w $a7, 169
    syscall 0
    ret

.global __mygo_vdso_clock_getres
__mygo_vdso_clock_getres:
    li.w $a7, 114
    syscall 0
    ret

.global __mygo_vdso_getcpu
__mygo_vdso_getcpu:
    li.w $a7, 168
    syscall 0
    ret

.global __mygo_vdso_text_end
__mygo_vdso_text_end:

.org 0xe00
.global __mygo_vdso_blob_end
.global __mygo_vdso_data_anchor
__mygo_vdso_blob_end:
__mygo_vdso_data_anchor:
