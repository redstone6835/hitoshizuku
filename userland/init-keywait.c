#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

enum {
    KEYWAIT_DETECTED = 0,
    KEYWAIT_TIMEOUT = 1,
    KEYWAIT_ERROR = 2,
    DEFAULT_TIMEOUT_MS = 3000,
    MAX_TIMEOUT_MS = 60000,
};

static volatile sig_atomic_t interrupted;

static void handle_signal(int signal_number)
{
    interrupted = signal_number;
}

static int install_signal_handlers(void)
{
    struct sigaction action = {0};

    action.sa_handler = handle_signal;
    if (sigemptyset(&action.sa_mask) < 0) {
        return -1;
    }
    if (sigaction(SIGINT, &action, NULL) < 0) {
        return -1;
    }
    if (sigaction(SIGTERM, &action, NULL) < 0) {
        return -1;
    }
    return 0;
}

static int parse_timeout_ms(const char *text, uint64_t *timeout_ms)
{
    char *end = NULL;
    unsigned long value;

    if (text == NULL || *text == '\0') {
        return -1;
    }
    errno = 0;
    value = strtoul(text, &end, 10);
    if (errno != 0 || *end != '\0' || value == 0 || value > MAX_TIMEOUT_MS) {
        return -1;
    }
    *timeout_ms = value;
    return 0;
}

static int monotonic_ms(uint64_t *value)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) < 0) {
        return -1;
    }
    *value = (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
    return 0;
}

static int wait_for_ctrl_c(int console, uint64_t timeout_ms)
{
    uint64_t start;
    uint64_t now;

    if (monotonic_ms(&start) < 0) {
        return KEYWAIT_ERROR;
    }

    for (;;) {
        unsigned char input[16];
        ssize_t length;

        if (interrupted == SIGINT) {
            return KEYWAIT_DETECTED;
        }
        if (interrupted != 0) {
            return KEYWAIT_ERROR;
        }

        length = read(console, input, sizeof(input));
        if (length > 0) {
            for (ssize_t index = 0; index < length; ++index) {
                if (input[index] == 3) {
                    return KEYWAIT_DETECTED;
                }
            }
        } else if (length < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) {
            return KEYWAIT_ERROR;
        }

        if (monotonic_ms(&now) < 0) {
            return KEYWAIT_ERROR;
        }
        if (now - start >= timeout_ms) {
            return KEYWAIT_TIMEOUT;
        }
        (void)sched_yield();
    }
}

int main(int argc, char **argv)
{
    uint64_t timeout_ms = DEFAULT_TIMEOUT_MS;
    struct termios saved;
    struct termios raw;
    int have_termios = 0;
    int console;
    int result;

    if (argc > 2 || (argc == 2 && parse_timeout_ms(argv[1], &timeout_ms) < 0)) {
        return KEYWAIT_ERROR;
    }
    if (install_signal_handlers() < 0) {
        return KEYWAIT_ERROR;
    }

    console = open("/dev/console", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (console < 0) {
        return KEYWAIT_ERROR;
    }

    if (tcgetattr(console, &saved) == 0) {
        raw = saved;
        cfmakeraw(&raw);
        raw.c_cc[VMIN] = 0;
        raw.c_cc[VTIME] = 0;
        if (tcsetattr(console, TCSANOW, &raw) == 0) {
            have_termios = 1;
        }
    }

    result = wait_for_ctrl_c(console, timeout_ms);
    if (have_termios) {
        (void)tcsetattr(console, TCSANOW, &saved);
    }
    (void)close(console);
    return result;
}
