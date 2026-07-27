// A libuv-style event loop: one epoll set multiplexing the three wake sources a
// real loop uses at once - a periodic timerfd (the timer heartbeat), an eventfd
// (async self-wakeup, how the loop wakes itself), and a pipe (I/O readiness). This
// is the shape of libuv's uv__io_poll, and thus of Node.js's core loop
// (docs/LINUX-COMPAT.md L8-TIMERFD). Each mechanism was proven alone (timerx,
// eventfd, pollx); this proves they compose - epoll_wait blocking on a mixed
// TIMER+PEER source set, each fd read on readiness.
//
// Deterministic: the loop runs until it has observed the eventfd wake, the pipe
// read, and at least 3 timer expirations, then exits. Per-iteration counts vary
// with scheduling, so only those three milestones (never a timing value) are
// printed.
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <sys/timerfd.h>
#include <sys/eventfd.h>
#include <sys/epoll.h>

int main(void) {
    int ep = epoll_create1(0);

    int efd = eventfd(0, EFD_NONBLOCK);
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    int pfd[2];
    if (pipe(pfd) != 0) {
        printf("uvloop: pipe failed\n");
        return 1;
    }

    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = efd;
    epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev);
    ev.data.fd = tfd;
    epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev);
    ev.data.fd = pfd[0];
    epoll_ctl(ep, EPOLL_CTL_ADD, pfd[0], &ev);

    // Arm a 5 ms periodic timer - the loop's heartbeat.
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_nsec = 5 * 1000 * 1000;
    its.it_interval.tv_nsec = 5 * 1000 * 1000;
    timerfd_settime(tfd, 0, &its, NULL);

    // Drive one async wakeup and one pipe I/O from within the process; a real loop
    // gets these from other threads/handles, but the epoll path is identical.
    uint64_t one = 1;
    write(efd, &one, sizeof one);
    write(pfd[1], "hi", 2);

    int saw_event = 0, saw_pipe = 0;
    uint64_t ticks = 0;
    struct epoll_event out[8];
    while (!(saw_event && saw_pipe && ticks >= 3)) {
        int n = epoll_wait(ep, out, 8, -1);
        for (int i = 0; i < n; i++) {
            int fd = out[i].data.fd;
            if (fd == efd) {
                uint64_t v = 0;
                read(efd, &v, sizeof v);
                if (!saw_event) {
                    saw_event = 1;
                    printf("uvloop: eventfd woke\n");
                }
            } else if (fd == tfd) {
                uint64_t exp = 0;
                read(tfd, &exp, sizeof exp);
                ticks += exp;
            } else if (fd == pfd[0]) {
                char b[8];
                ssize_t r = read(pfd[0], b, sizeof b);
                if (!saw_pipe && r == 2 && b[0] == 'h' && b[1] == 'i') {
                    saw_pipe = 1;
                    printf("uvloop: pipe got hi\n");
                }
            }
        }
    }
    printf("uvloop: 3 ticks\n");
    printf("uvloop OK\n");
    return 0;
}
