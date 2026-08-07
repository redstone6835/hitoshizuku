#include <assert.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/unistd.h>

static struct mygo_native_result next_result;
static uint64_t captured_slot;
static uint64_t captured_handle;
static uint64_t captured_args[5];
static unsigned int call_count;

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    assert(requirement_id == MYGO_REQUIREMENT_STDOUT);
    return UINT64_C(0x0000000100000002);
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    captured_slot = slot;
    captured_handle = object_handle;
    captured_args[0] = arg0;
    captured_args[1] = arg1;
    captured_args[2] = arg2;
    captured_args[3] = arg3;
    captured_args[4] = arg4;
    ++call_count;
    return next_result;
}

static void reset(uint32_t status, uint64_t value) {
    next_result.status = status;
    next_result.reserved = 0;
    next_result.value0 = value;
    next_result.value1 = 0;
    captured_slot = 0;
    captured_handle = 0;
    for (unsigned int index = 0; index < 5; ++index) {
        captured_args[index] = UINT64_MAX;
    }
    call_count = 0;
    errno = 0;
}

static void stdout_write_uses_generated_slot_and_handle(void) {
    static const char message[] = "hello";
    reset(MYGO_STATUS_OK, sizeof(message) - 1);

    long result = write(1, message, sizeof(message) - 1);

    assert(result == (long)(sizeof(message) - 1));
    assert(errno == 0);
    assert(call_count == 1);
    assert(captured_slot == MYGO_SLOT_STREAM_WRITE);
    assert(captured_handle == UINT64_C(0x0000000100000002));
    assert(captured_args[0] == (uintptr_t)message);
    assert(captured_args[1] == sizeof(message) - 1);
    assert(captured_args[2] == 0);
    assert(captured_args[3] == 0);
    assert(captured_args[4] == 0);
}

static void unsupported_descriptor_is_rejected_without_native_call(void) {
    reset(MYGO_STATUS_OK, 1);
    assert(write(2, "x", 1) == -1);
    assert(errno == EBADF);
    assert(call_count == 0);
}

static void native_status_has_deterministic_errno_mapping(void) {
    const struct {
        uint32_t status;
        int expected_errno;
    } cases[] = {
        {MYGO_STATUS_IO_WOULD_BLOCK, EAGAIN},
        {MYGO_STATUS_IO_FAULT, EFAULT},
        {MYGO_STATUS_IO_CLOSED, EPIPE},
        {MYGO_STATUS_IO_ERROR, EIO},
        {MYGO_STATUS_CORE_INVALID_ARGUMENT, EIO},
    };

    for (unsigned int index = 0; index < sizeof(cases) / sizeof(cases[0]); ++index) {
        reset(cases[index].status, 0);
        assert(write(1, "x", 1) == -1);
        assert(errno == cases[index].expected_errno);
        assert(call_count == 1);
    }
}

int main(void) {
    stdout_write_uses_generated_slot_and_handle();
    unsupported_descriptor_is_rejected_without_native_call();
    native_status_has_deterministic_errno_mapping();
    return 0;
}
