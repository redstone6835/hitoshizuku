extern unsigned long helper(unsigned long value);

unsigned long (*helper_pointer)(unsigned long) = helper;

