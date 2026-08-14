#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ARRAY_SIZE(array) (sizeof(array) / sizeof((array)[0]))
#define MUTEX_THREADS 4
#define MUTEX_LOOPS 5000
#define WAIT_TIMEOUT_MS 2000

static void add_ms(struct timespec *time, long milliseconds) {
    time->tv_sec += milliseconds / 1000;
    time->tv_nsec += (milliseconds % 1000) * 1000000L;
    if (time->tv_nsec >= 1000000000L) {
        time->tv_sec++;
        time->tv_nsec -= 1000000000L;
    }
}

static void sleep_ms(long milliseconds) {
    struct timespec request = {
        .tv_sec = milliseconds / 1000,
        .tv_nsec = (milliseconds % 1000) * 1000000L,
    };
    while (nanosleep(&request, &request) != 0 && errno == EINTR) {
    }
}

static int wait_for_flag(atomic_int *flag, int expected) {
    struct timespec deadline;
    if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
        return errno;
    }
    add_ms(&deadline, WAIT_TIMEOUT_MS);
    for (;;) {
        if (atomic_load_explicit(flag, memory_order_acquire) == expected) {
            return 0;
        }
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
            return errno;
        }
        if (now.tv_sec > deadline.tv_sec ||
            (now.tv_sec == deadline.tv_sec && now.tv_nsec >= deadline.tv_nsec)) {
            return ETIMEDOUT;
        }
        sched_yield();
    }
}

static pthread_mutex_t counter_mutex = PTHREAD_MUTEX_INITIALIZER;
static long counter_value;

static void *counter_worker(void *unused) {
    (void)unused;
    for (int i = 0; i < MUTEX_LOOPS; i++) {
        int error = pthread_mutex_lock(&counter_mutex);
        if (error != 0) {
            return (void *)(intptr_t)error;
        }
        counter_value++;
        error = pthread_mutex_unlock(&counter_mutex);
        if (error != 0) {
            return (void *)(intptr_t)error;
        }
    }
    return NULL;
}

static int test_mutex_contention(void) {
    pthread_t threads[MUTEX_THREADS];
    counter_value = 0;
    for (size_t i = 0; i < ARRAY_SIZE(threads); i++) {
        int error = pthread_create(&threads[i], NULL, counter_worker, NULL);
        if (error != 0) {
            return error;
        }
    }
    for (size_t i = 0; i < ARRAY_SIZE(threads); i++) {
        void *result = NULL;
        int error = pthread_join(threads[i], &result);
        if (error != 0) {
            return error;
        }
        if (result != NULL) {
            return (int)(intptr_t)result;
        }
    }
    return counter_value == MUTEX_THREADS * MUTEX_LOOPS ? 0 : EIO;
}

struct cond_context {
    pthread_mutex_t mutex;
    pthread_cond_t cond;
    int ready;
    int release;
    int woken;
};

static void *cond_worker(void *opaque) {
    struct cond_context *context = opaque;
    int error = pthread_mutex_lock(&context->mutex);
    if (error != 0) {
        return (void *)(intptr_t)error;
    }
    context->ready++;
    pthread_cond_broadcast(&context->cond);
    while (!context->release) {
        error = pthread_cond_wait(&context->cond, &context->mutex);
        if (error != 0) {
            pthread_mutex_unlock(&context->mutex);
            return (void *)(intptr_t)error;
        }
    }
    context->woken++;
    pthread_mutex_unlock(&context->mutex);
    return NULL;
}

static int test_cond_broadcast(void) {
    struct cond_context context = {
        .mutex = PTHREAD_MUTEX_INITIALIZER,
        .cond = PTHREAD_COND_INITIALIZER,
    };
    pthread_t threads[MUTEX_THREADS];
    for (size_t i = 0; i < ARRAY_SIZE(threads); i++) {
        int error = pthread_create(&threads[i], NULL, cond_worker, &context);
        if (error != 0) {
            return error;
        }
    }

    int error = pthread_mutex_lock(&context.mutex);
    if (error != 0) {
        return error;
    }
    while (context.ready != MUTEX_THREADS) {
        error = pthread_cond_wait(&context.cond, &context.mutex);
        if (error != 0) {
            pthread_mutex_unlock(&context.mutex);
            return error;
        }
    }
    context.release = 1;
    pthread_cond_broadcast(&context.cond);
    pthread_mutex_unlock(&context.mutex);

    for (size_t i = 0; i < ARRAY_SIZE(threads); i++) {
        void *result = NULL;
        error = pthread_join(threads[i], &result);
        if (error != 0) {
            return error;
        }
        if (result != NULL) {
            return (int)(intptr_t)result;
        }
    }
    pthread_cond_destroy(&context.cond);
    pthread_mutex_destroy(&context.mutex);
    return context.woken == MUTEX_THREADS ? 0 : EIO;
}

static int test_cond_timedwait(void) {
    pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
    struct timespec deadline;
    int error = clock_gettime(CLOCK_REALTIME, &deadline);
    if (error != 0) {
        return errno;
    }
    add_ms(&deadline, 100);
    error = pthread_mutex_lock(&mutex);
    if (error != 0) {
        return error;
    }
    int wait_error = pthread_cond_timedwait(&cond, &mutex, &deadline);
    int unlock_error = pthread_mutex_unlock(&mutex);
    pthread_cond_destroy(&cond);
    pthread_mutex_destroy(&mutex);
    if (unlock_error != 0) {
        return unlock_error;
    }
    return wait_error == ETIMEDOUT ? 0 : (wait_error != 0 ? wait_error : EIO);
}

static pthread_mutex_t signal_mutex = PTHREAD_MUTEX_INITIALIZER;
static atomic_int signal_waiter_started;
static atomic_int signal_count;

static void signal_handler(int signal_number) {
    (void)signal_number;
    atomic_fetch_add_explicit(&signal_count, 1, memory_order_relaxed);
}

static void *signal_waiter(void *unused) {
    (void)unused;
    atomic_store_explicit(&signal_waiter_started, 1, memory_order_release);
    int error = pthread_mutex_lock(&signal_mutex);
    if (error == 0) {
        pthread_mutex_unlock(&signal_mutex);
    }
    return (void *)(intptr_t)error;
}

static int test_signal_interrupted_wait(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = signal_handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        return errno;
    }
    atomic_store(&signal_waiter_started, 0);
    atomic_store(&signal_count, 0);
    int error = pthread_mutex_lock(&signal_mutex);
    if (error != 0) {
        return error;
    }
    pthread_t thread;
    error = pthread_create(&thread, NULL, signal_waiter, NULL);
    if (error != 0) {
        pthread_mutex_unlock(&signal_mutex);
        return error;
    }
    error = wait_for_flag(&signal_waiter_started, 1);
    if (error == 0) {
        error = pthread_kill(thread, SIGUSR1);
    }
    sleep_ms(20);
    pthread_mutex_unlock(&signal_mutex);
    void *result = NULL;
    int join_error = pthread_join(thread, &result);
    if (error != 0) {
        return error;
    }
    if (join_error != 0) {
        return join_error;
    }
    if (result != NULL) {
        return (int)(intptr_t)result;
    }
    return atomic_load(&signal_count) > 0 ? 0 : EIO;
}

static atomic_int termination_workers_ready;

static void *termination_worker(void *unused) {
    (void)unused;
    atomic_fetch_add_explicit(&termination_workers_ready, 1, memory_order_release);
    for (;;) {
        pause();
    }
    return NULL;
}

static int wait_for_child_exit(pid_t child, int *status) {
    struct timespec deadline;
    if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
        return errno;
    }
    add_ms(&deadline, WAIT_TIMEOUT_MS);
    for (;;) {
        pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) {
            return 0;
        }
        if (result < 0) {
            return errno;
        }
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
            return errno;
        }
        if (now.tv_sec > deadline.tv_sec ||
            (now.tv_sec == deadline.tv_sec && now.tv_nsec >= deadline.tv_nsec)) {
            return ETIMEDOUT;
        }
        sched_yield();
    }
}

static int test_default_sigterm_exits_thread_group(void) {
    int ready_pipe[2];
    if (pipe(ready_pipe) != 0) {
        return errno;
    }
    pid_t child = fork();
    if (child < 0) {
        int error = errno;
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        return error;
    }
    if (child == 0) {
        close(ready_pipe[0]);
        atomic_store(&termination_workers_ready, 0);
        pthread_t workers[MUTEX_THREADS];
        for (size_t i = 0; i < ARRAY_SIZE(workers); i++) {
            if (pthread_create(&workers[i], NULL, termination_worker, NULL) != 0) {
                _exit(2);
            }
        }
        if (wait_for_flag(&termination_workers_ready, MUTEX_THREADS) != 0 ||
            write(ready_pipe[1], "R", 1) != 1) {
            _exit(3);
        }
        for (;;) {
            pause();
        }
    }

    close(ready_pipe[1]);
    char ready = 0;
    ssize_t ready_size = read(ready_pipe[0], &ready, 1);
    int ready_error = ready_size < 0 ? errno : EIO;
    close(ready_pipe[0]);
    if (ready_size != 1 || ready != 'R') {
        kill(child, SIGKILL);
        waitpid(child, NULL, 0);
        return ready_error;
    }
    if (kill(child, SIGTERM) != 0) {
        int error = errno;
        kill(child, SIGKILL);
        waitpid(child, NULL, 0);
        return error;
    }

    int status = 0;
    int error = wait_for_child_exit(child, &status);
    if (error != 0) {
        kill(child, SIGKILL);
        waitpid(child, NULL, 0);
        return error;
    }
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGTERM) {
        return EIO;
    }
    return 0;
}

static pthread_mutex_t robust_mutex;

static void *robust_owner(void *unused) {
    (void)unused;
    return (void *)(intptr_t)pthread_mutex_lock(&robust_mutex);
}

static int test_robust_owner_exit(void) {
    pthread_mutexattr_t attr;
    int error = pthread_mutexattr_init(&attr);
    if (error != 0) {
        return error;
    }
    error = pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST);
    if (error == 0) {
        error = pthread_mutex_init(&robust_mutex, &attr);
    }
    pthread_mutexattr_destroy(&attr);
    if (error != 0) {
        return error;
    }
    pthread_t thread;
    error = pthread_create(&thread, NULL, robust_owner, NULL);
    if (error != 0) {
        pthread_mutex_destroy(&robust_mutex);
        return error;
    }
    void *result = NULL;
    error = pthread_join(thread, &result);
    if (error != 0) {
        return error;
    }
    if (result != NULL) {
        return (int)(intptr_t)result;
    }
    error = pthread_mutex_lock(&robust_mutex);
    if (error != EOWNERDEAD) {
        return error != 0 ? error : EIO;
    }
    error = pthread_mutex_consistent(&robust_mutex);
    if (error == 0) {
        error = pthread_mutex_unlock(&robust_mutex);
    }
    pthread_mutex_destroy(&robust_mutex);
    return error;
}

struct pi_waiter {
    int priority;
    atomic_int attempting;
    int released;
    int sched_error;
    int lock_error;
    int order;
};

struct pi_gate {
    pthread_mutex_t mutex;
    pthread_cond_t cond;
    int ready;
};

static pthread_mutex_t pi_mutex;
static atomic_int pi_order;
static struct pi_gate pi_gate;

static void *pi_waiter_thread(void *opaque) {
    struct pi_waiter *waiter = opaque;
    int error = pthread_mutex_lock(&pi_gate.mutex);
    if (error != 0) {
        waiter->lock_error = error;
        return NULL;
    }
    pi_gate.ready++;
    pthread_cond_broadcast(&pi_gate.cond);
    while (!waiter->released) {
        error = pthread_cond_wait(&pi_gate.cond, &pi_gate.mutex);
        if (error != 0) {
            pthread_mutex_unlock(&pi_gate.mutex);
            waiter->lock_error = error;
            return NULL;
        }
    }
    pthread_mutex_unlock(&pi_gate.mutex);

    atomic_store_explicit(&waiter->attempting, 1, memory_order_release);
    waiter->lock_error = pthread_mutex_lock(&pi_mutex);
    if (waiter->lock_error == 0) {
        waiter->order = atomic_fetch_add_explicit(&pi_order, 1, memory_order_relaxed) + 1;
        pthread_mutex_unlock(&pi_mutex);
    }
    return NULL;
}

static int init_pi_mutex(pthread_mutex_t *mutex) {
    pthread_mutexattr_t attr;
    int error = pthread_mutexattr_init(&attr);
    if (error != 0) {
        return error;
    }
    error = pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT);
    if (error == 0) {
        error = pthread_mutex_init(mutex, &attr);
    }
    pthread_mutexattr_destroy(&attr);
    return error;
}

static int test_pi_priority_handoff(void) {
    int error = init_pi_mutex(&pi_mutex);
    if (error != 0) {
        return error;
    }
    error = pthread_mutex_lock(&pi_mutex);
    if (error != 0) {
        return error;
    }
    atomic_store(&pi_order, 0);
    pi_gate = (struct pi_gate){
        .mutex = PTHREAD_MUTEX_INITIALIZER,
        .cond = PTHREAD_COND_INITIALIZER,
    };
    struct pi_waiter low = {.priority = 20};
    struct pi_waiter high = {.priority = 60};
    pthread_t low_thread;
    pthread_t high_thread;

    error = pthread_create(&low_thread, NULL, pi_waiter_thread, &low);
    if (error != 0) {
        pthread_mutex_unlock(&pi_mutex);
        pthread_mutex_destroy(&pi_mutex);
        return error;
    }
    error = pthread_create(&high_thread, NULL, pi_waiter_thread, &high);
    if (error != 0) {
        pthread_mutex_lock(&pi_gate.mutex);
        low.released = 1;
        pthread_cond_broadcast(&pi_gate.cond);
        pthread_mutex_unlock(&pi_gate.mutex);
        pthread_mutex_unlock(&pi_mutex);
        pthread_join(low_thread, NULL);
        pthread_mutex_destroy(&pi_mutex);
        return error;
    }

    error = pthread_mutex_lock(&pi_gate.mutex);
    while (error == 0 && pi_gate.ready != 2) {
        error = pthread_cond_wait(&pi_gate.cond, &pi_gate.mutex);
    }
    if (error == 0) {
        struct sched_param low_param = {.sched_priority = low.priority};
        struct sched_param high_param = {.sched_priority = high.priority};
        low.sched_error = pthread_setschedparam(low_thread, SCHED_FIFO, &low_param);
        high.sched_error = pthread_setschedparam(high_thread, SCHED_FIFO, &high_param);
        error = low.sched_error != 0 ? low.sched_error : high.sched_error;
    }
    if (error == 0) {
        low.released = 1;
        pthread_cond_broadcast(&pi_gate.cond);
    }
    pthread_mutex_unlock(&pi_gate.mutex);

    if (error == 0) {
        error = wait_for_flag(&low.attempting, 1);
    }
    sleep_ms(20);
    if (error == 0) {
        error = pthread_mutex_lock(&pi_gate.mutex);
        if (error == 0) {
            high.released = 1;
            pthread_cond_broadcast(&pi_gate.cond);
            pthread_mutex_unlock(&pi_gate.mutex);
        }
    }
    if (error == 0) {
        error = wait_for_flag(&high.attempting, 1);
    }
    sleep_ms(20);
    if (error != 0) {
        pthread_mutex_lock(&pi_gate.mutex);
        low.released = 1;
        high.released = 1;
        pthread_cond_broadcast(&pi_gate.cond);
        pthread_mutex_unlock(&pi_gate.mutex);
    }
    pthread_mutex_unlock(&pi_mutex);
    pthread_join(low_thread, NULL);
    pthread_join(high_thread, NULL);
    pthread_cond_destroy(&pi_gate.cond);
    pthread_mutex_destroy(&pi_gate.mutex);
    pthread_mutex_destroy(&pi_mutex);
    if (error != 0) {
        return error;
    }
    if (low.sched_error != 0 || high.sched_error != 0) {
        return low.sched_error != 0 ? low.sched_error : high.sched_error;
    }
    if (low.lock_error != 0 || high.lock_error != 0) {
        return low.lock_error != 0 ? low.lock_error : high.lock_error;
    }
    return high.order == 1 && low.order == 2 ? 0 : EIO;
}

struct timed_pi_result {
    atomic_int started;
    int error;
};

static void *timed_pi_waiter(void *opaque) {
    struct timed_pi_result *result = opaque;
    atomic_store_explicit(&result->started, 1, memory_order_release);
    struct timespec deadline;
    if (clock_gettime(CLOCK_REALTIME, &deadline) != 0) {
        result->error = errno;
        return NULL;
    }
    add_ms(&deadline, 100);
    result->error = pthread_mutex_timedlock(&pi_mutex, &deadline);
    if (result->error == 0) {
        pthread_mutex_unlock(&pi_mutex);
    }
    return NULL;
}

static int test_pi_timedlock(void) {
    int error = init_pi_mutex(&pi_mutex);
    if (error != 0) {
        return error;
    }
    error = pthread_mutex_lock(&pi_mutex);
    if (error != 0) {
        return error;
    }
    struct timed_pi_result result = {0};
    pthread_t thread;
    error = pthread_create(&thread, NULL, timed_pi_waiter, &result);
    if (error != 0) {
        pthread_mutex_unlock(&pi_mutex);
        return error;
    }
    error = wait_for_flag(&result.started, 1);
    if (error != 0) {
        pthread_mutex_unlock(&pi_mutex);
        pthread_join(thread, NULL);
        pthread_mutex_destroy(&pi_mutex);
        return error;
    }
    sleep_ms(200);
    pthread_mutex_unlock(&pi_mutex);
    pthread_join(thread, NULL);
    pthread_mutex_destroy(&pi_mutex);
    return result.error == ETIMEDOUT ? 0 : (result.error != 0 ? result.error : EIO);
}

struct rt_bandwidth_context {
    pthread_mutex_t mutex;
    pthread_cond_t cond;
    int ready;
    int release_hog;
    int release_observer;
    atomic_int hog_running;
    atomic_int observer_ran;
    atomic_int stop;
};

static struct rt_bandwidth_context rt_bandwidth;

static void *rt_bandwidth_hog(void *unused) {
    (void)unused;
    int error = pthread_mutex_lock(&rt_bandwidth.mutex);
    if (error != 0) {
        return (void *)(intptr_t)error;
    }
    rt_bandwidth.ready++;
    pthread_cond_broadcast(&rt_bandwidth.cond);
    while (!rt_bandwidth.release_hog) {
        error = pthread_cond_wait(&rt_bandwidth.cond, &rt_bandwidth.mutex);
        if (error != 0) {
            pthread_mutex_unlock(&rt_bandwidth.mutex);
            return (void *)(intptr_t)error;
        }
    }
    pthread_mutex_unlock(&rt_bandwidth.mutex);

    atomic_store_explicit(&rt_bandwidth.hog_running, 1, memory_order_release);
    while (!atomic_load_explicit(&rt_bandwidth.stop, memory_order_acquire)) {
        atomic_signal_fence(memory_order_seq_cst);
    }
    return NULL;
}

static void *rt_bandwidth_observer(void *unused) {
    (void)unused;
    int error = pthread_mutex_lock(&rt_bandwidth.mutex);
    if (error != 0) {
        return (void *)(intptr_t)error;
    }
    rt_bandwidth.ready++;
    pthread_cond_broadcast(&rt_bandwidth.cond);
    while (!rt_bandwidth.release_observer) {
        error = pthread_cond_wait(&rt_bandwidth.cond, &rt_bandwidth.mutex);
        if (error != 0) {
            pthread_mutex_unlock(&rt_bandwidth.mutex);
            return (void *)(intptr_t)error;
        }
    }
    pthread_mutex_unlock(&rt_bandwidth.mutex);

    atomic_store_explicit(&rt_bandwidth.observer_ran, 1, memory_order_release);
    atomic_store_explicit(&rt_bandwidth.stop, 1, memory_order_release);
    return NULL;
}

static int test_rt_bandwidth_recovery(void) {
    cpu_set_t original_affinity;
    int error = pthread_getaffinity_np(pthread_self(), sizeof(original_affinity),
                                       &original_affinity);
    if (error != 0) {
        return error;
    }
    int main_cpu = -1;
    int rt_cpu = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
        if (!CPU_ISSET(cpu, &original_affinity)) {
            continue;
        }
        if (main_cpu < 0) {
            main_cpu = cpu;
        } else {
            rt_cpu = cpu;
            break;
        }
    }
    if (rt_cpu < 0) {
        return ENOTSUP;
    }

    cpu_set_t main_affinity;
    CPU_ZERO(&main_affinity);
    CPU_SET(main_cpu, &main_affinity);
    error = pthread_setaffinity_np(pthread_self(), sizeof(main_affinity), &main_affinity);
    if (error != 0) {
        return error;
    }

    rt_bandwidth = (struct rt_bandwidth_context){
        .mutex = PTHREAD_MUTEX_INITIALIZER,
        .cond = PTHREAD_COND_INITIALIZER,
    };
    pthread_t hog_thread;
    pthread_t observer_thread;
    int hog_created = 0;
    int observer_created = 0;

    error = pthread_create(&hog_thread, NULL, rt_bandwidth_hog, NULL);
    if (error == 0) {
        hog_created = 1;
        error = pthread_create(&observer_thread, NULL, rt_bandwidth_observer, NULL);
    }
    if (error == 0) {
        observer_created = 1;
        error = pthread_mutex_lock(&rt_bandwidth.mutex);
    }
    while (error == 0 && rt_bandwidth.ready != 2) {
        error = pthread_cond_wait(&rt_bandwidth.cond, &rt_bandwidth.mutex);
    }
    if (error == 0) {
        cpu_set_t target_affinity;
        CPU_ZERO(&target_affinity);
        CPU_SET(rt_cpu, &target_affinity);
        error = pthread_setaffinity_np(hog_thread, sizeof(target_affinity), &target_affinity);
        if (error == 0) {
            error = pthread_setaffinity_np(observer_thread, sizeof(target_affinity),
                                           &target_affinity);
        }
    }
    if (error == 0) {
        struct sched_param param = {.sched_priority = 50};
        error = pthread_setschedparam(hog_thread, SCHED_FIFO, &param);
    }
    if (error == 0) {
        rt_bandwidth.release_hog = 1;
        pthread_cond_broadcast(&rt_bandwidth.cond);
    }
    if (observer_created) {
        pthread_mutex_unlock(&rt_bandwidth.mutex);
    }

    if (error == 0) {
        error = wait_for_flag(&rt_bandwidth.hog_running, 1);
    }
    if (error == 0) {
        error = pthread_mutex_lock(&rt_bandwidth.mutex);
        if (error == 0) {
            rt_bandwidth.release_observer = 1;
            pthread_cond_broadcast(&rt_bandwidth.cond);
            pthread_mutex_unlock(&rt_bandwidth.mutex);
        }
    }
    if (error == 0) {
        error = wait_for_flag(&rt_bandwidth.observer_ran, 1);
    }

    atomic_store_explicit(&rt_bandwidth.stop, 1, memory_order_release);
    if (hog_created || observer_created) {
        pthread_mutex_lock(&rt_bandwidth.mutex);
        rt_bandwidth.release_hog = 1;
        rt_bandwidth.release_observer = 1;
        pthread_cond_broadcast(&rt_bandwidth.cond);
        pthread_mutex_unlock(&rt_bandwidth.mutex);
    }
    if (hog_created) {
        void *result = NULL;
        int join_error = pthread_join(hog_thread, &result);
        if (error == 0 && join_error != 0) {
            error = join_error;
        } else if (error == 0 && result != NULL) {
            error = (int)(intptr_t)result;
        }
    }
    if (observer_created) {
        void *result = NULL;
        int join_error = pthread_join(observer_thread, &result);
        if (error == 0 && join_error != 0) {
            error = join_error;
        } else if (error == 0 && result != NULL) {
            error = (int)(intptr_t)result;
        }
    }
    int restore_error =
        pthread_setaffinity_np(pthread_self(), sizeof(original_affinity), &original_affinity);
    pthread_cond_destroy(&rt_bandwidth.cond);
    pthread_mutex_destroy(&rt_bandwidth.mutex);
    return error != 0 ? error : restore_error;
}

struct test_case {
    const char *name;
    int (*run)(void);
};

int main(void) {
    static const struct test_case tests[] = {
        {"pthread mutex contention", test_mutex_contention},
        {"pthread condvar broadcast", test_cond_broadcast},
        {"pthread condvar timed wait", test_cond_timedwait},
        {"pthread wait signal restart", test_signal_interrupted_wait},
        {"pthread default SIGTERM exits thread group", test_default_sigterm_exits_thread_group},
        {"pthread robust owner exit", test_robust_owner_exit},
        {"pthread PI timed lock", test_pi_timedlock},
        {"pthread PI priority handoff", test_pi_priority_handoff},
        {"pthread RT bandwidth recovery", test_rt_bandwidth_recovery},
    };

    setvbuf(stdout, NULL, _IONBF, 0);
    printf("TAP version 14\n");
    printf("1..%zu\n", ARRAY_SIZE(tests));
    int failures = 0;
    for (size_t i = 0; i < ARRAY_SIZE(tests); i++) {
        int error = tests[i].run();
        if (error == 0) {
            printf("ok %zu - %s\n", i + 1, tests[i].name);
        } else {
            printf("not ok %zu - %s # error=%d %s\n", i + 1, tests[i].name, error,
                   strerror(error));
            failures++;
        }
    }
    return failures == 0 ? 0 : 1;
}
