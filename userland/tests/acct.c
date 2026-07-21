#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/acct.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ACCT_FLAG_GROUP 0x20
#define RECORD_EXIT_STATUS 37

static const char accounting_path[] = "/tmp/pacct-test";

static int enable_and_exit(void) {
    int fd = open(accounting_path, O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (fd < 0) {
        return errno;
    }
    if (close(fd) != 0) {
        int error = errno;
        unlink(accounting_path);
        return error;
    }
    if (syscall(SYS_acct, accounting_path) != 0) {
        int error = errno;
        unlink(accounting_path);
        return error;
    }
    return RECORD_EXIT_STATUS;
}

static int disable_and_verify(int recorded_status) {
    int error = 0;
    if (syscall(SYS_acct, NULL) != 0) {
        error = errno;
    }

    struct acct_v3 record;
    memset(&record, 0, sizeof(record));
    int fd = open(accounting_path, O_RDONLY);
    if (fd < 0) {
        if (error == 0) {
            error = errno;
        }
    } else {
        ssize_t count = read(fd, &record, sizeof(record));
        if (error == 0 && count != (ssize_t)sizeof(record)) {
            error = count < 0 ? errno : EIO;
        }
        close(fd);
    }
    unlink(accounting_path);
    if (error != 0) {
        return error;
    }
    if (recorded_status != RECORD_EXIT_STATUS || record.ac_version != 2 || record.ac_pid == 0 ||
        record.ac_exitcode != (uint32_t)(RECORD_EXIT_STATUS << 8) ||
        (record.ac_flag & ACCT_FLAG_GROUP) == 0) {
        return EPROTO;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "record") == 0) {
        return enable_and_exit();
    }
    if (argc != 3 || strcmp(argv[1], "verify") != 0) {
        return EINVAL;
    }

    int status = 0;
    for (const char *digit = argv[2]; *digit != '\0'; digit++) {
        if (*digit < '0' || *digit > '9') {
            return EINVAL;
        }
        status = status * 10 + (*digit - '0');
    }
    int error = disable_and_verify(status);
    printf("TAP version 14\n1..1\n");
    if (error == 0) {
        printf("ok 1 - process acct_v3 accounting\n");
        return 0;
    }
    printf("not ok 1 - process acct_v3 accounting # error=%d %s\n", error, strerror(error));
    return 1;
}
