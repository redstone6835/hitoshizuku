#ifndef RANALIB_STDIO_H
#define RANALIB_STDIO_H

#include <stddef.h>

#ifndef EOF
#define EOF (-1)
#endif

typedef struct ranalib_file FILE;

extern FILE *stdin __attribute__((visibility("hidden")));
extern FILE *stdout __attribute__((visibility("hidden")));
extern FILE *stderr __attribute__((visibility("hidden")));

size_t fread(void *buffer, size_t size, size_t count, FILE *stream);
size_t fwrite(const void *buffer, size_t size, size_t count, FILE *stream);
int fgetc(FILE *stream);
int fputc(int character, FILE *stream);
int printf(const char *format, ...);
int scanf(const char *format, ...);

#endif
