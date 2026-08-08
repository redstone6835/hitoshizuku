#include <assert.h>

#include <ranalib/string.h>

static void memory_primitives_preserve_byte_semantics(void) {
    unsigned char source[] = {0, 1, 2, 0xff};
    unsigned char target[4];
    assert(memset(target, 0x5a, sizeof(target)) == target);
    assert(target[0] == 0x5a && target[3] == 0x5a);
    assert(memcpy(target, source, sizeof(source)) == target);
    assert(memcmp(target, source, sizeof(source)) == 0);
    target[3] = 0x7f;
    assert(memcmp(target, source, sizeof(source)) < 0);
}

static void memmove_handles_both_overlap_directions(void) {
    char right[] = "abcdef";
    assert(memmove(right + 1, right, 5) == right + 1);
    assert(memcmp(right, "aabcde", 6) == 0);

    char left[] = "abcdef";
    assert(memmove(left, left + 1, 5) == left);
    assert(memcmp(left, "bcdeff", 6) == 0);
}

static void string_primitives_stop_at_nul_or_limit(void) {
    assert(strlen("") == 0);
    assert(strlen("soyo") == 4);
    assert(strcmp("abc", "abc") == 0);
    assert(strcmp("abc", "abd") < 0);
    assert(strcmp("abe", "abd") > 0);
    assert(strncmp("abc", "abd", 2) == 0);
    assert(strncmp("abc", "abd", 3) < 0);
    assert(strncmp("abc", "xyz", 0) == 0);
}

int main(void) {
    memory_primitives_preserve_byte_semantics();
    memmove_handles_both_overlap_directions();
    string_primitives_stop_at_nul_or_limit();
    return 0;
}
