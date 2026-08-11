#include <limits.h>

#include <ranalib/locale.h>
#include <ranalib/string.h>

static char empty[] = "";
static char dot[] = ".";
static char c_locale[] = "C";

static struct lconv c_conventions = {
    .decimal_point = dot,
    .thousands_sep = empty,
    .grouping = empty,
    .int_curr_symbol = empty,
    .currency_symbol = empty,
    .mon_decimal_point = empty,
    .mon_thousands_sep = empty,
    .mon_grouping = empty,
    .positive_sign = empty,
    .negative_sign = empty,
    .int_frac_digits = CHAR_MAX,
    .frac_digits = CHAR_MAX,
    .p_cs_precedes = CHAR_MAX,
    .p_sep_by_space = CHAR_MAX,
    .n_cs_precedes = CHAR_MAX,
    .n_sep_by_space = CHAR_MAX,
    .p_sign_posn = CHAR_MAX,
    .n_sign_posn = CHAR_MAX,
};

char *setlocale(int category, const char *locale) {
    if (category < LC_ALL || category > LC_TIME) {
        return 0;
    }
    if (locale == 0 || locale[0] == '\0' || strcmp(locale, "C") == 0 ||
        strcmp(locale, "POSIX") == 0) {
        return c_locale;
    }
    return 0;
}

struct lconv *localeconv(void) { return &c_conventions; }
