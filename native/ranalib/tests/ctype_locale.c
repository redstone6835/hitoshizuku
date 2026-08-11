#include <assert.h>
#include <limits.h>

#include <ranalib/ctype.h>
#include <ranalib/locale.h>

int main(void) {
    assert(isalpha('A') && isdigit('9') && isxdigit('f'));
    assert(!isalpha('1') && isspace('\n') && ispunct('!'));
    assert(tolower('Q') == 'q' && toupper('m') == 'M');
    assert(setlocale(LC_ALL, "C") != 0);
    assert(setlocale(LC_ALL, "unknown") == 0);
    assert(localeconv()->decimal_point[0] == '.');
    assert(localeconv()->frac_digits == CHAR_MAX);
    return 0;
}
