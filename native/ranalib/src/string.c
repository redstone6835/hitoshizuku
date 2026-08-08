#include <stdint.h>

#include <ranalib/string.h>

void *memcpy(void *destination, const void *source, size_t count) {
    unsigned char *output = destination;
    const unsigned char *input = source;
    for (size_t index = 0; index < count; ++index) {
        output[index] = input[index];
    }
    return destination;
}

void *memmove(void *destination, const void *source, size_t count) {
    unsigned char *output = destination;
    const unsigned char *input = source;
    if ((uintptr_t)output > (uintptr_t)input) {
        for (size_t index = count; index != 0; --index) {
            output[index - 1] = input[index - 1];
        }
    } else {
        for (size_t index = 0; index < count; ++index) {
            output[index] = input[index];
        }
    }
    return destination;
}

void *memset(void *destination, int value, size_t count) {
    unsigned char *output = destination;
    for (size_t index = 0; index < count; ++index) {
        output[index] = (unsigned char)value;
    }
    return destination;
}

int memcmp(const void *left, const void *right, size_t count) {
    const unsigned char *left_bytes = left;
    const unsigned char *right_bytes = right;
    for (size_t index = 0; index < count; ++index) {
        if (left_bytes[index] != right_bytes[index]) {
            return (int)left_bytes[index] - (int)right_bytes[index];
        }
    }
    return 0;
}

size_t strlen(const char *string) {
    size_t length = 0;
    while (string[length] != '\0') {
        ++length;
    }
    return length;
}

int strcmp(const char *left, const char *right) {
    size_t index = 0;
    while (left[index] != '\0' && left[index] == right[index]) {
        ++index;
    }
    return (int)(unsigned char)left[index] - (int)(unsigned char)right[index];
}

int strncmp(const char *left, const char *right, size_t count) {
    for (size_t index = 0; index < count; ++index) {
        unsigned char left_byte = (unsigned char)left[index];
        unsigned char right_byte = (unsigned char)right[index];
        if (left_byte != right_byte) {
            return (int)left_byte - (int)right_byte;
        }
        if (left_byte == 0) {
            return 0;
        }
    }
    return 0;
}
