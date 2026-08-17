/* mm-probe: 内存管理子系统功能冒烟测试(直接在 guest 内核上运行)。
 *
 * 全部通过裸 syscall 调用(LA/RV 共用 asm-generic 编号),不依赖新 libc 头文件。
 * 输出格式:每个用例打印 MM_PROBE <name>: <PASS|FAIL|SKIP> [细节]。
 * 退出码:0 = 全部通过,1 = 存在失败。
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

/* ── asm-generic syscall 编号 ─────────────────────────────────────────── */
#define SYS_mbind 235
#define SYS_remap_file_pages 234
#define SYS_swapon 224
#define SYS_swapoff 225
#define SYS_mlock2 284
#define SYS_memfd_create 279
#define SYS_process_vm_readv 270
#define SYS_process_vm_writev 271
#define SYS_userfaultfd 282
#define SYS_pidfd_open 434
#define SYS_process_madvise 440
#define SYS_cachestat 451
#define SYS_set_mempolicy_home_node 450
#define SYS_mseal 462
#define SYS_sysinfo 179
#define SYS_get_mempolicy 236
#define SYS_set_mempolicy 237
#define SYS_migrate_pages 238
#define SYS_move_pages 239
#define SYS_pkey_mprotect 288
#define SYS_pkey_alloc 289
#define SYS_pkey_free 290
#define SYS_memfd_secret 447
#define SYS_map_shadow_stack 453

/* ── userfaultfd UAPI ──────────────────────────────────────────────────── */
#define UFFD_API 0xAAULL
#define _UFFDIO_REGISTER 0x00
#define _UFFDIO_UNREGISTER 0x01
#define _UFFDIO_WAKE 0x02
#define _UFFDIO_COPY 0x03
#define _UFFDIO_ZEROPAGE 0x04
#define _UFFDIO_WRITEPROTECT 0x05
#define _UFFDIO_API 0x3F
#define UFFDIO_API_CMD ((3ULL << 30) | (24ULL << 16) | (0xAAULL << 8) | _UFFDIO_API)
#define UFFDIO_REGISTER_CMD ((3ULL << 30) | (32ULL << 16) | (0xAAULL << 8) | _UFFDIO_REGISTER)
#define UFFDIO_COPY_CMD ((3ULL << 30) | (40ULL << 16) | (0xAAULL << 8) | _UFFDIO_COPY)
#define UFFDIO_REGISTER_MODE_MISSING (1ULL << 0)
#define UFFDIO_COPY_MODE_DONTWAKE (1ULL << 0)
#define UFFD_EVENT_PAGEFAULT 0x12
#define UFFD_PAGEFAULT_FLAG_WRITE 1ULL

struct uffdio_range {
    uint64_t start;
    uint64_t len;
};
struct uffdio_register {
    struct uffdio_range range;
    uint64_t mode;
    uint64_t ioctls;
};
struct uffdio_copy {
    uint64_t dst;
    uint64_t src;
    uint64_t len;
    uint64_t mode;
    int64_t copy;
};
struct uffdio_api {
    uint64_t api;
    uint64_t features;
    uint64_t ioctls;
};
struct uffd_msg {
    uint8_t event;
    uint8_t r1;
    uint16_t r2;
    uint32_t r3;
    uint64_t pf_flags;
    uint64_t pf_address;
    uint32_t feat_ptid;
    uint32_t ptid;
};

/* cachestat */
struct cachestat_range {
    uint64_t off;
    uint64_t len;
};
struct cachestat {
    uint64_t nr_cache;
    uint64_t nr_dirty;
    uint64_t nr_writeback;
    uint64_t nr_evicted;
    uint64_t nr_recently_evicted;
};

/* sysinfo */
struct sysinfo {
    int64_t uptime;
    uint64_t loads[3];
    uint64_t totalram;
    uint64_t freeram;
    uint64_t sharedram;
    uint64_t bufferram;
    uint64_t totalswap;
    uint64_t freeswap;
    uint16_t procs;
    uint16_t pad;
    uint64_t totalhigh;
    uint64_t freehigh;
    uint32_t mem_unit;
    /* Linux struct sysinfo 总大小 112 字节(尾部对齐填充) */
    char _f[4];
};

#define MPOL_BIND 2
#define MPOL_DEFAULT 0
#define MPOL_MF_STRICT 1
#define MPOL_F_ADDR 2

#define MADV_DONTNEED 4
#define MADV_WIPEONFORK 18
#define MADV_PAGEOUT 21
#define MADV_DONTNEED_LOCKED 24

#define PAGE 4096

static int failures = 0;
static int runs = 0;

#define CHECK(name, cond, ...)                                    \
    do {                                                          \
        runs++;                                                   \
        if (cond) {                                               \
            printf("MM_PROBE %s: PASS\n", name);                  \
        } else {                                                  \
            failures++;                                           \
            printf("MM_PROBE %s: FAIL errno=%d %s\n", name,       \
                   errno, strerror(errno));                       \
        }                                                         \
    } while (0)

static long sys_syscall(long nr, long a0, long a1, long a2, long a3, long a4, long a5) {
    return syscall(nr, a0, a1, a2, a3, a4, a5);
}

static void test_mlock_rlimit_and_status(void) {
    size_t len = 4 * PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("mmap-basic", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    memset(p, 0x5a, len);
    errno = 0;
    CHECK("mlock-basic", mlock(p, len) == 0);
    /* 从 /proc/self/status 读 VmLck */
    FILE *f = fopen("/proc/self/status", "r");
    int vmlck_kb = -1;
    if (f) {
        char line[256];
        while (fgets(line, sizeof line, f)) {
            if (sscanf(line, "VmLck: %d kB", &vmlck_kb) == 1) break;
        }
        fclose(f);
    }
    CHECK("status-vmlck-reported", vmlck_kb >= (int)(len / 1024));
    errno = 0;
    CHECK("munlock-basic", munlock(p, len) == 0);
    /* mlock 后 MADV_DONTNEED 必须 EINVAL; DONTNEED_LOCKED 允许 */
    errno = 0;
    CHECK("mlock-again", mlock(p, len) == 0);
    errno = 0;
    CHECK("madvise-dontneed-locked-einval",
          madvise(p, len, MADV_DONTNEED) == -1 && errno == EINVAL);
    errno = 0;
    CHECK("madvise-dontneed-locked-ok", madvise(p, len, MADV_DONTNEED_LOCKED) == 0);
    errno = 0;
    CHECK("munlock-again", munlock(p, len) == 0);
    /* mlock2 MLOCK_ONFAULT: 只打标不填充 */
    errno = 0;
    CHECK("mlock2-onfault", syscall(SYS_mlock2, p, len, 1) == 0);
    errno = 0;
    CHECK("munlock-after-onfault", munlock(p, len) == 0);
    munmap(p, len);
}

static void test_mmap_flags(void) {
    size_t len = 2 * PAGE;
    /* MAP_POPULATE:mincore 应立即驻留 */
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
    CHECK("mmap-populate", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        unsigned char vec[2] = {0, 0};
        errno = 0;
        CHECK("mincore-after-populate", mincore(p, len, vec) == 0 && (vec[0] & 1));
        munmap(p, len);
    }
    /* MAP_LOCKED 立即锁定 */
    p = mmap(NULL, len, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
    CHECK("mmap-locked", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        munlock(p, len);
        munmap(p, len);
    }
    /* MAP_HUGETLB → ENODEV(无 hugetlb 支持) */
    errno = 0;
    p = mmap(NULL, len, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | 0x40000, -1, 0);
    CHECK("mmap-hugeltb-enodev", p == MAP_FAILED && errno == ENODEV);
    /* 未知 flag → EINVAL */
    errno = 0;
    p = mmap(NULL, len, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | 0x80000000u, -1, 0);
    CHECK("mmap-unknown-flag-einval", p == MAP_FAILED && errno == EINVAL);
    /* MAP_DROPPABLE 仅匿名私有 */
    errno = 0;
    p = mmap(NULL, len, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | 0x800000, -1, 0);
    CHECK("mmap-droppable", p != MAP_FAILED);
    if (p != MAP_FAILED) munmap(p, len);
    errno = 0;
    p = mmap(NULL, len, PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS | 0x800000, -1, 0);
    CHECK("mmap-droppable-shared-einval", p == MAP_FAILED && errno == EINVAL);
    /* MAP_GROWSDOWN 匿名栈 */
    errno = 0;
    p = mmap(NULL, 8 * PAGE, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | 0x100, -1, 0);
    CHECK("mmap-growsdown", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        /* 向低地址生长一页 */
        volatile char *q = p;
        q[-PAGE] = 1;
        CHECK("growsdown-write-below", q[-PAGE] == 1);
        munmap(p, 8 * PAGE);
    }
}

static void test_mprotect_shared_ro(void) {
    /* 可写 fd 的 MAP_SHARED 映射: mprotect 提权成功(Linux 语义) */
    int fd = memfd_create("mprotect-test", 0);
    CHECK("mprotect-memfd", fd >= 0);
    if (fd < 0) return;
    ftruncate(fd, PAGE);
    char *p = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, 0);
    CHECK("mprotect-map-shared-ro", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        errno = 0;
        CHECK("mprotect-write-writable-fd-ok", mprotect(p, PAGE, PROT_READ | PROT_WRITE) == 0);
        munmap(p, PAGE);
    }
    close(fd);
    /* 只读 fd 的 MAP_SHARED 映射: mmap(PROT_WRITE) 与 mprotect 提权都应 EACCES */
    int ro = open("/mprotect-ro-file", O_CREAT | O_RDWR | O_TRUNC, 0600);
    CHECK("mprotect-ro-create", ro >= 0);
    if (ro < 0) return;
    write(ro, "x", 1);
    int rofd = open("/mprotect-ro-file", O_RDONLY);
    close(ro);
    CHECK("mprotect-ro-open", rofd >= 0);
    if (rofd < 0) return;
    errno = 0;
    p = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, rofd, 0);
    CHECK("mprotect-shared-write-fd-eacces", p == MAP_FAILED && errno == EACCES);
    p = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, rofd, 0);
    CHECK("mprotect-shared-ro-map", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        errno = 0;
        CHECK("mprotect-shared-ro-eacces",
              mprotect(p, PAGE, PROT_READ | PROT_WRITE) == -1 && errno == EACCES);
        /* MAP_PRIVATE + 只读 fd: 允许提权(COW) */
        char *pr = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE, rofd, 0);
        CHECK("mprotect-private-map", pr != MAP_FAILED);
        if (pr != MAP_FAILED) {
            errno = 0;
            CHECK("mprotect-private-write-ok", mprotect(pr, PAGE, PROT_READ | PROT_WRITE) == 0);
            munmap(pr, PAGE);
        }
        munmap(p, PAGE);
    }
    close(rofd);
    unlink("/mprotect-ro-file");
}

static void test_mseal(void) {
    size_t len = 2 * PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("mseal-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    errno = 0;
    CHECK("mseal-basic", syscall(SYS_mseal, p, len, 0) == 0);
    errno = 0;
    CHECK("mseal-mprotect-eperm",
          mprotect(p, len, PROT_READ) == -1 && errno == EPERM);
    errno = 0;
    CHECK("mseal-munmap-eperm",
          munmap(p, len) == -1 && errno == EPERM);
    /* MAP_FIXED 覆盖密封区域 → EPERM */
    errno = 0;
    CHECK("mseal-map-fixed-eperm",
          mmap(p, len, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == MAP_FAILED &&
              errno == EPERM);
    /* mseal 非法参数 */
    errno = 0;
    CHECK("mseal-badflags-einval", syscall(SYS_mseal, p, len, 1) == -1 && errno == EINVAL);
    errno = 0;
    CHECK("mseal-unaligned-einval", syscall(SYS_mseal, p + 1, len, 0) == -1 && errno == EINVAL);
    /* 未映射范围 → ENOMEM */
    errno = 0;
    CHECK("mseal-unmapped-enomem",
          syscall(SYS_mseal, (uintptr_t)p + 0x100000, len, 0) == -1 && errno == ENOMEM);
    /* 进程退出时密封区域正常回收(能走到这里即未 panic) */
    munmap(p + len, 0); /* no-op */
}

static void test_madvise_family(void) {
    size_t len = 4 * PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("madvise-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    memset(p, 0x33, len);
    /* DONTNEED 丢弃后读回零 */
    CHECK("madvise-dontneed", madvise(p, len, MADV_DONTNEED) == 0);
    CHECK("madvise-dontneed-zerofill", p[0] == 0 && p[PAGE * 3] == 0);
    /* WIPEONFORK:fork 后子进程清零 */
    p[0] = 0x42;
    CHECK("madvise-wipeonfork", madvise(p, len, MADV_WIPEONFORK) == 0);
    pid_t child = fork();
    if (child == 0) {
        _exit(p[0] == 0 ? 0 : 1);
    }
    int st = 0;
    waitpid(child, &st, 0);
    CHECK("wipeonfork-child-zero", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK("wipeonfork-parent-kept", (unsigned char)p[0] == 0x42);
    /* POPULATE_WRITE */
    CHECK("madvise-populate-write", madvise(p, len, 23) == 0);
    /* 未映射范围 → ENOMEM */
    errno = 0;
    CHECK("madvise-unmapped-enomem",
          madvise((void *)((uintptr_t)p + 0x1000000), len, MADV_DONTNEED) == -1 &&
              errno == ENOMEM);
    /* 未对齐 → EINVAL */
    errno = 0;
    CHECK("madvise-unaligned-einval",
          madvise((void *)((uintptr_t)p + 1), len, MADV_DONTNEED) == -1 && errno == EINVAL);
    /* 未知 advice → EINVAL */
    errno = 0;
    CHECK("madvise-unknown-einval", madvise(p, len, 999) == -1 && errno == EINVAL);
    /* COLD/FREE 可用 */
    CHECK("madvise-cold", madvise(p, len, 20) == 0);
    CHECK("madvise-free", madvise(p, len, 8) == 0);
    /* HWPOISON 无特权 → EPERM(我们以 root 运行,内核返回 EINVAL 表示"有特权但无法执行") */
    errno = 0;
    CHECK("madvise-hwpoison", madvise(p, len, 100) == -1 && (errno == EPERM || errno == EINVAL));
    munmap(p, len);
}

static void test_fork_dontfork(void) {
    size_t len = 2 * PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("dontfork-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    p[0] = 0x77;
    CHECK("madvise-dontfork", madvise(p, len, 10) == 0); /* MADV_DONTFORK */
    pid_t child = fork();
    if (child == 0) {
        /* 子进程不应有该映射:访问应 SIGSEGV */
        volatile char x = p[0];
        (void)x;
        _exit(2);
    }
    int st = 0;
    waitpid(child, &st, 0);
    CHECK("dontfork-child-segv", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV);
    CHECK("dontfork-parent-kept", (unsigned char)p[0] == 0x77);
    munmap(p, len);
}

static void test_remap_file_pages(void) {
    /* 注:本内核基线对 memfd/anonfs 的 MAP_PRIVATE 缺页读就有缺陷,改用 tmpfs */
    int fd = open("/dev/shm/remap-test", O_CREAT | O_RDWR | O_TRUNC, 0600);
    CHECK("remap-tmpfs-open", fd >= 0);
    if (fd < 0) return;
    ftruncate(fd, 4 * PAGE);
    /* 写入已知内容:页0=0xAA 页1=0xBB 页2=0xCC 页3=0xDD */
    char *buf = malloc(4 * PAGE);
    for (int i = 0; i < 4; i++) memset(buf + i * PAGE, 0xAA + i * 0x11, PAGE);
    write(fd, buf, 4 * PAGE);
    char *p = mmap(NULL, 4 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK("remap-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) { free(buf); close(fd); return; }
    /* 触发页0、页1驻留 */
    char back[4] = {0};
    CHECK("remap-pread", pread(fd, back, 4, 0) == 4 && back[0] == (char)0xAA);
    printf("MM_PROBE remap-diag: pread=%02x %02x %02x %02x map0=%02x map1=%02x map3=%02x\n",
           (unsigned char)back[0], (unsigned char)back[1], (unsigned char)back[2],
           (unsigned char)back[3], (unsigned char)p[0], (unsigned char)p[PAGE],
           (unsigned char)p[3 * PAGE]);
    CHECK("remap-writeback", pread(fd, back, 4, PAGE) == 4 && back[0] == (char)0xBB);
    CHECK("remap-read0", (unsigned char)p[0] == 0xAA);
    CHECK("remap-read1", (unsigned char)p[PAGE] == 0xBB);
    /* 把页0(驻留)重映射到文件偏移 2 页 */
    errno = 0;
    CHECK("remap-file-pages", syscall(SYS_remap_file_pages, p, PAGE, 0, 2, 0) == 0);
    CHECK("remap-new-content", (unsigned char)p[0] == 0xCC);
    /* 未驻留页不受影响:页3 仍是线性偏移内容 */
    CHECK("remap-linear-unaffected", (unsigned char)p[3 * PAGE] == 0xDD);
    /* prot 非 0 → EINVAL */
    errno = 0;
    CHECK("remap-badprot-einval",
          syscall(SYS_remap_file_pages, p, PAGE, 1, 2, 0) == -1 && errno == EINVAL);
    /* 匿名映射 → EINVAL */
    char *anon = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    errno = 0;
    CHECK("remap-anon-einval",
          syscall(SYS_remap_file_pages, anon, PAGE, 0, 0, 0) == -1 && errno == EINVAL);
    munmap(anon, PAGE);
    munmap(p, 4 * PAGE);
    free(buf);
    close(fd);
    unlink("/dev/shm/remap-test");
}

static void test_numa(void) {
    size_t len = 2 * PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("numa-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    memset(p, 0x11, len);
    unsigned long mask = 1; /* node 0 */
    errno = 0;
    CHECK("mbind-node0", syscall(SYS_mbind, p, len, MPOL_BIND, &mask, 64, 0) == 0);
    int mode = -1;
    errno = 0;
    CHECK("get-mempolicy-addr",
          syscall(SYS_get_mempolicy, &mode, NULL, 0, p, MPOL_F_ADDR) == 0 && mode == MPOL_BIND);
    errno = 0;
    CHECK("set-mempolicy-bind", syscall(SYS_set_mempolicy, MPOL_BIND, &mask, 64) == 0);
    mode = -1;
    errno = 0;
    CHECK("get-mempolicy-default",
          syscall(SYS_get_mempolicy, &mode, NULL, 0, 0, 0) == 0 && mode == MPOL_BIND);
    /* 不存在的节点 → EINVAL */
    unsigned long bad_mask = 2; /* node 1 不存在 */
    errno = 0;
    CHECK("mbind-badnode-einval",
          syscall(SYS_mbind, p, len, MPOL_BIND, &bad_mask, 64, 0) == -1 && errno == EINVAL);
    /* 未映射范围 → ENOMEM */
    errno = 0;
    CHECK("mbind-unmapped-enomem",
          syscall(SYS_mbind, (uintptr_t)p + 0x1000000, len, MPOL_BIND, &mask, 64, 0) == -1 &&
              errno == ENOMEM);
    /* migrate_pages(self) → 0 */
    errno = 0;
    CHECK("migrate-pages-self", syscall(SYS_migrate_pages, 0, 64, &mask, &mask) == 0);
    /* move_pages(self):已映射页 status=0 */
    void *pages[2] = {p, (void *)((uintptr_t)p + 0x1000000)};
    int nodes[2] = {0, 0};
    int status[2] = {99, 99};
    errno = 0;
    CHECK("move-pages-self",
          syscall(SYS_move_pages, 0, 2, pages, nodes, status, 0) == 0 && status[0] == 0 &&
              status[1] == -ENOENT);
    /* set_mempolicy_home_node */
    errno = 0;
    CHECK("home-node-ok", syscall(SYS_set_mempolicy_home_node, p, len, 0, 0) == 0);
    errno = 0;
    CHECK("home-node-bad", syscall(SYS_set_mempolicy_home_node, p, len, 1, 0) == -1 && errno == EINVAL);
    munmap(p, len);
}

static void test_process_vm(void) {
    size_t len = 2 * PAGE;
    char *src = mmap(NULL, len, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("pvm-mmap", src != MAP_FAILED);
    if (src == MAP_FAILED) return;
    memset(src, 0x2a, len);
    struct iovec {
        void *base;
        size_t len;
    } remote = {src, len};
    /* 读自己 */
    char *dst = mmap(NULL, len, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(dst, 0, len);
    struct iovec l2 = {dst, len};
    errno = 0;
    CHECK("process-vm-readv",
          syscall(SYS_process_vm_readv, getpid(), &l2, 1, &remote, 1, 0) == (ssize_t)len &&
              (unsigned char)dst[0] == 0x2a && (unsigned char)dst[PAGE] == 0x2a);
    /* 写自己 */
    memset(src, 0, len);
    char *payload = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(payload, 0x7e, len);
    struct iovec l3 = {payload, len};
    errno = 0;
    CHECK("process-vm-writev",
          syscall(SYS_process_vm_writev, getpid(), &l3, 1, &remote, 1, 0) == (ssize_t)len &&
              (unsigned char)src[0] == 0x7e);
    /* 无效 pid → ESRCH */
    errno = 0;
    CHECK("process-vm-badpid",
          syscall(SYS_process_vm_readv, 999999, &l2, 1, &remote, 1, 0) == -1 && errno == ESRCH);
    munmap(payload, len);
    munmap(dst, len);
    munmap(src, len);
}

static void test_cachestat(void) {
    /* 页缓存协议只对 ext4 等文件系统生效; /mnt 是 rcS 挂载的测试盘 */
    int fd = open("/mnt/cachestat-test", O_CREAT | O_RDWR | O_TRUNC, 0600);
    CHECK("cachestat-tmpfs-open", fd >= 0);
    if (fd < 0) return;
    ftruncate(fd, 4 * PAGE);
    char buf[PAGE];
    memset(buf, 0x5c, PAGE);
    for (int i = 0; i < 4; i++) write(fd, buf, PAGE);
    char *p = mmap(NULL, 4 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK("cachestat-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) { close(fd); return; }
    /* 触发缓存 */
    CHECK("cachestat-touch", (unsigned char)p[0] == 0x5c);
    struct cachestat_range range = {0, 4 * PAGE};
    struct cachestat cs;
    memset(&cs, 0, sizeof cs);
    errno = 0;
    CHECK("cachestat-ok", syscall(SYS_cachestat, fd, &range, &cs, 0) == 0);
    CHECK("cachestat-counts", cs.nr_cache >= 1);
    /* 非法 flags → EINVAL */
    errno = 0;
    CHECK("cachestat-badflags",
          syscall(SYS_cachestat, fd, &range, &cs, 1) == -1 && errno == EINVAL);
    /* len=0 → EINVAL */
    struct cachestat_range zero = {0, 0};
    errno = 0;
    CHECK("cachestat-zerolen",
          syscall(SYS_cachestat, fd, &zero, &cs, 0) == -1 && errno == EINVAL);
    munmap(p, 4 * PAGE);
    close(fd);
    unlink("/mnt/cachestat-test");
}

static void test_swapon_swapoff(void) {
    /* 在根文件系统(内存)上创建 swap 文件 */
    int fd = open("/swapfile", O_CREAT | O_RDWR | O_TRUNC, 0600);
    CHECK("swapfile-open", fd >= 0);
    if (fd < 0) return;
    char buf[PAGE];
    memset(buf, 0, PAGE);
    for (int i = 0; i < 64; i++) write(fd, buf, PAGE); /* 256 KiB */
    close(fd);
    errno = 0;
    CHECK("swapon", syscall(SYS_swapon, "/swapfile", 0) == 0);
    /* /proc/swaps 应包含 swapfile */
    FILE *f = fopen("/proc/swaps", "r");
    int found = 0;
    if (f) {
        char line[256];
        while (fgets(line, sizeof line, f)) {
            if (strstr(line, "swapfile")) found = 1;
        }
        fclose(f);
    }
    CHECK("proc-swaps-listed", found);
    /* 重复 swapon → EBUSY */
    errno = 0;
    CHECK("swapon-ebusy", syscall(SYS_swapon, "/swapfile", 0) == -1 && errno == EBUSY);
    errno = 0;
    CHECK("swapoff", syscall(SYS_swapoff, "/swapfile") == 0);
    errno = 0;
    CHECK("swapoff-again-einval", syscall(SYS_swapoff, "/swapfile") == -1 && errno == EINVAL);
    unlink("/swapfile");
}

static void test_sysinfo(void) {
    struct sysinfo si;
    memset(&si, 0, sizeof si);
    errno = 0;
    CHECK("sysinfo-call", syscall(SYS_sysinfo, &si) == 0);
    /* 1G 内存下 totalram 应显著大于 256MB */
    CHECK("sysinfo-totalram-real", si.totalram > (256ULL << 20));
    CHECK("sysinfo-freeram-nonzero", si.freeram > 0);
    CHECK("sysinfo-procs-nonzero", si.procs > 0);
    CHECK("sysinfo-mem-unit-one", si.mem_unit == 1);
}

static void test_uffd_thread(void);
static void test_uffd(void) {
    /* 需要线程:创建 handler 线程读取事件并 COPY */
    pthread_t th;
    if (pthread_create(&th, NULL, (void *(*)(void *))test_uffd_thread, NULL) != 0) {
        CHECK("uffd-thread-create", 0);
        return;
    }
    pthread_join(th, NULL);
}

static int uffd_fd = -1;
static void *uffd_page = NULL;
static void *uffd_src = NULL;

static void *uffd_handler(void *arg) {
    (void)arg;
    /* 等待缺页事件 */
    struct uffd_msg msg;
    ssize_t n = read(uffd_fd, &msg, sizeof msg);
    if (n != (ssize_t)sizeof msg || msg.event != UFFD_EVENT_PAGEFAULT) {
        printf("MM_PROBE uffd-event: FAIL read=%zd event=%d errno=%d\n", n, msg.event, errno);
        _exit(1);
    }
    if (msg.pf_address != (uint64_t)(uintptr_t)uffd_page) {
        printf("MM_PROBE uffd-addr: FAIL addr=%llx\n",
               (unsigned long long)msg.pf_address);
        _exit(1);
    }
    /* 从独立 src 页拷贝内容并 UFFDIO_COPY */
    struct uffdio_copy cp;
    memset(&cp, 0, sizeof cp);
    cp.dst = (uint64_t)(uintptr_t)uffd_page;
    cp.src = (uint64_t)(uintptr_t)uffd_src;
    cp.len = PAGE;
    cp.mode = 0;
    if (ioctl(uffd_fd, UFFDIO_COPY_CMD, &cp) != 0) {
        printf("MM_PROBE uffd-copy: FAIL errno=%d\n", errno);
        _exit(1);
    }
    return NULL;
}

static void test_uffd_thread(void) {
    printf("MM_PROBE uffd: step1 mmap\n");
    /* 页面初始有内容,随后 DONTNEED 使其缺页 */
    uffd_page = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (uffd_page == MAP_FAILED) {
        printf("MM_PROBE uffd-mmap: FAIL\n");
        _exit(1);
    }
    uffd_src = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (uffd_src == MAP_FAILED) {
        printf("MM_PROBE uffd-src-mmap: FAIL\n");
        _exit(1);
    }
    memset(uffd_src, 0x6f, PAGE);
    memset(uffd_page, 0x11, PAGE);
    printf("MM_PROBE uffd: step2 uffd-fd\n");
    uffd_fd = syscall(SYS_userfaultfd, 0);
    if (uffd_fd < 0) {
        printf("MM_PROBE uffd-create: FAIL errno=%d\n", errno);
        _exit(1);
    }
    struct uffdio_api api;
    memset(&api, 0, sizeof api);
    api.api = UFFD_API;
    if (ioctl(uffd_fd, UFFDIO_API_CMD, &api) != 0) {
        printf("MM_PROBE uffd-api: FAIL errno=%d\n", errno);
        _exit(1);
    }
    struct uffdio_register reg;
    memset(&reg, 0, sizeof reg);
    reg.range.start = (uint64_t)(uintptr_t)uffd_page;
    reg.range.len = PAGE;
    reg.mode = UFFDIO_REGISTER_MODE_MISSING;
    printf("MM_PROBE uffd: step3 registered\n");
    if (ioctl(uffd_fd, UFFDIO_REGISTER_CMD, &reg) != 0) {
        printf("MM_PROBE uffd-register: FAIL errno=%d\n", errno);
        _exit(1);
    }
    /* 制造缺页 */
    madvise(uffd_page, PAGE, MADV_DONTNEED);
    printf("MM_PROBE uffd: step4 dontneed done, spawning handler\n");
    /* handler 线程读取事件并 COPY */
    pthread_t th;
    if (pthread_create(&th, NULL, uffd_handler, NULL) != 0) {
        printf("MM_PROBE uffd-thread: FAIL\n");
        _exit(1);
    }
    /* 主线程触发缺页:读取应阻塞直到 COPY 完成 */
    printf("MM_PROBE uffd: step5 faulting\n");
    volatile char *vp = (volatile char *)uffd_page;
    volatile char v = vp[0];
    printf("MM_PROBE uffd: step6 fault resolved\n");
    pthread_join(th, NULL);
    if (v == 0x6f && ((volatile char *)uffd_page)[PAGE - 1] == 0x6f) {
        printf("MM_PROBE uffd-missing-roundtrip: PASS\n");
    } else {
        printf("MM_PROBE uffd-missing-roundtrip: FAIL v=%d\n", v);
        _exit(1);
    }
    close(uffd_fd);
    munmap(uffd_page, PAGE);
    munmap(uffd_src, PAGE);
}

static void test_process_madvise(void) {
    size_t len = 2 * PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("pmadvise-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    memset(p, 0x44, len);
    int pidfd = syscall(SYS_pidfd_open, getpid(), 0);
    CHECK("pmadvise-pidfd", pidfd >= 0);
    if (pidfd < 0) { munmap(p, len); return; }
    struct iovec {
        void *base;
        size_t len;
    } iov = {p, len};
    errno = 0;
    CHECK("process-madvise-dontneed",
          syscall(SYS_process_madvise, pidfd, &iov, 1, MADV_DONTNEED, 0) == 0);
    CHECK("process-madvise-zerofill", p[0] == 0 && p[PAGE] == 0);
    /* 非法 advice → EINVAL */
    errno = 0;
    CHECK("process-madvise-badadvice",
          syscall(SYS_process_madvise, pidfd, &iov, 1, 100, 0) == -1 && errno == EINVAL);
    /* 非法 flags → EINVAL */
    errno = 0;
    CHECK("process-madvise-badflags",
          syscall(SYS_process_madvise, pidfd, &iov, 1, MADV_DONTNEED, 1) == -1 && errno == EINVAL);
    close(pidfd);
    munmap(p, len);
}

static void test_pkey_and_unsupported(void) {
    size_t len = PAGE;
    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK("pkey-mmap", p != MAP_FAILED);
    if (p == MAP_FAILED) return;
    /* pkey == -1 退化为 mprotect */
    errno = 0;
    CHECK("pkey-mprotect-minus1", syscall(SYS_pkey_mprotect, p, len, PROT_READ, -1) == 0);
    /* pkey != -1 → ENOSYS(无 PKU 架构) */
    errno = 0;
    CHECK("pkey-mprotect-enosys",
          syscall(SYS_pkey_mprotect, p, len, PROT_READ, 1) == -1 && errno == ENOSYS);
    errno = 0;
    CHECK("pkey-alloc-enosys", syscall(SYS_pkey_alloc, 0, 0) == -1 && errno == ENOSYS);
    /* memfd_secret / map_shadow_stack → ENOSYS */
    errno = 0;
    CHECK("memfd-secret-enosys", syscall(SYS_memfd_secret, 0) == -1 && errno == ENOSYS);
    errno = 0;
    CHECK("map-shadow-stack-enosys",
          syscall(SYS_map_shadow_stack, 0, len, 0) == -1 && errno == ENOSYS);
    munmap(p, len);
}

static void test_brk_overcommit(void) {
    /* brk 基本可用 */
    void *b = sbrk(0);
    CHECK("brk-extend", brk(b + 4 * PAGE) == 0);
    memset(b, 0x21, 4 * PAGE);
    CHECK("brk-write", (unsigned char)((char *)b)[0] == 0x21);
    brk(b);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("MM_PROBE suite start (pid=%d)\n", getpid());
    printf("MM_PROBE enter mlock\n");
    test_mlock_rlimit_and_status();
    printf("MM_PROBE enter mmap-flags\n");
    test_mmap_flags();
    printf("MM_PROBE enter mprotect-ro\n");
    test_mprotect_shared_ro();
    printf("MM_PROBE enter mseal\n");
    test_mseal();
    printf("MM_PROBE enter madvise\n");
    test_madvise_family();
    printf("MM_PROBE enter dontfork\n");
    test_fork_dontfork();
    test_remap_file_pages();
    test_numa();
    test_process_vm();
    test_cachestat();
    test_swapon_swapoff();
    test_sysinfo();
    test_uffd();
    test_process_madvise();
    test_pkey_and_unsupported();
    test_brk_overcommit();
    printf("MM_PROBE suite done: %d runs, %d failures\n", runs, failures);
    return failures == 0 ? 0 : 1;
}
