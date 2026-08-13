#include <limits.h>
#include <stdarg.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/ctype.h>
#include <ranalib/errno.h>
#include <ranalib/stdio.h>
#include <ranalib/stdlib.h>

struct ranalib_file {
    uint32_t requirement;
    unsigned char end;
    unsigned char error;
    unsigned char has_pushback;
    unsigned char pushback;
};

static struct ranalib_file stdin_file = {MYGO_REQUIREMENT_stdin, 0, 0, 0, 0};
static struct ranalib_file stdout_file = {MYGO_REQUIREMENT_stdout, 0, 0, 0, 0};
static struct ranalib_file stderr_file = {MYGO_REQUIREMENT_stderr, 0, 0, 0, 0};

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
        stream->error = 1;
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
        stream->end = 1;
        return result.value0 / size;
    }
    if (result.status != MYGO_STATUS_ok) {
        errno = status_errno(result.status);
        stream->error = 1;
        return result.value0 == 0 ? 0 : result.value0 / size;
    }
    return result.value0 / size;
}

int fputc(int character, FILE *stream) {
    unsigned char value = (unsigned char)character;
    return fwrite(&value, 1, 1, stream) == 1 ? (int)value : EOF;
}

int fgetc(FILE *stream) {
    if (stream != 0 && stream->has_pushback) {
        stream->has_pushback = 0;
        stream->end = 0;
        return stream->pushback;
    }
    unsigned char value = 0;
    size_t result = fread(&value, 1, 1, stream);
    return result == 1 ? (int)value : EOF;
}

int ungetc(int character, FILE *stream) {
    if (stream == 0 || character == EOF || stream->has_pushback) {
        return EOF;
    }
    stream->pushback = (unsigned char)character;
    stream->has_pushback = 1;
    stream->end = 0;
    return stream->pushback;
}

int getchar(void) { return fgetc(stdin); }
int putchar(int character) { return fputc(character, stdout); }

int puts(const char *string) {
    if (string == 0) {
        errno = EFAULT;
        return EOF;
    }
    size_t length = 0;
    while (string[length] != 0) {
        ++length;
    }
    return fwrite(string, 1, length, stdout) == length && fputc('\n', stdout) != EOF
        ? 0
        : EOF;
}

int fflush(FILE *stream) {
    if (stream == 0) {
        return 0;
    }
    return stream_handle(stream) == 0 ? EOF : 0;
}

int feof(FILE *stream) { return stream != 0 && stream->end != 0; }
int ferror(FILE *stream) { return stream != 0 && stream->error != 0; }
void clearerr(FILE *stream) {
    if (stream != 0) {
        stream->end = 0;
        stream->error = 0;
    }
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

struct format_output {
    FILE *stream;
    char *buffer;
    size_t capacity;
    size_t used;
    int failed;
};

static void emit(struct format_output *output, const char *bytes, size_t length) {
    if (output->failed) {
        return;
    }
    if (output->stream != 0) {
        if (fwrite(bytes, 1, length, output->stream) != length) {
            output->failed = 1;
            return;
        }
    } else if (output->capacity != 0 && output->buffer != 0) {
        size_t writable = output->capacity - 1;
        if (output->used < writable) {
            size_t available = writable - output->used;
            size_t copied = length < available ? length : available;
            for (size_t index = 0; index < copied; ++index) {
                output->buffer[output->used + index] = bytes[index];
            }
        }
    }
    output->used += length;
}

static void emit_padding(struct format_output *output, char character, size_t count) {
    char padding[16];
    for (size_t index = 0; index < sizeof(padding); ++index) {
        padding[index] = character;
    }
    while (count != 0) {
        size_t chunk = count < sizeof(padding) ? count : sizeof(padding);
        emit(output, padding, chunk);
        count -= chunk;
    }
}

static size_t string_length(const char *value, size_t precision) {
    size_t length = 0;
    while (value[length] != 0 && length < precision) {
        ++length;
    }
    return length;
}

static void put_string(
    struct format_output *output,
    const char *value,
    size_t width,
    size_t precision,
    int left) {
    if (value == 0) {
        value = "(null)";
    }
    size_t length = string_length(value, precision);
    if (!left && width > length) {
        emit_padding(output, ' ', width - length);
    }
    emit(output, value, length);
    if (left && width > length) {
        emit_padding(output, ' ', width - length);
    }
}

static int format_output(
    struct format_output *output,
    const char *format,
    va_list arguments) {
    if (format == 0) {
        errno = EFAULT;
        return -1;
    }
    for (size_t index = 0; format[index] != 0; ++index) {
        if (format[index] != '%') {
            emit(output, &format[index], 1);
            continue;
        }
        ++index;
        int left = 0;
        int plus = 0;
        int alternate = 0;
        int zero = 0;
        for (;;) {
            if (format[index] == '-') left = 1;
            else if (format[index] == '+') plus = 1;
            else if (format[index] == '#') alternate = 1;
            else if (format[index] == '0') zero = 1;
            else break;
            ++index;
        }
        size_t width = 0;
        while (format[index] >= '0' && format[index] <= '9') {
            width = width * 10 + (unsigned char)format[index++] - '0';
        }
        size_t precision = SIZE_MAX;
        if (format[index] == '.') {
            precision = 0;
            ++index;
            while (format[index] >= '0' && format[index] <= '9') {
                precision = precision * 10 + (unsigned char)format[index++] - '0';
            }
        }
        int length = 0;
        if (format[index] == 'l') {
            length = 1;
            if (format[++index] == 'l') {
                length = 2;
                ++index;
            }
        } else if (format[index] == 'z') {
            length = 3;
            ++index;
        }
        char conversion = format[index];
        char number[66];
        if (conversion == '%') {
            emit(output, "%", 1);
        } else if (conversion == 's') {
            put_string(output, va_arg(arguments, const char *), width, precision, left);
        } else if (conversion == 'c') {
            char character = (char)va_arg(arguments, int);
            if (!left && width > 1) emit_padding(output, ' ', width - 1);
            emit(output, &character, 1);
            if (left && width > 1) emit_padding(output, ' ', width - 1);
        } else if (conversion == 'p') {
            void *pointer = va_arg(arguments, void *);
            number[0] = '0';
            number[1] = 'x';
            size_t digits = format_unsigned(
                number + 2, (uint64_t)(uintptr_t)pointer, 16);
            emit(output, number, digits + 2);
        } else if (conversion == 'u' || conversion == 'x' || conversion == 'X' ||
                   conversion == 'o' || conversion == 'd' || conversion == 'i') {
            int signed_conversion = conversion == 'd' || conversion == 'i';
            uint64_t value = 0;
            int negative = 0;
            if (signed_conversion) {
                int64_t signed_value = length == 2
                    ? va_arg(arguments, long long)
                    : length == 1
                        ? va_arg(arguments, long)
                        : length == 3
                            ? (int64_t)va_arg(arguments, ptrdiff_t)
                            : va_arg(arguments, int);
                negative = signed_value < 0;
                value = negative ? 0u - (uint64_t)signed_value : (uint64_t)signed_value;
            } else {
                value = length == 2
                    ? va_arg(arguments, unsigned long long)
                    : length == 1
                        ? va_arg(arguments, unsigned long)
                        : length == 3
                            ? va_arg(arguments, size_t)
                            : va_arg(arguments, unsigned int);
            }
            unsigned base = conversion == 'o' ? 8 :
                (conversion == 'x' || conversion == 'X') ? 16 : 10;
            size_t prefix = 0;
            if (negative) number[prefix++] = '-';
            else if (plus && signed_conversion) number[prefix++] = '+';
            if (alternate && value != 0 && base == 16) {
                number[prefix++] = '0';
                number[prefix++] = conversion == 'X' ? 'X' : 'x';
            } else if (alternate && value != 0 && base == 8) {
                number[prefix++] = '0';
            }
            size_t digits = format_unsigned(number + prefix, value, base);
            if (conversion == 'X') {
                for (size_t position = prefix; position < prefix + digits; ++position) {
                    if (number[position] >= 'a' && number[position] <= 'f') {
                        number[position] -= 'a' - 'A';
                    }
                }
            }
            size_t total = prefix + digits;
            if (!left && width > total) emit_padding(output, zero ? '0' : ' ', width - total);
            emit(output, number, total);
            if (left && width > total) emit_padding(output, ' ', width - total);
        } else {
            errno = EINVAL;
            return -1;
        }
        if (output->failed || output->used > INT_MAX) {
            errno = output->used > INT_MAX ? EOVERFLOW : errno;
            return -1;
        }
    }
    if (output->stream == 0 && output->capacity != 0 && output->buffer != 0) {
        size_t terminator = output->used < output->capacity - 1
            ? output->used
            : output->capacity - 1;
        output->buffer[terminator] = '\0';
    }
    return (int)output->used;
}

int vfprintf(FILE *stream, const char *format, va_list arguments) {
    if (stream == 0) {
        errno = EBADF;
        return -1;
    }
    struct format_output output = {stream, 0, 0, 0, 0};
    return format_output(&output, format, arguments);
}

int fprintf(FILE *stream, const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int result = vfprintf(stream, format, arguments);
    va_end(arguments);
    return result;
}

int vprintf(const char *format, va_list arguments) {
    return vfprintf(stdout, format, arguments);
}

int printf(const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int result = vfprintf(stdout, format, arguments);
    va_end(arguments);
    return result;
}

int vsnprintf(char *buffer, size_t capacity, const char *format, va_list arguments) {
    if (capacity != 0 && buffer == 0) {
        errno = EFAULT;
        return -1;
    }
    struct format_output output = {0, buffer, capacity, 0, 0};
    return format_output(&output, format, arguments);
}

int snprintf(char *buffer, size_t capacity, const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int result = vsnprintf(buffer, capacity, format, arguments);
    va_end(arguments);
    return result;
}

static int skip_input_space(FILE *stream) {
    int input = 0;
    do {
        input = fgetc(stream);
    } while (input != EOF && isspace(input));
    if (input != EOF) {
        (void)ungetc(input, stream);
    }
    return input;
}

int vfscanf(FILE *stream, const char *format, va_list arguments) {
    if (format == 0) {
        errno = EFAULT;
        return EOF;
    }
    int converted = 0;
    int input_failure = 0;
    for (size_t index = 0; format[index] != 0; ++index) {
        if (isspace((unsigned char)format[index])) {
            while (isspace((unsigned char)format[index + 1])) {
                ++index;
            }
            if (skip_input_space(stream) == EOF) {
                input_failure = 1;
                break;
            }
            continue;
        }
        if (format[index] != '%') {
            int input = fgetc(stream);
            if (input == EOF) {
                input_failure = 1;
                break;
            }
            if (input != (unsigned char)format[index]) {
                break;
            }
            continue;
        }
        ++index;
        size_t width = 0;
        while (format[index] >= '0' && format[index] <= '9') {
            width = width * 10 + (unsigned char)format[index++] - '0';
        }
        char conversion = format[index];
        if (conversion == '%') {
            int input = fgetc(stream);
            if (input == EOF) input_failure = 1;
            else if (input != '%') (void)ungetc(input, stream);
            continue;
        }
        if (conversion == 'c') {
            char *value = va_arg(arguments, char *);
            size_t count = width == 0 ? 1 : width;
            if (value == 0) {
                break;
            }
            size_t read = 0;
            while (read < count) {
                int input = fgetc(stream);
                if (input == EOF) {
                    input_failure = 1;
                    break;
                }
                value[read++] = (char)input;
            }
            if (read != count) {
                break;
            }
            ++converted;
        } else if (conversion == 's') {
            char *value = va_arg(arguments, char *);
            if (value == 0 || skip_input_space(stream) == EOF) {
                input_failure = 1;
                break;
            }
            size_t limit = width == 0 ? SIZE_MAX : width;
            size_t read = 0;
            while (read < limit) {
                int input = fgetc(stream);
                if (input == EOF || isspace(input)) {
                    if (input != EOF) (void)ungetc(input, stream);
                    break;
                }
                value[read++] = (char)input;
            }
            if (read == 0) {
                break;
            }
            value[read] = '\0';
            ++converted;
        } else if (conversion == 'd' || conversion == 'i' || conversion == 'u' ||
                   conversion == 'x') {
            if (skip_input_space(stream) == EOF) {
                input_failure = 1;
                break;
            }
            char token[96];
            size_t limit = width == 0 || width >= sizeof(token) ? sizeof(token) - 1 : width;
            size_t read = 0;
            while (read < limit) {
                int input = fgetc(stream);
                if (input == EOF || isspace(input)) {
                    if (input != EOF) (void)ungetc(input, stream);
                    break;
                }
                token[read++] = (char)input;
            }
            token[read] = '\0';
            char *end = token;
            int base = conversion == 'i' ? 0 : conversion == 'x' ? 16 : 10;
            if (conversion == 'd' || conversion == 'i') {
                long value = strtol(token, &end, base);
                if (end == token) break;
                *va_arg(arguments, int *) = (int)value;
            } else {
                unsigned long value = strtoul(token, &end, base);
                if (end == token) break;
                *va_arg(arguments, unsigned int *) = (unsigned int)value;
            }
            ++converted;
        } else {
            errno = EINVAL;
            break;
        }
    }
    return converted == 0 && input_failure ? EOF : converted;
}

int fscanf(FILE *stream, const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int result = vfscanf(stream, format, arguments);
    va_end(arguments);
    return result;
}

int vscanf(const char *format, va_list arguments) {
    return vfscanf(stdin, format, arguments);
}

int scanf(const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int result = vfscanf(stdin, format, arguments);
    va_end(arguments);
    return result;
}
