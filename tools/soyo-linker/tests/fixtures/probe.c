extern unsigned long external_value;

static const char message[] = "SOYO";
unsigned long writable_value = 7;
unsigned long zero_value;

unsigned long helper(unsigned long value) {
    return value + external_value + writable_value + message[0];
}

void _start(void) {
    zero_value = helper(1);
}

