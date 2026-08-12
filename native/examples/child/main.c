#include <stdint.h>

#include <ranalib/stdio.h>

int main(void) {
    static const char message[] = "C child\n";
    return fwrite(message, 1, sizeof(message) - 1, stdout) == sizeof(message) - 1
        ? 0x4a11u
        : 1;
}
