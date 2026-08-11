#include <assert.h>
#include <stdint.h>
#include <string.h>

#include <mrt/mrt.h>

struct optional_fixture {
    struct mygo_start_info info;
    struct mygo_initial_handle handles[MYGO_CAPABILITY_COUNT];
};

static struct optional_fixture fixture_with_count(uint32_t count) {
    struct optional_fixture fixture;
    memset(&fixture, 0, sizeof(fixture));
    fixture.info.magic[0] = 's';
    fixture.info.magic[1] = 'y';
    fixture.info.magic[2] = 's';
    fixture.info.magic[3] = 't';
    fixture.info.version = 1;
    fixture.info.header_size = MYGO_START_INFO_SIZE;
    fixture.info.total_size = MYGO_START_INFO_SIZE + count * MYGO_INITIAL_HANDLE_SIZE;
    fixture.info.abi_epoch = MYGO_ABI_EPOCH;
    fixture.info.target_arch = MYGO_TARGET_ARCH;
    fixture.info.image_base = UINT64_C(0x400000);
    fixture.info.page_size = MYGO_PAGE_SIZE;
    fixture.info.initial_handle_count = count;
    fixture.info.initial_handle_record_size = MYGO_INITIAL_HANDLE_SIZE;
    fixture.info.initial_handle_offset = MYGO_START_INFO_SIZE;
    fixture.info.call_slot_count = MYGO_CALL_SLOT_COUNT;
    fixture.info.random_seed[0] = 1;

    fixture.handles[0].requirement_id = MYGO_REQUIREMENT_self_process;
    fixture.handles[0].object_interface = MYGO_INTERFACE_process;
    fixture.handles[0].handle = UINT64_C(0x0000000100000001);
    fixture.handles[0].granted_rights = MYGO_RIGHT_exit;
    fixture.handles[1].requirement_id = MYGO_REQUIREMENT_stdin;
    fixture.handles[1].object_interface = MYGO_INTERFACE_stream;
    fixture.handles[1].handle = UINT64_C(0x0000000100000002);
    fixture.handles[1].granted_rights = MYGO_RIGHT_read;
    fixture.handles[2].requirement_id = MYGO_REQUIREMENT_stdout;
    fixture.handles[2].object_interface = MYGO_INTERFACE_stream;
    fixture.handles[2].handle = UINT64_C(0x0000000100000003);
    fixture.handles[2].granted_rights = MYGO_RIGHT_write;
    return fixture;
}

static enum mrt_start_error validate(struct optional_fixture *fixture) {
    struct mrt_start_view view;
    memset(&view, 0, sizeof(view));
    enum mrt_start_error result = mrt_validate_start_info(
        &fixture->info,
        fixture->info.total_size,
        fixture->info.image_base,
        0,
        &view);
    if (result == MRT_START_OK) {
        assert(view.initial_handle_count == fixture->info.initial_handle_count);
        assert(view.self_process == fixture->handles[0].handle);
        assert(view.stdout_stream == fixture->handles[2].handle);
        if (fixture->info.initial_handle_count == 3) {
            assert(view.stdin_stream == fixture->handles[1].handle);
        } else {
            assert(view.stdin_stream == 0);
        }
    }
    return result;
}

int main(void) {
    struct optional_fixture fixture = fixture_with_count(2);
    fixture.handles[1] = fixture.handles[2];
    assert(validate(&fixture) == MRT_START_OK);

    fixture = fixture_with_count(3);
    assert(validate(&fixture) == MRT_START_OK);

    fixture = fixture_with_count(1);
    assert(validate(&fixture) == MRT_START_BAD_HANDLES);
    return 0;
}
