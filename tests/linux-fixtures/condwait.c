/* condwait.c - does a futex timeout actually time out?
 * (docs/LINUX-COMPAT.md L4, the `futex` row.)
 *
 * An **unmodified static-glibc C program** whose whole point is a wait that must
 * END BY ITSELF. It calls `pthread_cond_timedwait` on a condition variable that
 * **nobody ever signals**, with no other thread in the process. glibc turns that
 * into `futex(FUTEX_WAIT_BITSET, ..., abstime)`.
 *
 * Before this fixture the personality ignored the timeout argument entirely and
 * treated every wait as infinite: with no sibling context runnable the kernel
 * returned 0 - "you were woken" - and glibc looped forever re-checking a word
 * nothing would ever change. A program that asked to wait 50 ms hung, and nothing
 * in the transcript pointed at the futex.
 *
 * Two waits, so both clock shapes are covered:
 *
 *   1. the **default** condition variable, whose deadline is in CLOCK_REALTIME
 *      (glibc sets FUTEX_CLOCK_REALTIME in the futex op);
 *   2. one with `pthread_condattr_setclock(CLOCK_MONOTONIC)`, the shape most
 *      modern code uses.
 *
 * Each must return ETIMEDOUT, and the program measures its own elapsed time with
 * `clock_gettime` on the same clock it asked to wait on - so the assertion
 * "at least as long as I asked for" is made in one consistent clock domain
 * (docs/ENGINEERING.md 11). The elapsed value itself is never printed: it is not
 * deterministic. What is printed is that it was **not short**.
 *
 * Built from source by xtask (static glibc, ET_EXEC, no relink), never committed.
 *
 * Expected stdout:
 *   condwait: realtime timedout
 *   condwait: monotonic timedout
 *   condwait OK
 */

#include <errno.h>
#include <pthread.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }
static int fail(const char *s) {
    outs(s);
    outs("\n");
    return 1;
}

/* How long each wait asks for. Small enough to keep the boot test quick, large
 * enough to be far above any measurement granularity. */
#define WAIT_NS 50000000L /* 50 ms */

static long long ns_of(const struct timespec *t) {
    return (long long)t->tv_sec * 1000000000LL + t->tv_nsec;
}

/* Wait on a never-signalled condvar for WAIT_NS on `clk`; returns 0 on success. */
static int timed_wait(clockid_t clk, int monotonic, const char *label) {
    pthread_condattr_t attr;
    pthread_cond_t cond;
    pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;

    if (monotonic) {
        if (pthread_condattr_init(&attr) != 0) return fail("condwait: condattr FAIL");
        if (pthread_condattr_setclock(&attr, clk) != 0) return fail("condwait: setclock FAIL");
        if (pthread_cond_init(&cond, &attr) != 0) return fail("condwait: cond_init FAIL");
    } else {
        if (pthread_cond_init(&cond, NULL) != 0) return fail("condwait: cond_init FAIL");
    }

    struct timespec t0, t1, deadline;
    if (clock_gettime(clk, &t0) != 0) return fail("condwait: clock_gettime FAIL");
    long long target = ns_of(&t0) + WAIT_NS;
    deadline.tv_sec = (time_t)(target / 1000000000LL);
    deadline.tv_nsec = (long)(target % 1000000000LL);

    if (pthread_mutex_lock(&mtx) != 0) return fail("condwait: lock FAIL");
    int rc = pthread_cond_timedwait(&cond, &mtx, &deadline);
    pthread_mutex_unlock(&mtx);

    if (clock_gettime(clk, &t1) != 0) return fail("condwait: clock_gettime2 FAIL");
    if (rc != ETIMEDOUT) return fail("condwait: not ETIMEDOUT FAIL");
    if (ns_of(&t1) - ns_of(&t0) < WAIT_NS) return fail("condwait: returned too early FAIL");

    outs("condwait: ");
    outs(label);
    outs(" timedout\n");
    return 0;
}

int main(void) {
    if (timed_wait(CLOCK_REALTIME, 0, "realtime") != 0) return 1;
    if (timed_wait(CLOCK_MONOTONIC, 1, "monotonic") != 0) return 1;
    outs("condwait OK\n");
    return 0;
}
