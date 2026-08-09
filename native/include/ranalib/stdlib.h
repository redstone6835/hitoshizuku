#ifndef RANALIB_STDLIB_H
#define RANALIB_STDLIB_H

#include <stddef.h>

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 2147483647

typedef struct { int quot; int rem; } div_t;
typedef struct { long quot; long rem; } ldiv_t;
typedef struct { long long quot; long long rem; } lldiv_t;

void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *pointer, size_t size);
void free(void *pointer);

int abs(int value);
long labs(long value);
long long llabs(long long value);
div_t div(int numerator, int denominator);
ldiv_t ldiv(long numerator, long denominator);
lldiv_t lldiv(long long numerator, long long denominator);

long strtol(const char *string, char **end, int base);
unsigned long strtoul(const char *string, char **end, int base);
long long strtoll(const char *string, char **end, int base);
unsigned long long strtoull(const char *string, char **end, int base);
int atoi(const char *string);
long atol(const char *string);
long long atoll(const char *string);

void qsort(void *base, size_t count, size_t size, int (*compare)(const void *, const void *));
void *bsearch(
    const void *key,
    const void *base,
    size_t count,
    size_t size,
    int (*compare)(const void *, const void *));

void srand(unsigned int seed);
int rand(void);
char *getenv(const char *name);

_Noreturn void exit(int status);
_Noreturn void _Exit(int status);
_Noreturn void abort(void);

#endif
