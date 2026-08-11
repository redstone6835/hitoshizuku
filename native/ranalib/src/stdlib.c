#include <limits.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdlib.h>
#include <ranalib/string.h>

extern char **environ;

int abs(int value) { return value < 0 ? -value : value; }
long labs(long value) { return value < 0 ? -value : value; }
long long llabs(long long value) { return value < 0 ? -value : value; }

div_t div(int numerator, int denominator) {
    return (div_t){numerator / denominator, numerator % denominator};
}

ldiv_t ldiv(long numerator, long denominator) {
    return (ldiv_t){numerator / denominator, numerator % denominator};
}

lldiv_t lldiv(long long numerator, long long denominator) {
    return (lldiv_t){numerator / denominator, numerator % denominator};
}

static int digit_value(unsigned char character) {
    if (character >= '0' && character <= '9') {
        return character - '0';
    }
    if (character >= 'a' && character <= 'z') {
        return character - 'a' + 10;
    }
    if (character >= 'A' && character <= 'Z') {
        return character - 'A' + 10;
    }
    return -1;
}

static unsigned long long parse_unsigned(
    const char *string,
    char **end,
    int base,
    unsigned long long maximum,
    int *negative) {
    const char *cursor = string;
    while (*cursor == ' ' || *cursor == '\t' || *cursor == '\n' ||
           *cursor == '\r' || *cursor == '\f' || *cursor == '\v') {
        ++cursor;
    }
    *negative = 0;
    if (*cursor == '+' || *cursor == '-') {
        *negative = *cursor == '-';
        ++cursor;
    }
    if ((base == 0 || base == 16) && cursor[0] == '0' &&
        (cursor[1] == 'x' || cursor[1] == 'X')) {
        base = 16;
        cursor += 2;
    } else if (base == 0) {
        base = cursor[0] == '0' ? 8 : 10;
    }
    if (base < 2 || base > 36) {
        errno = EINVAL;
        if (end != 0) {
            *end = (char *)string;
        }
        return 0;
    }

    const char *digits = cursor;
    unsigned long long value = 0;
    int overflow = 0;
    for (;;) {
        int digit = digit_value((unsigned char)*cursor);
        if (digit < 0 || digit >= base) {
            break;
        }
        if (value > (maximum - (unsigned int)digit) / (unsigned int)base) {
            overflow = 1;
            value = maximum;
        } else if (!overflow) {
            value = value * (unsigned int)base + (unsigned int)digit;
        }
        ++cursor;
    }
    if (cursor == digits) {
        cursor = string;
        value = 0;
    }
    if (overflow) {
        errno = EOVERFLOW;
    }
    if (end != 0) {
        *end = (char *)cursor;
    }
    return value;
}

unsigned long long strtoull(const char *string, char **end, int base) {
    int negative = 0;
    unsigned long long value = parse_unsigned(string, end, base, ULLONG_MAX, &negative);
    return negative ? 0u - value : value;
}

long long strtoll(const char *string, char **end, int base) {
    int negative = 0;
    unsigned long long limit = (unsigned long long)LLONG_MAX + 1u;
    unsigned long long value = parse_unsigned(string, end, base, limit, &negative);
    if (negative) {
        if (value > limit) {
            errno = EOVERFLOW;
            return LLONG_MIN;
        }
        return value == limit ? LLONG_MIN : -(long long)value;
    }
    if (value > (unsigned long long)LLONG_MAX) {
        errno = EOVERFLOW;
        return LLONG_MAX;
    }
    return (long long)value;
}

unsigned long strtoul(const char *string, char **end, int base) {
    int negative = 0;
    unsigned long long value = parse_unsigned(string, end, base, ULONG_MAX, &negative);
    unsigned long result = (unsigned long)value;
    return negative ? 0ul - result : result;
}

long strtol(const char *string, char **end, int base) {
    long long value = strtoll(string, end, base);
    if (value > LONG_MAX) {
        errno = EOVERFLOW;
        return LONG_MAX;
    }
    if (value < LONG_MIN) {
        errno = EOVERFLOW;
        return LONG_MIN;
    }
    return (long)value;
}

int atoi(const char *string) { return (int)strtol(string, 0, 10); }
long atol(const char *string) { return strtol(string, 0, 10); }
long long atoll(const char *string) { return strtoll(string, 0, 10); }

static void swap_bytes(unsigned char *left, unsigned char *right, size_t size) {
    for (size_t index = 0; index < size; ++index) {
        unsigned char value = left[index];
        left[index] = right[index];
        right[index] = value;
    }
}

void qsort(void *base, size_t count, size_t size, int (*compare)(const void *, const void *)) {
    unsigned char *bytes = base;
    if (count < 2 || size == 0 || compare == 0) {
        return;
    }
    for (size_t end = count; end > 1; --end) {
        size_t maximum = 0;
        for (size_t index = 1; index < end; ++index) {
            if (compare(bytes + maximum * size, bytes + index * size) < 0) {
                maximum = index;
            }
        }
        if (maximum != end - 1) {
            swap_bytes(bytes + maximum * size, bytes + (end - 1) * size, size);
        }
    }
}

void *bsearch(
    const void *key,
    const void *base,
    size_t count,
    size_t size,
    int (*compare)(const void *, const void *)) {
    const unsigned char *bytes = base;
    size_t lower = 0;
    size_t upper = count;
    while (lower < upper) {
        size_t middle = lower + (upper - lower) / 2;
        const void *candidate = bytes + middle * size;
        int order = compare(key, candidate);
        if (order < 0) {
            upper = middle;
        } else if (order > 0) {
            lower = middle + 1;
        } else {
            return (void *)candidate;
        }
    }
    return 0;
}

static _Thread_local uint64_t random_state = UINT64_C(0x6a09e667f3bcc909);

void srand(unsigned int seed) {
    random_state = seed == 0 ? UINT64_C(0x6a09e667f3bcc909) : seed;
}

int rand(void) {
    uint64_t value = random_state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    random_state = value;
    return (int)((value >> 33) & RAND_MAX);
}

char *getenv(const char *name) {
    if (name == 0 || *name == '\0' || strchr(name, '=') != 0 || environ == 0) {
        return 0;
    }
    size_t length = strlen(name);
    for (char **entry = environ; *entry != 0; ++entry) {
        if (strncmp(*entry, name, length) == 0 && (*entry)[length] == '=') {
            return *entry + length + 1;
        }
    }
    return 0;
}

_Noreturn void abort(void) {
    mrt_abort();
}
