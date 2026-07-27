/* pollx.c - does `poll` tell the truth, and does a sleep actually sleep?
 * (docs/ARCHITECTURE-DEBT.md 2.4, docs/LINUX-COMPAT.md the poll/epoll/nanosleep rows)
 *
 * An **unmodified static-glibc C program**. Before this slice the personality's
 * `poll` did not consult readiness **at all**: every open descriptor was reported
 * ready for whatever was asked, a closed one POLLNVAL, and the timeout was
 * ignored. `epoll_wait` computed readiness but never waited, and `nanosleep`
 * returned 0 immediately - a sleep that never slept. Each phase below fails
 * against that behaviour, and the failure is named with the phase.
 *
 * Phases (all deterministic, network-free):
 *
 *   1. **An empty pipe is not readable.** `poll(read end, POLLIN, 0)` must report
 *      0 ready and revents 0. Pre-fix this reported 1/POLLIN - the assertion that
 *      catches the old behaviour directly.
 *   2. **A pipe with a byte is readable**, and the write end is writable. So the
 *      answer tracks the descriptor's state rather than being a constant.
 *   3. **A closed descriptor is POLLNVAL** (this part was already right; asserted
 *      so the fix cannot regress it).
 *   4. **A timeout actually elapses.** `poll(empty pipe, POLLIN, 60 ms)` returns 0
 *      **and** CLOCK_MONOTONIC advanced by at least 40 ms across the call. Pre-fix
 *      it returned 1 immediately.
 *   5. **A wake arrives from another process.** fork(); the child sleeps then
 *      writes one byte; the parent `poll`s with an **indefinite** timeout and must
 *      be woken by that write. This is the cross-process readiness path.
 *   6. **`nanosleep` sleeps.** 40 ms requested, at least 30 ms observed on the
 *      program's own clock.
 *   7. **Creation-time O_NONBLOCK is honoured.** `pipe2(O_NONBLOCK)`: a read of the
 *      empty end is -1/EAGAIN with no `fcntl` call at all. This could not work
 *      before a readiness-computing, waiting `poll` (see the note in
 *      kernel/src/linux/fd.rs `fcntl`).
 *   8. **`epoll_wait` honours its timeout.** Nothing ready, 60 ms: 0 returned and
 *      at least 40 ms observed. Pre-fix it returned 0 immediately, which turns
 *      every epoll loop into a spin.
 *
 * Slack: the lower bounds (40 of 60, 30 of 40) leave room for the scheduler's
 * 1 ms deadline slice and for the emulator's clock granularity, while staying far
 * above the ~0 ms a non-waiting implementation produces.
 *
 * Built from source by xtask (static glibc, ET_EXEC, no relink), never committed.
 *
 * Expected stdout:
 *   poll: empty not ready
 *   poll: data ready
 *   poll: writable
 *   poll: closed NVAL
 *   poll: timeout elapsed
 *   poll: peer woke us
 *   nanosleep: slept
 *   nonblock: pipe2 EAGAIN
 *   epoll: timeout elapsed
 *   pollx OK
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }
static int fail(const char *s) {
    outs(s);
    outs("\n");
    return 1;
}

/* CLOCK_MONOTONIC in milliseconds - the program's own clock, which is the domain
 * the kernel compares its deadlines in. */
static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

int main(void) {
    int p[2];
    if (pipe(p) != 0) return fail("pollx: pipe FAIL");

    /* 1. An empty pipe is not readable. */
    struct pollfd pf = {p[0], POLLIN, 0};
    int n = poll(&pf, 1, 0);
    if (n != 0) return fail("poll: empty pipe reported ready FAIL");
    if (pf.revents != 0) return fail("poll: empty pipe revents FAIL");
    outs("poll: empty not ready\n");

    /* 2. With a byte queued it is readable; the write end is writable. */
    if (write(p[1], "x", 1) != 1) return fail("pollx: write FAIL");
    pf.revents = 0;
    if (poll(&pf, 1, 0) != 1) return fail("poll: data not reported ready FAIL");
    if (!(pf.revents & POLLIN)) return fail("poll: data revents FAIL");
    outs("poll: data ready\n");

    struct pollfd pw = {p[1], POLLOUT, 0};
    if (poll(&pw, 1, 0) != 1 || !(pw.revents & POLLOUT))
        return fail("poll: write end not writable FAIL");
    outs("poll: writable\n");

    /* Drain, so the pipe is empty again. */
    char b;
    if (read(p[0], &b, 1) != 1 || b != 'x') return fail("pollx: read FAIL");

    /* 3. A closed descriptor is POLLNVAL. */
    int dead[2];
    if (pipe(dead) != 0) return fail("pollx: pipe2 FAIL");
    close(dead[0]);
    close(dead[1]);
    struct pollfd pd = {dead[0], POLLIN, 0};
    if (poll(&pd, 1, 0) != 1 || !(pd.revents & POLLNVAL))
        return fail("poll: closed fd not POLLNVAL FAIL");
    outs("poll: closed NVAL\n");

    /* 4. The timeout elapses. */
    long t0 = now_ms();
    pf.revents = 0;
    n = poll(&pf, 1, 60);
    long dt = now_ms() - t0;
    if (n != 0) return fail("poll: timeout returned ready FAIL");
    if (dt < 40) return fail("poll: timeout did not elapse FAIL");
    outs("poll: timeout elapsed\n");

    /* 5. A wake from another process. The child sleeps first, so the parent is
     * genuinely parked in poll when the byte arrives. */
    pid_t kid = fork();
    if (kid < 0) return fail("pollx: fork FAIL");
    if (kid == 0) {
        struct timespec s = {0, 20 * 1000 * 1000};
        nanosleep(&s, 0);
        write(p[1], "y", 1);
        _exit(0);
    }
    pf.revents = 0;
    n = poll(&pf, 1, -1);
    if (n != 1 || !(pf.revents & POLLIN)) return fail("poll: peer wake FAIL");
    if (read(p[0], &b, 1) != 1 || b != 'y') return fail("pollx: peer byte FAIL");
    int st = 0;
    if (waitpid(kid, &st, 0) != kid) return fail("pollx: waitpid FAIL");
    outs("poll: peer woke us\n");

    /* 6. nanosleep really sleeps. */
    t0 = now_ms();
    struct timespec req = {0, 40 * 1000 * 1000};
    if (nanosleep(&req, 0) != 0) return fail("nanosleep: returned error FAIL");
    dt = now_ms() - t0;
    if (dt < 30) return fail("nanosleep: did not sleep FAIL");
    outs("nanosleep: slept\n");

    /* 7. Creation-time O_NONBLOCK, no fcntl involved. */
    int q[2];
    if (pipe2(q, O_NONBLOCK) != 0) return fail("pollx: pipe2 O_NONBLOCK FAIL");
    if (read(q[0], &b, 1) != -1) return fail("nonblock: pipe2 read returned data FAIL");
    if (errno != EAGAIN) return fail("nonblock: pipe2 errno FAIL");
    outs("nonblock: pipe2 EAGAIN\n");

    /* 8. epoll_wait honours its timeout. */
    int ep = epoll_create1(0);
    if (ep < 0) return fail("pollx: epoll_create1 FAIL");
    struct epoll_event ev;
    ev.events = EPOLLIN;
    ev.data.u64 = 0x1234;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev) != 0) return fail("pollx: epoll_ctl FAIL");
    t0 = now_ms();
    struct epoll_event out[4];
    n = epoll_wait(ep, out, 4, 60);
    dt = now_ms() - t0;
    if (n != 0) return fail("epoll: timeout returned an event FAIL");
    if (dt < 40) return fail("epoll: timeout did not elapse FAIL");
    outs("epoll: timeout elapsed\n");

    outs("pollx OK\n");
    return 0;
}
