#ifndef RANALIB_STDIO_H
#define RANALIB_STDIO_H

#include <stddef.h>
#include <stdarg.h>

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
int ungetc(int character, FILE *stream);
int getchar(void);
int putchar(int character);
int puts(const char *string);
int fflush(FILE *stream);
int feof(FILE *stream);
int ferror(FILE *stream);
void clearerr(FILE *stream);
int fprintf(FILE *stream, const char *format, ...);
int vfprintf(FILE *stream, const char *format, va_list arguments);
int printf(const char *format, ...);
int vprintf(const char *format, va_list arguments);
int snprintf(char *buffer, size_t capacity, const char *format, ...);
int vsnprintf(char *buffer, size_t capacity, const char *format, va_list arguments);
int fscanf(FILE *stream, const char *format, ...);
int vfscanf(FILE *stream, const char *format, va_list arguments);
int scanf(const char *format, ...);
int vscanf(const char *format, va_list arguments);

#endif
