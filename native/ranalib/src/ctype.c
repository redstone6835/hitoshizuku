#include <ranalib/ctype.h>

int isdigit(int character) { return character >= '0' && character <= '9'; }
int islower(int character) { return character >= 'a' && character <= 'z'; }
int isupper(int character) { return character >= 'A' && character <= 'Z'; }
int isalpha(int character) { return islower(character) || isupper(character); }
int isalnum(int character) { return isalpha(character) || isdigit(character); }
int isblank(int character) { return character == ' ' || character == '\t'; }
int iscntrl(int character) { return (character >= 0 && character < 0x20) || character == 0x7f; }
int isgraph(int character) { return character >= 0x21 && character <= 0x7e; }
int isprint(int character) { return character >= 0x20 && character <= 0x7e; }
int ispunct(int character) { return isgraph(character) && !isalnum(character); }
int isspace(int character) {
    return character == ' ' || character == '\t' || character == '\n' ||
           character == '\r' || character == '\f' || character == '\v';
}
int isxdigit(int character) {
    return isdigit(character) || (character >= 'a' && character <= 'f') ||
           (character >= 'A' && character <= 'F');
}
int tolower(int character) { return isupper(character) ? character - 'A' + 'a' : character; }
int toupper(int character) { return islower(character) ? character - 'a' + 'A' : character; }
