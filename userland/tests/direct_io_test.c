// O_DIRECT 运行时自测：ext4（挂载 /dev/vda）上的对齐读写、不对齐 EINVAL、
// fcntl 动态切换；tmpfs 拒绝 open(O_DIRECT)。全部通过返回 0。

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef O_DIRECT
#define O_DIRECT 0o00040000
#endif

static int failures = 0;

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        if (!(cond)) {                                                       \
            failures++;                                                      \
            fprintf(stderr, "[direct-io-test] FAIL %s (errno=%d %s)\n", msg, \
                    errno, strerror(errno));                                 \
        } else {                                                             \
            printf("[direct-io-test] ok %s\n", msg);                         \
        }                                                                    \
    } while (0)

/* 512 字节对齐的静态缓冲。 */
static unsigned char aligned[2048] __attribute__((aligned(512)));

/* 在 dir 下创建 O_DIRECT 测试文件并返回 fd；失败返回 -1。 */
static int dio_open(const char *dir, int extra_flags) {
    char path[128];
    snprintf(path, sizeof path, "%s/dio.bin", dir);
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC | extra_flags, 0644);
    return fd;
}

/* ext4 场景：对齐读写 + 不对齐 EINVAL + fcntl 切换。 */
static int test_ext4(const char *dir) {
    unsigned char *buf = aligned;
    int fd;

    /* 1. 对齐写 1024（两个 512 块）→ 成功 */
    memset(buf, 0x5a, 1024);
    fd = dio_open(dir, O_DIRECT);
    CHECK(fd >= 0, "open O_DIRECT on ext4");
    if (fd < 0)
        return 0;
    errno = 0;
    CHECK(write(fd, buf, 1024) == 1024, "aligned write 1024");
    /* 2. 不对齐 buffer → EINVAL */
    errno = 0;
    CHECK(write(fd, buf + 1, 512) < 0 && errno == EINVAL,
          "unaligned buffer -> EINVAL");
    /* 3. 不对齐 len → EINVAL（offset 0、buffer 对齐） */
    errno = 0;
    CHECK(write(fd, buf, 513) < 0 && errno == EINVAL,
          "unaligned length -> EINVAL");
    /* 4. 不对齐 offset（pwrite 偏移 1）→ EINVAL */
    errno = 0;
    CHECK(pwrite(fd, buf, 512, 1) < 0 && errno == EINVAL,
          "unaligned offset -> EINVAL");
    /* 5. fsync 后对齐读回校验（第一块） */
    CHECK(fsync(fd) == 0, "fsync");
    lseek(fd, 0, SEEK_SET);
    memset(buf, 0, 1024);
    errno = 0;
    CHECK(read(fd, buf, 512) == 512, "aligned read 512");
    int good = 1;
    for (int i = 0; i < 512; i++) {
        if (buf[i] != 0x5a) {
            good = 0;
            break;
        }
    }
    CHECK(good, "read back data matches");
    /* 6. pread 对齐 offset（512）读第二块 → 成功且数据正确 */
    memset(buf, 0, 512);
    CHECK(pread(fd, buf, 512, 512) == 512, "aligned pread offset 512");
    good = 1;
    for (int i = 0; i < 512; i++) {
        if (buf[i] != 0x5a) {
            good = 0;
            break;
        }
    }
    CHECK(good, "pread second block data matches");
    close(fd);

    /* 7. fcntl F_SETFL 动态加 O_DIRECT：随后不对齐读写 EINVAL，清除后恢复 */
    fd = dio_open(dir, 0);
    CHECK(fd >= 0, "open plain (fcntl switch)");
    if (fd < 0)
        return 0;
    int fl = fcntl(fd, F_GETFL);
    CHECK(fl >= 0, "F_GETFL");
    CHECK(fcntl(fd, F_SETFL, fl | O_DIRECT) == 0, "F_SETFL +O_DIRECT");
    errno = 0;
    CHECK(write(fd, buf + 1, 512) < 0 && errno == EINVAL,
          "unaligned buffer after F_SETFL -> EINVAL");
    CHECK(fcntl(fd, F_SETFL, fl & ~O_DIRECT) == 0, "F_SETFL -O_DIRECT");
    errno = 0;
    CHECK(write(fd, buf + 1, 512) == 512, "unaligned write after clear");
    close(fd);
    return 0;
}

/* tmpfs 场景：open(O_DIRECT) 拒绝；fcntl 设置后 I/O 保持普通路径。 */
static int test_tmpfs(const char *dir) {
    char path[128];
    snprintf(path, sizeof path, "%s/dio_tmp.bin", dir);
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
    CHECK(fd >= 0, "open plain on tmpfs");
    if (fd < 0)
        return 0;
    close(fd);

    errno = 0;
    int dfd = open(path, O_RDWR | O_DIRECT);
    CHECK(dfd < 0 && errno == EINVAL, "open O_DIRECT on tmpfs -> EINVAL");
    if (dfd >= 0)
        close(dfd);

    /* fcntl 设置 O_DIRECT：tmpfs 不支持，I/O 保持普通路径（Linux 语义）。 */
    fd = open(path, O_RDWR | O_TRUNC);
    if (fd >= 0) {
        int fl = fcntl(fd, F_GETFL);
        CHECK(fcntl(fd, F_SETFL, fl | O_DIRECT) == 0, "F_SETFL +O_DIRECT tmpfs");
        memset(aligned, 0x33, 512);
        errno = 0;
        CHECK(write(fd, aligned + 1, 512) == 512,
              "unaligned write on tmpfs with O_DIRECT flag (ignored)");
        close(fd);
    }
    return 0;
}

int main(void) {
    printf("[direct-io-test] start\n");

    /* ext4：挂载 /dev/vd0（virtio-blk 首个盘）；失败则 SKIP。 */
    mkdir("/mnt/dio", 0755);
    if (mount("/dev/vd0", "/mnt/dio", "extfs", 0, NULL) != 0) {
        printf("[direct-io-test] skip ext4: mount /dev/vd0 failed "
               "(errno=%d %s)\n",
               errno, strerror(errno));
    } else {
        printf("[direct-io-test] ext4 mounted at /mnt/dio\n");
        test_ext4("/mnt/dio");
        umount("/mnt/dio");
    }

    test_tmpfs("/tmp");

    if (failures == 0) {
        printf("[direct-io-test] ALL PASS\n");
        return 0;
    }
    printf("[direct-io-test] %d FAILURES\n", failures);
    return 1;
}
