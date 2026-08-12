#include <assert.h>

#include <ranalib/stdlib.h>

char **environ;

_Noreturn void mrt_abort(void) {
    __builtin_trap();
}

static int compare_int(const void *left, const void *right) {
    int a = *(const int *)left;
    int b = *(const int *)right;
    return (a > b) - (a < b);
}

int main(void) {
    char *end = 0;
    assert(strtol(" -0x2a!", &end, 0) == -42);
    assert(*end == '!');
    assert(strtoul("075", 0, 0) == 61);

    int values[] = {4, 1, 3, 2};
    qsort(values, 4, sizeof(values[0]), compare_int);
    assert(values[0] == 1 && values[3] == 4);
    int key = 3;
    assert(*(int *)bsearch(&key, values, 4, sizeof(values[0]), compare_int) == 3);

    srand(7);
    int first = rand();
    srand(7);
    assert(rand() == first);
    return 0;
}
