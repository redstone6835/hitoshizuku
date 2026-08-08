#ifndef RANALIB_STDLIB_H
#define RANALIB_STDLIB_H

#include <stddef.h>

void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *pointer, size_t size);
void free(void *pointer);

_Noreturn void exit(int status);
_Noreturn void _Exit(int status);

#endif
