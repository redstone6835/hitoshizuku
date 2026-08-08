#include <assert.h>
#include <stdint.h>
#include <string.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdio.h>

static char output[256];
static size_t output_length;
static const char *input;
static size_t input_length;
static size_t input_offset;

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    switch (requirement_id) {
    case MYGO_REQUIREMENT_stdin:
        return UINT64_C(0x0000000100000001);
    case MYGO_REQUIREMENT_stdout:
        return UINT64_C(0x0000000100000002);
    case MYGO_REQUIREMENT_stderr:
        return UINT64_C(0x0000000100000003);
    default:
        return 0;
    }
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    assert(arg2 == 0 && arg3 == 0 && arg4 == 0);
    if (slot == MYGO_SLOT_stream_write) {
        assert(object_handle == UINT64_C(0x0000000100000002) ||
               object_handle == UINT64_C(0x0000000100000003));
        assert(arg1 <= sizeof(output) - output_length);
        memcpy(output + output_length, (const void *)(uintptr_t)arg0, (size_t)arg1);
        output_length += (size_t)arg1;
        return (struct mygo_native_result){MYGO_STATUS_ok, 0, arg1, 0};
    }
    assert(slot == MYGO_SLOT_stream_read);
    assert(object_handle == UINT64_C(0x0000000100000001));
    size_t remaining = input_length - input_offset;
    size_t amount = arg1 < remaining ? (size_t)arg1 : remaining;
    memcpy((void *)(uintptr_t)arg0, input + input_offset, amount);
    input_offset += amount;
    uint32_t status = amount == 0 ? MYGO_STATUS_stream_end : MYGO_STATUS_ok;
    return (struct mygo_native_result){status, 0, amount, 0};
}

static void reset(const char *new_input) {
    memset(output, 0, sizeof(output));
    output_length = 0;
    input = new_input;
    input_length = strlen(new_input);
    input_offset = 0;
    errno = 0;
}

static void standard_streams_use_native_stream_capabilities(void) {
    char bytes[3] = {0};
    reset("abc");
    assert(fread(bytes, 1, sizeof(bytes), stdin) == sizeof(bytes));
    assert(memcmp(bytes, "abc", sizeof(bytes)) == 0);
    assert(fwrite(bytes, 1, sizeof(bytes), stdout) == sizeof(bytes));
    assert(output_length == sizeof(bytes));
    assert(memcmp(output, "abc", sizeof(bytes)) == 0);
}

static void printf_supports_the_p0_conversion_set(void) {
    static const char expected[] = "-7 9 2a Z ok 0x1234 %";
    reset("");
    int written = printf(
        "%d %u %x %c %s %p %%",
        -7,
        9u,
        0x2au,
        'Z',
        "ok",
        (void *)(uintptr_t)0x1234);
    assert(written == (int)(sizeof(expected) - 1));
    assert(output_length == sizeof(expected) - 1);
    assert(memcmp(output, expected, sizeof(expected) - 1) == 0);
}

static void scanf_character_conversion_writes_a_char(void) {
    char value = 0;
    reset("Q");
    assert(scanf("%c", &value) == 1);
    assert(value == 'Q');
}

static void scanf_returns_eof_when_input_fails_before_conversion(void) {
    char value = 0;
    reset("");
    assert(scanf("%c", &value) == EOF);
    assert(value == 0);
}

int main(void) {
    standard_streams_use_native_stream_capabilities();
    printf_supports_the_p0_conversion_set();
    scanf_character_conversion_writes_a_char();
    scanf_returns_eof_when_input_fails_before_conversion();
    return 0;
}
