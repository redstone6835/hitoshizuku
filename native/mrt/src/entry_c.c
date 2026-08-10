#include <limits.h>
#include <stddef.h>
#include <stdint.h>

#include <mrt/mrt.h>

#if !MYGO_HAS_memory_allocate
#error "mrt C 入口要求 manifest 导入 memory.allocate"
#endif

#if !MYGO_CAP_current_address_space_required
#error "mrt C 入口要求 manifest 声明 current_address_space capability"
#endif

static char **program_argv;
static char **program_envp;
static int program_argc;

char **environ;

static int checked_add(uint64_t left, uint64_t right, uint64_t *result) {
    if (left > UINT64_MAX - right) {
        return 0;
    }
    *result = left + right;
    return 1;
}

static int checked_align(uint64_t value, uint64_t alignment, uint64_t *result) {
    uint64_t adjusted;
    if (!checked_add(value, alignment - 1, &adjusted)) {
        return 0;
    }
    *result = adjusted & ~(alignment - 1);
    return 1;
}

static void copy_bytes(unsigned char *destination, const unsigned char *source, uint64_t length) {
    for (uint64_t index = 0; index < length; ++index) {
        destination[index] = source[index];
    }
}

static int copy_string_array(
    const struct mygo_start_info *info,
    const struct mygo_string_ref *refs,
    uint32_t count,
    char **pointers,
    unsigned char *strings,
    uint64_t *string_cursor) {
    const unsigned char *base = (const unsigned char *)info;
    for (uint32_t index = 0; index < count; ++index) {
        uint64_t length = refs[index].length;
        uint64_t offset = refs[index].offset;
        pointers[index] = (char *)(strings + *string_cursor);
        copy_bytes(strings + *string_cursor, base + offset, length + 1);
        if (!checked_add(*string_cursor, length + 1, string_cursor)) {
            return 0;
        }
    }
    pointers[count] = 0;
    return 1;
}

int mrt_prepare_program(const struct mrt_start_view *view) {
    if (view == 0 || view->info == 0 || view->info->argc > INT_MAX) {
        return -1;
    }

    const struct mygo_start_info *info = view->info;
    uint64_t pointer_count = (uint64_t)info->argc + (uint64_t)info->envc + 2;
    uint64_t pointer_bytes;
    uint64_t arena_bytes;
    if (pointer_count > UINT64_MAX / sizeof(char *) ||
        !checked_add(pointer_count * sizeof(char *), info->string_bytes_size, &arena_bytes) ||
        !checked_align(arena_bytes, MYGO_PAGE_SIZE, &arena_bytes) || arena_bytes == 0 ||
        !checked_add((uint64_t)info->argc, 1, &pointer_bytes)) {
        return -1;
    }

    uint64_t address_space = mrt_initial_handle(MYGO_REQUIREMENT_current_address_space);
    if (address_space == 0) {
        return -1;
    }
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_memory_allocate,
        address_space,
        arena_bytes,
        MYGO_PAGE_SIZE,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_ok || result.value0 == 0) {
        return -1;
    }

    unsigned char *arena = (unsigned char *)(uintptr_t)result.value0;
    char **argv = (char **)arena;
    char **envp = argv + pointer_bytes;
    unsigned char *strings = (unsigned char *)(envp + info->envc + 1);
    uint64_t string_cursor = 0;
    const struct mygo_string_ref *argv_refs =
        info->argc == 0 ? 0 : (const struct mygo_string_ref *)((const unsigned char *)info + info->argv_offset);
    const struct mygo_string_ref *env_refs =
        info->envc == 0 ? 0 : (const struct mygo_string_ref *)((const unsigned char *)info + info->env_offset);
    if (!copy_string_array(info, argv_refs, info->argc, argv, strings, &string_cursor) ||
        !copy_string_array(info, env_refs, info->envc, envp, strings, &string_cursor) ||
        string_cursor != info->string_bytes_size) {
        return -1;
    }

    program_argc = (int)info->argc;
    program_argv = argv;
    program_envp = envp;
    environ = envp;
    return 0;
}

extern int main(int argc, char **argv, char **envp);

struct mrt_program_result mrt_invoke_program(const struct mrt_start_view *view) {
    (void)view;
    struct mrt_program_result result = {0};
    result.status = main(program_argc, program_argv, program_envp);
    return result;
}
