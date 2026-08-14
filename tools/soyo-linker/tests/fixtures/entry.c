extern unsigned long helper(unsigned long value);

unsigned long result;

void _start(void) {
    result = helper(3);
}

