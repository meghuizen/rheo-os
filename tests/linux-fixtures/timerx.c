// timerfd proof (docs/LINUX-COMPAT.md L8-TIMERFD, GOAL-TIMERFD): the two ways a
// program uses a timerfd - a blocking read that parks on the deadline, and the
// libuv shape (add to epoll, epoll_wait wakes on expiry, read the count). A
// one-shot fires exactly once, so the expiration count is deterministically 1 and
// the disarmed timer reads zero - no wall-clock value is asserted.
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <sys/timerfd.h>
#include <sys/epoll.h>

static void arm_oneshot(int fd, long ns) {
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_nsec = ns;
    timerfd_settime(fd, 0, &its, NULL);
}

int main(void) {
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    if (tfd < 0) {
        printf("timerx: create failed\n");
        return 1;
    }

    // Phase 1: a blocking read must park on the deadline and return one expiration.
    arm_oneshot(tfd, 20 * 1000 * 1000); // 20 ms
    uint64_t exp = 0;
    ssize_t r = read(tfd, &exp, sizeof exp);
    printf("timerx: blocking r=%zd exp=%llu\n", r, (unsigned long long)exp);

    // Phase 2: the libuv shape - epoll_wait wakes when the timer fires.
    int ep = epoll_create1(0);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = tfd;
    epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev);
    arm_oneshot(tfd, 20 * 1000 * 1000);
    struct epoll_event out[4];
    int n = epoll_wait(ep, out, 4, -1);
    exp = 0;
    r = read(tfd, &exp, sizeof exp);
    printf("timerx: epoll n=%d exp=%llu\n", n, (unsigned long long)exp);

    // gettime on the now-disarmed one-shot reads zero.
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    timerfd_gettime(tfd, &cur);
    long long left = (long long)cur.it_value.tv_sec * 1000000000LL + cur.it_value.tv_nsec;
    printf("timerx: disarmed val=%lld\n", left);

    printf("timerx OK\n");
    return 0;
}
