#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define SYS_ELM_CTL 509

#define ELM_CTL_MAGIC 0x314d4c45u
#define ELM_CTL_ABI_VERSION 1u
#define ELM_CTL_CMD_CORE_QUERY 1u
#define ELM_CTL_CMD_MGR_CALL 2u

#define ELM_CORE_CAP_MGR_CHANNEL (1ull << 2)

#define ELM_MGR_STATUS_OK 0
#define ELM_MGR_STATUS_TODO (-4096)
#define ELM_MGR_CALL_QUERY_MENU 1u
#define ELM_MGR_CALL_LOAD_CELL 2u
#define ELM_MGR_CALL_DETACH_CELL 3u
#define ELM_MGR_CALL_QUERY_POLICY 8u
#define ELM_MGR_CALL_QUERY_AUDIT 10u
#define ELM_MGR_CALL_QUERY_NEXUS_BINDINGS 11u
#define ELM_MGR_CALL_COMMIT_BIND 13u
#define ELM_MGR_CALL_INVOKE_PROVIDER 23u
#define ELM_MGR_CALL_QUERY_HEALTH 25u
#define ELM_MGR_CALL_QUERY_API_REGISTRY 30u
#define ELM_MGR_CALL_SUBSCRIBE_EVENT 31u
#define ELM_MGR_CALL_UNSUBSCRIBE_EVENT 32u
#define ELM_MGR_CALL_QUERY_EVENT_SUBSCRIPTIONS 33u
#define ELM_MGR_CALL_READ_SUBSCRIBED_EVENTS 34u
#define ELM_MGR_MAX_INPUT (4096u + 16u)

#define ELM_MGR_BUILTIN_ID 1ull
#define ELM_MGR_ACTION_PORT_ID 4ull
#define ELM_MGR_ACTION_PROVIDER_INVOKE (1u << 12)
#define ELM_MGR_ACTION_HEALTH_QUERY (1u << 13)
#define ELM_MGR_ACTION_API_QUERY (1u << 15)
#define ELM_MGR_ACTION_EVENT_SUBSCRIBE (1u << 16)
#define ELM_MGR_ACTION_EVENT_READ (1u << 18)
#define ELM_MGR_POLICY_HEALTH (1ull << 8)
#define ELM_MGR_POLICY_PROVIDER_ASYNC (1ull << 9)
#define ELM_MGR_POLICY_API_REGISTRY (1ull << 10)
#define ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS (1ull << 11)
#define ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED (1ull << 23)

#define ELM_MGR_API_NAMESPACE_LEN 32u
#define ELM_MGR_API_NAME_LEN 48u
#define ELM_MGR_API_CONTRACT_LEN 48u
#define ELM_MGR_EVENT_READ_FLAG_ADVANCE (1u << 0)

#define ELM_NEXUS_CONTRACT_LEN 64u
#define ELM_MENU_LABEL_LEN 64u
#define ELM_MENU_DESCRIPTION_LEN 128u
#define ELM_MENU_ROUTE_LEN 64u
#define ELM_FRAME_PAYLOAD_LEN 256u

#define ELM_ACTION_OPCODE_INVOKE 1u
#define ELM_ACTION_RESULT_HEALTH 1u
#define ELM_CALL_STATUS_OK 0

#define ELM_EBI_SOURCE_ABI_VERSION 1u
#define ELM_EBI_SOURCE_KIND_EKI 1u
#define ELM_EBI_LOAD_NATIVE_CODE_TODO (-4096)

#define ELM_EKI_FORMAT_VERSION 1u
#define ELM_EKI_HEADER_SIZE 64u
#define ELM_EKI_BLOCK_DESC_SIZE 48u
#define ELM_EKI_MANIFEST_NAME_LEN 128u
#define ELM_EKI_MANIFEST_VERSION_LEN 64u
#define ELM_EKI_BLOCK_MANIFEST 1u
#define ELM_EKI_BLOCK_MENU 2u
#define ELM_EKI_BLOCK_LIFECYCLE_HOOKS 18u
#define ELM_KIND_EXTENSION 4u
#define ELM_MENU_KIND_ACTION 2u
#define ELM_LIFECYCLE_HOOK_INITIALIZE 1u
#define ELM_LIFECYCLE_HOOK_FINALIZE 2u
#define ELM_EBI_RUST_ABI_VERSION 1u
#define ELM_EBI_RUST_HOOK_CONTEXT_RESULT 1u
#define ELM_EBI_SYMBOL_NAME_LEN 128u

#define ELM_STATE_LOADED 3u
#define ELM_STATE_RETIRED 10u

struct elm_core_info {
    uint32_t magic;
    uint16_t abi_version;
    uint16_t core_version;
    uint64_t capabilities;
    uint32_t cell_count;
    uint32_t port_count;
    uint32_t lease_count;
    uint64_t event_sequence;
};

struct elm_mgr_call_header {
    uint32_t kind;
    uint32_t flags;
    uint32_t payload_len;
    uint32_t reserved;
};

struct elm_mgr_response_header {
    int32_t status;
    uint32_t payload_len;
    uint64_t reserved;
};

struct elm_mgr_policy_info {
    uint16_t abi_version;
    uint16_t reserved0;
    uint32_t supported_actions;
    uint64_t policy_flags;
    uint64_t blocker_mask;
    uint32_t audit_capacity;
    uint32_t reserved1;
};

struct elm_menu_snapshot_header {
    uint16_t abi_version;
    uint16_t item_entry_size;
    uint32_t item_count;
    uint64_t generation;
};

struct elm_menu_item_snapshot {
    uint64_t id;
    uint64_t owner;
    uint64_t action;
    uint32_t kind;
    uint32_t flags;
    uint16_t label_len;
    uint16_t description_len;
    uint16_t route_len;
    uint16_t reserved;
    uint8_t label[ELM_MENU_LABEL_LEN];
    uint8_t description[ELM_MENU_DESCRIPTION_LEN];
    uint8_t route[ELM_MENU_ROUTE_LEN];
};

struct elm_core_health_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    int32_t status;
    uint32_t flags;
    uint64_t event_sequence;
};

struct elm_mgr_audit_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint32_t dropped_count;
    uint32_t reserved;
    uint64_t last_sequence;
};

struct elm_nexus_bind_request {
    uint64_t cell_id;
    uint64_t port_id;
    uint32_t flags;
    uint16_t contract_len;
    uint16_t reserved;
    uint8_t contract[ELM_NEXUS_CONTRACT_LEN];
};

struct elm_nexus_bind_response {
    uint64_t cell_id;
    uint64_t port_id;
    uint64_t binding_id;
    uint64_t lease_id;
    uint64_t generation;
    int32_t status;
    uint32_t allowed;
    uint64_t blockers;
    uint64_t reserved;
};

struct elm_nexus_binding_snapshot_header {
    uint16_t abi_version;
    uint16_t binding_entry_size;
    uint32_t binding_count;
    uint64_t event_sequence;
};

struct elm_nexus_binding_record {
    uint64_t binding_id;
    uint64_t cell_id;
    uint64_t port_id;
    uint64_t lease_id;
    uint64_t generation;
    uint32_t active;
    uint32_t flags;
    uint16_t contract_len;
    uint16_t reserved;
    uint8_t contract[ELM_NEXUS_CONTRACT_LEN];
};

struct elm_action_invoke_request {
    uint64_t action_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_action_invoke_reply {
    uint64_t action_id;
    uint64_t menu_item_id;
    uint64_t owner_cell_id;
    uint32_t result_kind;
    int32_t result_code;
    uint64_t event_sequence;
    uint64_t reserved;
};

struct elm_call_frame {
    uint64_t binding_id;
    uint64_t call_id;
    uint32_t opcode;
    uint32_t flags;
    uint16_t payload_len;
    uint16_t reserved0;
    uint32_t reserved1;
    uint8_t payload[ELM_FRAME_PAYLOAD_LEN];
};

struct elm_reply_frame {
    uint64_t binding_id;
    uint64_t call_id;
    int32_t status;
    uint32_t flags;
    uint16_t payload_len;
    uint16_t reserved0;
    uint32_t reserved1;
    uint8_t payload[ELM_FRAME_PAYLOAD_LEN];
};

struct elm_provider_invoke_response {
    struct elm_reply_frame reply;
};

struct elm_lifecycle_request {
    uint64_t cell_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_lifecycle_response {
    uint64_t cell_id;
    int32_t status;
    uint32_t final_state;
    uint32_t revoked_leases;
    uint32_t removed_menu_items;
    uint32_t reason;
    uint32_t reserved;
};

struct elm_ebi_source_request {
    uint16_t abi_version;
    uint16_t source_kind;
    uint32_t flags;
    uint32_t payload_len;
    uint32_t reserved;
};

struct elm_load_cell_response {
    uint64_t cell_id;
    int32_t status;
    uint32_t final_state;
    uint32_t reason;
    uint32_t reserved;
};

struct elm_mgr_api_registry_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint32_t flags;
    uint32_t reserved;
    uint64_t generation;
};

struct elm_mgr_api_descriptor {
    uint64_t id;
    uint64_t owner_cell_id;
    uint32_t kind;
    uint32_t flags;
    uint32_t call_kind;
    uint16_t min_abi_version;
    uint16_t current_abi_version;
    uint16_t namespace_len;
    uint16_t name_len;
    uint16_t contract_len;
    uint16_t reserved0;
    uint64_t capabilities;
    uint8_t namespace[ELM_MGR_API_NAMESPACE_LEN];
    uint8_t name[ELM_MGR_API_NAME_LEN];
    uint8_t contract[ELM_MGR_API_CONTRACT_LEN];
};

struct elm_mgr_event_subscribe_request {
    uint64_t owner_cell_id;
    uint32_t kind_filter;
    uint32_t flags;
    uint64_t cell_filter;
    uint64_t port_filter;
    uint64_t binding_filter;
    uint64_t lease_filter;
};

struct elm_mgr_event_subscribe_response {
    uint64_t subscription_id;
    uint64_t lease_id;
    uint64_t owner_cell_id;
    uint64_t cursor;
    int32_t status;
    uint32_t flags;
    uint64_t dropped_events;
};

struct elm_mgr_event_unsubscribe_request {
    uint64_t subscription_id;
    uint64_t owner_cell_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_mgr_event_unsubscribe_response {
    uint64_t subscription_id;
    uint64_t lease_id;
    uint64_t owner_cell_id;
    int32_t status;
    uint32_t revoked;
    uint64_t delivered_events;
    uint64_t dropped_events;
};

struct elm_mgr_event_subscription_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint64_t event_sequence;
};

struct elm_mgr_event_subscription_record {
    uint64_t subscription_id;
    uint64_t owner_cell_id;
    uint64_t lease_id;
    uint64_t cursor;
    uint32_t kind_filter;
    uint32_t flags;
    uint64_t cell_filter;
    uint64_t port_filter;
    uint64_t binding_filter;
    uint64_t lease_filter;
    uint64_t delivered_events;
    uint64_t dropped_events;
};

struct elm_mgr_subscribed_event_read_request {
    uint64_t subscription_id;
    uint64_t cursor;
    uint32_t max_records;
    uint32_t flags;
};

struct elm_mgr_subscribed_event_read_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    int32_t status;
    uint32_t flags;
    uint64_t subscription_id;
    uint64_t cursor;
    uint64_t next_cursor;
    uint64_t dropped_events;
};

_Static_assert(sizeof(struct elm_mgr_call_header) == 16, "bad mgr call header size");
_Static_assert(sizeof(struct elm_mgr_response_header) == 16, "bad mgr response header size");
_Static_assert(sizeof(struct elm_core_info) == 40, "bad core info size");
_Static_assert(sizeof(struct elm_mgr_policy_info) == 32, "bad policy info size");
_Static_assert(sizeof(struct elm_menu_snapshot_header) == 16, "bad menu header size");
_Static_assert(sizeof(struct elm_menu_item_snapshot) == 296, "bad menu item size");
_Static_assert(sizeof(struct elm_core_health_header) == 24, "bad health header size");
_Static_assert(sizeof(struct elm_mgr_audit_header) == 24, "bad audit header size");
_Static_assert(sizeof(struct elm_nexus_bind_request) == 88, "bad bind request size");
_Static_assert(sizeof(struct elm_nexus_bind_response) == 64, "bad bind response size");
_Static_assert(sizeof(struct elm_nexus_binding_record) == 120, "bad binding record size");
_Static_assert(sizeof(struct elm_action_invoke_request) == 16, "bad action request size");
_Static_assert(sizeof(struct elm_action_invoke_reply) == 48, "bad action reply size");
_Static_assert(sizeof(struct elm_call_frame) == 288, "bad call frame size");
_Static_assert(sizeof(struct elm_provider_invoke_response) == 288, "bad invoke response size");
_Static_assert(sizeof(struct elm_lifecycle_request) == 16, "bad lifecycle request size");
_Static_assert(sizeof(struct elm_lifecycle_response) == 32, "bad lifecycle response size");
_Static_assert(sizeof(struct elm_ebi_source_request) == 16, "bad ebi source request size");
_Static_assert(sizeof(struct elm_load_cell_response) == 24, "bad load response size");
_Static_assert(sizeof(struct elm_mgr_api_registry_header) == 24, "bad api registry header size");
_Static_assert(sizeof(struct elm_mgr_api_descriptor) == 176, "bad api descriptor size");
_Static_assert(sizeof(struct elm_mgr_event_subscribe_request) == 48, "bad event subscribe request size");
_Static_assert(sizeof(struct elm_mgr_event_subscribe_response) == 48, "bad event subscribe response size");
_Static_assert(sizeof(struct elm_mgr_event_unsubscribe_request) == 24, "bad event unsubscribe request size");
_Static_assert(sizeof(struct elm_mgr_event_unsubscribe_response) == 48, "bad event unsubscribe response size");
_Static_assert(sizeof(struct elm_mgr_event_subscription_header) == 16, "bad event subscription header size");
_Static_assert(sizeof(struct elm_mgr_event_subscription_record) == 88, "bad event subscription record size");
_Static_assert(sizeof(struct elm_mgr_subscribed_event_read_request) == 24, "bad subscribed event read request size");
_Static_assert(sizeof(struct elm_mgr_subscribed_event_read_header) == 48, "bad subscribed event read header size");

static int fail_msg(const char *step, const char *msg)
{
    fprintf(stderr, "[elm-smoke] FAIL %s: %s\n", step, msg);
    return -1;
}

static int fail_errno(const char *step)
{
    fprintf(stderr, "[elm-smoke] FAIL %s: errno=%d\n", step, errno);
    return -1;
}

static int field_eq(const uint8_t *field, uint16_t len, const char *value)
{
    size_t expected = strlen(value);
    return len == expected && memcmp(field, value, expected) == 0;
}

static void put_u16(uint8_t *buf, size_t off, uint16_t value)
{
    buf[off + 0] = (uint8_t)(value & 0xffu);
    buf[off + 1] = (uint8_t)((value >> 8) & 0xffu);
}

static void put_u32(uint8_t *buf, size_t off, uint32_t value)
{
    put_u16(buf, off, (uint16_t)(value & 0xffffu));
    put_u16(buf, off + 2, (uint16_t)((value >> 16) & 0xffffu));
}

static void put_u64(uint8_t *buf, size_t off, uint64_t value)
{
    put_u32(buf, off, (uint32_t)(value & 0xffffffffull));
    put_u32(buf, off + 4, (uint32_t)((value >> 32) & 0xffffffffull));
}

static int copy_fixed(uint8_t *dst, size_t cap, const char *value)
{
    size_t len = strlen(value);
    if (len > cap) {
        return -1;
    }
    memcpy(dst, value, len);
    return (int)len;
}

static int elm_ctl(uint32_t command, const void *input, size_t input_len, void *output,
                   size_t output_len, ssize_t *written)
{
    long ret = syscall(SYS_ELM_CTL, command, input, input_len, output, output_len);
    if (ret < 0) {
        return -1;
    }
    *written = (ssize_t)ret;
    return 0;
}

static int mgr_call(uint32_t kind, const void *payload, size_t payload_len, uint8_t *out,
                    size_t out_len, int32_t *status, const uint8_t **reply_payload,
                    uint32_t *reply_len)
{
    uint8_t input[ELM_MGR_MAX_INPUT];
    struct elm_mgr_call_header header = {
        .kind = kind,
        .flags = 0,
        .payload_len = (uint32_t)payload_len,
        .reserved = 0,
    };
    ssize_t written = 0;
    struct elm_mgr_response_header response;
    size_t input_len = sizeof(header) + payload_len;

    if (payload_len > 4096u || input_len > sizeof(input)) {
        return fail_msg("mgr-call", "payload too large");
    }
    memcpy(input, &header, sizeof(header));
    if (payload_len != 0) {
        memcpy(input + sizeof(header), payload, payload_len);
    }
    if (elm_ctl(ELM_CTL_CMD_MGR_CALL, input, input_len, out, out_len, &written) != 0) {
        return fail_errno("mgr-call");
    }
    if ((size_t)written < sizeof(response)) {
        return fail_msg("mgr-call", "short response header");
    }
    memcpy(&response, out, sizeof(response));
    if (response.reserved != 0) {
        return fail_msg("mgr-call", "non-zero response reserved");
    }
    if ((size_t)written != sizeof(response) + response.payload_len) {
        return fail_msg("mgr-call", "response length mismatch");
    }
    *status = response.status;
    *reply_payload = out + sizeof(response);
    *reply_len = response.payload_len;
    return 0;
}

static int require_mgr_payload(uint32_t kind, const void *payload, size_t payload_len,
                               uint8_t *out, size_t out_len, const uint8_t **reply_payload,
                               uint32_t *reply_len)
{
    int32_t status = 0;
    if (mgr_call(kind, payload, payload_len, out, out_len, &status, reply_payload, reply_len) != 0) {
        return -1;
    }
    if (status != ELM_MGR_STATUS_OK) {
        fprintf(stderr, "[elm-smoke] FAIL mgr kind %u: status=%d\n", kind, status);
        return -1;
    }
    return 0;
}

static int run_core_query(void)
{
    struct elm_core_info info;
    ssize_t written = 0;

    memset(&info, 0, sizeof(info));
    if (elm_ctl(ELM_CTL_CMD_CORE_QUERY, NULL, 0, &info, sizeof(info), &written) != 0) {
        return fail_errno("core-query");
    }
    if ((size_t)written != sizeof(info)) {
        return fail_msg("core-query", "bad output size");
    }
    if (info.magic != ELM_CTL_MAGIC || info.abi_version != ELM_CTL_ABI_VERSION) {
        return fail_msg("core-query", "bad magic or abi");
    }
    if ((info.capabilities & ELM_CORE_CAP_MGR_CHANNEL) == 0) {
        return fail_msg("core-query", "missing mgr channel capability");
    }
    if (info.cell_count < 1 || info.port_count < 4) {
        return fail_msg("core-query", "elm-mgr core objects missing");
    }
    printf("[elm-smoke] core query ok: cells=%u ports=%u leases=%u events=%llu\n",
           info.cell_count, info.port_count, info.lease_count,
           (unsigned long long)info.event_sequence);
    return 0;
}

static int run_policy_query(uint8_t *out, size_t out_len)
{
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    struct elm_mgr_policy_info policy;

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_POLICY, NULL, 0, out, out_len, &payload,
                            &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(policy)) {
        return fail_msg("policy-query", "bad payload size");
    }
    memcpy(&policy, payload, sizeof(policy));
    if (policy.abi_version != ELM_CTL_ABI_VERSION) {
        return fail_msg("policy-query", "bad abi");
    }
    if ((policy.supported_actions & ELM_MGR_ACTION_PROVIDER_INVOKE) == 0 ||
        (policy.supported_actions & ELM_MGR_ACTION_HEALTH_QUERY) == 0 ||
        (policy.supported_actions & ELM_MGR_ACTION_API_QUERY) == 0 ||
        (policy.supported_actions & ELM_MGR_ACTION_EVENT_SUBSCRIBE) == 0 ||
        (policy.supported_actions & ELM_MGR_ACTION_EVENT_READ) == 0) {
        return fail_msg("policy-query", "missing supported action");
    }
    if ((policy.policy_flags & ELM_MGR_POLICY_HEALTH) == 0 ||
        (policy.policy_flags & ELM_MGR_POLICY_PROVIDER_ASYNC) == 0 ||
        (policy.policy_flags & ELM_MGR_POLICY_API_REGISTRY) == 0 ||
        (policy.policy_flags & ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS) == 0) {
        return fail_msg("policy-query", "missing policy flag");
    }
    if ((policy.blocker_mask & ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED) == 0) {
        return fail_msg("policy-query", "missing hook failure blocker");
    }
    printf("[elm-smoke] policy query ok: actions=0x%x policy=0x%llx blockers=0x%llx\n",
           policy.supported_actions, (unsigned long long)policy.policy_flags,
           (unsigned long long)policy.blocker_mask);
    return 0;
}

static int run_menu_query(uint8_t *out, size_t out_len, uint64_t *health_action)
{
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    struct elm_menu_snapshot_header header;

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_MENU, NULL, 0, out, out_len, &payload,
                            &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(header)) {
        return fail_msg("menu-query", "short payload");
    }
    memcpy(&header, payload, sizeof(header));
    if (header.abi_version != ELM_CTL_ABI_VERSION ||
        header.item_entry_size < sizeof(struct elm_menu_item_snapshot)) {
        return fail_msg("menu-query", "bad header");
    }
    for (uint32_t i = 0; i < header.item_count; i++) {
        size_t off = sizeof(header) + (size_t)i * header.item_entry_size;
        struct elm_menu_item_snapshot item;
        if (off + sizeof(item) > payload_len) {
            return fail_msg("menu-query", "truncated item");
        }
        memcpy(&item, payload + off, sizeof(item));
        if (field_eq(item.route, item.route_len, "elm/mgr/health")) {
            *health_action = item.action;
            printf("[elm-smoke] menu query ok: items=%u health_action=%llu\n", header.item_count,
                   (unsigned long long)*health_action);
            return 0;
        }
    }
    return fail_msg("menu-query", "health action not found");
}

static int run_health_query(uint8_t *out, size_t out_len)
{
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    struct elm_core_health_header header;

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_HEALTH, NULL, 0, out, out_len, &payload,
                            &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(header)) {
        return fail_msg("health-query", "short payload");
    }
    memcpy(&header, payload, sizeof(header));
    if (header.abi_version != ELM_CTL_ABI_VERSION || header.status != ELM_MGR_STATUS_OK ||
        header.record_count < 9) {
        return fail_msg("health-query", "core is not healthy");
    }
    printf("[elm-smoke] health query ok: checks=%u event=%llu\n", header.record_count,
           (unsigned long long)header.event_sequence);
    return 0;
}

static int query_existing_action_binding(uint8_t *out, size_t out_len, uint64_t *binding_id)
{
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    struct elm_nexus_binding_snapshot_header header;

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_NEXUS_BINDINGS, NULL, 0, out, out_len, &payload,
                            &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(header)) {
        return fail_msg("binding-query", "short payload");
    }
    memcpy(&header, payload, sizeof(header));
    if (header.abi_version != ELM_CTL_ABI_VERSION ||
        header.binding_entry_size < sizeof(struct elm_nexus_binding_record)) {
        return fail_msg("binding-query", "bad header");
    }
    for (uint32_t i = 0; i < header.binding_count; i++) {
        size_t off = sizeof(header) + (size_t)i * header.binding_entry_size;
        struct elm_nexus_binding_record record;
        if (off + sizeof(record) > payload_len) {
            return fail_msg("binding-query", "truncated record");
        }
        memcpy(&record, payload + off, sizeof(record));
        if (record.active != 0 && record.cell_id == ELM_MGR_BUILTIN_ID &&
            record.port_id == ELM_MGR_ACTION_PORT_ID &&
            field_eq(record.contract, record.contract_len, "mgr.action.invoke@1")) {
            *binding_id = record.binding_id;
            return 1;
        }
    }
    return 0;
}

static int run_bind_action_provider(uint8_t *out, size_t out_len, uint64_t *binding_id)
{
    int found = query_existing_action_binding(out, out_len, binding_id);
    if (found < 0) {
        return -1;
    }
    if (found > 0) {
        printf("[elm-smoke] bind mgr action provider ok: reused binding=%llu\n",
               (unsigned long long)*binding_id);
        return 0;
    }

    struct elm_nexus_bind_request request;
    struct elm_nexus_bind_response response;
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    const char *contract = "mgr.action.invoke@1";

    memset(&request, 0, sizeof(request));
    request.cell_id = ELM_MGR_BUILTIN_ID;
    request.port_id = ELM_MGR_ACTION_PORT_ID;
    request.contract_len = (uint16_t)strlen(contract);
    memcpy(request.contract, contract, request.contract_len);

    if (require_mgr_payload(ELM_MGR_CALL_COMMIT_BIND, &request, sizeof(request), out, out_len,
                            &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(response)) {
        return fail_msg("commit-bind", "bad payload size");
    }
    memcpy(&response, payload, sizeof(response));
    if (response.status != ELM_MGR_STATUS_OK || response.allowed == 0 ||
        response.binding_id == 0) {
        return fail_msg("commit-bind", "bind rejected");
    }
    *binding_id = response.binding_id;
    printf("[elm-smoke] bind mgr action provider ok: binding=%llu lease=%llu\n",
           (unsigned long long)response.binding_id, (unsigned long long)response.lease_id);
    return 0;
}

static int run_invoke_health_action(uint8_t *out, size_t out_len, uint64_t binding_id,
                                    uint64_t health_action)
{
    struct elm_call_frame request;
    struct elm_action_invoke_request action;
    struct elm_provider_invoke_response response;
    struct elm_action_invoke_reply reply;
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;

    memset(&request, 0, sizeof(request));
    memset(&action, 0, sizeof(action));
    action.action_id = health_action;

    request.binding_id = binding_id;
    request.call_id = 1;
    request.opcode = ELM_ACTION_OPCODE_INVOKE;
    request.payload_len = sizeof(action);
    memcpy(request.payload, &action, sizeof(action));

    if (require_mgr_payload(ELM_MGR_CALL_INVOKE_PROVIDER, &request, sizeof(request), out, out_len,
                            &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(response)) {
        return fail_msg("invoke-health", "bad payload size");
    }
    memcpy(&response, payload, sizeof(response));
    if (response.reply.status != ELM_CALL_STATUS_OK ||
        response.reply.payload_len < sizeof(reply)) {
        return fail_msg("invoke-health", "provider call failed");
    }
    memcpy(&reply, response.reply.payload, sizeof(reply));
    if (reply.action_id != health_action || reply.owner_cell_id != ELM_MGR_BUILTIN_ID ||
        reply.result_kind != ELM_ACTION_RESULT_HEALTH || reply.result_code != ELM_MGR_STATUS_OK) {
        return fail_msg("invoke-health", "bad action reply");
    }
    printf("[elm-smoke] invoke health action ok: action=%llu event=%llu\n",
           (unsigned long long)reply.action_id, (unsigned long long)reply.event_sequence);
    return 0;
}

static int run_audit_query(uint8_t *out, size_t out_len)
{
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    struct elm_mgr_audit_header header;

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_AUDIT, NULL, 0, out, out_len, &payload,
                            &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(header)) {
        return fail_msg("audit-query", "short payload");
    }
    memcpy(&header, payload, sizeof(header));
    if (header.abi_version != ELM_CTL_ABI_VERSION || header.record_count == 0) {
        return fail_msg("audit-query", "empty audit stream");
    }
    printf("[elm-smoke] audit query ok: records=%u last=%llu dropped=%u\n", header.record_count,
           (unsigned long long)header.last_sequence, header.dropped_count);
    return 0;
}

static void write_block_desc(uint8_t *image, size_t desc_off, uint32_t kind, size_t payload_off,
                             size_t payload_size)
{
    put_u32(image, desc_off + 0, kind);
    put_u32(image, desc_off + 4, 0);
    put_u64(image, desc_off + 8, payload_off);
    put_u64(image, desc_off + 16, payload_size);
    put_u64(image, desc_off + 24, payload_size);
    put_u64(image, desc_off + 32, 0);
    put_u32(image, desc_off + 40, 0);
    put_u32(image, desc_off + 44, 0);
}

static size_t write_manifest_block(uint8_t *payload, const char *name)
{
    const char *version = "0.1.0";
    size_t size = 16u + ELM_EKI_MANIFEST_NAME_LEN + ELM_EKI_MANIFEST_VERSION_LEN;
    memset(payload, 0, size);
    put_u32(payload, 0, ELM_KIND_EXTENSION);
    put_u16(payload, 8, (uint16_t)strlen(name));
    put_u16(payload, 10, (uint16_t)strlen(version));
    (void)copy_fixed(payload + 16, ELM_EKI_MANIFEST_NAME_LEN, name);
    (void)copy_fixed(payload + 16 + ELM_EKI_MANIFEST_NAME_LEN,
                     ELM_EKI_MANIFEST_VERSION_LEN, version);
    return size;
}

static size_t write_menu_block(uint8_t *payload)
{
    const char *label = "Smoke";
    const char *description = "ELM smoke EKI";
    const char *route = "elm/smoke/eki";
    size_t size = 16u + ELM_MENU_LABEL_LEN + ELM_MENU_DESCRIPTION_LEN + ELM_MENU_ROUTE_LEN;
    memset(payload, 0, size);
    put_u32(payload, 0, ELM_MENU_KIND_ACTION);
    put_u16(payload, 8, (uint16_t)strlen(label));
    put_u16(payload, 10, (uint16_t)strlen(description));
    put_u16(payload, 12, (uint16_t)strlen(route));
    (void)copy_fixed(payload + 16, ELM_MENU_LABEL_LEN, label);
    (void)copy_fixed(payload + 16 + ELM_MENU_LABEL_LEN, ELM_MENU_DESCRIPTION_LEN, description);
    (void)copy_fixed(payload + 16 + ELM_MENU_LABEL_LEN + ELM_MENU_DESCRIPTION_LEN,
                     ELM_MENU_ROUTE_LEN, route);
    return size;
}

static void write_hook_record(uint8_t *payload, size_t off, uint32_t kind, const char *symbol)
{
    put_u32(payload, off + 0, kind);
    put_u32(payload, off + 4, 0);
    put_u16(payload, off + 8, ELM_EBI_RUST_ABI_VERSION);
    put_u16(payload, off + 10, ELM_EBI_RUST_HOOK_CONTEXT_RESULT);
    put_u16(payload, off + 12, (uint16_t)strlen(symbol));
    put_u16(payload, off + 14, 0);
    put_u32(payload, off + 16, 0);
    (void)copy_fixed(payload + off + 20, ELM_EBI_SYMBOL_NAME_LEN, symbol);
}

static size_t write_lifecycle_hooks_block(uint8_t *payload)
{
    size_t record_size = 20u + ELM_EBI_SYMBOL_NAME_LEN;
    size_t size = 8u + 2u * record_size;
    memset(payload, 0, size);
    put_u32(payload, 0, 2);
    put_u32(payload, 4, 0);
    write_hook_record(payload, 8, ELM_LIFECYCLE_HOOK_INITIALIZE, "on_initialize");
    write_hook_record(payload, 8 + record_size, ELM_LIFECYCLE_HOOK_FINALIZE, "on_finalize");
    return size;
}

static int build_minimal_eki(uint8_t *image, size_t cap, size_t *image_len)
{
    char name[64];
    size_t block_count = 3;
    size_t table_off = ELM_EKI_HEADER_SIZE;
    size_t payload_off = table_off + block_count * ELM_EKI_BLOCK_DESC_SIZE;
    size_t off = payload_off;
    size_t size = 0;

    if (snprintf(name, sizeof(name), "smoke-%ld", (long)getpid()) <= 0) {
        return fail_msg("load-eki", "cannot build unique name");
    }
    if (cap < 1024) {
        return fail_msg("load-eki", "image buffer too small");
    }
    memset(image, 0, cap);

    size = write_manifest_block(image + off, name);
    write_block_desc(image, table_off, ELM_EKI_BLOCK_MANIFEST, off, size);
    off += size;

    size = write_menu_block(image + off);
    write_block_desc(image, table_off + ELM_EKI_BLOCK_DESC_SIZE, ELM_EKI_BLOCK_MENU, off, size);
    off += size;

    size = write_lifecycle_hooks_block(image + off);
    write_block_desc(image, table_off + 2u * ELM_EKI_BLOCK_DESC_SIZE,
                     ELM_EKI_BLOCK_LIFECYCLE_HOOKS, off, size);
    off += size;

    memcpy(image, "ELM_EKI", 7);
    image[7] = 0;
    put_u16(image, 8, ELM_EKI_FORMAT_VERSION);
    put_u16(image, 10, 1);
    put_u32(image, 12, ELM_EKI_HEADER_SIZE);
    put_u64(image, 16, off);
    put_u64(image, 24, table_off);
    put_u64(image, 32, 0);
    put_u32(image, 40, 0);
    put_u16(image, 44, 1);
    put_u16(image, 46, 0);
    put_u32(image, 48, (uint32_t)block_count);
    put_u32(image, 52, 0);

    *image_len = off;
    return 0;
}

static int run_load_minimal_eki(uint8_t *out, size_t out_len)
{
    uint8_t image[1024];
    uint8_t source[sizeof(struct elm_ebi_source_request) + sizeof(image)];
    struct elm_ebi_source_request source_request;
    struct elm_load_cell_response load;
    struct elm_lifecycle_request detach_request;
    struct elm_lifecycle_response detach;
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    size_t image_len = 0;

    if (build_minimal_eki(image, sizeof(image), &image_len) != 0) {
        return -1;
    }
    memset(&source_request, 0, sizeof(source_request));
    source_request.abi_version = ELM_EBI_SOURCE_ABI_VERSION;
    source_request.source_kind = ELM_EBI_SOURCE_KIND_EKI;
    source_request.payload_len = (uint32_t)image_len;
    memcpy(source, &source_request, sizeof(source_request));
    memcpy(source + sizeof(source_request), image, image_len);

    if (require_mgr_payload(ELM_MGR_CALL_LOAD_CELL, source, sizeof(source_request) + image_len,
                            out, out_len, &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(load)) {
        return fail_msg("load-eki", "bad payload size");
    }
    memcpy(&load, payload, sizeof(load));
    if (load.status != ELM_EBI_LOAD_NATIVE_CODE_TODO || load.final_state != ELM_STATE_LOADED ||
        load.cell_id == 0) {
        return fail_msg("load-eki", "unexpected load result");
    }
    printf("[elm-smoke] load minimal EKI ok: cell=%llu status=%d state=%u\n",
           (unsigned long long)load.cell_id, load.status, load.final_state);

    memset(&detach_request, 0, sizeof(detach_request));
    detach_request.cell_id = load.cell_id;
    if (require_mgr_payload(ELM_MGR_CALL_DETACH_CELL, &detach_request, sizeof(detach_request),
                            out, out_len, &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(detach)) {
        return fail_msg("detach-eki", "bad payload size");
    }
    memcpy(&detach, payload, sizeof(detach));
    if (detach.status != ELM_MGR_STATUS_OK || detach.final_state != ELM_STATE_RETIRED) {
        return fail_msg("detach-eki", "detach failed");
    }
    printf("[elm-smoke] detach minimal EKI ok: cell=%llu state=%u\n",
           (unsigned long long)detach.cell_id, detach.final_state);
    return 0;
}

static int run_mgr_runtime_query(uint8_t *out, size_t out_len)
{
    const uint8_t *payload = NULL;
    uint32_t payload_len = 0;
    struct elm_mgr_api_registry_header api;
    struct elm_mgr_event_subscribe_request subscribe;
    struct elm_mgr_event_subscribe_response subscribe_response;
    struct elm_mgr_event_subscription_header subscriptions;
    struct elm_mgr_subscribed_event_read_request read_request;
    struct elm_mgr_subscribed_event_read_header read_response;
    struct elm_mgr_event_unsubscribe_request unsubscribe;
    struct elm_mgr_event_unsubscribe_response unsubscribe_response;

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_API_REGISTRY, NULL, 0, out, out_len,
                            &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(api)) {
        return fail_msg("api-query", "short payload");
    }
    memcpy(&api, payload, sizeof(api));
    if (api.abi_version != ELM_CTL_ABI_VERSION ||
        api.record_entry_size != sizeof(struct elm_mgr_api_descriptor) ||
        api.record_count < 21 ||
        payload_len != sizeof(api) + api.record_count * api.record_entry_size) {
        return fail_msg("api-query", "bad registry");
    }
    printf("[elm-smoke] api registry ok: records=%u generation=%llu\n", api.record_count,
           (unsigned long long)api.generation);

    memset(&subscribe, 0, sizeof(subscribe));
    subscribe.owner_cell_id = ELM_MGR_BUILTIN_ID;
    if (require_mgr_payload(ELM_MGR_CALL_SUBSCRIBE_EVENT, &subscribe, sizeof(subscribe),
                            out, out_len, &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(subscribe_response)) {
        return fail_msg("event-subscribe", "bad payload size");
    }
    memcpy(&subscribe_response, payload, sizeof(subscribe_response));
    if (subscribe_response.status != ELM_MGR_STATUS_OK ||
        subscribe_response.subscription_id == 0 || subscribe_response.lease_id == 0) {
        return fail_msg("event-subscribe", "subscribe failed");
    }
    printf("[elm-smoke] event subscribe ok: subscription=%llu lease=%llu cursor=%llu\n",
           (unsigned long long)subscribe_response.subscription_id,
           (unsigned long long)subscribe_response.lease_id,
           (unsigned long long)subscribe_response.cursor);

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_EVENT_SUBSCRIPTIONS, NULL, 0, out, out_len,
                            &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(subscriptions)) {
        return fail_msg("event-subscriptions", "short payload");
    }
    memcpy(&subscriptions, payload, sizeof(subscriptions));
    if (subscriptions.abi_version != ELM_CTL_ABI_VERSION ||
        subscriptions.record_entry_size != sizeof(struct elm_mgr_event_subscription_record) ||
        subscriptions.record_count != 1) {
        return fail_msg("event-subscriptions", "bad subscription snapshot");
    }

    if (run_load_minimal_eki(out, out_len) != 0) {
        return -1;
    }

    memset(&read_request, 0, sizeof(read_request));
    read_request.subscription_id = subscribe_response.subscription_id;
    read_request.max_records = 8;
    read_request.flags = ELM_MGR_EVENT_READ_FLAG_ADVANCE;
    if (require_mgr_payload(ELM_MGR_CALL_READ_SUBSCRIBED_EVENTS, &read_request,
                            sizeof(read_request), out, out_len, &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len < sizeof(read_response)) {
        return fail_msg("event-read", "short payload");
    }
    memcpy(&read_response, payload, sizeof(read_response));
    if (read_response.status != ELM_MGR_STATUS_OK ||
        read_response.subscription_id != subscribe_response.subscription_id ||
        read_response.record_count == 0 ||
        read_response.next_cursor <= read_response.cursor) {
        return fail_msg("event-read", "no subscribed events delivered");
    }
    printf("[elm-smoke] subscribed event read ok: records=%u next=%llu dropped=%llu\n",
           read_response.record_count, (unsigned long long)read_response.next_cursor,
           (unsigned long long)read_response.dropped_events);

    memset(&unsubscribe, 0, sizeof(unsubscribe));
    unsubscribe.subscription_id = subscribe_response.subscription_id;
    unsubscribe.owner_cell_id = ELM_MGR_BUILTIN_ID;
    if (require_mgr_payload(ELM_MGR_CALL_UNSUBSCRIBE_EVENT, &unsubscribe, sizeof(unsubscribe),
                            out, out_len, &payload, &payload_len) != 0) {
        return -1;
    }
    if (payload_len != sizeof(unsubscribe_response)) {
        return fail_msg("event-unsubscribe", "bad payload size");
    }
    memcpy(&unsubscribe_response, payload, sizeof(unsubscribe_response));
    if (unsubscribe_response.status != ELM_MGR_STATUS_OK ||
        unsubscribe_response.revoked == 0) {
        return fail_msg("event-unsubscribe", "unsubscribe failed");
    }

    if (require_mgr_payload(ELM_MGR_CALL_QUERY_EVENT_SUBSCRIPTIONS, NULL, 0, out, out_len,
                            &payload, &payload_len) != 0) {
        return -1;
    }
    memcpy(&subscriptions, payload, sizeof(subscriptions));
    if (subscriptions.record_count != 0) {
        return fail_msg("event-subscriptions", "subscription leak");
    }
    printf("[elm-smoke] event unsubscribe ok: delivered=%llu dropped=%llu\n",
           (unsigned long long)unsubscribe_response.delivered_events,
           (unsigned long long)unsubscribe_response.dropped_events);
    return 0;
}

int main(void)
{
    uint8_t out[8192];
    uint64_t health_action = 0;
    uint64_t binding_id = 0;

    if (run_core_query() != 0) {
        return 1;
    }
    if (run_policy_query(out, sizeof(out)) != 0) {
        return 1;
    }
    if (run_menu_query(out, sizeof(out), &health_action) != 0) {
        return 1;
    }
    if (run_health_query(out, sizeof(out)) != 0) {
        return 1;
    }
    if (run_bind_action_provider(out, sizeof(out), &binding_id) != 0) {
        return 1;
    }
    if (run_invoke_health_action(out, sizeof(out), binding_id, health_action) != 0) {
        return 1;
    }
    if (run_mgr_runtime_query(out, sizeof(out)) != 0) {
        return 1;
    }
    if (run_audit_query(out, sizeof(out)) != 0) {
        return 1;
    }
    if (run_health_query(out, sizeof(out)) != 0) {
        return 1;
    }

    printf("[elm-smoke] PASS\n");
    return 0;
}
