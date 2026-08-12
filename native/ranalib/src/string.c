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

void *memchr(const void *memory, int value, size_t count) {
    const unsigned char *bytes = memory;
    unsigned char needle = (unsigned char)value;
    for (size_t index = 0; index < count; ++index) {
        if (bytes[index] == needle) {
            return (void *)(bytes + index);
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

size_t strnlen(const char *string, size_t maximum) {
    size_t length = 0;
    while (length < maximum && string[length] != '\0') {
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

char *strcpy(char *destination, const char *source) {
    size_t index = 0;
    do {
        destination[index] = source[index];
    } while (source[index++] != '\0');
    return destination;
}

char *strncpy(char *destination, const char *source, size_t count) {
    size_t index = 0;
    while (index < count && source[index] != '\0') {
        destination[index] = source[index];
        ++index;
    }
    while (index < count) {
        destination[index++] = '\0';
    }
    return destination;
}

char *strcat(char *destination, const char *source) {
    strcpy(destination + strlen(destination), source);
    return destination;
}

char *strncat(char *destination, const char *source, size_t count) {
    size_t offset = strlen(destination);
    size_t index = 0;
    while (index < count && source[index] != '\0') {
        destination[offset + index] = source[index];
        ++index;
    }
    destination[offset + index] = '\0';
    return destination;
}

char *strchr(const char *string, int character) {
    unsigned char needle = (unsigned char)character;
    for (;;) {
        if ((unsigned char)*string == needle) {
            return (char *)string;
        }
        if (*string++ == '\0') {
            return 0;
        }
    }
}

char *strrchr(const char *string, int character) {
    unsigned char needle = (unsigned char)character;
    const char *last = 0;
    do {
        if ((unsigned char)*string == needle) {
            last = string;
        }
    } while (*string++ != '\0');
    return (char *)last;
}

char *strstr(const char *haystack, const char *needle) {
    if (*needle == '\0') {
        return (char *)haystack;
    }
    size_t needle_length = strlen(needle);
    for (; *haystack != '\0'; ++haystack) {
        if (*haystack == *needle && strncmp(haystack, needle, needle_length) == 0) {
            return (char *)haystack;
        }
    }
    return 0;
}

static int character_in(const char *set, unsigned char character) {
    while (*set != '\0') {
        if ((unsigned char)*set++ == character) {
            return 1;
        }
    }
    return 0;
}

size_t strspn(const char *string, const char *accepted) {
    size_t length = 0;
    while (string[length] != '\0' && character_in(accepted, (unsigned char)string[length])) {
        ++length;
    }
    return length;
}

size_t strcspn(const char *string, const char *rejected) {
    size_t length = 0;
    while (string[length] != '\0' && !character_in(rejected, (unsigned char)string[length])) {
        ++length;
    }
    return length;
}

char *strpbrk(const char *string, const char *accepted) {
    size_t offset = strcspn(string, accepted);
    return string[offset] == '\0' ? 0 : (char *)(string + offset);
}

char *strtok(char *string, const char *delimiters) {
    static _Thread_local char *next;
    char *cursor = string == 0 ? next : string;
    if (cursor == 0) {
        return 0;
    }
    cursor += strspn(cursor, delimiters);
    if (*cursor == '\0') {
        next = 0;
        return 0;
    }
    char *token = cursor;
    cursor += strcspn(cursor, delimiters);
    if (*cursor == '\0') {
        next = 0;
    } else {
        *cursor = '\0';
        next = cursor + 1;
    }
    return token;
}
