#include <assert.h>
#include <stdint.h>
#include <string.h>

#include "mygo_program.h"
#include <mrt/mrt.h>

enum {
    FIXTURE_SIZE = MYGO_START_INFO_SIZE + MYGO_CAPABILITY_COUNT * MYGO_INITIAL_HANDLE_SIZE,
};

struct start_fixture {
    struct mygo_start_info info;
    struct mygo_initial_handle handles[MYGO_CAPABILITY_COUNT];
};

_Static_assert(sizeof(struct start_fixture) == FIXTURE_SIZE, "fixture layout");

struct string_fixture {
    struct mygo_start_info info;
    struct mygo_string_ref argv[1];
    struct mygo_initial_handle handles[MYGO_CAPABILITY_COUNT];
    unsigned char strings[8];
};

_Static_assert(
    sizeof(struct string_fixture) ==
        MYGO_START_INFO_SIZE + MYGO_STRING_REF_SIZE +
            MYGO_CAPABILITY_COUNT * MYGO_INITIAL_HANDLE_SIZE + 8,
    "string fixture layout");

static struct start_fixture valid_fixture(void) {
    struct start_fixture fixture;
    memset(&fixture, 0, sizeof(fixture));

    memcpy(fixture.info.magic, "syst", 4);
    fixture.info.version = 1;
    fixture.info.header_size = MYGO_START_INFO_SIZE;
    fixture.info.total_size = sizeof(fixture);
    fixture.info.abi_epoch = MYGO_ABI_EPOCH;
    fixture.info.target_arch = MYGO_TARGET_ARCH;
    fixture.info.image_base = UINT64_C(0x400000);
    fixture.info.page_size = MYGO_PAGE_SIZE;
    fixture.info.initial_handle_count = MYGO_CAPABILITY_COUNT;
    fixture.info.initial_handle_record_size = MYGO_INITIAL_HANDLE_SIZE;
    fixture.info.initial_handle_offset = MYGO_START_INFO_SIZE;
    fixture.info.call_slot_count = MYGO_CALL_SLOT_COUNT;
    fixture.info.random_seed[0] = 1;

    fixture.handles[0].requirement_id = MYGO_REQUIREMENT_self_process;
    fixture.handles[0].object_interface = MYGO_INTERFACE_process;
    fixture.handles[0].handle = UINT64_C(0x0000000100000001);
    fixture.handles[0].granted_rights = MYGO_RIGHT_exit;

    fixture.handles[1].requirement_id = MYGO_REQUIREMENT_current_address_space;
    fixture.handles[1].object_interface = MYGO_INTERFACE_address_space;
    fixture.handles[1].handle = UINT64_C(0x0000000100000002);
    fixture.handles[1].granted_rights = MYGO_RIGHT_allocate | MYGO_RIGHT_free;

    fixture.handles[2].requirement_id = MYGO_REQUIREMENT_stdin;
    fixture.handles[2].object_interface = MYGO_INTERFACE_stream;
    fixture.handles[2].handle = UINT64_C(0x0000000100000003);
    fixture.handles[2].granted_rights = MYGO_RIGHT_read;

    fixture.handles[3].requirement_id = MYGO_REQUIREMENT_stdout;
    fixture.handles[3].object_interface = MYGO_INTERFACE_stream;
    fixture.handles[3].handle = UINT64_C(0x0000000100000004);
    fixture.handles[3].granted_rights = MYGO_RIGHT_write | MYGO_RIGHT_duplicate;
    fixture.handles[4].requirement_id = MYGO_REQUIREMENT_monotonic_clock;
    fixture.handles[4].object_interface = MYGO_INTERFACE_clock;
    fixture.handles[4].handle = UINT64_C(0x0000000100000005);
    fixture.handles[4].granted_rights = MYGO_RIGHT_read;
    return fixture;
}

static void assert_zero_view(const struct mrt_start_view *view) {
    const unsigned char *bytes = (const unsigned char *)view;
    for (unsigned long index = 0; index < sizeof(*view); ++index) {
        assert(bytes[index] == 0);
    }
}

static void expect_error(struct start_fixture *fixture, enum mrt_start_error expected) {
    struct mrt_start_view view;
    memset(&view, 0xa5, sizeof(view));
    enum mrt_start_error actual = mrt_validate_start_info(
        &fixture->info,
        sizeof(*fixture),
        UINT64_C(0x400000),
        0,
        &view);
    assert(actual == expected);
    assert_zero_view(&view);
}

static void expect_string_error(
    struct string_fixture *fixture,
    enum mrt_start_error expected) {
    struct mrt_start_view view;
    memset(&view, 0xa5, sizeof(view));
    enum mrt_start_error actual = mrt_validate_start_info(
        &fixture->info,
        sizeof(*fixture),
        UINT64_C(0x400000),
        0,
        &view);
    assert(actual == expected);
    assert_zero_view(&view);
}

static struct string_fixture valid_string_fixture(void) {
    struct start_fixture base = valid_fixture();
    struct string_fixture fixture;
    memset(&fixture, 0, sizeof(fixture));
    fixture.info = base.info;
    fixture.info.total_size = sizeof(fixture);
    fixture.info.argc = 1;
    fixture.info.argv_offset = MYGO_START_INFO_SIZE;
    fixture.info.initial_handle_offset = MYGO_START_INFO_SIZE + MYGO_STRING_REF_SIZE;
    fixture.info.string_bytes_offset =
        MYGO_START_INFO_SIZE + MYGO_STRING_REF_SIZE +
        MYGO_CAPABILITY_COUNT * MYGO_INITIAL_HANDLE_SIZE;
    fixture.info.string_bytes_size = 2;
    fixture.argv[0].offset = fixture.info.string_bytes_offset;
    fixture.argv[0].length = 1;
    for (unsigned int index = 0; index < MYGO_CAPABILITY_COUNT; ++index) {
        fixture.handles[index] = base.handles[index];
    }
    fixture.strings[0] = 'x';
    return fixture;
}

static void valid_start_info_publishes_initial_handles(void) {
    struct start_fixture fixture = valid_fixture();
    struct mrt_start_view view;
    memset(&view, 0, sizeof(view));

    enum mrt_start_error result = mrt_validate_start_info(
        &fixture.info,
        sizeof(fixture),
        UINT64_C(0x400000),
        0,
        &view);

    assert(result == MRT_START_OK);
    assert(view.info == &fixture.info);
    assert(view.self_process == fixture.handles[0].handle);
    assert(view.address_space == fixture.handles[1].handle);
    assert(view.stdin_stream == fixture.handles[2].handle);
    assert(view.stdout_stream == fixture.handles[3].handle);
}

static void malformed_header_and_entry_range_are_rejected(void) {
    struct start_fixture fixture = valid_fixture();
    fixture.info.magic[0] = 'x';
    expect_error(&fixture, MRT_START_BAD_HEADER);

    fixture = valid_fixture();
    fixture.info.version = 2;
    expect_error(&fixture, MRT_START_BAD_HEADER);

    fixture = valid_fixture();
    fixture.info.header_size = MYGO_START_INFO_SIZE - 1;
    expect_error(&fixture, MRT_START_BAD_HEADER);

    fixture = valid_fixture();
    fixture.info.total_size = sizeof(fixture) - 8;
    expect_error(&fixture, MRT_START_BAD_RANGE);
}

static void reserved_and_program_contract_fields_are_rejected(void) {
    struct start_fixture fixture = valid_fixture();
    fixture.info.flags = 1;
    expect_error(&fixture, MRT_START_BAD_RESERVED);

    fixture = valid_fixture();
    fixture.info.reserved4 = 1;
    expect_error(&fixture, MRT_START_BAD_RESERVED);

    fixture = valid_fixture();
    fixture.info.abi_epoch += 1;
    expect_error(&fixture, MRT_START_BAD_CONTRACT);

    fixture = valid_fixture();
    fixture.info.target_arch += 1;
    expect_error(&fixture, MRT_START_BAD_CONTRACT);

    fixture = valid_fixture();
    fixture.info.image_base += MYGO_PAGE_SIZE;
    expect_error(&fixture, MRT_START_BAD_CONTRACT);

    fixture = valid_fixture();
    fixture.info.call_slot_count += 1;
    expect_error(&fixture, MRT_START_BAD_CONTRACT);
}

static void valid_init_fini_arrays_are_published(void) {
    static _Alignas(4096) uint64_t image[512];
    struct start_fixture fixture = valid_fixture();
    fixture.info.image_base = (uint64_t)(uintptr_t)image;
    fixture.info.enabled_features = MYGO_FEATURE_INIT_FINI_ARRAY;
    fixture.info.runtime_flags =
        MYGO_RUNTIME_RUN_INIT_ARRAY | MYGO_RUNTIME_RUN_FINI_ARRAY;
    fixture.info.init_array_offset = 8;
    fixture.info.init_array_count = 1;
    fixture.info.init_array_entry_size = sizeof(uint64_t);
    fixture.info.fini_array_offset = 16;
    fixture.info.fini_array_count = 1;
    fixture.info.fini_array_entry_size = sizeof(uint64_t);

    struct mrt_start_view view;
    memset(&view, 0, sizeof(view));
    enum mrt_start_error result = mrt_validate_start_info(
        &fixture.info,
        sizeof(fixture),
        fixture.info.image_base,
        0,
        &view);

    assert(result == MRT_START_OK);
    assert(view.init_array == &image[1]);
    assert(view.init_array_count == 1);
    assert(view.fini_array == &image[2]);
    assert(view.fini_array_count == 1);
}

static void tls_and_random_seed_invariants_are_rejected(void) {
    struct start_fixture fixture = valid_fixture();
    fixture.info.initial_tls_size = MYGO_PAGE_SIZE;
    expect_error(&fixture, MRT_START_BAD_TLS);

    fixture = valid_fixture();
    fixture.info.enabled_features = MYGO_FEATURE_STATIC_TLS;
    expect_error(&fixture, MRT_START_BAD_TLS);

    fixture = valid_fixture();
    fixture.info.enabled_features = UINT64_C(1) << 63;
    expect_error(&fixture, MRT_START_BAD_CONTRACT);

    fixture = valid_fixture();
    fixture.info.random_seed[0] = 0;
    expect_error(&fixture, MRT_START_BAD_RANDOM);
}

static void static_tls_accepts_template_aligned_size(void) {
    struct start_fixture fixture = valid_fixture();
    struct mrt_start_view view;
    memset(&view, 0, sizeof(view));
    fixture.info.enabled_features = MYGO_FEATURE_STATIC_TLS;
    fixture.info.initial_tls_base = UINT64_C(0x70000020);
    fixture.info.initial_tls_size = 32;
    fixture.info.initial_thread_pointer = fixture.info.initial_tls_base;

    enum mrt_start_error result = mrt_validate_start_info(
        &fixture.info,
        sizeof(fixture),
        UINT64_C(0x400000),
        fixture.info.initial_thread_pointer,
        &view);

    assert(result == MRT_START_OK);
    assert(view.info == &fixture.info);
}

static void malformed_handle_table_is_rejected(void) {
    struct start_fixture fixture = valid_fixture();
    fixture.info.initial_handle_offset += 8;
    expect_error(&fixture, MRT_START_BAD_RANGE);

    fixture = valid_fixture();
    fixture.handles[1].requirement_id = MYGO_REQUIREMENT_self_process;
    expect_error(&fixture, MRT_START_BAD_HANDLES);

    fixture = valid_fixture();
    fixture.handles[0].object_interface = MYGO_INTERFACE_stream;
    expect_error(&fixture, MRT_START_BAD_HANDLES);

    fixture = valid_fixture();
    fixture.handles[1].granted_rights = MYGO_RIGHT_write | MYGO_RIGHT_duplicate;
    expect_error(&fixture, MRT_START_BAD_HANDLES);

    fixture = valid_fixture();
    fixture.handles[0].flags = 1;
    expect_error(&fixture, MRT_START_BAD_HANDLES);
}

static void malformed_string_regions_are_rejected(void) {
    struct start_fixture fixture = valid_fixture();
    fixture.info.argc = 1;
    fixture.info.argv_offset = MYGO_START_INFO_SIZE;
    expect_error(&fixture, MRT_START_BAD_RANGE);

    fixture = valid_fixture();
    fixture.info.string_bytes_size = 1;
    expect_error(&fixture, MRT_START_BAD_RANGE);

    struct string_fixture strings = valid_string_fixture();
    strings.strings[0] = 0;
    expect_string_error(&strings, MRT_START_BAD_STRINGS);

    strings = valid_string_fixture();
    strings.strings[1] = 'y';
    expect_string_error(&strings, MRT_START_BAD_STRINGS);
}

int main(void) {
    valid_start_info_publishes_initial_handles();
    malformed_header_and_entry_range_are_rejected();
    reserved_and_program_contract_fields_are_rejected();
    tls_and_random_seed_invariants_are_rejected();
    static_tls_accepts_template_aligned_size();
    valid_init_fini_arrays_are_published();
    malformed_handle_table_is_rejected();
    malformed_string_regions_are_rejected();
    return 0;
}
