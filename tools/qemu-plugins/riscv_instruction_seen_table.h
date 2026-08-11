#ifndef MYGO_RISCV_INSTRUCTION_SEEN_TABLE_H
#define MYGO_RISCV_INSTRUCTION_SEEN_TABLE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#ifndef RISCV_SEEN_CALLOC
#define RISCV_SEEN_CALLOC calloc
#endif

#ifndef RISCV_SEEN_FREE
#define RISCV_SEEN_FREE free
#endif

struct riscv_seen_table {
    uint64_t *keys;
    size_t capacity;
    size_t count;
};

enum riscv_seen_result {
    RISCV_SEEN_FIRST,
    RISCV_SEEN_DUPLICATE,
    RISCV_SEEN_DROPPED,
};

static inline uint64_t riscv_seen_mix64(uint64_t value)
{
    value ^= value >> 33;
    value *= UINT64_C(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value *= UINT64_C(0xc4ceb9fe1a85ec53);
    return value ^ (value >> 33);
}

static inline bool riscv_seen_power_of_two(size_t value)
{
    return value != 0 && (value & (value - 1)) == 0;
}

static inline bool riscv_seen_table_init(struct riscv_seen_table *table,
                                         size_t capacity)
{
    if (!riscv_seen_power_of_two(capacity) ||
        capacity > SIZE_MAX / sizeof(*table->keys)) {
        return false;
    }
    table->keys = RISCV_SEEN_CALLOC(capacity, sizeof(*table->keys));
    if (!table->keys) {
        table->capacity = 0;
        table->count = 0;
        return false;
    }
    table->capacity = capacity;
    table->count = 0;
    return true;
}

static inline void riscv_seen_table_release(struct riscv_seen_table *table)
{
    RISCV_SEEN_FREE(table->keys);
    table->keys = NULL;
    table->capacity = 0;
    table->count = 0;
}

static inline size_t riscv_seen_slot(const struct riscv_seen_table *table,
                                     uint64_t key)
{
    return (size_t)riscv_seen_mix64(key) & (table->capacity - 1);
}

static inline void riscv_seen_insert_rehashed(struct riscv_seen_table *table,
                                              uint64_t key)
{
    size_t slot = riscv_seen_slot(table, key);

    while (table->keys[slot] != 0) {
        slot = (slot + 1) & (table->capacity - 1);
    }
    table->keys[slot] = key;
    ++table->count;
}

static inline bool riscv_seen_table_grow(struct riscv_seen_table *table)
{
    if (!table->keys || table->capacity > SIZE_MAX / 2) {
        return false;
    }
    size_t new_capacity = table->capacity * 2;
    if (new_capacity > SIZE_MAX / sizeof(*table->keys)) {
        return false;
    }
    uint64_t *new_keys =
        RISCV_SEEN_CALLOC(new_capacity, sizeof(*table->keys));
    if (!new_keys) {
        return false;
    }

    struct riscv_seen_table grown = {
        .keys = new_keys,
        .capacity = new_capacity,
        .count = 0,
    };
    for (size_t index = 0; index < table->capacity; ++index) {
        if (table->keys[index] != 0) {
            riscv_seen_insert_rehashed(&grown, table->keys[index]);
        }
    }
    RISCV_SEEN_FREE(table->keys);
    *table = grown;
    return true;
}

static inline enum riscv_seen_result
riscv_seen_table_insert(struct riscv_seen_table *table, uint64_t key)
{
    if (!table->keys || !riscv_seen_power_of_two(table->capacity)) {
        return RISCV_SEEN_DROPPED;
    }
    if (key == 0) {
        key = UINT64_C(1);
    }

    size_t slot = riscv_seen_slot(table, key);
    for (size_t probe = 0; probe < table->capacity; ++probe) {
        if (table->keys[slot] == key) {
            return RISCV_SEEN_DUPLICATE;
        }
        if (table->keys[slot] == 0) {
            size_t maximum_count = table->capacity - table->capacity / 4;
            if (table->count + 1 > maximum_count) {
                if (!riscv_seen_table_grow(table)) {
                    return RISCV_SEEN_DROPPED;
                }
                riscv_seen_insert_rehashed(table, key);
            } else {
                table->keys[slot] = key;
                ++table->count;
            }
            return RISCV_SEEN_FIRST;
        }
        slot = (slot + 1) & (table->capacity - 1);
    }

    if (!riscv_seen_table_grow(table)) {
        return RISCV_SEEN_DROPPED;
    }
    riscv_seen_insert_rehashed(table, key);
    return RISCV_SEEN_FIRST;
}

#endif
