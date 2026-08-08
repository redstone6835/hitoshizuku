#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/stdlib.h>
#include <ranalib/stdio.h>


static volatile unsigned int lifecycle_state;

__attribute__((constructor(101))) static void first_constructor(void) {
    if (lifecycle_state == 0 && fwrite("ctor\n", 1, 5, stdout) == 5) {
        lifecycle_state = 1;
    }
}

__attribute__((constructor(102))) static void second_constructor(void) {
    if (lifecycle_state == 1 && fwrite("ctor2\n", 1, 6, stdout) == 6) {
        lifecycle_state = 2;
    }
}

__attribute__((destructor(101))) static void final_destructor(void) {
    if (lifecycle_state == 4) {
        (void)fwrite("dtor\n", 1, 5, stdout);
        lifecycle_state = 5;
    }
}

static int check_heap(void) {
    unsigned char *buffer = malloc(8);
    if (buffer == 0) {
        return 0;
    }
    for (unsigned int index = 0; index < 8; ++index) {
        buffer[index] = (unsigned char)(index + 1);
    }
    unsigned char *zeroed = calloc(4, 1);
    if (zeroed == 0 || zeroed[0] != 0 || zeroed[3] != 0) {
        free(buffer);
        free(zeroed);
        return 0;
    }
    unsigned char *grown = realloc(buffer, 32);
    if (grown == 0) {
        free(zeroed);
        return 0;
    }
    for (unsigned int index = 0; index < 8; ++index) {
        if (grown[index] != (unsigned char)(index + 1)) {
            free(grown);
            free(zeroed);
            return 0;
        }
    }
    free(grown);
    free(zeroed);
    return 1;
}

static int check_handles(void) {
    uint64_t stdout_handle = mrt_initial_handle(MYGO_REQUIREMENT_stdout);
    struct mrt_handle_result duplicate = mrt_handle_duplicate(stdout_handle);
    if (duplicate.status != MYGO_STATUS_ok || duplicate.handle == 0) {
        return 0;
    }
    struct mrt_handle_result restricted =
        mrt_handle_restrict(duplicate.handle, MYGO_RIGHT_duplicate);
    if (restricted.status != MYGO_STATUS_ok || restricted.handle == 0) {
        return 0;
    }
    struct mygo_native_result denied =
        mrt_call(MYGO_SLOT_stream_write, restricted.handle, (uintptr_t)"x", 1, 0, 0, 0);
    if (denied.status != MYGO_STATUS_security_rights_denied) {
        return 0;
    }
    if (mrt_handle_close(duplicate.handle) != MYGO_STATUS_ok) {
        return 0;
    }
    struct mygo_native_result stale =
        mrt_call(MYGO_SLOT_stream_write, duplicate.handle, (uintptr_t)"x", 1, 0, 0, 0);
    if (stale.status != MYGO_STATUS_handle_stale) {
        return 0;
    }
    return mrt_handle_close(restricted.handle) == MYGO_STATUS_ok;
}

static int check_clock(void) {
    uint64_t clock_handle = mrt_initial_handle(MYGO_REQUIREMENT_monotonic_clock);
    struct mygo_native_result first =
        mrt_call(MYGO_SLOT_clock_read, clock_handle, 0, 0, 0, 0, 0);
    struct mygo_native_result second =
        mrt_call(MYGO_SLOT_clock_read, clock_handle, 0, 0, 0, 0, 0);
    return clock_handle != 0 && first.status == MYGO_STATUS_ok &&
           second.status == MYGO_STATUS_ok && second.value0 >= first.value0;
}

int main(int argc, char **argv, char **envp) {
    (void)argv;
    (void)envp;
    char input;
    if (argc == 0 || lifecycle_state != 2 || scanf("%c", &input) != 1 || input != 'x' ||
        !check_handles() || !check_clock() || !check_heap() || printf("main %d\n", 37) != 8) {
        return 1;
    }
    lifecycle_state = 4;
    return 37;
}
