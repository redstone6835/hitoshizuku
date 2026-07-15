#define _GNU_SOURCE

#include "elmctl_client.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint8_t g_out[ELM_MGR_MAX_INPUT];

static void usage(void)
{
    puts("elmctl commands:");
    puts("  core | snapshot | event-read | event-ack <seq> | debug-dump");
    puts("  menu | policy | trust | health | topology | audit | bindings | runtime-ports");
    puts("  providers | provider-stats | provider-queue | api | subscriptions | native | todo");
    puts("  load-eki <file> | replace-eki <cell> <file>");
    puts("  detach <cell> | pause <cell> | resume <cell> | preflight-lifecycle <cell> <action>");
    puts("  bind <cell> <port> <contract> | preflight-bind <cell> <port> <contract>");
    puts("  unbind <binding> | preflight-unbind <binding>");
    puts("  runtime-log <binding> <level> <message> | runtime-event-read <binding> <cursor> | runtime-event-ack <binding> <cursor>");
    puts("  register-provider <owner> <contract> <direction> <mode> <access>");
    puts("  unregister-provider <port>");
    puts("  invoke-provider <binding> <opcode> [payload-hex]");
    puts("  async-submit <binding> <opcode> <timeout-ms> <ttl-ms> [payload-hex]");
    puts("  async-poll <ticket> | async-cancel <ticket>");
    puts("  provider-snapshot (--port <id>|--binding <id>) [--paged <cursor>]");
    puts("  event-subscribe <owner> | event-read-sub <subscription> <cursor> <max> [advance] | event-unsubscribe <subscription> <owner>");
}

static int fail(const char *what)
{
    fprintf(stderr, "elmctl: %s: errno=%d\n", what, errno);
    return 1;
}

static int require_u64(const char *text, uint64_t *out)
{
    if (elmctl_parse_u64(text, out) != 0) {
        fprintf(stderr, "elmctl: invalid u64: %s\n", text);
        return -1;
    }
    return 0;
}

static int require_u32(const char *text, uint32_t *out)
{
    if (elmctl_parse_u32(text, out) != 0) {
        fprintf(stderr, "elmctl: invalid u32: %s\n", text);
        return -1;
    }
    return 0;
}

static int print_mgr_response(const char *name, const struct elmctl_mgr_response *response)
{
    printf("%s: status=%d(%s) payload_len=%u\n", name, response->status,
           elmctl_status_name(response->status), response->payload_len);
    if (response->payload_len != 0) {
        elmctl_print_hex(response->payload, response->payload_len);
        putchar('\n');
    }
    return response->status == ELM_MGR_STATUS_OK ? 0 : 2;
}

static int require_payload(const struct elmctl_mgr_response *response, size_t len, const char *name)
{
    if (response->status != ELM_MGR_STATUS_OK) {
        return print_mgr_response(name, response);
    }
    if (response->payload_len < len) {
        errno = EPROTO;
        return fail(name);
    }
    return 0;
}

static int cmd_policy(void)
{
    struct elmctl_mgr_response response;
    struct elm_mgr_policy_info info;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_POLICY, g_out, sizeof(g_out), &response) != 0) {
        return fail("policy");
    }
    if (require_payload(&response, sizeof(info), "policy") != 0) return 2;
    memcpy(&info, response.payload, sizeof(info));
    printf("policy: actions=0x%x flags=0x%llx blockers=0x%llx audit_capacity=%u\n",
           info.supported_actions, (unsigned long long)info.policy_flags,
           (unsigned long long)info.blocker_mask, info.audit_capacity);
    return 0;
}

static int cmd_trust(void)
{
    struct elmctl_mgr_response response;
    struct elm_trust_runtime_info_v1 info;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_TRUST_STATE, g_out, sizeof(g_out), &response) != 0) {
        return fail("trust");
    }
    if (require_payload(&response, sizeof(info), "trust") != 0) return 2;
    memcpy(&info, response.payload, sizeof(info));
    if (info.abi_version != ELM_CTL_ABI_VERSION || info.struct_size != sizeof(info)) {
        errno = EPROTO;
        return fail("trust-layout");
    }
    printf("trust: flags=0x%x sealed=%u allow_unsigned=%u unsigned_active=%u anchors=%u revoked=%u accepted_epochs=%u\n",
           info.flags, !!(info.flags & ELM_TRUST_FLAG_SEALED),
           !!(info.flags & ELM_TRUST_FLAG_ALLOW_UNSIGNED),
           !!(info.flags & ELM_TRUST_FLAG_UNSIGNED_ACTIVE), info.anchor_count,
           info.revoked_count, info.accepted_epoch_count);
    return 0;
}

static int cmd_menu(void)
{
    struct elmctl_mgr_response response;
    struct elm_menu_snapshot_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_MENU, g_out, sizeof(g_out), &response) != 0) {
        return fail("menu");
    }
    if (require_payload(&response, sizeof(header), "menu") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("menu: items=%u generation=%llu entry_size=%u\n", header.item_count,
           (unsigned long long)header.generation, header.item_entry_size);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.item_count; i++) {
        struct elm_menu_item_snapshot item;
        if ((size_t)(cursor - response.payload) + header.item_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("menu-item");
        }
        memset(&item, 0, sizeof(item));
        memcpy(&item, cursor, header.item_entry_size < sizeof(item) ? header.item_entry_size : sizeof(item));
        printf("item id=%llu owner=%llu action=%llu kind=%u flags=0x%x label=",
               (unsigned long long)item.id, (unsigned long long)item.owner,
               (unsigned long long)item.action, item.kind, item.flags);
        elmctl_print_fixed_string(item.label, item.label_len);
        printf(" route=");
        elmctl_print_fixed_string(item.route, item.route_len);
        putchar('\n');
        cursor += header.item_entry_size;
    }
    return 0;
}

static int cmd_health(void)
{
    struct elmctl_mgr_response response;
    struct elm_core_health_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_HEALTH, g_out, sizeof(g_out), &response) != 0) {
        return fail("health");
    }
    if (require_payload(&response, sizeof(header), "health") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("health: status=%d(%s) records=%u flags=0x%x events=%llu\n", header.status,
           elmctl_status_name(header.status), header.record_count, header.flags,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_core_health_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("health-record");
        }
        memcpy(&record, cursor, sizeof(record));
        printf("check kind=%u status=%d(%s) subject=%llu detail=0x%llx\n", record.check_kind,
               record.status, elmctl_status_name(record.status),
               (unsigned long long)record.subject_id, (unsigned long long)record.detail);
        cursor += header.record_entry_size;
    }
    return header.status == ELM_MGR_STATUS_OK ? 0 : 2;
}

static int cmd_topology(void)
{
    struct elmctl_mgr_response response;
    struct elm_mgr_topology_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_TOPOLOGY, g_out, sizeof(g_out), &response) != 0) {
        return fail("topology");
    }
    if (require_payload(&response, sizeof(header), "topology") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("topology: cells=%u relations=%u events=%llu entry_size=%u\n", header.cell_count,
           header.relation_count, (unsigned long long)header.event_sequence,
           header.relation_entry_size);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.relation_count; i++) {
        struct elm_mgr_relation_record record;
        if ((size_t)(cursor - response.payload) + header.relation_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("topology-relation");
        }
        memset(&record, 0, sizeof(record));
        memcpy(&record, cursor, header.relation_entry_size < sizeof(record) ? header.relation_entry_size : sizeof(record));
        printf("relation kind=%u source=%llu target=%llu contract=", record.kind,
               (unsigned long long)record.source, (unsigned long long)record.target);
        elmctl_print_fixed_string(record.contract, record.contract_len);
        printf(" point=");
        elmctl_print_fixed_string(record.point, record.point_len);
        putchar('\n');
        cursor += header.relation_entry_size;
    }
    return 0;
}

static int cmd_audit(void)
{
    struct elmctl_mgr_response response;
    struct elm_mgr_audit_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_AUDIT, g_out, sizeof(g_out), &response) != 0) {
        return fail("audit");
    }
    if (require_payload(&response, sizeof(header), "audit") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("audit: records=%u dropped=%u last=%llu\n", header.record_count, header.dropped_count,
           (unsigned long long)header.last_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_mgr_audit_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("audit-record");
        }
        memcpy(&record, cursor, sizeof(record));
        printf("audit seq=%llu action=%u status=%d(%s) cell=%llu blockers=0x%llx final=%u flags=0x%x actor_kind=%u actor=%llu authority=%u authority_id=%llu generation=%llu policy_epoch=%llu credential=%llu\n",
               (unsigned long long)record.sequence, record.action, record.status,
               elmctl_status_name(record.status), (unsigned long long)record.cell_id,
               (unsigned long long)record.blockers, record.final_state, record.flags,
               record.actor_kind, (unsigned long long)record.actor_id, record.authority,
               (unsigned long long)record.authority_id,
               (unsigned long long)record.actor_generation,
               (unsigned long long)record.policy_epoch,
               (unsigned long long)record.credential_id);
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_bindings(void)
{
    struct elmctl_mgr_response response;
    struct elm_nexus_binding_snapshot_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_NEXUS_BINDINGS, g_out, sizeof(g_out), &response) != 0) {
        return fail("bindings");
    }
    if (require_payload(&response, sizeof(header), "bindings") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("bindings: count=%u events=%llu\n", header.binding_count,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.binding_count; i++) {
        struct elm_nexus_binding_record record;
        if ((size_t)(cursor - response.payload) + header.binding_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("binding-record");
        }
        memset(&record, 0, sizeof(record));
        memcpy(&record, cursor, header.binding_entry_size < sizeof(record) ? header.binding_entry_size : sizeof(record));
        printf("binding id=%llu cell=%llu port=%llu lease=%llu active=%u contract=",
               (unsigned long long)record.binding_id, (unsigned long long)record.cell_id,
               (unsigned long long)record.port_id, (unsigned long long)record.lease_id,
               record.active);
        elmctl_print_fixed_string(record.contract, record.contract_len);
        putchar('\n');
        cursor += header.binding_entry_size;
    }
    return 0;
}

static int cmd_runtime_ports(void)
{
    struct elmctl_mgr_response response;
    struct elm_runtime_port_stats_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_RUNTIME_PORTS, g_out, sizeof(g_out), &response) != 0) {
        return fail("runtime-ports");
    }
    if (require_payload(&response, sizeof(header), "runtime-ports") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("runtime-ports: records=%u events=%llu\n", header.record_count,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_runtime_port_stats_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("runtime-port-record");
        }
        memcpy(&record, cursor, sizeof(record));
        printf("runtime binding=%llu cell=%llu port=%llu lease=%llu cursor=%llu logs=%llu delivered=%llu dropped=%llu\n",
               (unsigned long long)record.binding_id, (unsigned long long)record.cell_id,
               (unsigned long long)record.port_id, (unsigned long long)record.lease_id,
               (unsigned long long)record.cursor, (unsigned long long)record.submitted_logs,
               (unsigned long long)record.delivered_events, (unsigned long long)record.dropped_events);
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_providers(void)
{
    struct elmctl_mgr_response response;
    struct elm_provider_port_stats_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_PROVIDER_PORTS, g_out, sizeof(g_out), &response) != 0) {
        return fail("providers");
    }
    if (require_payload(&response, sizeof(header), "providers") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("providers: records=%u events=%llu\n", header.record_count,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_provider_port_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("provider-record");
        }
        memset(&record, 0, sizeof(record));
        memcpy(&record, cursor, header.record_entry_size < sizeof(record) ? header.record_entry_size : sizeof(record));
        printf("provider port=%llu owner=%llu dir=%s mode=%s access=%u implemented=%u invokable=%u bindings=%u flags=0x%x calls=%llu failed=%llu contract=",
               (unsigned long long)record.port_id, (unsigned long long)record.owner_cell_id,
               elmctl_direction_name(record.direction), elmctl_mode_name(record.mode),
               record.access_policy, record.implemented, record.invokable, record.binding_count,
               record.flags, (unsigned long long)record.calls, (unsigned long long)record.failed_calls);
        elmctl_print_fixed_string(record.contract, record.contract_len);
        putchar('\n');
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_provider_stats(void)
{
    struct elmctl_mgr_response response;
    struct elm_provider_port_stats_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_PROVIDER_STATS, g_out, sizeof(g_out), &response) != 0) {
        return fail("provider-stats");
    }
    if (require_payload(&response, sizeof(header), "provider-stats") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("provider-stats: records=%u events=%llu\n", header.record_count,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_provider_port_stats_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("provider-stats-record");
        }
        memcpy(&record, cursor, sizeof(record));
        printf("provider port=%llu owner=%llu bindings=%u flags=0x%x calls=%llu failed=%llu revokes=%llu\n",
               (unsigned long long)record.port_id, (unsigned long long)record.owner_cell_id,
               record.binding_count, record.flags, (unsigned long long)record.calls,
               (unsigned long long)record.failed_calls, (unsigned long long)record.revokes);
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_provider_queue(void)
{
    struct elmctl_mgr_response response;
    struct elm_provider_queue_stats_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_PROVIDER_QUEUE, g_out, sizeof(g_out), &response) != 0) {
        return fail("provider-queue");
    }
    if (require_payload(&response, sizeof(header), "provider-queue") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("provider-queue: records=%u events=%llu\n", header.record_count,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_provider_queue_stats_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("provider-queue-record");
        }
        memcpy(&record, cursor, sizeof(record));
        printf("queue port=%llu queued=%u running=%u retained=%u limit=%u submitted=%llu completed=%llu canceled=%llu expired=%llu rejected=%llu\n",
               (unsigned long long)record.port_id, record.queued, record.running,
               record.retained, record.queue_limit, (unsigned long long)record.submitted,
               (unsigned long long)record.completed, (unsigned long long)record.canceled,
               (unsigned long long)record.expired, (unsigned long long)record.rejected);
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_api(void)
{
    struct elmctl_mgr_response response;
    struct elm_mgr_api_registry_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_API_REGISTRY, g_out, sizeof(g_out), &response) != 0) {
        return fail("api");
    }
    if (require_payload(&response, sizeof(header), "api") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("api: records=%u generation=%llu flags=0x%x\n", header.record_count,
           (unsigned long long)header.generation, header.flags);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_mgr_api_descriptor record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("api-record");
        }
        memset(&record, 0, sizeof(record));
        memcpy(&record, cursor, header.record_entry_size < sizeof(record) ? header.record_entry_size : sizeof(record));
        printf("api id=%llu owner=%llu kind=%u call=%u abi=%u..%u flags=0x%x ns=",
               (unsigned long long)record.id, (unsigned long long)record.owner_cell_id,
               record.kind, record.call_kind, record.min_abi_version,
               record.current_abi_version, record.flags);
        elmctl_print_fixed_string(record.namespace, record.namespace_len);
        printf(" name=");
        elmctl_print_fixed_string(record.name, record.name_len);
        printf(" contract=");
        elmctl_print_fixed_string(record.contract, record.contract_len);
        putchar('\n');
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_subscriptions(void)
{
    struct elmctl_mgr_response response;
    struct elm_mgr_event_subscription_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_EVENT_SUBSCRIPTIONS, g_out, sizeof(g_out), &response) != 0) {
        return fail("subscriptions");
    }
    if (require_payload(&response, sizeof(header), "subscriptions") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("subscriptions: records=%u events=%llu\n", header.record_count,
           (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_mgr_event_subscription_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("subscription-record");
        }
        memcpy(&record, cursor, sizeof(record));
        printf("subscription id=%llu owner=%llu lease=%llu cursor=%llu kind=%u flags=0x%x delivered=%llu dropped=%llu\n",
               (unsigned long long)record.subscription_id,
               (unsigned long long)record.owner_cell_id, (unsigned long long)record.lease_id,
               (unsigned long long)record.cursor, record.kind_filter, record.flags,
               (unsigned long long)record.delivered_events,
               (unsigned long long)record.dropped_events);
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_native(void)
{
    struct elmctl_mgr_response response;
    struct elm_native_capability_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_NATIVE_CAPABILITIES, g_out, sizeof(g_out), &response) != 0) {
        return fail("native");
    }
    if (require_payload(&response, sizeof(header), "native") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("native: records=%u flags=0x%x events=%llu\n", header.record_count,
           header.flags, (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_native_capability_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("native-record");
        }
        memset(&record, 0, sizeof(record));
        memcpy(&record, cursor, header.record_entry_size < sizeof(record) ? header.record_entry_size : sizeof(record));
        printf("native kind=%u status=%d(%s) owner=%llu peer=%llu version=%u/%u flags=0x%x name=",
               record.kind, record.status, elmctl_status_name(record.status),
               (unsigned long long)record.owner_cell_id, (unsigned long long)record.peer_cell_id,
               record.requested_version, record.selected_version, record.flags);
        elmctl_print_fixed_string(record.name, record.name_len);
        printf(" contract=");
        elmctl_print_fixed_string(record.contract, record.contract_len);
        putchar('\n');
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_todo(void)
{
    struct elmctl_mgr_response response;
    struct elm_todo_registry_header header;
    const uint8_t *cursor;
    if (elmctl_mgr_call_empty(ELM_MGR_CALL_QUERY_TODO_REGISTRY, g_out, sizeof(g_out), &response) != 0) {
        return fail("todo");
    }
    if (require_payload(&response, sizeof(header), "todo") != 0) return 2;
    memcpy(&header, response.payload, sizeof(header));
    printf("todo: records=%u active=%u flags=0x%x events=%llu\n", header.record_count,
           header.active_count, header.flags, (unsigned long long)header.event_sequence);
    cursor = response.payload + sizeof(header);
    for (uint32_t i = 0; i < header.record_count; i++) {
        struct elm_todo_registry_record record;
        if ((size_t)(cursor - response.payload) + header.record_entry_size > response.payload_len) {
            errno = EPROTO;
            return fail("todo-record");
        }
        memset(&record, 0, sizeof(record));
        memcpy(&record, cursor, header.record_entry_size < sizeof(record) ? header.record_entry_size : sizeof(record));
        printf("todo kind=%u flags=0x%x blocker=0x%llx subject=%llu status=%d(%s) name=",
               record.kind, record.flags, (unsigned long long)record.blocker,
               (unsigned long long)record.subject_id, record.status,
               elmctl_status_name(record.status));
        elmctl_print_fixed_string(record.name, record.name_len);
        printf(" detail=");
        elmctl_print_fixed_string(record.detail, record.detail_len);
        putchar('\n');
        cursor += header.record_entry_size;
    }
    return 0;
}

static int cmd_core(void)
{
    struct elm_core_info info;
    if (elmctl_core_query(&info) != 0) {
        return fail("core");
    }
    printf("core: version=%u caps=0x%llx cells=%u ports=%u leases=%u events=%llu\n",
           info.core_version, (unsigned long long)info.capabilities, info.cell_count,
           info.port_count, info.lease_count, (unsigned long long)info.event_sequence);
    return 0;
}

static int cmd_snapshot(void)
{
    ssize_t written = 0;
    struct elm_snapshot_header header;
    const uint8_t *cursor;
    if (elmctl_snapshot(g_out, sizeof(g_out), &written) != 0) {
        return fail("snapshot");
    }
    if ((size_t)written < sizeof(header)) {
        errno = EPROTO;
        return fail("snapshot");
    }
    memcpy(&header, g_out, sizeof(header));
    printf("snapshot: cells=%u ports=%u leases=%u events=%llu cell_size=%u port_size=%u\n",
           header.cell_count, header.port_count, header.lease_count,
           (unsigned long long)header.event_sequence, header.cell_entry_size,
           header.port_entry_size);
    cursor = g_out + sizeof(header);
    for (uint32_t i = 0; i < header.cell_count; i++) {
        struct elm_cell_snapshot cell;
        if ((size_t)(cursor - g_out) + header.cell_entry_size > (size_t)written) {
            errno = EPROTO;
            return fail("snapshot-cell");
        }
        memset(&cell, 0, sizeof(cell));
        memcpy(&cell, cursor, header.cell_entry_size < sizeof(cell) ? header.cell_entry_size : sizeof(cell));
        printf("cell id=%llu parent=%llu name=", (unsigned long long)cell.id,
               (unsigned long long)cell.parent);
        elmctl_print_fixed_string(cell.name, cell.name_len);
        printf(" state=%s kind=%s source=%s generation=%llu native=%u isolated=%u faults=%u blocker=0x%llx\n",
               elmctl_state_name(cell.state), elmctl_kind_name(cell.kind),
               elmctl_source_name(cell.ebi_source), (unsigned long long)cell.generation,
               cell.native_code, cell.isolated, cell.native_faults,
               (unsigned long long)cell.isolation_blocker);
        printf("  budget ports=%u queue=%u events=%u pending=%u images=%u faults=%u audit=%u usage ports=%u queue=%u events=%u pending=%u images=%u faults=%u audit=%u\n",
               cell.budget_max_provider_ports, cell.budget_max_provider_queue,
               cell.budget_max_event_subscriptions, cell.budget_max_pending_loads,
               cell.budget_max_native_images, cell.budget_max_native_faults,
               cell.budget_max_audit_records, cell.usage_provider_ports,
               cell.usage_provider_queue, cell.usage_event_subscriptions,
               cell.usage_pending_loads, cell.usage_native_images, cell.usage_native_faults,
               cell.usage_audit_records);
        printf("  trust flags=0x%x release_epoch=%llu signer_key_id=",
               cell.trust_flags, (unsigned long long)cell.release_epoch);
        elmctl_print_hex(cell.signer_key_id, sizeof(cell.signer_key_id));
        putchar('\n');
        cursor += header.cell_entry_size;
    }
    for (uint32_t i = 0; i < header.port_count; i++) {
        struct elm_port_snapshot port;
        if ((size_t)(cursor - g_out) + header.port_entry_size > (size_t)written) {
            errno = EPROTO;
            return fail("snapshot-port");
        }
        memset(&port, 0, sizeof(port));
        memcpy(&port, cursor, header.port_entry_size < sizeof(port) ? header.port_entry_size : sizeof(port));
        printf("port id=%llu owner=%llu direction=%s mode=%s implemented=%u contract=",
               (unsigned long long)port.id, (unsigned long long)port.owner,
               elmctl_direction_name(port.direction), elmctl_mode_name(port.mode),
               port.implemented);
        elmctl_print_fixed_string(port.contract, port.contract_len);
        putchar('\n');
        cursor += header.port_entry_size;
    }
    return 0;
}

static int cmd_event_read(void)
{
    struct elm_event_record record;
    if (elmctl_event_read(&record) != 0) {
        return fail("event-read");
    }
    printf("event: seq=%llu kind=%u cell=%llu port=%llu binding=%llu lease=%llu\n",
           (unsigned long long)record.sequence, record.kind, (unsigned long long)record.cell,
           (unsigned long long)record.port, (unsigned long long)record.binding,
           (unsigned long long)record.lease);
    return 0;
}

static int cmd_event_ack(int argc, char **argv)
{
    uint64_t seq;
    if (argc < 3 || require_u64(argv[2], &seq) != 0) {
        usage();
        return 1;
    }
    if (elmctl_event_ack(seq) != 0) {
        return fail("event-ack");
    }
    printf("event-ack: seq=%llu\n", (unsigned long long)seq);
    return 0;
}

static int cmd_debug_dump(void)
{
    ssize_t written = 0;
    if (elmctl_debug_dump(g_out, sizeof(g_out), &written) != 0) {
        return fail("debug-dump");
    }
    fwrite(g_out, 1, (size_t)written, stdout);
    return 0;
}

static int lifecycle_call(uint32_t kind, uint64_t cell, const char *name)
{
    struct elm_lifecycle_request request = {
        .cell_id = cell,
        .flags = 0,
        .reserved = 0,
    };
    struct elmctl_mgr_response response;
    if (elmctl_mgr_call(kind, &request, sizeof(request), g_out, sizeof(g_out), &response) != 0) {
        return fail(name);
    }
    return print_mgr_response(name, &response);
}

static int cmd_lifecycle(int argc, char **argv, uint32_t kind, const char *name)
{
    uint64_t cell;
    if (argc < 3 || require_u64(argv[2], &cell) != 0) {
        usage();
        return 1;
    }
    return lifecycle_call(kind, cell, name);
}

static int cmd_preflight_lifecycle(int argc, char **argv)
{
    uint64_t cell;
    uint32_t action;
    struct elm_lifecycle_plan_request request;
    struct elmctl_mgr_response response;
    if (argc < 4 || require_u64(argv[2], &cell) != 0 || require_u32(argv[3], &action) != 0) {
        usage();
        return 1;
    }
    request.cell_id = cell;
    request.action = action;
    request.flags = 0;
    if (elmctl_mgr_call(ELM_MGR_CALL_PREFLIGHT_LIFECYCLE, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("preflight-lifecycle");
    }
    return print_mgr_response("preflight-lifecycle", &response);
}

static int source_request_from_session(uint64_t session_id, uint8_t *out, size_t cap, size_t *len)
{
    struct elm_ebi_source_request request;
    struct elm_projection_source_request projection;
    struct elm_image_session_reference_v1 reference;
    size_t total = sizeof(request) + sizeof(projection) + sizeof(reference);
    if (session_id == 0) {
        errno = EINVAL;
        return -1;
    }
    if (cap < total) {
        errno = EMSGSIZE;
        return -1;
    }
    memset(&request, 0, sizeof(request));
    memset(&projection, 0, sizeof(projection));
    memset(&reference, 0, sizeof(reference));
    request.abi_version = ELM_EBI_SOURCE_ABI_VERSION;
    request.source_kind = ELM_EBI_SOURCE_KIND_PROJECTION;
    request.flags = ELM_EBI_SOURCE_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS;
    request.parent_cell_id = ELM_MGR_BUILTIN_ID;
    request.budget.max_provider_ports = ELM_RESOURCE_BUDGET_DEFAULT_PROVIDER_PORTS;
    request.budget.max_provider_queue = ELM_RESOURCE_BUDGET_DEFAULT_PROVIDER_QUEUE;
    request.budget.max_event_subscriptions = ELM_RESOURCE_BUDGET_DEFAULT_EVENT_SUBSCRIPTIONS;
    request.budget.max_pending_loads = ELM_RESOURCE_BUDGET_DEFAULT_PENDING_LOADS;
    request.budget.max_native_images = ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_IMAGES;
    request.budget.max_native_faults = ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_FAULTS;
    request.budget.max_audit_records = ELM_RESOURCE_BUDGET_DEFAULT_AUDIT_RECORDS;
    request.budget.max_concurrent_calls = ELM_RESOURCE_BUDGET_DEFAULT_CONCURRENT_CALLS;
    request.budget.max_native_image_bytes = ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_IMAGE_BYTES;
    request.budget.max_native_stack_bytes = ELM_RESOURCE_BUDGET_DEFAULT_NATIVE_STACK_BYTES;
    request.budget.max_dynamic_alloc_bytes = ELM_RESOURCE_BUDGET_DEFAULT_DYNAMIC_ALLOC_BYTES;
    request.budget.max_cpu_time_ns_per_call = ELM_RESOURCE_BUDGET_DEFAULT_CPU_TIME_NS_PER_CALL;
    request.budget.cpu_budget_ns_per_period = ELM_RESOURCE_BUDGET_DEFAULT_CPU_BUDGET_NS_PER_PERIOD;
    request.budget.cpu_period_ns = ELM_RESOURCE_BUDGET_DEFAULT_CPU_PERIOD_NS;
    request.payload_len = (uint32_t)(sizeof(projection) + sizeof(reference));
    projection.abi_version = ELM_EBI_PROJECTION_SOURCE_ABI_VERSION;
    projection.flags = ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION;
    projection.provider_id = ELM_EKI_PROJECTION_SOURCE_ID;
    projection.payload_len = sizeof(reference);
    reference.abi_version = ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION;
    reference.session_id = session_id;
    memcpy(out, &request, sizeof(request));
    memcpy(out + sizeof(request), &projection, sizeof(projection));
    memcpy(out + sizeof(request) + sizeof(projection), &reference, sizeof(reference));
    *len = total;
    return 0;
}

static void abort_session_preserving_errno(uint64_t session_id)
{
    int saved = errno;
    (void)elmctl_abort_image_session(session_id);
    errno = saved;
}

static int cmd_load_eki(int argc, char **argv)
{
    uint8_t input[sizeof(struct elm_ebi_source_request) +
                  sizeof(struct elm_projection_source_request) +
                  sizeof(struct elm_image_session_reference_v1)];
    size_t input_len = 0;
    uint64_t session_id = 0;
    struct elmctl_mgr_response response;
    struct elm_load_cell_response load;
    int ret;
    if (argc < 3) {
        usage();
        return 1;
    }
    if (elmctl_upload_image_file(argv[2], &session_id) != 0) {
        return fail("load-eki");
    }
    if (source_request_from_session(session_id, input, sizeof(input), &input_len) != 0) {
        abort_session_preserving_errno(session_id);
        return fail("load-eki");
    }
    if (elmctl_mgr_call(ELM_MGR_CALL_LOAD_CELL, input, input_len, g_out, sizeof(g_out),
                        &response) != 0) {
        abort_session_preserving_errno(session_id);
        return fail("load-eki");
    }
    if (response.status != ELM_MGR_STATUS_OK) {
        abort_session_preserving_errno(session_id);
        ret = print_mgr_response("load-eki", &response);
    } else if (response.payload_len != sizeof(load)) {
        errno = EPROTO;
        ret = fail("load-eki-response");
    } else {
        memcpy(&load, response.payload, sizeof(load));
        if (load.reserved != 0) {
            errno = EPROTO;
            ret = fail("load-eki-response");
        } else {
            printf("load-eki: cell=%llu status=%d(%s) state=%s reason=%u\n",
                   (unsigned long long)load.cell_id, load.status,
                   elmctl_ebi_load_status_name(load.status),
                   elmctl_state_name(load.final_state), load.reason);
            ret = load.status == ELM_EBI_LOAD_STATUS_OK ? 0 : 2;
        }
    }
    return ret;
}

static int cmd_replace_eki(int argc, char **argv)
{
    uint8_t input[sizeof(struct elm_replace_cell_request_v1) +
                  sizeof(struct elm_projection_source_request) +
                  sizeof(struct elm_image_session_reference_v1)];
    struct elm_replace_cell_request_v1 request;
    struct elm_projection_source_request projection;
    struct elm_image_session_reference_v1 reference;
    uint64_t cell;
    uint64_t session_id = 0;
    size_t input_len = sizeof(input);
    struct elmctl_mgr_response response;
    struct elm_replace_cell_response_v1 replace;
    int ret;
    if (argc < 4 || require_u64(argv[2], &cell) != 0) {
        usage();
        return 1;
    }
    if (elmctl_upload_image_file(argv[3], &session_id) != 0) {
        return fail("replace-eki");
    }
    memset(&request, 0, sizeof(request));
    memset(&projection, 0, sizeof(projection));
    memset(&reference, 0, sizeof(reference));
    request.abi_version = ELM_REPLACE_CELL_ABI_VERSION;
    request.flags = ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS;
    request.source_kind = ELM_EBI_SOURCE_KIND_PROJECTION;
    request.target_cell_id = cell;
    request.source_payload_len = (uint32_t)(sizeof(projection) + sizeof(reference));
    projection.abi_version = ELM_EBI_PROJECTION_SOURCE_ABI_VERSION;
    projection.flags = ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION;
    projection.provider_id = ELM_EKI_PROJECTION_SOURCE_ID;
    projection.payload_len = sizeof(reference);
    reference.abi_version = ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION;
    reference.session_id = session_id;
    memcpy(input, &request, sizeof(request));
    memcpy(input + sizeof(request), &projection, sizeof(projection));
    memcpy(input + sizeof(request) + sizeof(projection), &reference, sizeof(reference));
    if (elmctl_mgr_call(ELM_MGR_CALL_REPLACE_CELL, input, input_len, g_out,
                        sizeof(g_out), &response) != 0) {
        abort_session_preserving_errno(session_id);
        return fail("replace-eki");
    }
    if (response.status != ELM_MGR_STATUS_OK) {
        abort_session_preserving_errno(session_id);
        ret = print_mgr_response("replace-eki", &response);
    } else if (response.payload_len != sizeof(replace)) {
        errno = EPROTO;
        ret = fail("replace-eki-response");
    } else {
        memcpy(&replace, response.payload, sizeof(replace));
        printf("replace-eki: cell=%llu status=%d(%s) state=%s generation=%llu migrated=%u reason=%u blockers=0x%llx\n",
               (unsigned long long)replace.cell_id, replace.status,
               elmctl_status_name(replace.status), elmctl_state_name(replace.final_state),
               (unsigned long long)replace.generation, replace.migrated_len, replace.reason,
               (unsigned long long)replace.blockers);
        ret = replace.status == ELM_MGR_STATUS_OK ? 0 : 2;
    }
    return ret;
}

static int bind_common(int argc, char **argv, uint32_t kind, const char *name)
{
    struct elm_nexus_bind_request request;
    struct elmctl_mgr_response response;
    if (argc < 5 || require_u64(argv[2], &request.cell_id) != 0 ||
        require_u64(argv[3], &request.port_id) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved = 0;
    memset(request.contract, 0, sizeof(request.contract));
    elmctl_copy_string(request.contract, sizeof(request.contract), &request.contract_len, argv[4]);
    if (elmctl_mgr_call(kind, &request, sizeof(request), g_out, sizeof(g_out), &response) != 0) {
        return fail(name);
    }
    return print_mgr_response(name, &response);
}

static int unbind_common(int argc, char **argv, uint32_t kind, const char *name)
{
    struct elm_nexus_unbind_request request;
    if (argc < 3 || require_u64(argv[2], &request.binding_id) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved = 0;
    struct elmctl_mgr_response response;
    if (elmctl_mgr_call(kind, &request, sizeof(request), g_out, sizeof(g_out), &response) != 0) {
        return fail(name);
    }
    return print_mgr_response(name, &response);
}

static int cmd_runtime_log(int argc, char **argv)
{
    struct elm_runtime_log_request request;
    struct elmctl_mgr_response response;
    if (argc < 5 || require_u64(argv[2], &request.binding_id) != 0 ||
        require_u32(argv[3], &request.level) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved0 = 0;
    request.reserved1 = 0;
    memset(request.message, 0, sizeof(request.message));
    elmctl_copy_string(request.message, sizeof(request.message), &request.message_len, argv[4]);
    if (elmctl_mgr_call(ELM_MGR_CALL_SUBMIT_RUNTIME_LOG, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("runtime-log");
    }
    return print_mgr_response("runtime-log", &response);
}

static int cmd_runtime_event(int argc, char **argv, uint32_t kind, const char *name)
{
    struct elm_runtime_event_request request;
    struct elmctl_mgr_response response;
    if (argc < 4 || require_u64(argv[2], &request.binding_id) != 0 ||
        require_u64(argv[3], &request.cursor) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved = 0;
    if (elmctl_mgr_call(kind, &request, sizeof(request), g_out, sizeof(g_out), &response) != 0) {
        return fail(name);
    }
    return print_mgr_response(name, &response);
}

static int cmd_register_provider(int argc, char **argv)
{
    struct elm_provider_port_register_request request;
    struct elmctl_mgr_response response;
    if (argc < 7 || require_u64(argv[2], &request.owner_cell_id) != 0 ||
        require_u32(argv[4], &request.direction) != 0 ||
        require_u32(argv[5], &request.mode) != 0 ||
        require_u32(argv[6], &request.access_policy) != 0) {
        usage();
        return 1;
    }
    request.flags = ELM_PROVIDER_PORT_FLAG_NONE;
    request.reserved0 = 0;
    request.reserved1 = 0;
    memset(request.contract, 0, sizeof(request.contract));
    elmctl_copy_string(request.contract, sizeof(request.contract), &request.contract_len, argv[3]);
    if (elmctl_mgr_call(ELM_MGR_CALL_REGISTER_PROVIDER_PORT, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("register-provider");
    }
    return print_mgr_response("register-provider", &response);
}

static int cmd_unregister_provider(int argc, char **argv)
{
    struct elm_provider_port_unregister_request request;
    struct elmctl_mgr_response response;
    if (argc < 3 || require_u64(argv[2], &request.port_id) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved = 0;
    if (elmctl_mgr_call(ELM_MGR_CALL_UNREGISTER_PROVIDER_PORT, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("unregister-provider");
    }
    return print_mgr_response("unregister-provider", &response);
}

static int fill_call_frame(int argc, char **argv, struct elm_call_frame *frame, int base)
{
    size_t payload_len = 0;
    if (argc < base + 2 || require_u64(argv[base], &frame->binding_id) != 0 ||
        require_u32(argv[base + 1], &frame->opcode) != 0) {
        usage();
        return -1;
    }
    frame->call_id = 1;
    frame->flags = 0;
    frame->payload_len = 0;
    frame->reserved0 = 0;
    frame->reserved1 = 0;
    memset(frame->payload, 0, sizeof(frame->payload));
    if (argc > base + 2 &&
        elmctl_parse_hex(argv[base + 2], frame->payload, sizeof(frame->payload), &payload_len) != 0) {
        return -1;
    }
    frame->payload_len = (uint16_t)payload_len;
    return 0;
}

static int cmd_invoke_provider(int argc, char **argv)
{
    struct elm_provider_invoke_request request;
    struct elmctl_mgr_response response;
    if (fill_call_frame(argc, argv, &request.frame, 2) != 0) {
        return 1;
    }
    if (elmctl_mgr_call(ELM_MGR_CALL_INVOKE_PROVIDER, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("invoke-provider");
    }
    return print_mgr_response("invoke-provider", &response);
}

static int cmd_async_submit(int argc, char **argv)
{
    struct elm_provider_async_submit_request request;
    struct elmctl_mgr_response response;
    size_t payload_len = 0;
    if (argc < 6 || require_u64(argv[2], &request.frame.binding_id) != 0 ||
        require_u32(argv[3], &request.frame.opcode) != 0 ||
        require_u32(argv[4], &request.timeout_ms) != 0 ||
        require_u32(argv[5], &request.result_ttl_ms) != 0) {
        usage();
        return 1;
    }
    request.frame.call_id = 1;
    request.frame.flags = 0;
    request.frame.payload_len = 0;
    request.frame.reserved0 = 0;
    request.frame.reserved1 = 0;
    memset(request.frame.payload, 0, sizeof(request.frame.payload));
    if (argc > 6) {
        if (elmctl_parse_hex(argv[6], request.frame.payload, sizeof(request.frame.payload),
                             &payload_len) != 0) {
            return fail("async-submit-payload");
        }
    }
    request.frame.payload_len = (uint16_t)payload_len;
    request.flags = 0;
    request.reserved = 0;
    if (elmctl_mgr_call(ELM_MGR_CALL_SUBMIT_PROVIDER_CALL, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("async-submit");
    }
    return print_mgr_response("async-submit", &response);
}

static int cmd_ticket(int argc, char **argv, uint32_t kind, const char *name)
{
    struct elm_provider_async_poll_request request;
    struct elmctl_mgr_response response;
    if (argc < 3 || require_u64(argv[2], &request.ticket_id) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved = 0;
    if (elmctl_mgr_call(kind, &request, sizeof(request), g_out, sizeof(g_out), &response) != 0) {
        return fail(name);
    }
    return print_mgr_response(name, &response);
}

static int cmd_provider_snapshot(int argc, char **argv)
{
    struct elm_provider_snapshot_request request = {0};
    struct elmctl_mgr_response response;
    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--port") == 0 && i + 1 < argc) {
            if (require_u64(argv[++i], &request.port_id) != 0) {
                return 1;
            }
        } else if (strcmp(argv[i], "--binding") == 0 && i + 1 < argc) {
            if (require_u64(argv[++i], &request.binding_id) != 0) {
                return 1;
            }
        } else if (strcmp(argv[i], "--paged") == 0 && i + 1 < argc) {
            request.flags |= ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED;
            if (require_u32(argv[++i], &request.reserved) != 0) {
                return 1;
            }
        } else {
            usage();
            return 1;
        }
    }
    if (request.port_id == 0 && request.binding_id == 0) {
        usage();
        return 1;
    }
    if (elmctl_mgr_call(ELM_MGR_CALL_QUERY_PROVIDER_SNAPSHOT, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("provider-snapshot");
    }
    return print_mgr_response("provider-snapshot", &response);
}

static int cmd_event_subscribe(int argc, char **argv)
{
    struct elm_mgr_event_subscribe_request request = {0};
    struct elmctl_mgr_response response;
    if (argc < 3 || require_u64(argv[2], &request.owner_cell_id) != 0) {
        usage();
        return 1;
    }
    if (elmctl_mgr_call(ELM_MGR_CALL_SUBSCRIBE_EVENT, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("event-subscribe");
    }
    return print_mgr_response("event-subscribe", &response);
}

static int cmd_event_read_sub(int argc, char **argv)
{
    struct elm_mgr_subscribed_event_read_request request;
    struct elmctl_mgr_response response;
    if (argc < 5 || require_u64(argv[2], &request.subscription_id) != 0 ||
        require_u64(argv[3], &request.cursor) != 0 || require_u32(argv[4], &request.max_records) != 0) {
        usage();
        return 1;
    }
    request.flags = argc > 5 && strcmp(argv[5], "advance") == 0 ? ELM_MGR_EVENT_READ_FLAG_ADVANCE : 0;
    if (elmctl_mgr_call(ELM_MGR_CALL_READ_SUBSCRIBED_EVENTS, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("event-read-sub");
    }
    return print_mgr_response("event-read-sub", &response);
}

static int cmd_event_unsubscribe(int argc, char **argv)
{
    struct elm_mgr_event_unsubscribe_request request;
    struct elmctl_mgr_response response;
    if (argc < 4 || require_u64(argv[2], &request.subscription_id) != 0 ||
        require_u64(argv[3], &request.owner_cell_id) != 0) {
        usage();
        return 1;
    }
    request.flags = 0;
    request.reserved = 0;
    if (elmctl_mgr_call(ELM_MGR_CALL_UNSUBSCRIBE_EVENT, &request, sizeof(request), g_out,
                        sizeof(g_out), &response) != 0) {
        return fail("event-unsubscribe");
    }
    return print_mgr_response("event-unsubscribe", &response);
}

int main(int argc, char **argv)
{
    if (argc < 2 || strcmp(argv[1], "help") == 0 || strcmp(argv[1], "--help") == 0) {
        usage();
        return argc < 2 ? 1 : 0;
    }
    if (strcmp(argv[1], "core") == 0) return cmd_core();
    if (strcmp(argv[1], "snapshot") == 0) return cmd_snapshot();
    if (strcmp(argv[1], "event-read") == 0) return cmd_event_read();
    if (strcmp(argv[1], "event-ack") == 0) return cmd_event_ack(argc, argv);
    if (strcmp(argv[1], "debug-dump") == 0) return cmd_debug_dump();
    if (strcmp(argv[1], "menu") == 0) return cmd_menu();
    if (strcmp(argv[1], "policy") == 0) return cmd_policy();
    if (strcmp(argv[1], "trust") == 0) return cmd_trust();
    if (strcmp(argv[1], "health") == 0) return cmd_health();
    if (strcmp(argv[1], "topology") == 0) return cmd_topology();
    if (strcmp(argv[1], "audit") == 0) return cmd_audit();
    if (strcmp(argv[1], "bindings") == 0) return cmd_bindings();
    if (strcmp(argv[1], "runtime-ports") == 0) return cmd_runtime_ports();
    if (strcmp(argv[1], "providers") == 0) return cmd_providers();
    if (strcmp(argv[1], "provider-stats") == 0) return cmd_provider_stats();
    if (strcmp(argv[1], "provider-queue") == 0) return cmd_provider_queue();
    if (strcmp(argv[1], "api") == 0) return cmd_api();
    if (strcmp(argv[1], "subscriptions") == 0) return cmd_subscriptions();
    if (strcmp(argv[1], "native") == 0) return cmd_native();
    if (strcmp(argv[1], "todo") == 0) return cmd_todo();
    if (strcmp(argv[1], "load-eki") == 0) return cmd_load_eki(argc, argv);
    if (strcmp(argv[1], "replace-eki") == 0) return cmd_replace_eki(argc, argv);
    if (strcmp(argv[1], "detach") == 0) return cmd_lifecycle(argc, argv, ELM_MGR_CALL_DETACH_CELL, "detach");
    if (strcmp(argv[1], "pause") == 0) return cmd_lifecycle(argc, argv, ELM_MGR_CALL_PAUSE_CELL, "pause");
    if (strcmp(argv[1], "resume") == 0) return cmd_lifecycle(argc, argv, ELM_MGR_CALL_RESUME_CELL, "resume");
    if (strcmp(argv[1], "preflight-lifecycle") == 0) return cmd_preflight_lifecycle(argc, argv);
    if (strcmp(argv[1], "bind") == 0) return bind_common(argc, argv, ELM_MGR_CALL_COMMIT_BIND, "bind");
    if (strcmp(argv[1], "preflight-bind") == 0) return bind_common(argc, argv, ELM_MGR_CALL_PREFLIGHT_BIND, "preflight-bind");
    if (strcmp(argv[1], "unbind") == 0) return unbind_common(argc, argv, ELM_MGR_CALL_COMMIT_UNBIND, "unbind");
    if (strcmp(argv[1], "preflight-unbind") == 0) return unbind_common(argc, argv, ELM_MGR_CALL_PREFLIGHT_UNBIND, "preflight-unbind");
    if (strcmp(argv[1], "runtime-log") == 0) return cmd_runtime_log(argc, argv);
    if (strcmp(argv[1], "runtime-event-read") == 0) return cmd_runtime_event(argc, argv, ELM_MGR_CALL_READ_RUNTIME_EVENT, "runtime-event-read");
    if (strcmp(argv[1], "runtime-event-ack") == 0) return cmd_runtime_event(argc, argv, ELM_MGR_CALL_ACK_RUNTIME_EVENT, "runtime-event-ack");
    if (strcmp(argv[1], "register-provider") == 0) return cmd_register_provider(argc, argv);
    if (strcmp(argv[1], "unregister-provider") == 0) return cmd_unregister_provider(argc, argv);
    if (strcmp(argv[1], "invoke-provider") == 0) return cmd_invoke_provider(argc, argv);
    if (strcmp(argv[1], "async-submit") == 0) return cmd_async_submit(argc, argv);
    if (strcmp(argv[1], "async-poll") == 0) return cmd_ticket(argc, argv, ELM_MGR_CALL_POLL_PROVIDER_REPLY, "async-poll");
    if (strcmp(argv[1], "async-cancel") == 0) return cmd_ticket(argc, argv, ELM_MGR_CALL_CANCEL_PROVIDER_CALL, "async-cancel");
    if (strcmp(argv[1], "provider-snapshot") == 0) return cmd_provider_snapshot(argc, argv);
    if (strcmp(argv[1], "event-subscribe") == 0) return cmd_event_subscribe(argc, argv);
    if (strcmp(argv[1], "event-read-sub") == 0) return cmd_event_read_sub(argc, argv);
    if (strcmp(argv[1], "event-unsubscribe") == 0) return cmd_event_unsubscribe(argc, argv);
    usage();
    return 1;
}
