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

#define ELM_MGR_MAX_PAYLOAD 4096u
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

#define ELM_EBI_SOURCE_ABI_VERSION 1u
#define ELM_EBI_SOURCE_KIND_EKI 1u
#define ELM_EBI_SOURCE_KIND_PROJECTION 2u
#define ELM_EBI_SOURCE_KIND_BUILTIN 3u
#define ELM_EBI_SOURCE_KIND_MEMORY 4u
#define ELM_EBI_SOURCE_KIND_REMOTE 5u

#define ELM_REPLACE_CELL_ABI_VERSION 1u
#define ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED (1u << 0)
#define ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE (1u << 0)
#define ELM_MGR_EVENT_READ_FLAG_ADVANCE (1u << 0)
#define ELM_PROVIDER_PORT_FLAG_NONE 0u

#define ELM_PORT_ACCESS_PUBLIC 1u
#define ELM_PORT_ACCESS_EXTENSION_ONLY 2u
#define ELM_PORT_ACCESS_INTERNAL 3u
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
    uint32_t reserved2;
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
    uint32_t reserved;
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
_Static_assert(sizeof(struct elm_cell_snapshot) == 184, "bad cell snapshot size");
_Static_assert(sizeof(struct elm_port_snapshot) == 96, "bad port snapshot size");
_Static_assert(sizeof(struct elm_event_record) == 48, "bad event record size");
_Static_assert(sizeof(struct elm_menu_snapshot_header) == 16, "bad menu header size");
_Static_assert(sizeof(struct elm_menu_item_snapshot) == 296, "bad menu item size");
_Static_assert(sizeof(struct elm_mgr_topology_header) == 24, "bad topology header size");
_Static_assert(sizeof(struct elm_mgr_relation_record) == 128, "bad relation record size");
_Static_assert(sizeof(struct elm_mgr_audit_header) == 24, "bad audit header size");
_Static_assert(sizeof(struct elm_mgr_audit_record) == 40, "bad audit record size");
_Static_assert(sizeof(struct elm_nexus_binding_snapshot_header) == 16, "bad binding header size");
_Static_assert(sizeof(struct elm_nexus_binding_record) == 120, "bad binding record size");
_Static_assert(sizeof(struct elm_call_frame) == 288, "bad call frame size");
_Static_assert(sizeof(struct elm_reply_frame) == 288, "bad reply frame size");
_Static_assert(sizeof(struct elm_lifecycle_request) == 16, "bad lifecycle request size");
_Static_assert(sizeof(struct elm_lifecycle_response) == 32, "bad lifecycle response size");
_Static_assert(sizeof(struct elm_ebi_source_request) == 16, "bad ebi source request size");
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
