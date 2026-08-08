#include <limits.h>
#include <stdarg.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdio.h>

struct ranalib_file {
    uint32_t requirement;
};

static struct ranalib_file stdin_file = {MYGO_REQUIREMENT_stdin};
static struct ranalib_file stdout_file = {MYGO_REQUIREMENT_stdout};
static struct ranalib_file stderr_file = {MYGO_REQUIREMENT_stderr};

FILE *stdin = &stdin_file;
FILE *stdout = &stdout_file;
FILE *stderr = &stderr_file;

static int status_errno(uint32_t status) {
    switch (status) {
    case MYGO_STATUS_stream_would_block:
        return EAGAIN;
    case MYGO_STATUS_stream_fault:
        return EFAULT;
    case MYGO_STATUS_stream_closed:
        return EPIPE;
    default:
        return EIO;
    }
}

static uint64_t stream_handle(FILE *stream) {
    return stream == 0 ? 0 : mrt_initial_handle(stream->requirement);
}

size_t fwrite(const void *buffer, size_t size, size_t count, FILE *stream) {
    if (size == 0 || count == 0) {
        return 0;
    }
    if (buffer == 0 || stream == 0 || count > SIZE_MAX / size) {
        errno = EFAULT;
        return 0;
    }
    uint64_t handle = stream_handle(stream);
    if (handle == 0) {
        errno = EBADF;
        return 0;
    }
    size_t bytes = count * size;
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_stream_write, handle, (uintptr_t)buffer, bytes, 0, 0, 0);
    if (result.status != MYGO_STATUS_ok) {
        errno = status_errno(result.status);
        return result.value0 == 0 ? 0 : result.value0 / size;
    }
    return result.value0 / size;
}

size_t fread(void *buffer, size_t size, size_t count, FILE *stream) {
    if (size == 0 || count == 0) {
        return 0;
    }
    if (buffer == 0 || stream == 0 || count > SIZE_MAX / size) {
        errno = EFAULT;
        return 0;
    }
    uint64_t handle = stream_handle(stream);
    if (handle == 0) {
        errno = EBADF;
        return 0;
    }
    size_t bytes = count * size;
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_stream_read, handle, (uintptr_t)buffer, bytes, 0, 0, 0);
    if (result.status == MYGO_STATUS_stream_end) {
        return result.value0 / size;
    }
    if (result.status != MYGO_STATUS_ok) {
        errno = status_errno(result.status);
        return result.value0 == 0 ? 0 : result.value0 / size;
    }
    return result.value0 / size;
}

int fputc(int character, FILE *stream) {
    unsigned char value = (unsigned char)character;
    return fwrite(&value, 1, 1, stream) == 1 ? (int)value : EOF;
}

int fgetc(FILE *stream) {
    unsigned char value = 0;
    size_t result = fread(&value, 1, 1, stream);
    return result == 1 ? (int)value : EOF;
}

static size_t format_unsigned(char *buffer, uint64_t value, unsigned base) {
    static const char digits[] = "0123456789abcdef";
    char reversed[32];
    size_t length = 0;
    do {
        reversed[length++] = digits[value % base];
        value /= base;
    } while (value != 0 && length < sizeof(reversed));
    for (size_t index = 0; index < length; ++index) {
        buffer[index] = reversed[length - index - 1];
    }
    return length;
}

static int put_string(const char *value) {
    if (value == 0) {
        value = "(null)";
    }
    size_t length = 0;
    while (value[length] != 0) {
        ++length;
    }
    return fwrite(value, 1, length, stdout) == length ? (int)length : -1;
}

int printf(const char *format, ...) {
    if (format == 0) {
        errno = EFAULT;
        return -1;
    }
    va_list arguments;
    va_start(arguments, format);
    int written = 0;
    for (size_t index = 0; format[index] != 0; ++index) {
        if (format[index] != '%') {
            if (fputc((unsigned char)format[index], stdout) == EOF) {
                va_end(arguments);
                return -1;
            }
            ++written;
            continue;
        }
        char conversion = format[++index];
        char number[32];
        int count = 0;
        if (conversion == '%') {
            count = fputc('%', stdout) == EOF ? -1 : 1;
        } else if (conversion == 's') {
            count = put_string(va_arg(arguments, const char *));
        } else if (conversion == 'c') {
            count = fputc(va_arg(arguments, int), stdout) == EOF ? -1 : 1;
        } else if (conversion == 'p') {
            void *pointer = va_arg(arguments, void *);
            number[0] = '0';
            number[1] = 'x';
            size_t length = format_unsigned(
                number + 2, (uint64_t)(uintptr_t)pointer, 16);
            count = fwrite(number, 1, length + 2, stdout) == length + 2
                ? (int)(length + 2)
                : -1;
        } else if (conversion == 'u' || conversion == 'x') {
            size_t length = format_unsigned(
                number, va_arg(arguments, unsigned int), conversion == 'x' ? 16 : 10);
            count = fwrite(number, 1, length, stdout) == length ? (int)length : -1;
        } else if (conversion == 'd') {
            int value = va_arg(arguments, int);
            size_t offset = 0;
            if (value < 0) {
                number[offset++] = '-';
            }
            uint64_t magnitude = value < 0 ? (uint64_t)(-(int64_t)value) : (uint64_t)value;
            size_t length = format_unsigned(number + offset, magnitude, 10);
            count = fwrite(number, 1, offset + length, stdout) == offset + length
                ? (int)(offset + length)
                : -1;
        } else {
            errno = EINVAL;
            va_end(arguments);
            return -1;
        }
        if (count < 0) {
            va_end(arguments);
            return -1;
        }
        written += count;
    }
    va_end(arguments);
    return written;
}

int scanf(const char *format, ...) {
    if (format == 0) {
        errno = EFAULT;
        return EOF;
    }
    va_list arguments;
    va_start(arguments, format);
    int converted = 0;
    int input_failure = 0;
    for (size_t index = 0; format[index] != 0; ++index) {
        if (format[index] != '%') {
            int input = fgetc(stdin);
            if (input == EOF) {
                input_failure = 1;
                break;
            }
            if (input != (unsigned char)format[index]) {
                break;
            }
            continue;
        }
        char conversion = format[++index];
        if (conversion == 'c') {
            char *value = va_arg(arguments, char *);
            int input = fgetc(stdin);
            if (input == EOF) {
                input_failure = 1;
                break;
            }
            if (value == 0) {
                break;
            }
            *value = (char)input;
            ++converted;
        } else {
            errno = EINVAL;
            break;
        }
    }
    va_end(arguments);
    return converted == 0 && input_failure ? EOF : converted;
}
