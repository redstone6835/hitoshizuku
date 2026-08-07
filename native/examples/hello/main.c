#include <ranalib/unistd.h>

int main(void) {
    static const char message[] = "Hello Soyo!\n";
    long written = write(1, message, sizeof(message) - 1);
    return written == (long)(sizeof(message) - 1) ? 37 : 1;
}
