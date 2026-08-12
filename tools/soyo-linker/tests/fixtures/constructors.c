__attribute__((constructor(101))) void init_first(void) {}
__attribute__((constructor(200))) void init_second(void) {}

__attribute__((destructor(101))) void fini_first(void) {}
__attribute__((destructor(200))) void fini_second(void) {}
