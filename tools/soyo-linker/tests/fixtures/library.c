static const char message[] = "SOYO";

unsigned long writable_value = 7;

unsigned long helper(unsigned long value) {
    return value + writable_value + message[0];
}

