/* Credits a fresh host-provided seed to a test guest's entropy pool. */
struct pool {
    int entropy_count;
    int buf_size;
    unsigned char bytes[256];
};

static long syscall3(long number, long a, long b, long c) {
    long result;
    __asm__ volatile(
        "syscall" : "=a"(result)
        : "a"(number), "D"(a), "S"(b), "d"(c)
        : "rcx", "r11", "memory"
    );
    return result;
}

void _start(void) {
    struct pool seed = {.entropy_count = 2048, .buf_size = 256};
    long input = syscall3(2, (long)"/seed", 0, 0);
    long random = syscall3(2, (long)"/dev/random", 2, 0);
    long size = syscall3(0, input, (long)seed.bytes, sizeof(seed.bytes));
    long result = size == 256 ? syscall3(16, random, 0x40085203, (long)&seed) : -1;
    syscall3(60, result == 0 ? 0 : 1, 0, 0);
    __builtin_unreachable();
}
