#include <ranalib/inttypes.h>
#include <ranalib/stdlib.h>

intmax_t imaxabs(intmax_t value) { return value < 0 ? -value : value; }

imaxdiv_t imaxdiv(intmax_t numerator, intmax_t denominator) {
    return (imaxdiv_t){numerator / denominator, numerator % denominator};
}

intmax_t strtoimax(const char *string, char **end, int base) {
    return (intmax_t)strtoll(string, end, base);
}

uintmax_t strtoumax(const char *string, char **end, int base) {
    return (uintmax_t)strtoull(string, end, base);
}
