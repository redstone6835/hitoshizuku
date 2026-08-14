_Thread_local int tls_value = 7;

int _start(void) {
    return tls_value;
}
