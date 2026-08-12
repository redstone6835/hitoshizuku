/* TLSF 的诊断分支不会在基准热路径执行；Native 无 libc printf。 */
int printf(const char *format, ...) {
    (void)format;
    return 0;
}
