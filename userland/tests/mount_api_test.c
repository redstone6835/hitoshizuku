// 新 mount API 运行时自测：fsopen/fsconfig/fsmount/move_mount/open_tree/fspick。
// 全部通过返回 0。

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/mount.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef __NR_open_tree
#define __NR_open_tree 428
#endif
#ifndef __NR_move_mount
#define __NR_move_mount 429
#endif
#ifndef __NR_fsopen
#define __NR_fsopen 430
#endif
#ifndef __NR_fsconfig
#define __NR_fsconfig 431
#endif
#ifndef __NR_fsmount
#define __NR_fsmount 432
#endif
#ifndef __NR_fspick
#define __NR_fspick 433
#endif

#ifndef FSOPEN_CLOEXEC
#define FSOPEN_CLOEXEC 1
#endif
#ifndef FSMOUNT_CLOEXEC
#define FSMOUNT_CLOEXEC 1
#endif
#ifndef OPEN_TREE_CLONE
#define OPEN_TREE_CLONE 1
#endif
#ifndef OPEN_TREE_CLOEXEC
/* LoongArch/asm-generic 的 O_CLOEXEC */
#define OPEN_TREE_CLOEXEC 0x80000
#endif
#ifndef FSPICK_CLOEXEC
#define FSPICK_CLOEXEC 1
#endif
#ifndef FSPICK_EMPTY_PATH
#define FSPICK_EMPTY_PATH 8
#endif

#ifndef FSCONFIG_SET_FLAG
#define FSCONFIG_SET_FLAG 0
#endif
#ifndef FSCONFIG_SET_STRING
#define FSCONFIG_SET_STRING 1
#endif
#ifndef FSCONFIG_CMD_CREATE
#define FSCONFIG_CMD_CREATE 6
#endif
#ifndef FSCONFIG_CMD_RECONFIGURE
#define FSCONFIG_CMD_RECONFIGURE 7
#endif

#ifndef MOVE_MOUNT_F_EMPTY_PATH
#define MOVE_MOUNT_F_EMPTY_PATH 0x4
#endif

static int failures = 0;

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        if (!(cond)) {                                                       \
            failures++;                                                      \
            fprintf(stderr, "[mount-api-test] FAIL %s (errno=%d %s)\n", msg, \
                    errno, strerror(errno));                                 \
        } else {                                                             \
            printf("[mount-api-test] ok %s\n", msg);                         \
        }                                                                    \
    } while (0)

static long s_fsopen(const char *name, unsigned flags) {
    return syscall(__NR_fsopen, name, flags);
}
static long s_fsconfig(int fd, unsigned cmd, const char *key, const char *value,
                       int aux) {
    return syscall(__NR_fsconfig, fd, cmd, key, value, aux);
}
static long s_fsmount(int fd, unsigned flags, unsigned mount_flags) {
    return syscall(__NR_fsmount, fd, flags, mount_flags);
}
static long s_move_mount(int from_fd, const char *from_path, int to_fd,
                         const char *to_path, unsigned flags) {
    return syscall(__NR_move_mount, from_fd, from_path, to_fd, to_path, flags);
}
static long s_open_tree(int dirfd, const char *path, unsigned flags) {
    return syscall(__NR_open_tree, dirfd, path, flags);
}
static long s_fspick(int dirfd, const char *path, unsigned flags) {
    return syscall(__NR_fspick, dirfd, path, flags);
}

/* 完整流程：fsopen tmpfs → source → CREATE → fsmount → move_mount 到 dir。 */
static int do_mount_tmpfs(const char *dir, const char *extra, int readonly) {
    int fd = (int)s_fsopen("tmpfs", 0);
    if (fd < 0)
        return -1;
    if (s_fsconfig(fd, FSCONFIG_SET_STRING, "source", "none", 0) != 0) {
        close(fd);
        return -1;
    }
    if (extra && s_fsconfig(fd, FSCONFIG_SET_STRING, extra, "", 0) != 0) {
        close(fd);
        return -1;
    }
    if (readonly && s_fsconfig(fd, FSCONFIG_SET_FLAG, "ro", NULL, 0) != 0) {
        close(fd);
        return -1;
    }
    if (s_fsconfig(fd, FSCONFIG_CMD_CREATE, NULL, NULL, 0) != 0) {
        close(fd);
        return -1;
    }
    int mfd = (int)s_fsmount(fd, 0, 0);
    if (mfd < 0) {
        close(fd);
        return -1;
    }
    if (s_move_mount(mfd, "", AT_FDCWD, dir, MOVE_MOUNT_F_EMPTY_PATH) != 0) {
        close(mfd);
        close(fd);
        return -1;
    }
    close(mfd);
    close(fd);
    return 0;
}

static int test_basic(void) {
    /* fsopen 错误路径 */
    errno = 0;
    CHECK(s_fsopen("no-such-fs-xyz", 0) < 0 && errno == ENODEV,
          "fsopen unknown fs -> ENODEV");
    errno = 0;
    CHECK(s_fsopen("tmpfs", 0x100) < 0 && errno == EINVAL,
          "fsopen invalid flags -> EINVAL");
    int fd = (int)s_fsopen("tmpfs", FSOPEN_CLOEXEC);
    CHECK(fd >= 0, "fsopen tmpfs");
    if (fd < 0)
        return 0;
    /* fsconfig 非法命令 */
    errno = 0;
    CHECK(s_fsconfig(fd, 99, NULL, NULL, 0) < 0 && errno == EINVAL,
          "fsconfig invalid cmd -> EINVAL");
    /* 未 CREATE 就 fsmount → EINVAL */
    errno = 0;
    CHECK(s_fsmount(fd, 0, 0) < 0 && errno == EINVAL,
          "fsmount before create -> EINVAL");
    CHECK(s_fsconfig(fd, FSCONFIG_SET_STRING, "source", "none", 0) == 0,
          "fsconfig set source");
    CHECK(s_fsconfig(fd, FSCONFIG_CMD_CREATE, NULL, NULL, 0) == 0,
          "fsconfig create");
    /* 重复 CREATE → EPERM */
    errno = 0;
    CHECK(s_fsconfig(fd, FSCONFIG_CMD_CREATE, NULL, NULL, 0) < 0 &&
              errno == EPERM,
          "fsconfig double create -> EPERM");
    int mfd = (int)s_fsmount(fd, 0, 0);
    CHECK(mfd >= 0, "fsmount");
    if (mfd < 0) {
        close(fd);
        return 0;
    }
    mkdir("/mnt/api", 0755);
    errno = 0;
    CHECK(s_move_mount(mfd, "", AT_FDCWD, "/mnt/api",
                       MOVE_MOUNT_F_EMPTY_PATH) == 0,
          "move_mount fs_context to /mnt/api");
    if (errno == 0 || 1) {
        /* 挂载生效：可写文件 */
        int f = open("/mnt/api/f1", O_CREAT | O_WRONLY, 0644);
        CHECK(f >= 0, "tmpfs mounted: create file");
        if (f >= 0) {
            CHECK(write(f, "api", 3) == 3, "write to mounted tmpfs");
            close(f);
        }
        CHECK(access("/mnt/api/f1", F_OK) == 0, "file visible");
        /* open_tree 克隆 */
        int otd = (int)s_open_tree(AT_FDCWD, "/mnt/api",
                                   OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC);
        CHECK(otd >= 0, "open_tree clone");
        if (otd >= 0) {
            mkdir("/mnt/api2", 0755);
            errno = 0;
            CHECK(s_move_mount(otd, "", AT_FDCWD, "/mnt/api2",
                               MOVE_MOUNT_F_EMPTY_PATH) == 0,
                  "move_mount open_tree clone to /mnt/api2");
            CHECK(access("/mnt/api2/f1", F_OK) == 0,
                  "clone sees original content");
            CHECK(access("/mnt/api/f1", F_OK) == 0,
                  "original mount intact after clone move");
            close(otd);
            umount("/mnt/api2");
        }
        /* fspick */
        int sp = (int)s_fspick(AT_FDCWD, "/mnt/api", FSPICK_CLOEXEC);
        CHECK(sp >= 0, "fspick mounted path");
        if (sp >= 0)
            close(sp);
        umount("/mnt/api");
    }
    close(mfd);
    close(fd);
    return 0;
}

static int test_readonly(void) {
    mkdir("/mnt/apiro", 0755);
    CHECK(do_mount_tmpfs("/mnt/apiro", NULL, 1) == 0,
          "mount tmpfs ro via fs_context");
    int f = open("/mnt/apiro/r1", O_CREAT | O_WRONLY, 0644);
    CHECK(f < 0 && errno == EROFS, "write to ro mount -> EROFS");
    if (f >= 0)
        close(f);
    CHECK(access("/mnt/apiro", F_OK) == 0, "ro mount accessible");
    umount("/mnt/apiro");
    return 0;
}

int main(void) {
    printf("[mount-api-test] start\n");
    test_basic();
    test_readonly();

    if (failures == 0) {
        printf("[mount-api-test] ALL PASS\n");
        return 0;
    }
    printf("[mount-api-test] %d FAILURES\n", failures);
    return 1;
}
