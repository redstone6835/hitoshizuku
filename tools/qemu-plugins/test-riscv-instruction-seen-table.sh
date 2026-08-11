#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT INT TERM

cat >"$temporary/test.c" <<'EOF'
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static bool fail_allocations;

static void *test_calloc(size_t count, size_t size)
{
    if (fail_allocations) {
        return NULL;
    }
    return calloc(count, size);
}

#define RISCV_SEEN_CALLOC test_calloc
#include "tools/qemu-plugins/riscv_instruction_seen_table.h"

static void require(bool condition, const char *message)
{
    if (!condition) {
        fprintf(stderr, "seen-table test: %s\n", message);
        exit(1);
    }
}

int main(void)
{
    const uint64_t entry_count = UINT64_C(1200000);
    struct riscv_seen_table table = {0};
    require(riscv_seen_table_init(&table, 8), "initial allocation failed");

    for (uint64_t key = 1; key <= entry_count; ++key) {
        require(riscv_seen_table_insert(&table, key) == RISCV_SEEN_FIRST,
                "new key was not recorded");
    }
    require(table.count == entry_count, "entry count did not close");
    require(table.capacity > (UINT64_C(1) << 20),
            "table did not grow beyond the former fixed limit");
    for (uint64_t key = 1; key <= entry_count; ++key) {
        require(riscv_seen_table_insert(&table, key) == RISCV_SEEN_DUPLICATE,
                "duplicate key was not detected");
    }

    riscv_seen_table_release(&table);
    require(riscv_seen_table_init(&table, 8), "failure fixture allocation failed");
    for (uint64_t key = 1; key <= 6; ++key) {
        require(riscv_seen_table_insert(&table, key) == RISCV_SEEN_FIRST,
                "failure fixture setup failed");
    }
    fail_allocations = true;
    require(riscv_seen_table_insert(&table, 7) == RISCV_SEEN_DROPPED,
            "allocation failure did not report a tracking drop");
    fail_allocations = false;
    require(riscv_seen_table_insert(&table, 7) == RISCV_SEEN_FIRST,
            "table did not recover after allocation failure");
    require(riscv_seen_table_insert(&table, 7) == RISCV_SEEN_DUPLICATE,
            "recovered key was not retained");
    riscv_seen_table_release(&table);
    return 0;
}
EOF

cc -std=c11 -O2 -Wall -Wextra -Werror -I"$root" \
    "$temporary/test.c" -o "$temporary/test"
"$temporary/test"
