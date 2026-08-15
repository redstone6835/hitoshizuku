// fanotify 运行时自测：组创建/标记校验、inode 作用域通知事件（fd 读取时
// 打开）、FAN_MARK_IGNORED_MASK、REMOVE/FLUSH、非阻塞 EAGAIN、权限事件
// （ALLOW/DENY/信号中断 EINTR）、mount 作用域标记。全部通过返回 0。

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef __NR_fanotify_init
#define __NR_fanotify_init 262
#endif
#ifndef __NR_fanotify_mark
#define __NR_fanotify_mark 263
#endif

#define FAN_CLASS_NOTIF 0x0
#define FAN_CLASS_CONTENT 0x4
#define FAN_CLASS_PRE_CONTENT 0x8
#define FAN_UNLIMITED_QUEUE 0x10
#define FAN_UNLIMITED_MARKS 0x20
#define FAN_CLOEXEC 0x1
#define FAN_NONBLOCK 0x2

#define FAN_ACCESS 0x1
#define FAN_MODIFY 0x2
#define FAN_ATTRIB 0x4
#define FAN_CLOSE_WRITE 0x8
#define FAN_CLOSE_NOWRITE 0x10
#define FAN_OPEN 0x20
#define FAN_MOVED_FROM 0x40
#define FAN_MOVED_TO 0x80
#define FAN_CREATE 0x100
#define FAN_DELETE 0x200
#define FAN_DELETE_SELF 0x400
#define FAN_MOVE_SELF 0x800
#define FAN_OPEN_EXEC 0x1000
#define FAN_Q_OVERFLOW 0x4000
#define FAN_OPEN_PERM 0x10000
#define FAN_ACCESS_PERM 0x20000
#define FAN_OPEN_EXEC_PERM 0x40000
#define FAN_ONDIR 0x40000000
#define FAN_EVENT_ON_CHILD 0x08000000

#define FAN_MARK_ADD 0x1
#define FAN_MARK_REMOVE 0x2
#define FAN_MARK_DONT_FOLLOW 0x4
#define FAN_MARK_ONLYDIR 0x8
#define FAN_MARK_MOUNT 0x10
#define FAN_MARK_IGNORED_MASK 0x20
#define FAN_MARK_FLUSH 0x80
#define FAN_MARK_FILESYSTEM 0x100

#define FAN_ALLOW 0x1
#define FAN_DENY 0x2

#define FANOTIFY_METADATA_LEN 24

struct fanotify_event_metadata {
    uint32_t event_len;
    uint8_t vers;
    uint8_t reserved;
    uint16_t metadata_len;
    uint64_t mask;
    int32_t fd;
    int32_t pid;
};

struct fanotify_response {
    int32_t fd;
    uint32_t response;
    uint32_t flags;
};

static int failures = 0;

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        if (!(cond)) {                                                       \
            failures++;                                                      \
            fprintf(stderr, "[fanotify-test] FAIL %s (errno=%d %s)\n", msg,  \
                    errno, strerror(errno));                                 \
        } else {                                                             \
            printf("[fanotify-test] ok %s\n", msg);                          \
        }                                                                    \
    } while (0)

static int f_init(int flags, int ef_flags) {
    return (int)syscall(__NR_fanotify_init, flags, ef_flags);
}

static int f_mark(int fd, unsigned flags, uint64_t mask, int dirfd,
                  const char *path) {
    return (int)syscall(__NR_fanotify_mark, fd, flags, mask, dirfd, path);
}

/* 读一个事件；返回 0=读到、-1=EAGAIN、-2=其它错误。 */
static int read_one(int fd, struct fanotify_event_metadata *ev, char *name,
                    size_t name_cap) {
    char buf[512];
    ssize_t r = read(fd, buf, sizeof buf);
    if (r < 0) {
        if (errno == EAGAIN)
            return -1;
        return -2;
    }
    if (r < (ssize_t)FANOTIFY_METADATA_LEN)
        return -2;
    memcpy(ev, buf, FANOTIFY_METADATA_LEN);
    if (ev->event_len < FANOTIFY_METADATA_LEN || ev->event_len > (uint32_t)r)
        return -2;
    size_t len = ev->event_len - FANOTIFY_METADATA_LEN;
    if (len > name_cap - 1)
        len = name_cap - 1;
    memcpy(name, buf + FANOTIFY_METADATA_LEN, len);
    name[len] = 0;
    return 0;
}

static int test_init_and_errors(void) {
    errno = 0;
    CHECK(f_init(0x100000, 0) < 0 && errno == EINVAL,
          "invalid init flags -> EINVAL");
    errno = 0;
    CHECK(f_init(FAN_CLASS_NOTIF, 0x4) < 0 && errno == EINVAL,
          "invalid event_f_flags -> EINVAL");
    int fd = f_init(FAN_CLASS_NOTIF, 0);
    CHECK(fd >= 0, "fanotify_init NOTIF");
    errno = 0;
    CHECK(f_init(FAN_CLASS_NOTIF, 0) < 0 && errno == EMFILE,
          "second NOTIF group -> EMFILE");
    int nullfd = open("/dev/null", O_RDONLY);
    CHECK(nullfd >= 0, "open /dev/null");
    errno = 0;
    CHECK(f_mark(nullfd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, "/tmp") < 0 &&
              errno == EINVAL,
          "mark on non-fanotify fd -> EINVAL");
    close(nullfd);
    close(fd);
    fd = f_init(FAN_CLASS_NOTIF, 0);
    CHECK(fd >= 0, "group recreated after close");
    close(fd);
    return 0;
}

static int test_inode_notif(void) {
    int fd = f_init(FAN_CLASS_NOTIF | FAN_NONBLOCK, 0);
    CHECK(fd >= 0, "init NOTIF nonblock");
    struct fanotify_event_metadata ev;
    char name[64];
    errno = 0;
    CHECK(read_one(fd, &ev, name, sizeof name) == -1 && errno == EAGAIN,
          "empty queue -> EAGAIN");

    CHECK(f_mark(fd, FAN_MARK_ADD,
                 FAN_OPEN | FAN_CLOSE_WRITE | FAN_CLOSE_NOWRITE | FAN_MODIFY |
                     FAN_CREATE | FAN_EVENT_ON_CHILD,
                 AT_FDCWD, "/tmp/ft1") == 0,
          "mark dir (inode scope)");
    errno = 0;
    CHECK(f_mark(fd, FAN_MARK_ADD | FAN_MARK_ONLYDIR, FAN_OPEN, AT_FDCWD,
                 "/tmp/ft1/f") < 0 &&
              errno == ENOTDIR,
          "ONLYDIR on file -> ENOTDIR");

    int f = open("/tmp/ft1/new", O_CREAT | O_WRONLY, 0644);
    CHECK(f >= 0, "create /tmp/ft1/new");
    if (f >= 0)
        close(f);
    int got_create = 0, got_open = 0;
    char cname[64] = "";
    for (int i = 0; i < 8; i++) {
        if (read_one(fd, &ev, name, sizeof name) != 0)
            break;
        CHECK(ev.vers == 2 && ev.metadata_len == FANOTIFY_METADATA_LEN,
              "metadata header vers=2 len=24");
        CHECK(ev.pid > 0, "event pid set");
        if ((ev.mask & FAN_CREATE) && !got_create) {
            got_create = 1;
            snprintf(cname, sizeof cname, "%s", name);
        }
        if ((ev.mask & FAN_OPEN) && !got_open)
            got_open = 1;
        if (ev.fd >= 0)
            close(ev.fd);
    }
    CHECK(got_create && strcmp(cname, "new") == 0, "FAN_CREATE with name");
    CHECK(got_open, "FAN_OPEN on create");

    f = open("/tmp/ft1/f", O_WRONLY);
    CHECK(f >= 0, "open /tmp/ft1/f");
    if (f >= 0) {
        CHECK(write(f, "world", 5) == 5, "write /tmp/ft1/f");
        close(f);
    }
    int got_modify = 0, got_close_w = 0;
    got_open = 0;
    int fd_readable = 0;
    for (int i = 0; i < 8; i++) {
        if (read_one(fd, &ev, name, sizeof name) != 0)
            break;
        if (ev.mask & FAN_OPEN)
            got_open = 1;
        if (ev.mask & FAN_MODIFY)
            got_modify = 1;
        if (ev.mask & FAN_CLOSE_WRITE)
            got_close_w = 1;
        if (ev.fd >= 0) {
            if (ev.mask & FAN_MODIFY) {
                char buf[32];
                ssize_t nr = read(ev.fd, buf, sizeof buf - 1);
                if (nr >= 0) {
                    buf[nr] = 0;
                    if (strcmp(buf, "world") == 0)
                        fd_readable = 1;
                }
            }
            close(ev.fd);
        }
    }
    CHECK(got_open, "FAN_OPEN existing");
    CHECK(got_modify, "FAN_MODIFY");
    CHECK(got_close_w, "FAN_CLOSE_WRITE");
    CHECK(fd_readable, "event fd opens object");

    f = open("/tmp/ft1/f", O_RDONLY);
    CHECK(f >= 0, "open /tmp/ft1/f readonly");
    if (f >= 0)
        close(f);
    int got_close_nw = 0;
    for (int i = 0; i < 4; i++) {
        if (read_one(fd, &ev, name, sizeof name) != 0)
            break;
        if (ev.mask & FAN_CLOSE_NOWRITE)
            got_close_nw = 1;
        if (ev.fd >= 0)
            close(ev.fd);
    }
    CHECK(got_close_nw, "FAN_CLOSE_NOWRITE");
    close(fd);
    return 0;
}

static int test_ignored_mask(void) {
    int fd = f_init(FAN_CLASS_NOTIF | FAN_NONBLOCK, 0);
    CHECK(fd >= 0, "init NOTIF nonblock (ignored)");
    CHECK(f_mark(fd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, "/tmp/ft1/f2") == 0,
          "mark f2 open");
    CHECK(f_mark(fd, FAN_MARK_ADD | FAN_MARK_IGNORED_MASK, FAN_CLOSE_WRITE,
                 AT_FDCWD, "/tmp/ft1/f2") == 0,
          "mark f2 ignored close_write");
    int f = open("/tmp/ft1/f2", O_WRONLY);
    CHECK(f >= 0, "open f2");
    if (f >= 0)
        close(f);
    struct fanotify_event_metadata ev;
    char name[64];
    int got_open = 0, got_close = 0;
    for (int i = 0; i < 4; i++) {
        if (read_one(fd, &ev, name, sizeof name) != 0)
            break;
        if (ev.mask & FAN_OPEN)
            got_open = 1;
        if (ev.mask & FAN_CLOSE_WRITE)
            got_close = 1;
        if (ev.fd >= 0)
            close(ev.fd);
    }
    CHECK(got_open && !got_close, "ignored mask suppresses CLOSE_WRITE");
    close(fd);
    return 0;
}

static int test_remove_flush(void) {
    int fd = f_init(FAN_CLASS_NOTIF | FAN_NONBLOCK, 0);
    CHECK(fd >= 0, "init NOTIF nonblock (remove)");
    CHECK(f_mark(fd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, "/tmp/ft1/f2") == 0,
          "mark f2");
    CHECK(f_mark(fd, FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, "/tmp/ft1/f2") == 0,
          "remove mark");
    int f = open("/tmp/ft1/f2", O_WRONLY);
    if (f >= 0)
        close(f);
    struct fanotify_event_metadata ev;
    char name[64];
    errno = 0;
    CHECK(read_one(fd, &ev, name, sizeof name) == -1 && errno == EAGAIN,
          "no events after REMOVE");
    errno = 0;
    CHECK(f_mark(fd, FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, "/tmp/ft1/f2") < 0 &&
              errno == ENOENT,
          "REMOVE missing mark -> ENOENT");
    CHECK(f_mark(fd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, "/tmp/ft1/f2") == 0,
          "re-mark f2");
    CHECK(f_mark(fd, FAN_MARK_FLUSH, 0, AT_FDCWD, "/tmp/ft1/f2") == 0,
          "flush marks");
    f = open("/tmp/ft1/f2", O_WRONLY);
    if (f >= 0)
        close(f);
    errno = 0;
    CHECK(read_one(fd, &ev, name, sizeof name) == -1 && errno == EAGAIN,
          "no events after FLUSH");
    close(fd);
    return 0;
}

static void sigalrm_handler(int sig) {
    (void)sig;
}

static int test_perm(void) {
    int pfd = f_init(FAN_CLASS_CONTENT, 0);
    CHECK(pfd >= 0, "init CONTENT");
    CHECK(f_mark(pfd, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, "/tmp/ft1/f3") ==
              0,
          "mark f3 open_perm");
    struct fanotify_event_metadata ev;
    char name[64];
    struct fanotify_response resp;
    int st;
    pid_t pid;

    /* ALLOW：子进程 open 被阻塞，父进程响应 FAN_ALLOW 后放行。 */
    pid = fork();
    CHECK(pid >= 0, "fork allow");
    if (pid == 0) {
        int f = open("/tmp/ft1/f3", O_RDONLY);
        _exit(f >= 0 ? 0 : 1);
    }
    CHECK(read_one(pfd, &ev, name, sizeof name) == 0 &&
              (ev.mask & FAN_OPEN_PERM) != 0,
          "perm event on open");
    CHECK(ev.fd >= 0, "perm event fd");
    resp.fd = ev.fd;
    resp.response = FAN_ALLOW;
    resp.flags = 0;
    CHECK(write(pfd, &resp, sizeof resp) == (ssize_t)sizeof resp,
          "write ALLOW response");
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
              WEXITSTATUS(st) == 0,
          "child open allowed");
    if (ev.fd >= 0)
        close(ev.fd);

    /* DENY：子进程 open 返回 EACCES。 */
    pid = fork();
    CHECK(pid >= 0, "fork deny");
    if (pid == 0) {
        errno = 0;
        int f = open("/tmp/ft1/f3", O_RDONLY);
        _exit((f < 0 && errno == EACCES) ? 2 : 1);
    }
    CHECK(read_one(pfd, &ev, name, sizeof name) == 0 &&
              (ev.mask & FAN_OPEN_PERM) != 0,
          "perm event on deny");
    resp.fd = ev.fd;
    resp.response = FAN_DENY;
    CHECK(write(pfd, &resp, sizeof resp) == (ssize_t)sizeof resp,
          "write DENY response");
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
              WEXITSTATUS(st) == 2,
          "child open denied with EACCES");
    if (ev.fd >= 0)
        close(ev.fd);

    /* EINTR：子进程 open 阻塞中收到父进程发来的 SIGALRM → EINTR。 */
    pid = fork();
    CHECK(pid >= 0, "fork intr");
    if (pid == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = sigalrm_handler;
        sigaction(SIGALRM, &sa, NULL); /* 不带 SA_RESTART：syscall 返回 EINTR */
        errno = 0;
        int f = open("/tmp/ft1/f3", O_RDONLY);
        _exit((f < 0 && errno == EINTR) ? 3 : 1);
    }
    CHECK(read_one(pfd, &ev, name, sizeof name) == 0 &&
              (ev.mask & FAN_OPEN_PERM) != 0,
          "perm event on intr");
    /* 等子进程阻塞在权限等待后投递 SIGALRM。 */
    usleep(200000);
    CHECK(kill(pid, SIGALRM) == 0, "kill child SIGALRM");
    int r;
    do {
        r = waitpid(pid, &st, 0);
    } while (r < 0 && errno == EINTR);
    CHECK(r == pid && WIFEXITED(st) && WEXITSTATUS(st) == 3,
          "child open interrupted with EINTR");
    /* 事件仍留在队列：补响应清理（pending 已被 EINTR 路径移除，响应仅
       作废 fd 绑定；ENOENT 属预期，忽略）。 */
    if (ev.fd >= 0) {
        resp.fd = ev.fd;
        resp.response = FAN_ALLOW;
        ssize_t wr = write(pfd, &resp, sizeof resp);
        if (wr != (ssize_t)sizeof resp && errno != ENOENT) {
            /* 仅清理：pending 已移除时 ENOENT 属预期 */
        }
        close(ev.fd);
    }
    close(pfd);
    return 0;
}

static int test_mount_scope(void) {
    mkdir("/tmp/ftm", 0755);
    if (mount("none", "/tmp/ftm", "tmpfs", 0, NULL) != 0) {
        printf("[fanotify-test] skip mount scope: mount failed (errno=%d %s)\n",
               errno, strerror(errno));
        return 0;
    }
    int fd = f_init(FAN_CLASS_NOTIF | FAN_NONBLOCK, 0);
    CHECK(fd >= 0, "init NOTIF nonblock (mount)");
    CHECK(f_mark(fd, FAN_MARK_ADD | FAN_MARK_MOUNT,
                 FAN_OPEN | FAN_CREATE | FAN_MODIFY | FAN_EVENT_ON_CHILD,
                 AT_FDCWD, "/tmp/ftm") == 0,
          "mark mount scope");
    int f = open("/tmp/ftm/a", O_CREAT | O_WRONLY, 0644);
    CHECK(f >= 0, "create /tmp/ftm/a");
    if (f >= 0)
        close(f);
    struct fanotify_event_metadata ev;
    char name[64];
    int got_create = 0, got_open = 0;
    for (int i = 0; i < 6; i++) {
        if (read_one(fd, &ev, name, sizeof name) != 0)
            break;
        if (ev.mask & FAN_CREATE)
            got_create = 1;
        if (ev.mask & FAN_OPEN)
            got_open = 1;
        if (ev.fd >= 0)
            close(ev.fd);
    }
    CHECK(got_create && got_open, "mount scope events");

    /* 其它 mount（rootfs）上的操作不触发 mount 标记。 */
    f = open("/tmp/ft1/f4", O_CREAT | O_WRONLY, 0644);
    if (f >= 0)
        close(f);
    errno = 0;
    CHECK(read_one(fd, &ev, name, sizeof name) == -1 && errno == EAGAIN,
          "no events on other mount");

    /* 文件系统作用域：CAP_SYS_ADMIN 下标记 tmpfs 实例。 */
    CHECK(f_mark(fd, FAN_MARK_ADD | FAN_MARK_FILESYSTEM, FAN_OPEN, AT_FDCWD,
                 "/tmp/ftm") == 0,
          "mark filesystem scope");
    f = open("/tmp/ftm/b", O_CREAT | O_WRONLY, 0644);
    if (f >= 0)
        close(f);
    int got_fs = 0;
    for (int i = 0; i < 6; i++) {
        if (read_one(fd, &ev, name, sizeof name) != 0)
            break;
        if (ev.mask & FAN_OPEN)
            got_fs = 1;
        if (ev.fd >= 0)
            close(ev.fd);
    }
    CHECK(got_fs, "filesystem scope events");
    close(fd);
    umount("/tmp/ftm");
    return 0;
}

int main(void) {
    mkdir("/tmp/ft1", 0755);
    int f = open("/tmp/ft1/f", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (f >= 0) {
        if (write(f, "hello", 5) != 5) {
            /* 忽略：测试文件内容由下方断言覆盖 */
        }
        close(f);
    }
    f = open("/tmp/ft1/f2", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (f >= 0)
        close(f);
    f = open("/tmp/ft1/f3", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (f >= 0)
        close(f);

    printf("[fanotify-test] start\n");
    test_init_and_errors();
    test_inode_notif();
    test_ignored_mask();
    test_remove_flush();
    test_perm();
    test_mount_scope();

    if (failures == 0) {
        printf("[fanotify-test] ALL PASS\n");
        return 0;
    }
    printf("[fanotify-test] %d FAILURES\n", failures);
    return 1;
}
