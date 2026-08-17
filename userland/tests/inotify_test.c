// inotify 运行时自测：监视目录与文件，验证事件序列、cookie 配对、
// DELETE_SELF/IGNORED、fdinfo。全部通过返回 0。

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        if (!(cond)) {                                                       \
            fprintf(stderr, "[inotify-test] FAIL %s (errno=%d %s)\n", msg,   \
                    errno, strerror(errno));                                 \
            return 1;                                                        \
        }                                                                    \
        printf("[inotify-test] ok %s\n", msg);                               \
    } while (0)

#define IN_MASK (IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO)

struct ev {
    int wd;
    uint32_t mask;
    uint32_t cookie;
    char name[64];
};

static int drain(int fd, struct ev *evs, int cap) {
    int n = 0;
    for (;;) {
        char buf[512];
        ssize_t r = read(fd, buf, sizeof buf);
        if (r < 0) {
            if (errno == EAGAIN) break;
            return -1;
        }
        if (r == 0) break;
        ssize_t off = 0;
        while (off < r && n < cap) {
            struct inotify_event *ie = (struct inotify_event *)(buf + off);
            evs[n].wd = ie->wd;
            evs[n].mask = ie->mask;
            evs[n].cookie = ie->cookie;
            size_t len = ie->len;
            if (len > sizeof(evs[n].name) - 1) len = sizeof(evs[n].name) - 1;
            memcpy(evs[n].name, ie->name, len);
            evs[n].name[len] = 0;
            n++;
            off += sizeof(struct inotify_event) + ie->len;
        }
    }
    return n;
}

int main(void) {
    printf("[inotify-test] start\n");
    struct ev evs[64];

    if (system("rm -rf /tmp/w") != 0) { /* 忽略清理失败 */ }
    CHECK(mkdir("/tmp/w", 0755) == 0, "mkdir /tmp/w");

    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    CHECK(fd >= 0, "inotify_init1");

    int wd_dir = inotify_add_watch(fd, "/tmp/w", IN_MASK);
    CHECK(wd_dir == 1, "add_watch dir -> wd=1");

    // 创建文件：父目录 CREATE
    int f = open("/tmp/w/a", O_CREAT | O_RDWR, 0644);
    CHECK(f >= 0, "create /tmp/w/a");
    int wd_file = inotify_add_watch(fd, "/tmp/w/a",
                                    IN_MODIFY | IN_OPEN | IN_CLOSE_WRITE |
                                        IN_CLOSE_NOWRITE | IN_DELETE_SELF |
                                        IN_MOVE_SELF | IN_ATTRIB);
    CHECK(wd_file == 2, "add_watch file -> wd=2");

    // 写 + chmod + close
    CHECK(write(f, "hello", 5) == 5, "write");
    CHECK(chmod("/tmp/w/a", 0600) == 0, "chmod");
    close(f);

    // 重开（OPEN + CLOSE_NOWRITE）
    f = open("/tmp/w/a", O_RDONLY);
    CHECK(f >= 0, "reopen");
    close(f);

    // fdinfo 应包含两条 watch 行（dir + file，均在监视中）。
    char finfo[256];
    snprintf(finfo, sizeof finfo, "/proc/self/fdinfo/%d", fd);
    FILE *fp = fopen(finfo, "r");
    CHECK(fp != NULL, "open fdinfo");
    int wd_lines = 0;
    char line[128];
    while (fgets(line, sizeof line, fp)) {
        printf("[inotify-test] fdinfo: %s", line);
        if (strncmp(line, "inotify wd:", 11) == 0) wd_lines++;
    }
    fclose(fp);
    CHECK(wd_lines == 2, "fdinfo has 2 inotify watch lines");

    // rename → MOVED_FROM/TO + MOVE_SELF
    CHECK(rename("/tmp/w/a", "/tmp/w/b") == 0, "rename");

    // unlink → DELETE + DELETE_SELF + IGNORED
    CHECK(unlink("/tmp/w/b") == 0, "unlink");

    int n = drain(fd, evs, 64);
    CHECK(n >= 0, "drain events");
    printf("[inotify-test] drained %d events\n", n);
    for (int i = 0; i < n; i++) {
        printf("[inotify-test]   ev%d wd=%d mask=%08x cookie=%u name=%s\n",
               i, evs[i].wd, evs[i].mask, evs[i].cookie, evs[i].name);
    }

    // 事件序列断言（按注入顺序）：
    // 0: CREATE(dir, a)  1: MODIFY(file)  2: ATTRIB(file)
    // 3: CLOSE_WRITE(file)  4: OPEN(file)  5: CLOSE_NOWRITE(file)
    // 6: MOVED_FROM(dir, a)  7: MOVE_SELF(file)  8: MOVED_TO(dir, b)
    // 9: DELETE(dir, b)  10: DELETE_SELF(file)  11: IGNORED(file)
    if (n < 12) {
        fprintf(stderr, "[inotify-test] FAIL only %d events\n", n);
        return 1;
    }
    CHECK(evs[0].wd == wd_dir && (evs[0].mask & IN_CREATE) &&
              strcmp(evs[0].name, "a") == 0,
          "ev0 CREATE(dir,a)");
    CHECK(evs[1].wd == wd_file && evs[1].mask == IN_MODIFY,
          "ev1 MODIFY(file)");
    CHECK(evs[2].wd == wd_file && evs[2].mask == IN_ATTRIB,
          "ev2 ATTRIB(file)");
    CHECK(evs[3].wd == wd_file && evs[3].mask == IN_CLOSE_WRITE,
          "ev3 CLOSE_WRITE(file)");
    CHECK(evs[4].wd == wd_file && evs[4].mask == IN_OPEN,
          "ev4 OPEN(file)");
    CHECK(evs[5].wd == wd_file && evs[5].mask == IN_CLOSE_NOWRITE,
          "ev5 CLOSE_NOWRITE(file)");
    CHECK(evs[6].wd == wd_dir && (evs[6].mask & IN_MOVED_FROM) &&
              strcmp(evs[6].name, "a") == 0,
          "ev6 MOVED_FROM(dir,a)");
    CHECK(evs[7].wd == wd_file && evs[7].mask == IN_MOVE_SELF,
          "ev7 MOVE_SELF(file)");
    CHECK(evs[8].wd == wd_dir && (evs[8].mask & IN_MOVED_TO) &&
              strcmp(evs[8].name, "b") == 0,
          "ev8 MOVED_TO(dir,b)");
    CHECK(evs[6].cookie != 0 && evs[6].cookie == evs[8].cookie,
          "ev6/ev8 same cookie");
    CHECK(evs[9].wd == wd_dir && (evs[9].mask & IN_DELETE) &&
              strcmp(evs[9].name, "b") == 0,
          "ev9 DELETE(dir,b)");
    CHECK(evs[10].wd == wd_file && evs[10].mask == IN_DELETE_SELF,
          "ev10 DELETE_SELF(file)");
    CHECK(evs[11].wd == wd_file && evs[11].mask == IN_IGNORED,
          "ev11 IGNORED(file)");

    // rm_watch → IGNORED；未知 wd → EINVAL
    CHECK(inotify_rm_watch(fd, wd_dir) == 0, "rm_watch");
    errno = 0;
    CHECK(inotify_rm_watch(fd, 999) == -1 && errno == EINVAL,
          "rm_watch unknown -> EINVAL");
    n = drain(fd, evs, 64);
    CHECK(n >= 1 && evs[0].wd == wd_dir && evs[0].mask == IN_IGNORED,
          "rm_watch IGNORED");

    // ONESHOT：只投递一次 + IGNORED
    int wd_oneshot = inotify_add_watch(fd, "/tmp/w", IN_CREATE | IN_ONESHOT);
    CHECK(wd_oneshot == 3, "add_watch oneshot");
    CHECK(open("/tmp/w/c", O_CREAT | O_RDWR, 0644) >= 0, "create c");
    close(open("/tmp/w/d", O_CREAT | O_RDWR, 0644));
    close(open("/tmp/w/c", O_RDONLY));
    n = drain(fd, evs, 64);
    int oneshot_events = 0;
    for (int i = 0; i < n; i++) {
        if (evs[i].wd == wd_oneshot && (evs[i].mask & IN_CREATE)) oneshot_events++;
        if (evs[i].wd == wd_oneshot && (evs[i].mask & IN_IGNORED)) oneshot_events += 100;
    }
    CHECK(oneshot_events == 101, "oneshot: 1 CREATE + 1 IGNORED");

    close(fd);
    printf("[inotify-test] ALL PASS\n");
    return 0;
}
