#include <assert.h>
#include <stdint.h>
#include <string.h>

#include "mygo_program.h"
#include <mrt/mrt.h>

extern char **environ;

static unsigned char arena[4096];
static uint64_t address_space = UINT64_C(0x0000000100000002);
static uint64_t mapped_size;

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    return requirement_id == MYGO_REQUIREMENT_current_address_space ? address_space : 0;
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    assert(slot == MYGO_SLOT_memory_allocate);
    assert(object_handle == address_space);
    assert(arg0 == MYGO_PAGE_SIZE);
    assert(arg1 == MYGO_PAGE_SIZE);
    assert(arg2 == 0 && arg3 == 0 && arg4 == 0);
    mapped_size = arg0;
    struct mygo_native_result result = {
        .status = MYGO_STATUS_ok,
        .value0 = (uintptr_t)arena,
        .value1 = MYGO_PAGE_SIZE,
    };
    return result;
}

_Noreturn void mrt_abort(void) {
    assert(!"unexpected abort");
    __builtin_unreachable();
}

int program_main(int argc, char **argv, char **envp) {
    assert(argc == 2);
    assert(strcmp(argv[0], "native") == 0);
    assert((unsigned char)argv[1][0] == 0xff);
    assert(argv[1][1] == 'x');
    assert(argv[2] == 0);
    assert(strcmp(envp[0], "A=B") == 0);
    assert((unsigned char)envp[1][0] == 0xfe);
    assert(envp[1][1] == 'y');
    assert(envp[2] == 0);
    assert(environ == envp);
    return 73;
}

struct fixture {
    struct mygo_start_info info;
    struct mygo_string_ref argv[2];
    struct mygo_string_ref envp[2];
    unsigned char strings[17];
};

static void make_fixture(struct fixture *fixture) {
    struct mygo_start_info *info = &fixture->info;
    unsigned char *storage = (unsigned char *)fixture;
    struct mygo_string_ref *argv = fixture->argv;
    struct mygo_string_ref *envp = argv + 2;
    unsigned char *strings = (unsigned char *)(envp + 2);
    memset(storage, 0, sizeof(*fixture));
    info->argc = 2;
    info->envc = 2;
    info->argv_offset = 192;
    info->env_offset = 192 + 2 * sizeof(*argv);
    info->string_bytes_offset = 192 + 4 * sizeof(*argv);
    info->string_bytes_size = sizeof(fixture->strings);
    memcpy(strings, "native\0", 7);
    strings[7] = 0xff;
    strings[8] = 'x';
    strings[9] = 0;
    memcpy(strings + 10, "A=B\0", 4);
    strings[14] = 0xfe;
    strings[15] = 'y';
    strings[16] = 0;
    argv[0] = (struct mygo_string_ref){info->string_bytes_offset, 6};
    argv[1] = (struct mygo_string_ref){info->string_bytes_offset + 7, 2};
    envp[0] = (struct mygo_string_ref){info->string_bytes_offset + 10, 3};
    envp[1] = (struct mygo_string_ref){info->string_bytes_offset + 14, 2};
}

int main(void) {
    struct fixture fixture;
    make_fixture(&fixture);
    struct mrt_start_view view = {.info = &fixture.info};
    assert(mrt_prepare_program(&view) == 0);
    assert(mapped_size == MYGO_PAGE_SIZE);
    struct mrt_program_result result = mrt_invoke_program(&view);
    assert(result.status == 73);
    return 0;
}
