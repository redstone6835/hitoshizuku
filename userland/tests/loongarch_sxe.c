#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#if !defined(__loongarch__)
#error "loongarch_sxe.c must be compiled for LoongArch"
#endif

#define REGISTER_COUNT 32
#define STATE_ROUNDS 64

struct lsx_state {
    uint64_t lane[REGISTER_COUNT][2];
} __attribute__((aligned(16)));

extern char **environ;

extern long fpu_roundtrip(const uint64_t *input, uint64_t *output, uint64_t rounds);
extern long lsx_roundtrip(const struct lsx_state *input, struct lsx_state *output,
                          uint64_t rounds);
extern long fpu_to_lsx(const uint64_t *input, struct lsx_state *output);
extern long lsx_fork_snapshot(const struct lsx_state *input, struct lsx_state *output);
extern long lsx_execve(const struct lsx_state *input, const char *path, char *const argv[],
                       char *const envp[]);
extern void lsx_snapshot(struct lsx_state *output);

#define FPU_REGISTERS(M)                                                                \
    M(0, 0) M(1, 8) M(2, 16) M(3, 24) M(4, 32) M(5, 40) M(6, 48) M(7, 56) M(8, 64)   \
        M(9, 72) M(10, 80) M(11, 88) M(12, 96) M(13, 104) M(14, 112) M(15, 120)       \
            M(16, 128) M(17, 136) M(18, 144) M(19, 152) M(20, 160) M(21, 168)         \
                M(22, 176) M(23, 184) M(24, 192) M(25, 200) M(26, 208) M(27, 216)     \
                    M(28, 224) M(29, 232) M(30, 240) M(31, 248)

#define LSX_REGISTERS(M)                                                                \
    M(0, 0) M(1, 16) M(2, 32) M(3, 48) M(4, 64) M(5, 80) M(6, 96) M(7, 112)          \
        M(8, 128) M(9, 144) M(10, 160) M(11, 176) M(12, 192) M(13, 208) M(14, 224)    \
            M(15, 240) M(16, 256) M(17, 272) M(18, 288) M(19, 304) M(20, 320)         \
                M(21, 336) M(22, 352) M(23, 368) M(24, 384) M(25, 400) M(26, 416)     \
                    M(27, 432) M(28, 448) M(29, 464) M(30, 480) M(31, 496)

#define FPU_LOAD(n, offset) "fld.d $f" #n ", $t1, " #offset "\n"
#define FPU_STORE(n, offset) "fst.d $f" #n ", $t2, " #offset "\n"
#define LSX_LOAD(n, offset) "vld $vr" #n ", $t1, " #offset "\n"
#define LSX_STORE(n, offset) "vst $vr" #n ", $t2, " #offset "\n"

/*
 * 寄存器装载和取样之间只执行系统调用，避免 libc 或 C ABI 合法覆盖
 * caller-saved FPR/VR。clone 与 execve 同样在汇编中直接进入内核。
 */
__asm__(".text\n"
        ".p2align 2\n"
        ".global fpu_roundtrip\n"
        ".type fpu_roundtrip, @function\n"
        "fpu_roundtrip:\n"
        "move $t1, $a0\n"
        "move $t2, $a1\n"
        "move $t0, $a2\n"
        FPU_REGISTERS(FPU_LOAD)
        "1:\n"
        "li.w $a7, 172\n"
        "syscall 0\n"
        "li.w $a7, 124\n"
        "syscall 0\n"
        "addi.d $t0, $t0, -1\n"
        "bnez $t0, 1b\n"
        FPU_REGISTERS(FPU_STORE)
        "move $a0, $zero\n"
        "jr $ra\n"
        ".size fpu_roundtrip, .-fpu_roundtrip\n"

        ".p2align 2\n"
        ".global lsx_roundtrip\n"
        ".type lsx_roundtrip, @function\n"
        "lsx_roundtrip:\n"
        "move $t1, $a0\n"
        "move $t2, $a1\n"
        "move $t0, $a2\n"
        LSX_REGISTERS(LSX_LOAD)
        "2:\n"
        "li.w $a7, 172\n"
        "syscall 0\n"
        "li.w $a7, 124\n"
        "syscall 0\n"
        "addi.d $t0, $t0, -1\n"
        "bnez $t0, 2b\n"
        LSX_REGISTERS(LSX_STORE)
        "move $a0, $zero\n"
        "jr $ra\n"
        ".size lsx_roundtrip, .-lsx_roundtrip\n"

        ".p2align 2\n"
        ".global fpu_to_lsx\n"
        ".type fpu_to_lsx, @function\n"
        "fpu_to_lsx:\n"
        "move $t1, $a0\n"
        "move $t2, $a1\n"
        FPU_REGISTERS(FPU_LOAD)
        LSX_REGISTERS(LSX_STORE)
        "move $a0, $zero\n"
        "jr $ra\n"
        ".size fpu_to_lsx, .-fpu_to_lsx\n"

        ".p2align 2\n"
        ".global lsx_fork_snapshot\n"
        ".type lsx_fork_snapshot, @function\n"
        "lsx_fork_snapshot:\n"
        "move $t1, $a0\n"
        "move $t2, $a1\n"
        LSX_REGISTERS(LSX_LOAD)
        "li.w $a0, 17\n"
        "move $a1, $zero\n"
        "move $a2, $zero\n"
        "move $a3, $zero\n"
        "move $a4, $zero\n"
        "li.w $a7, 220\n"
        "syscall 0\n"
        "move $t0, $a0\n"
        LSX_REGISTERS(LSX_STORE)
        "move $a0, $t0\n"
        "jr $ra\n"
        ".size lsx_fork_snapshot, .-lsx_fork_snapshot\n"

        ".p2align 2\n"
        ".global lsx_execve\n"
        ".type lsx_execve, @function\n"
        "lsx_execve:\n"
        "move $t1, $a0\n"
        LSX_REGISTERS(LSX_LOAD)
        "move $a0, $a1\n"
        "move $a1, $a2\n"
        "move $a2, $a3\n"
        "li.w $a7, 221\n"
        "syscall 0\n"
        "jr $ra\n"
        ".size lsx_execve, .-lsx_execve\n"

        ".p2align 2\n"
        ".global lsx_snapshot\n"
        ".type lsx_snapshot, @function\n"
        "lsx_snapshot:\n"
        "move $t2, $a0\n"
        LSX_REGISTERS(LSX_STORE)
        "jr $ra\n"
        ".size lsx_snapshot, .-lsx_snapshot\n");

static void fill_fpu_state(uint64_t state[REGISTER_COUNT]) {
    for (uint64_t index = 0; index < REGISTER_COUNT; index++) {
        state[index] = UINT64_C(0x5a17c0de00000000) ^ (index * UINT64_C(0x0102040810204081));
    }
}

static void fill_lsx_state(struct lsx_state *state) {
    for (uint64_t index = 0; index < REGISTER_COUNT; index++) {
        state->lane[index][0] =
            UINT64_C(0x13579bdf2468ace0) ^ (index * UINT64_C(0x1111111111111111));
        state->lane[index][1] =
            UINT64_C(0xfdb97531eca86420) ^ (index * UINT64_C(0x0101010101010101));
    }
}

static int fpu_states_equal(const uint64_t left[REGISTER_COUNT],
                            const uint64_t right[REGISTER_COUNT]) {
    for (size_t index = 0; index < REGISTER_COUNT; index++) {
        if (left[index] != right[index]) {
            return 0;
        }
    }
    return 1;
}

static int lsx_states_equal(const struct lsx_state *left, const struct lsx_state *right) {
    for (size_t index = 0; index < REGISTER_COUNT; index++) {
        if (left->lane[index][0] != right->lane[index][0] ||
            left->lane[index][1] != right->lane[index][1]) {
            return 0;
        }
    }
    return 1;
}

static int test_fpu_only(void) {
    uint64_t expected[REGISTER_COUNT];
    uint64_t actual[REGISTER_COUNT];
    fill_fpu_state(expected);
    fpu_roundtrip(expected, actual, STATE_ROUNDS);
    return fpu_states_equal(expected, actual) ? 0 : EIO;
}

static int test_first_lsx(void) {
    struct lsx_state expected;
    struct lsx_state actual;
    fill_lsx_state(&expected);
    lsx_roundtrip(&expected, &actual, STATE_ROUNDS);
    return lsx_states_equal(&expected, &actual) ? 0 : EIO;
}

static int test_fpu_to_lsx_low_lane(void) {
    uint64_t expected[REGISTER_COUNT];
    struct lsx_state actual;
    fill_fpu_state(expected);
    fpu_to_lsx(expected, &actual);
    for (size_t index = 0; index < REGISTER_COUNT; index++) {
        if (actual.lane[index][0] != expected[index] || actual.lane[index][1] != 0) {
            return EIO;
        }
    }
    return 0;
}

static int test_fork_inherits_lsx(void) {
    struct lsx_state expected;
    struct lsx_state actual;
    fill_lsx_state(&expected);

    long child = lsx_fork_snapshot(&expected, &actual);
    if (child < 0) {
        return (int)-child;
    }
    if (child == 0) {
        _exit(lsx_states_equal(&expected, &actual) ? 0 : 1);
    }

    int status = 0;
    if (waitpid((pid_t)child, &status, 0) != child) {
        return errno;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return ECHILD;
    }
    return lsx_states_equal(&expected, &actual) ? 0 : EIO;
}

static int is_exec_zero_child(int argc, char **argv) {
    return argc == 2 && argv[1][0] == 'x' && argv[1][1] == '\0';
}

static int exec_zero_child(void) {
    struct lsx_state actual;
    lsx_snapshot(&actual);
    for (size_t index = 0; index < REGISTER_COUNT; index++) {
        if (actual.lane[index][0] != 0 || actual.lane[index][1] != 0) {
            return 1;
        }
    }
    return 0;
}

static int test_exec_clears_lsx(const char *program) {
    struct lsx_state state;
    char *const child_argv[] = {(char *)program, "x", NULL};
    fill_lsx_state(&state);
    long result = lsx_execve(&state, program, child_argv, environ);
    return result < 0 ? (int)-result : EIO;
}

typedef int (*isolated_test_fn)(void);

static int run_isolated(isolated_test_fn test) {
    pid_t child = fork();
    if (child < 0) {
        return errno;
    }
    if (child == 0) {
        _exit(test() == 0 ? 0 : 1);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return errno;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : EIO;
}

static int run_exec_isolated(const char *program) {
    pid_t child = fork();
    if (child < 0) {
        return errno;
    }
    if (child == 0) {
        _exit(test_exec_clears_lsx(program) == 0 ? 0 : 1);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return errno;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : EIO;
}

int main(int argc, char **argv) {
    if (is_exec_zero_child(argc, argv)) {
        return exec_zero_child();
    }

    static const char *const names[] = {
        "FP-only state survives syscall and sched_yield",
        "first LSX state survives syscall and sched_yield",
        "first LSX inherits FP low lanes",
        "fork inherits LSX state",
        "exec clears LSX state",
    };
    int results[] = {
        run_isolated(test_fpu_only),
        run_isolated(test_first_lsx),
        run_isolated(test_fpu_to_lsx_low_lane),
        run_isolated(test_fork_inherits_lsx),
        run_exec_isolated(argv[0]),
    };
    int failures = 0;

    printf("TAP version 14\n1..%zu\n", sizeof(results) / sizeof(results[0]));
    for (size_t index = 0; index < sizeof(results) / sizeof(results[0]); index++) {
        if (results[index] == 0) {
            printf("ok %zu - %s\n", index + 1, names[index]);
        } else {
            printf("not ok %zu - %s # error=%d\n", index + 1, names[index], results[index]);
            failures++;
        }
    }
    return failures == 0 ? 0 : 1;
}
