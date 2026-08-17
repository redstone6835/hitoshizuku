// xattr + POSIX ACL 运行时自测（tmpfs 与 extfs 双后端）。
//
// 覆盖：setxattr/getxattr/listxattr/removexattr 全家族、XATTR_CREATE/REPLACE、
// ERANGE/ENODATA、user.* 权限模型、trusted.* 能力要求、POSIX ACL 存取、
// default ACL 派生与 chmod↔mask 同步。全部通过返回 0。

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        if (!(cond)) {                                                       \
            fprintf(stderr, "[xattr-test] FAIL %s (errno=%d %s)\n", msg,     \
                    errno, strerror(errno));                                 \
            return 1;                                                        \
        }                                                                    \
        printf("[xattr-test] ok %s\n", msg);                                 \
    } while (0)

#define ACL_ACCESS "system.posix_acl_access"
#define ACL_DEFAULT "system.posix_acl_default"

static int test_on(const char *dir) {
    char file[256], sub[256];
    snprintf(file, sizeof file, "%s/xattr-file", dir);
    snprintf(sub, sizeof sub, "%s/xattr-dir", dir);

    CHECK(unlink(file) == 0 || errno == ENOENT, "cleanup file");
    CHECK(rmdir(sub) == 0 || errno == ENOENT, "cleanup dir");

    // 基本 set/get/list/remove
    int fd = open(file, O_CREAT | O_RDWR, 0644);
    CHECK(fd >= 0, "open file");

    CHECK(setxattr(file, "user.k1", "v1", 2, 0) == 0, "setxattr user.k1");
    CHECK(setxattr(file, "user.k2", "long-value", 10, 0) == 0, "setxattr user.k2");
    CHECK(lsetxattr(file, "user.l", "lv", 2, 0) == 0, "lsetxattr");
    CHECK(fsetxattr(fd, "user.f", "fv", 2, 0) == 0, "fsetxattr");

    char buf[64];
    ssize_t n = getxattr(file, "user.k1", buf, sizeof buf);
    CHECK(n == 2 && memcmp(buf, "v1", 2) == 0, "getxattr value");
    n = lgetxattr(file, "user.l", buf, sizeof buf);
    CHECK(n == 2 && memcmp(buf, "lv", 2) == 0, "lgetxattr");
    n = fgetxattr(fd, "user.f", buf, sizeof buf);
    CHECK(n == 2 && memcmp(buf, "fv", 2) == 0, "fgetxattr");

    // listxattr
    char list[256];
    n = listxattr(file, list, sizeof list);
    CHECK(n > 0, "listxattr");
    CHECK(memmem(list, (size_t)n, "user.k1", 7) != NULL, "list contains k1");
    CHECK(memmem(list, (size_t)n, "user.k2", 7) != NULL, "list contains k2");

    // ERANGE：缓冲不足
    errno = 0;
    CHECK(getxattr(file, "user.k2", buf, 2) == -1 && errno == ERANGE,
          "getxattr ERANGE");

    // XATTR_CREATE / XATTR_REPLACE
    errno = 0;
    CHECK(setxattr(file, "user.k1", "x", 1, XATTR_CREATE) == -1 && errno == EEXIST,
          "XATTR_CREATE on existing -> EEXIST");
    errno = 0;
    CHECK(setxattr(file, "user.none", "x", 1, XATTR_REPLACE) == -1 &&
              errno == ENODATA,
          "XATTR_REPLACE on missing -> ENODATA");
    CHECK(setxattr(file, "user.k1", "v2", 2, XATTR_REPLACE) == 0,
          "XATTR_REPLACE ok");

    // 未知命名空间 → EOPNOTSUPP；system.* 其它 → EOPNOTSUPP
    errno = 0;
    CHECK(setxattr(file, "foo.bar", "x", 1, 0) == -1 && errno == EOPNOTSUPP,
          "unknown namespace -> EOPNOTSUPP");
    errno = 0;
    CHECK(setxattr(file, "system.other", "x", 1, 0) == -1 && errno == EOPNOTSUPP,
          "other system.* -> EOPNOTSUPP");

    // trusted.* 需要 CAP_SYS_ADMIN：root 应成功；非 root 应 EPERM/EOPNOTSUPP。
    if (geteuid() == 0) {
        CHECK(setxattr(file, "trusted.t", "x", 1, 0) == 0, "trusted.* allowed for root");
        CHECK(removexattr(file, "trusted.t") == 0, "removexattr trusted.t");
    } else {
        errno = 0;
        CHECK(setxattr(file, "trusted.t", "x", 1, 0) == -1 &&
                  (errno == EPERM || errno == EOPNOTSUPP),
              "trusted.* denied");
    }

    // removexattr 家族
    CHECK(removexattr(file, "user.k2") == 0, "removexattr");
    errno = 0;
    CHECK(removexattr(file, "user.k2") == -1 && errno == ENODATA,
          "removexattr missing -> ENODATA");
    CHECK(lremovexattr(file, "user.l") == 0, "lremovexattr");
    CHECK(fremovexattr(fd, "user.f") == 0, "fremovexattr");

    // POSIX ACL：写入 6 条目 ACL（version=2）
    unsigned char acl[52];
    unsigned int version = 2;
    memcpy(acl, &version, 4);
    struct {
        unsigned short tag;
        unsigned short perm;
        unsigned int id;
    } entries[] = {
        {0x01, 7, 0}, /* USER_OBJ */
        {0x02, 5, 1234}, /* USER 1234 r-x */
        {0x04, 4, 0}, /* GROUP_OBJ */
        {0x08, 2, 5678}, /* GROUP 5678 -w- */
        {0x10, 4, 0}, /* MASK */
        {0x20, 0, 0}, /* OTHER */
    };
    size_t off = 4;
    for (size_t i = 0; i < 6; i++) {
        memcpy(acl + off, &entries[i].tag, 2);
        memcpy(acl + off + 2, &entries[i].perm, 2);
        memcpy(acl + off + 4, &entries[i].id, 4);
        off += 8;
    }
    CHECK(setxattr(file, ACL_ACCESS, acl, off, 0) == 0, "set posix_acl_access");

    // 读取回环
    unsigned char back[64];
    n = getxattr(file, ACL_ACCESS, back, sizeof back);
    CHECK(n == (ssize_t)off && memcmp(back, acl, off) == 0, "get posix_acl_access");

    // mode 组位应同步为 mask（r-- = 4）
    struct stat st;
    CHECK(stat(file, &st) == 0, "stat after acl");
    CHECK(((st.st_mode >> 3) & 7) == 4, "mode group bits == acl mask");

    // chmod 后 mask 应同步
    CHECK(chmod(file, 0600) == 0, "chmod 0600");
    n = getxattr(file, ACL_ACCESS, back, sizeof back);
    CHECK(n == (ssize_t)off, "get acl after chmod");
    unsigned short mask_perm = 0xffff;
    size_t mask_off = 4;
    for (size_t i = 0; i < 6; i++) {
        unsigned short tag;
        memcpy(&tag, back + mask_off, 2);
        if (tag == 0x10) {
            memcpy(&mask_perm, back + mask_off + 2, 2);
            break;
        }
        mask_off += 8;
    }
    CHECK(mask_perm == 0, "acl mask synced to chmod group bits");

    // default ACL → 新文件继承 + mode 组位 = mask
    CHECK(mkdir(sub, 0700) == 0, "mkdir");
    unsigned char dacl[52];
    unsigned int dversion = 2;
    memcpy(dacl, &dversion, 4);
    struct {
        unsigned short tag;
        unsigned short perm;
        unsigned int id;
    } dentries[] = {
        {0x01, 7, 0}, {0x02, 5, 1111}, {0x04, 7, 0},
        {0x08, 1, 2222}, {0x10, 7, 0}, {0x20, 0, 0},
    };
    size_t doff = 4;
    for (size_t i = 0; i < 6; i++) {
        memcpy(dacl + doff, &dentries[i].tag, 2);
        memcpy(dacl + doff + 2, &dentries[i].perm, 2);
        memcpy(dacl + doff + 4, &dentries[i].id, 4);
        doff += 8;
    }
    CHECK(setxattr(sub, ACL_DEFAULT, dacl, doff, 0) == 0, "set default acl");

    char child[300];
    snprintf(child, sizeof child, "%s/child", sub);
    int cfd = open(child, O_CREAT | O_RDWR, 0640);
    CHECK(cfd >= 0, "create child in default-acl dir");
    close(cfd);
    n = getxattr(child, ACL_ACCESS, back, sizeof back);
    CHECK(n == (ssize_t)doff, "child inherits acl");
    CHECK(stat(child, &st) == 0, "stat child");
    // Linux posix_acl_create：child 的 MASK 取创建 mode 的组位（0640 → 4），
    // child mode 组位同步为 MASK 值。
    CHECK(((st.st_mode >> 3) & 7) == 4, "child mode group bits == create mode group");

    // 清理
    close(fd);
    unlink(child);
    rmdir(sub);
    unlink(file);
    return 0;
}

int main(void) {
    printf("[xattr-test] tmpfs backend (/tmp)\n");
    if (test_on("/tmp") != 0) return 1;
    // /mnt 是 extfs 测试盘（init 已挂载）；失败不阻断（可能未挂载）。
    struct stat mst;
    if (stat("/mnt", &mst) == 0) {
        printf("[xattr-test] extfs backend (/mnt)\n");
        if (test_on("/mnt") != 0) return 1;
    } else {
        printf("[xattr-test] /mnt not mounted, skip extfs backend\n");
    }
    printf("[xattr-test] ALL PASS\n");
    return 0;
}
