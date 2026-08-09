#ifndef RANALIB_STRING_H
#define RANALIB_STRING_H

#include <stddef.h>

void *memcpy(void *destination, const void *source, size_t count);
void *memmove(void *destination, const void *source, size_t count);
void *memset(void *destination, int value, size_t count);
int memcmp(const void *left, const void *right, size_t count);
void *memchr(const void *memory, int value, size_t count);
size_t strlen(const char *string);
size_t strnlen(const char *string, size_t maximum);
int strcmp(const char *left, const char *right);
int strncmp(const char *left, const char *right, size_t count);
char *strcpy(char *destination, const char *source);
char *strncpy(char *destination, const char *source, size_t count);
char *strcat(char *destination, const char *source);
char *strncat(char *destination, const char *source, size_t count);
char *strchr(const char *string, int character);
char *strrchr(const char *string, int character);
char *strstr(const char *haystack, const char *needle);
size_t strspn(const char *string, const char *accepted);
size_t strcspn(const char *string, const char *rejected);
char *strpbrk(const char *string, const char *accepted);
char *strtok(char *string, const char *delimiters);

#endif
