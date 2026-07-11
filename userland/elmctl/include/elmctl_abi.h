#ifndef ELMCTL_ABI_H
#define ELMCTL_ABI_H

#include <stdint.h>

#define SYS_ELM_CTL 509

#define ELM_CTL_MAGIC 0x314d4c45u
#define ELM_CTL_ABI_VERSION 1u
#define ELM_CTL_CMD_CORE_QUERY 1u
#define ELM_CTL_CMD_MGR_CALL 2u
#define ELM_CTL_CMD_EVENT_READ 3u
#define ELM_CTL_CMD_EVENT_ACK 4u
#define ELM_CTL_CMD_SNAPSHOT_READ 5u
#define ELM_CTL_CMD_DEBUG_DUMP 6u

#define ELM_CORE_CAP_SNAPSHOT (1ull << 0)
#define ELM_CORE_CAP_EVENTS (1ull << 1)
#define ELM_CORE_CAP_MGR_CHANNEL (1ull << 2)

#define ELM_MGR_STATUS_OK 0
#define ELM_MGR_STATUS_PERMISSION (-1)
#define ELM_MGR_STATUS_NOT_FOUND (-2)
#define ELM_MGR_STATUS_BUSY (-16)
#define ELM_MGR_STATUS_INVALID (-22)
#define ELM_MGR_STATUS_UNSUPPORTED (-95)
#define ELM_MGR_STATUS_TODO (-4096)

#define ELM_EBI_LOAD_STATUS_OK 0
#define ELM_EBI_LOAD_STATUS_INVALID_UNIT (-1)
#define ELM_EBI_LOAD_STATUS_UNSUPPORTED_ABI (-2)
#define ELM_EBI_LOAD_STATUS_INVALID_TARGET (-3)
#define ELM_EBI_LOAD_STATUS_INVALID_SEGMENT (-4)
#define ELM_EBI_LOAD_STATUS_ARCH_MISMATCH (-5)
#define ELM_EBI_LOAD_STATUS_INVALID_MANIFEST (-6)
#define ELM_EBI_LOAD_STATUS_INVALID_MENU (-7)
#define ELM_EBI_LOAD_STATUS_NATIVE_CODE_TODO (-4096)
#define ELM_EBI_LOAD_STATUS_RUNTIME_REJECTED (-4097)
#define ELM_EBI_LOAD_STATUS_UNTRUSTED_IMAGE (-4098)
#define ELM_EBI_LOAD_STATUS_ABI_FINGERPRINT_REJECTED (-4099)
#define ELM_EBI_LOAD_STATUS_ROLLBACK_REJECTED (-4100)

#define ELM_CALL_STATUS_OK 0
#define ELM_CALL_STATUS_NOT_FOUND (-2)
#define ELM_CALL_STATUS_BUSY (-16)
#define ELM_CALL_STATUS_INVALID (-22)
#define ELM_CALL_STATUS_UNSUPPORTED (-95)
#define ELM_CALL_STATUS_PROVIDER_FAULT (-4098)

#define ELM_MGR_CALL_QUERY_MENU 1u
#define ELM_MGR_CALL_LOAD_CELL 2u
#define ELM_MGR_CALL_DETACH_CELL 3u
#define ELM_MGR_CALL_PAUSE_CELL 4u
#define ELM_MGR_CALL_RESUME_CELL 5u
#define ELM_MGR_CALL_REPLACE_CELL 6u
#define ELM_MGR_CALL_QUERY_TOPOLOGY 7u
#define ELM_MGR_CALL_QUERY_POLICY 8u
#define ELM_MGR_CALL_PREFLIGHT_LIFECYCLE 9u
#define ELM_MGR_CALL_QUERY_AUDIT 10u
#define ELM_MGR_CALL_QUERY_NEXUS_BINDINGS 11u
#define ELM_MGR_CALL_PREFLIGHT_BIND 12u
#define ELM_MGR_CALL_COMMIT_BIND 13u
#define ELM_MGR_CALL_PREFLIGHT_UNBIND 14u
#define ELM_MGR_CALL_COMMIT_UNBIND 15u
#define ELM_MGR_CALL_SUBMIT_RUNTIME_LOG 16u
#define ELM_MGR_CALL_READ_RUNTIME_EVENT 17u
#define ELM_MGR_CALL_ACK_RUNTIME_EVENT 18u
#define ELM_MGR_CALL_QUERY_RUNTIME_PORTS 19u
#define ELM_MGR_CALL_REGISTER_PROVIDER_PORT 20u
#define ELM_MGR_CALL_UNREGISTER_PROVIDER_PORT 21u
#define ELM_MGR_CALL_QUERY_PROVIDER_PORTS 22u
#define ELM_MGR_CALL_INVOKE_PROVIDER 23u
#define ELM_MGR_CALL_QUERY_PROVIDER_STATS 24u
#define ELM_MGR_CALL_QUERY_HEALTH 25u
#define ELM_MGR_CALL_SUBMIT_PROVIDER_CALL 26u
#define ELM_MGR_CALL_POLL_PROVIDER_REPLY 27u
#define ELM_MGR_CALL_CANCEL_PROVIDER_CALL 28u
#define ELM_MGR_CALL_QUERY_PROVIDER_QUEUE 29u
#define ELM_MGR_CALL_QUERY_API_REGISTRY 30u
#define ELM_MGR_CALL_SUBSCRIBE_EVENT 31u
#define ELM_MGR_CALL_UNSUBSCRIBE_EVENT 32u
#define ELM_MGR_CALL_QUERY_EVENT_SUBSCRIPTIONS 33u
#define ELM_MGR_CALL_READ_SUBSCRIBED_EVENTS 34u
#define ELM_MGR_CALL_QUERY_PROVIDER_SNAPSHOT 35u
#define ELM_MGR_CALL_QUERY_NATIVE_CAPABILITIES 36u
#define ELM_MGR_CALL_QUERY_TODO_REGISTRY 37u
#define ELM_MGR_CALL_QUERY_EXTENSIONS 38u
#define ELM_MGR_CALL_PREFLIGHT_EXTENSION_ATTACH 39u
#define ELM_MGR_CALL_COMMIT_EXTENSION_ATTACH 40u
#define ELM_MGR_CALL_COMMIT_EXTENSION_DETACH 41u
#define ELM_MGR_CALL_DISPATCH_EXTENSION 42u
#define ELM_MGR_CALL_QUERY_FAULT_DUMP 43u
#define ELM_MGR_CALL_QUERY_LIFECYCLE_TRACE 44u
#define ELM_MGR_CALL_QUERY_PROVIDER_CALL_TRACE 45u
#define ELM_MGR_CALL_QUERY_MIXIN_TRACE 46u
#define ELM_MGR_CALL_QUERY_REPLACE_TRACE 47u
#define ELM_MGR_CALL_QUERY_POLICY_TRACE 48u
#define ELM_MGR_CALL_QUERY_RESOURCE_DIAGNOSTICS 49u
#define ELM_MGR_CALL_QUERY_RUNTIME_JOURNAL 50u
#define ELM_MGR_CALL_QUERY_CELL_POLICY 51u
#define ELM_MGR_CALL_UPDATE_CELL_POLICY 52u
#define ELM_MGR_CALL_QUERY_RESOURCE_BUDGET 53u
#define ELM_MGR_CALL_UPDATE_RESOURCE_BUDGET 54u
#define ELM_MGR_CALL_QUERY_TRUST_STATE 55u

#define ELM_POLICY_BLOCK_BUILTIN_PROTECTED (1ull << 0)
#define ELM_POLICY_BLOCK_CELL_NOT_FOUND (1ull << 1)
#define ELM_POLICY_BLOCK_INVALID_STATE (1ull << 2)
#define ELM_POLICY_BLOCK_NATIVE_TODO (1ull << 3)
#define ELM_POLICY_BLOCK_HAS_CHILDREN (1ull << 4)
#define ELM_POLICY_BLOCK_HAS_DEPENDENTS (1ull << 5)
#define ELM_POLICY_BLOCK_HAS_EXTENSIONS (1ull << 6)
#define ELM_POLICY_BLOCK_LEASE_BUSY (1ull << 7)
#define ELM_POLICY_BLOCK_GRAPH_INCONSISTENT (1ull << 9)
#define ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE (1ull << 10)
#define ELM_POLICY_BLOCK_PORT_NOT_FOUND (1ull << 11)
#define ELM_POLICY_BLOCK_CONTRACT_MISMATCH (1ull << 12)
#define ELM_POLICY_BLOCK_DUPLICATE_BINDING (1ull << 13)
#define ELM_POLICY_BLOCK_PORT_TODO (1ull << 14)
#define ELM_POLICY_BLOCK_BINDING_NOT_FOUND (1ull << 15)
#define ELM_POLICY_BLOCK_BINDING_PROTECTED (1ull << 16)
#define ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND (1ull << 17)
#define ELM_POLICY_BLOCK_PROVIDER_BUSY (1ull << 18)
#define ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED (1ull << 19)
#define ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL (1ull << 20)
#define ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED (1ull << 21)
#define ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED (1ull << 22)
#define ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED (1ull << 23)
#define ELM_POLICY_BLOCK_RESOURCE_QUOTA (1ull << 24)
#define ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND (1ull << 25)
#define ELM_POLICY_BLOCK_EXTENSION_DUPLICATE (1ull << 26)
#define ELM_POLICY_BLOCK_CAPABILITY_DENIED (1ull << 28)
#define ELM_POLICY_BLOCK_UNTRUSTED_IMAGE (1ull << 29)
#define ELM_POLICY_BLOCK_ABI_FINGERPRINT (1ull << 30)
#define ELM_POLICY_BLOCK_ROLLBACK_REJECTED (1ull << 31)
#define ELM_POLICY_BLOCK_CALLER_NOT_FOUND (1ull << 32)
#define ELM_POLICY_BLOCK_CALLER_STALE (1ull << 33)
#define ELM_POLICY_BLOCK_SCOPE_DENIED (1ull << 34)
#define ELM_POLICY_BLOCK_POLICY_ESCALATION (1ull << 35)
#define ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE (1ull << 36)

#define ELM_CELL_POLICY_FLAG_LOCKED (1u << 0)
#define ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION (1u << 1)
#define ELM_CELL_POLICY_FLAG_AUDIT_ALL (1u << 2)

#define ELM_TRUST_FLAG_SEALED (1u << 0)
#define ELM_TRUST_FLAG_ALLOW_UNSIGNED (1u << 1)
#define ELM_TRUST_FLAG_UNSIGNED_ACTIVE (1u << 2)

#define ELM_CELL_TRUST_INTERNAL (1u << 0)
#define ELM_CELL_TRUST_SIGNED (1u << 1)
#define ELM_CELL_TRUST_UNSIGNED (1u << 2)

#define ELM_MGR_MAX_PAYLOAD (256u * 1024u)
#define ELM_MGR_MAX_INPUT (ELM_MGR_MAX_PAYLOAD + 16u)
#define ELM_FRAME_PAYLOAD_LEN 256u
#define ELM_NEXUS_CONTRACT_LEN 64u
#define ELM_RUNTIME_LOG_MESSAGE_LEN 256u
#define ELM_CELL_NAME_LEN 64u
#define ELM_CONTRACT_NAME_LEN 64u
#define ELM_MENU_LABEL_LEN 64u
#define ELM_MENU_DESCRIPTION_LEN 128u
#define ELM_MENU_ROUTE_LEN 64u
#define ELM_MGR_RELATION_CONTRACT_LEN 64u
#define ELM_MGR_RELATION_POINT_LEN 32u
#define ELM_MGR_API_NAMESPACE_LEN 32u
#define ELM_MGR_API_NAME_LEN 48u
#define ELM_MGR_API_CONTRACT_LEN 48u
#define ELM_NATIVE_CAPABILITY_NAME_LEN 128u
#define ELM_TODO_NAME_LEN 64u
#define ELM_TODO_DETAIL_LEN 128u

#define ELM_MGR_BUILTIN_ID 1ull
#define ELM_EKI_BUILTIN_ID 2ull

#define ELM_EBI_SOURCE_ABI_VERSION 1u
#define ELM_EBI_SOURCE_KIND_PROJECTION 2u
#define ELM_EBI_SOURCE_KIND_BUILTIN 3u
#define ELM_EBI_SOURCE_KIND_MEMORY 4u
#define ELM_EBI_PROJECTION_SOURCE_ABI_VERSION 1u
#define ELM_EKI_PROJECTION_SOURCE_ID 0x454b490000000001ull

#define ELM_RESOURCE_BUDGET_DEFAULT_PROVIDER_PORTS 16u
#define ELM_RESOURCE_BUDGET_DEFAULT_PROVIDER_QUEUE 64u
#define ELM_RESOURCE_BUDGET_DEFAULT_EVENT_SUBSCRIPTIONS 16u
#define ELM_RESOURCE_BUDGET_DEFAULT_PENDING_LOADS 4u
#define ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_IMAGES 8u
#define ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_FAULTS 3u
#define ELM_RESOURCE_BUDGET_DEFAULT_AUDIT_RECORDS 128u
#define ELM_RESOURCE_BUDGET_DEFAULT_CONCURRENT_CALLS 16u
#define ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_IMAGE_BYTES (16ull * 1024ull * 1024ull)
#define ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_STACK_BYTES (4ull * 1024ull * 1024ull)
#define ELM_RESOURCE_BUDGET_DEFAULT_DYNAMIC_ALLOC_BYTES (64ull * 1024ull * 1024ull)
#define ELM_RESOURCE_BUDGET_DEFAULT_CPU_TIME_NS_PER_CALL 1000000000ull
#define ELM_RESOURCE_BUDGET_DEFAULT_CPU_BUDGET_NS_PER_PERIOD 2500000000ull
#define ELM_RESOURCE_BUDGET_DEFAULT_CPU_PERIOD_NS 10000000000ull

#define ELM_REPLACE_CELL_ABI_VERSION 1u
#define ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED (1u << 0)
#define ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE (1u << 0)
#define ELM_MGR_EVENT_READ_FLAG_ADVANCE (1u << 0)
#define ELM_PROVIDER_PORT_FLAG_NONE 0u

#define ELM_PORT_ACCESS_INTERNAL 1u
#define ELM_PORT_ACCESS_PUBLIC 2u
#define ELM_PORT_ACCESS_EXTENSION_ONLY 3u
#define ELM_FLOW_SOURCE 1u
#define ELM_FLOW_SINK 2u
#define ELM_FLOW_DUPLEX 3u
#define ELM_FLOW_CONTROL 4u
#define ELM_FLOW_EXCLUSIVE 1u
#define ELM_FLOW_SHARED 2u
#define ELM_FLOW_ORDERED 3u
#define ELM_FLOW_PIPELINE 4u
#define ELM_FLOW_BROADCAST 5u

#define ELM_LIFECYCLE_PAUSE 1u
#define ELM_LIFECYCLE_RESUME 2u
#define ELM_LIFECYCLE_DETACH 3u
#define ELM_LIFECYCLE_REPLACE 4u

#define ELM_CELL_LIFECYCLE_HOOKS_DECLARED (1u << 0)
#define ELM_CELL_LIFECYCLE_EXECUTOR_READY (1u << 1)
#define ELM_CELL_LIFECYCLE_INITIALIZED (1u << 2)
#define ELM_CELL_LIFECYCLE_FINALIZED (1u << 3)

#define ELM_STATE_DISCOVERED 1u
#define ELM_STATE_VERIFIED 2u
#define ELM_STATE_LOADED 3u
#define ELM_STATE_LINKED 4u
#define ELM_STATE_READY 5u
#define ELM_STATE_ACTIVE 6u
#define ELM_STATE_QUIESCING 7u
#define ELM_STATE_PAUSED 8u
#define ELM_STATE_DETACHED 9u
#define ELM_STATE_RETIRED 10u
#define ELM_STATE_FAULTED 11u
#define ELM_STATE_QUARANTINED 12u

#define ELM_KIND_MANAGER 1u
#define ELM_KIND_SERVICE 2u
#define ELM_KIND_DRIVER 3u
#define ELM_KIND_EXTENSION 4u
#define ELM_KIND_FILESYSTEM 5u
#define ELM_KIND_NETWORK 6u
#define ELM_KIND_DEBUG 7u
#define ELM_KIND_OTHER 255u

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

struct elm_snapshot_header {
    uint16_t abi_version;
    uint16_t cell_entry_size;
    uint16_t port_entry_size;
    uint16_t reserved;
    uint32_t cell_count;
    uint32_t port_count;
    uint32_t lease_count;
    uint64_t event_sequence;
};

struct elm_cell_snapshot {
    uint64_t id;
    uint64_t parent;
    uint32_t state;
    uint32_t kind;
    uint32_t ebi_arch;
    int32_t ebi_status;
    uint32_t native_code;
    uint32_t reserved0;
    uint64_t generation;
    uint16_t name_len;
    uint16_t reserved;
    uint8_t name[ELM_CELL_NAME_LEN];
    uint32_t ebi_source;
    uint32_t lifecycle_flags;
    uint16_t native_segment_count;
    uint16_t native_import_count;
    uint16_t native_export_count;
    uint16_t native_faults;
    uint32_t isolated;
    uint32_t reserved1;
    uint64_t isolation_blocker;
    uint16_t budget_max_provider_ports;
    uint16_t budget_max_provider_queue;
    uint16_t budget_max_event_subscriptions;
    uint16_t budget_max_pending_loads;
    uint16_t budget_max_native_images;
    uint16_t budget_max_native_faults;
    uint16_t budget_max_audit_records;
    uint16_t usage_provider_ports;
    uint16_t usage_provider_queue;
    uint16_t usage_event_subscriptions;
    uint16_t usage_pending_loads;
    uint16_t usage_native_images;
    uint16_t usage_native_faults;
    uint16_t usage_audit_records;
    uint32_t trust_flags;
    uint64_t release_epoch;
    uint8_t signer_key_id[32];
};

struct elm_trust_runtime_info_v1 {
    uint16_t abi_version;
    uint16_t struct_size;
    uint32_t flags;
    uint32_t anchor_count;
    uint32_t revoked_count;
    uint32_t accepted_epoch_count;
    uint32_t reserved;
};

struct elm_port_snapshot {
    uint64_t id;
    uint64_t owner;
    uint32_t direction;
    uint32_t mode;
    uint32_t implemented;
    uint16_t contract_len;
    uint16_t reserved;
    uint8_t contract[ELM_CONTRACT_NAME_LEN];
};

struct elm_event_record {
    uint64_t sequence;
    uint32_t kind;
    uint64_t cell;
    uint64_t port;
    uint64_t binding;
    uint64_t lease;
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

struct elm_lifecycle_plan_request {
    uint64_t cell_id;
    uint32_t action;
    uint32_t flags;
};

struct elm_replace_cell_request_v1 {
    uint16_t abi_version;
    uint16_t flags;
    uint16_t source_kind;
    uint16_t reserved0;
    uint64_t target_cell_id;
    uint32_t migration_limit;
    uint32_t source_payload_len;
    uint64_t reserved1;
};

struct elm_resource_budget {
    uint16_t max_provider_ports;
    uint16_t max_provider_queue;
    uint16_t max_event_subscriptions;
    uint16_t max_pending_loads;
    uint16_t max_native_images;
    uint16_t max_native_faults;
    uint16_t max_audit_records;
    uint16_t max_concurrent_calls;
    uint64_t max_native_image_bytes;
    uint64_t max_native_stack_bytes;
    uint64_t max_dynamic_alloc_bytes;
    uint64_t max_cpu_time_ns_per_call;
    uint64_t cpu_budget_ns_per_period;
    uint64_t cpu_period_ns;
};

struct elm_ebi_source_request {
    uint16_t abi_version;
    uint16_t source_kind;
    uint32_t flags;
    uint64_t parent_cell_id;
    struct elm_resource_budget budget;
    uint16_t reserved0;
    uint32_t payload_len;
    uint32_t reserved1;
};

struct elm_projection_source_request {
    uint16_t abi_version;
    uint16_t flags;
    uint32_t reserved0;
    uint64_t provider_id;
    uint32_t payload_len;
    uint32_t reserved1;
};

struct elm_load_cell_response {
    uint64_t cell_id;
    int32_t status;
    uint32_t final_state;
    uint32_t reason;
    uint32_t reserved;
};

struct elm_nexus_bind_request {
    uint64_t cell_id;
    uint64_t port_id;
    uint32_t flags;
    uint16_t contract_len;
    uint16_t reserved;
    uint8_t contract[ELM_NEXUS_CONTRACT_LEN];
};

struct elm_nexus_unbind_request {
    uint64_t binding_id;
    uint32_t flags;
    uint32_t reserved;
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

struct elm_provider_invoke_request {
    struct elm_call_frame frame;
};

struct elm_provider_invoke_response {
    struct elm_reply_frame reply;
};

struct elm_provider_async_submit_request {
    struct elm_call_frame frame;
    uint32_t timeout_ms;
    uint32_t result_ttl_ms;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_provider_async_poll_request {
    uint64_t ticket_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_provider_async_cancel_request {
    uint64_t ticket_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_provider_snapshot_request {
    uint64_t port_id;
    uint64_t binding_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_provider_port_register_request {
    uint64_t owner_cell_id;
    uint32_t flags;
    uint32_t access_policy;
    uint32_t direction;
    uint32_t mode;
    uint16_t contract_len;
    uint16_t reserved0;
    uint32_t reserved1;
    uint8_t contract[ELM_NEXUS_CONTRACT_LEN];
};

struct elm_provider_port_unregister_request {
    uint64_t port_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_runtime_log_request {
    uint64_t binding_id;
    uint32_t level;
    uint32_t flags;
    uint16_t message_len;
    uint16_t reserved0;
    uint32_t reserved1;
    uint8_t message[ELM_RUNTIME_LOG_MESSAGE_LEN];
};

struct elm_runtime_event_request {
    uint64_t binding_id;
    uint64_t cursor;
    uint32_t flags;
    uint32_t reserved;
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

struct elm_mgr_event_unsubscribe_request {
    uint64_t subscription_id;
    uint64_t owner_cell_id;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_mgr_subscribed_event_read_request {
    uint64_t subscription_id;
    uint64_t cursor;
    uint32_t max_records;
    uint32_t flags;
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

struct elm_mgr_topology_header {
    uint16_t abi_version;
    uint16_t relation_entry_size;
    uint32_t relation_count;
    uint32_t cell_count;
    uint32_t reserved;
    uint64_t event_sequence;
};

struct elm_mgr_relation_record {
    uint32_t kind;
    uint32_t flags;
    uint64_t source;
    uint64_t target;
    uint16_t contract_len;
    uint16_t point_len;
    uint32_t reserved;
    uint8_t contract[ELM_MGR_RELATION_CONTRACT_LEN];
    uint8_t point[ELM_MGR_RELATION_POINT_LEN];
};

struct elm_mgr_audit_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint32_t dropped_count;
    uint32_t reserved;
    uint64_t last_sequence;
};

struct elm_mgr_audit_record {
    uint64_t sequence;
    uint32_t action;
    int32_t status;
    uint64_t cell_id;
    uint64_t blockers;
    uint32_t final_state;
    uint32_t flags;
    uint32_t actor_kind;
    uint32_t authority;
    uint64_t actor_id;
    uint64_t authority_id;
    uint64_t actor_generation;
    uint64_t policy_epoch;
    uint64_t credential_id;
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

struct elm_runtime_log_response {
    uint64_t binding_id;
    uint32_t accepted_len;
    int32_t status;
    uint64_t submitted_logs;
    uint64_t reserved;
};

struct elm_runtime_event_response {
    uint64_t binding_id;
    uint64_t cursor;
    uint64_t next_cursor;
    uint64_t dropped_events;
    uint32_t has_event;
    int32_t status;
    struct elm_event_record event;
};

struct elm_runtime_port_stats_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint64_t event_sequence;
};

struct elm_runtime_port_stats_record {
    uint64_t binding_id;
    uint64_t cell_id;
    uint64_t port_id;
    uint64_t lease_id;
    uint64_t cursor;
    uint64_t submitted_logs;
    uint64_t delivered_events;
    uint64_t dropped_events;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_provider_snapshot_header {
    uint16_t abi_version;
    uint16_t header_size;
    int32_t status;
    uint64_t port_id;
    uint64_t binding_id;
    uint32_t payload_len;
    uint32_t record_count;
    uint32_t flags;
    uint32_t reserved;
};

struct elm_provider_async_submit_response {
    uint64_t ticket_id;
    uint64_t binding_id;
    uint64_t call_id;
    int32_t status;
    uint32_t state;
    uint32_t queue_depth;
    uint32_t reserved;
    uint64_t blockers;
};

struct elm_provider_async_poll_response {
    uint64_t ticket_id;
    uint32_t state;
    int32_t status;
    struct elm_reply_frame reply;
    uint64_t blockers;
    uint64_t expires_at_ns;
};

struct elm_provider_async_cancel_response {
    uint64_t ticket_id;
    uint32_t state;
    int32_t status;
    uint64_t blockers;
};

struct elm_provider_port_stats_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint64_t event_sequence;
};

struct elm_provider_port_record {
    uint64_t port_id;
    uint64_t owner_cell_id;
    uint32_t access_policy;
    uint32_t direction;
    uint32_t mode;
    uint32_t implemented;
    uint32_t invokable;
    uint32_t binding_count;
    uint16_t contract_len;
    uint16_t flags;
    uint64_t calls;
    uint64_t failed_calls;
    uint64_t revokes;
    uint8_t contract[ELM_NEXUS_CONTRACT_LEN];
};

struct elm_provider_port_stats_record {
    uint64_t port_id;
    uint64_t owner_cell_id;
    uint32_t binding_count;
    uint32_t flags;
    uint64_t calls;
    uint64_t failed_calls;
    uint64_t revokes;
};

struct elm_provider_queue_stats_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint64_t event_sequence;
};

struct elm_provider_queue_stats_record {
    uint64_t port_id;
    uint32_t queued;
    uint32_t running;
    uint32_t retained;
    uint32_t queue_limit;
    uint32_t max_in_flight;
    uint32_t reserved;
    uint64_t submitted;
    uint64_t completed;
    uint64_t canceled;
    uint64_t expired;
    uint64_t rejected;
};

struct elm_core_health_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    int32_t status;
    uint32_t flags;
    uint64_t event_sequence;
};

struct elm_core_health_record {
    uint32_t check_kind;
    int32_t status;
    uint64_t subject_id;
    uint64_t detail;
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

struct elm_mgr_event_subscribe_response {
    uint64_t subscription_id;
    uint64_t lease_id;
    uint64_t owner_cell_id;
    uint64_t cursor;
    int32_t status;
    uint32_t flags;
    uint64_t dropped_events;
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

struct elm_native_capability_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint32_t flags;
    uint32_t reserved;
    uint64_t event_sequence;
};

struct elm_native_capability_record {
    uint32_t kind;
    int32_t status;
    uint64_t owner_cell_id;
    uint64_t peer_cell_id;
    uint32_t requested_version;
    uint32_t selected_version;
    uint32_t flags;
    uint16_t name_len;
    uint16_t contract_len;
    uint32_t reserved;
    uint8_t name[ELM_NATIVE_CAPABILITY_NAME_LEN];
    uint8_t contract[ELM_NEXUS_CONTRACT_LEN];
};

struct elm_todo_registry_header {
    uint16_t abi_version;
    uint16_t record_entry_size;
    uint32_t record_count;
    uint32_t active_count;
    uint32_t flags;
    uint64_t event_sequence;
};

struct elm_todo_registry_record {
    uint32_t kind;
    uint32_t flags;
    uint64_t blocker;
    uint64_t subject_id;
    int32_t status;
    uint16_t name_len;
    uint16_t detail_len;
    uint32_t reserved;
    uint8_t name[ELM_TODO_NAME_LEN];
    uint8_t detail[ELM_TODO_DETAIL_LEN];
};

_Static_assert(sizeof(struct elm_mgr_call_header) == 16, "bad mgr call header size");
_Static_assert(sizeof(struct elm_mgr_response_header) == 16, "bad mgr response header size");
_Static_assert(sizeof(struct elm_core_info) == 40, "bad core info size");
_Static_assert(sizeof(struct elm_snapshot_header) == 32, "bad snapshot header size");
_Static_assert(sizeof(struct elm_cell_snapshot) == 224, "bad cell snapshot size");
_Static_assert(sizeof(struct elm_port_snapshot) == 96, "bad port snapshot size");
_Static_assert(sizeof(struct elm_event_record) == 48, "bad event record size");
_Static_assert(sizeof(struct elm_menu_snapshot_header) == 16, "bad menu header size");
_Static_assert(sizeof(struct elm_menu_item_snapshot) == 296, "bad menu item size");
_Static_assert(sizeof(struct elm_mgr_topology_header) == 24, "bad topology header size");
_Static_assert(sizeof(struct elm_mgr_relation_record) == 128, "bad relation record size");
_Static_assert(sizeof(struct elm_mgr_audit_header) == 24, "bad audit header size");
_Static_assert(sizeof(struct elm_mgr_audit_record) == 88, "bad audit record size");
_Static_assert(sizeof(struct elm_nexus_binding_snapshot_header) == 16, "bad binding header size");
_Static_assert(sizeof(struct elm_nexus_binding_record) == 120, "bad binding record size");
_Static_assert(sizeof(struct elm_call_frame) == 288, "bad call frame size");
_Static_assert(sizeof(struct elm_reply_frame) == 288, "bad reply frame size");
_Static_assert(sizeof(struct elm_lifecycle_request) == 16, "bad lifecycle request size");
_Static_assert(sizeof(struct elm_lifecycle_response) == 32, "bad lifecycle response size");
_Static_assert(sizeof(struct elm_resource_budget) == 64, "bad resource budget size");
_Static_assert(sizeof(struct elm_ebi_source_request) == 96, "bad ebi source request size");
_Static_assert(sizeof(struct elm_projection_source_request) == 24,
               "bad projection source request size");
_Static_assert(sizeof(struct elm_replace_cell_request_v1) == 32, "bad replace request size");
_Static_assert(sizeof(struct elm_nexus_bind_request) == 88, "bad bind request size");
_Static_assert(sizeof(struct elm_provider_snapshot_request) == 24, "bad provider snapshot request size");
_Static_assert(sizeof(struct elm_provider_snapshot_header) == 40, "bad provider snapshot header size");
_Static_assert(sizeof(struct elm_provider_port_register_request) == 96, "bad provider register request size");
_Static_assert(sizeof(struct elm_provider_async_submit_response) == 48, "bad async submit response size");
_Static_assert(sizeof(struct elm_provider_async_poll_response) == 320, "bad async poll response size");
_Static_assert(sizeof(struct elm_provider_async_cancel_response) == 24, "bad async cancel response size");
_Static_assert(sizeof(struct elm_provider_port_stats_header) == 16, "bad provider header size");
_Static_assert(sizeof(struct elm_provider_port_record) == 136, "bad provider record size");
_Static_assert(sizeof(struct elm_provider_port_stats_record) == 48, "bad provider stats record size");
_Static_assert(sizeof(struct elm_provider_queue_stats_header) == 16, "bad provider queue header size");
_Static_assert(sizeof(struct elm_provider_queue_stats_record) == 72, "bad provider queue record size");
_Static_assert(sizeof(struct elm_runtime_log_request) == 280, "bad runtime log request size");
_Static_assert(sizeof(struct elm_runtime_log_response) == 32, "bad runtime log response size");
_Static_assert(sizeof(struct elm_runtime_event_response) == 88, "bad runtime event response size");
_Static_assert(sizeof(struct elm_runtime_port_stats_header) == 16, "bad runtime port stats header size");
_Static_assert(sizeof(struct elm_runtime_port_stats_record) == 72, "bad runtime port stats record size");
_Static_assert(sizeof(struct elm_core_health_header) == 24, "bad health header size");
_Static_assert(sizeof(struct elm_core_health_record) == 24, "bad health record size");
_Static_assert(sizeof(struct elm_mgr_api_registry_header) == 24, "bad api header size");
_Static_assert(sizeof(struct elm_mgr_api_descriptor) == 176, "bad api descriptor size");
_Static_assert(sizeof(struct elm_mgr_event_subscribe_request) == 48, "bad event subscribe request size");
_Static_assert(sizeof(struct elm_mgr_event_subscribe_response) == 48, "bad event subscribe response size");
_Static_assert(sizeof(struct elm_mgr_event_unsubscribe_request) == 24, "bad event unsubscribe request size");
_Static_assert(sizeof(struct elm_mgr_event_unsubscribe_response) == 48, "bad event unsubscribe response size");
_Static_assert(sizeof(struct elm_mgr_event_subscription_header) == 16, "bad event subscription header size");
_Static_assert(sizeof(struct elm_mgr_event_subscription_record) == 88, "bad event subscription record size");
_Static_assert(sizeof(struct elm_mgr_subscribed_event_read_request) == 24, "bad event read request size");
_Static_assert(sizeof(struct elm_mgr_subscribed_event_read_header) == 48, "bad event read header size");
_Static_assert(sizeof(struct elm_native_capability_header) == 24, "bad native capability header size");
_Static_assert(sizeof(struct elm_native_capability_record) == 240, "bad native capability record size");
_Static_assert(sizeof(struct elm_todo_registry_header) == 24, "bad todo header size");
_Static_assert(sizeof(struct elm_todo_registry_record) == 232, "bad todo record size");

#endif
