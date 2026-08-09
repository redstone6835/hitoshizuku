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

    char buffer[16] = "so";
    assert(strcat(buffer, "yo") == buffer);
    assert(strcmp(buffer, "soyo") == 0);
    assert(strncat(buffer, "-native-extra", 7) == buffer);
    assert(strcmp(buffer, "soyo-native") == 0);
    assert(strchr(buffer, '-') == buffer + 4);
    const char *dashes = "a-b-c";
    assert(strrchr(dashes, '-') == dashes + 3);
    assert(strstr(buffer, "native") == buffer + 5);
    assert(strspn("abc123", "abc") == 3);
    assert(strcspn("abc123", "0123456789") == 3);
    const char *native = "native";
    assert(strpbrk(native, "xyzv") == native + 4);

    char copied[8];
    assert(strcpy(copied, "mygo") == copied);
    assert(strcmp(copied, "mygo") == 0);
    assert(strncpy(copied, "so", sizeof(copied)) == copied);
    assert(copied[2] == 0 && copied[7] == 0);

    char tokens[] = "a::b:c";
    assert(strcmp(strtok(tokens, ":"), "a") == 0);
    assert(strcmp(strtok(0, ":"), "b") == 0);
    assert(strcmp(strtok(0, ":"), "c") == 0);
    assert(strtok(0, ":") == 0);

    assert(memchr(buffer, '-', strlen(buffer)) == buffer + 4);
    assert(strnlen("abcd", 2) == 2);
}

int main(void) {
    memory_primitives_preserve_byte_semantics();
    memmove_handles_both_overlap_directions();
    string_primitives_stop_at_nul_or_limit();
    return 0;
}
