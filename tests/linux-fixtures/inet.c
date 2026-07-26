/* inet.c - the rheo-net L8-INET / Linux-personality proof fixture
 * (docs/LINUX-COMPAT.md L8-INET). An unmodified static-glibc C program that
 * exercises AF_INET / AF_INET6 sockets over the **loopback** interface
 * (127.0.0.1 / ::1), all deterministic and network-free:
 *
 *   1. TCP over 127.0.0.1: socket/bind/listen/accept a server + socket/connect
 *      a client, exchange "hello"/"world" both directions (single process).
 *   2. epoll: watch the client socket for EPOLLIN; epoll_wait reports it ready
 *      once the server has written.
 *   3. UDP over 127.0.0.1: sendto a datagram, recvfrom it on the bound server.
 *   4. TCP over ::1 (AF_INET6): the same connect/accept/exchange, proving the
 *      sockaddr_in6 path.
 *
 * Deterministic stdout + exit 0 are asserted by the `linuxinet` test kernel on
 * all three ISAs. Built from source by xtask (static glibc, ET_EXEC, no relink),
 * never committed - mirrors af_unix.c / hello.c.
 *
 * Expected stdout:
 *   tcp4: hello
 *   epoll: ready
 *   tcp4: world
 *   udp4: ping
 *   tcp6: hi
 *   inet OK
 */

#include <arpa/inet.h>
#include <netinet/in.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }
static int fail(const char *s) {
    outs(s);
    outs("\n");
    return 1;
}

/* A fixed loopback port pair (arbitrary, high). */
#define TCP4_PORT 8080
#define UDP4_PORT 9090
#define TCP6_PORT 8086

int main(void) {
    char b[64];
    int n;

    /* ---- 1. TCP over 127.0.0.1 ---- */
    int ls = socket(AF_INET, SOCK_STREAM, 0);
    if (ls < 0) return fail("socket4 FAIL");
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET;
    sa.sin_port = htons(TCP4_PORT);
    sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(ls, (struct sockaddr *)&sa, sizeof sa) != 0) return fail("bind4 FAIL");
    if (listen(ls, 4) != 0) return fail("listen4 FAIL");

    int cs = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(cs, (struct sockaddr *)&sa, sizeof sa) != 0) return fail("connect4 FAIL");
    if (write(cs, "hello", 5) != 5) return fail("write4 FAIL");

    int as = accept(ls, NULL, NULL);
    if (as < 0) return fail("accept4 FAIL");
    n = read(as, b, sizeof b);
    outs("tcp4: ");
    if (n > 0) write(1, b, n);
    outs("\n");

    /* server -> client, then prove epoll readiness before reading it back. */
    if (write(as, "world", 5) != 5) return fail("write4b FAIL");

    int ep = epoll_create1(0);
    if (ep < 0) return fail("epoll_create FAIL");
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = cs;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, cs, &ev) != 0) return fail("epoll_ctl FAIL");
    struct epoll_event out[4];
    int r = epoll_wait(ep, out, 4, 0);
    if (r == 1 && (out[0].events & EPOLLIN) && out[0].data.fd == cs)
        outs("epoll: ready\n");
    else
        return fail("epoll FAIL");
    close(ep);

    n = read(cs, b, sizeof b);
    outs("tcp4: ");
    if (n > 0) write(1, b, n);
    outs("\n");
    close(as);
    close(cs);
    close(ls);

    /* ---- 3. UDP over 127.0.0.1 ---- */
    int us = socket(AF_INET, SOCK_DGRAM, 0);
    if (us < 0) return fail("socketu FAIL");
    struct sockaddr_in ua;
    memset(&ua, 0, sizeof ua);
    ua.sin_family = AF_INET;
    ua.sin_port = htons(UDP4_PORT);
    ua.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(us, (struct sockaddr *)&ua, sizeof ua) != 0) return fail("bindu FAIL");

    int uc = socket(AF_INET, SOCK_DGRAM, 0);
    if (sendto(uc, "ping", 4, 0, (struct sockaddr *)&ua, sizeof ua) != 4)
        return fail("sendto FAIL");
    struct sockaddr_in from;
    socklen_t flen = sizeof from;
    n = recvfrom(us, b, sizeof b, 0, (struct sockaddr *)&from, &flen);
    outs("udp4: ");
    if (n > 0) write(1, b, n);
    outs("\n");
    close(uc);
    close(us);

    /* ---- 4. TCP over ::1 (AF_INET6) ---- */
    int ls6 = socket(AF_INET6, SOCK_STREAM, 0);
    if (ls6 < 0) return fail("socket6 FAIL");
    struct sockaddr_in6 sa6;
    memset(&sa6, 0, sizeof sa6);
    sa6.sin6_family = AF_INET6;
    sa6.sin6_port = htons(TCP6_PORT);
    sa6.sin6_addr = in6addr_loopback;
    if (bind(ls6, (struct sockaddr *)&sa6, sizeof sa6) != 0) return fail("bind6 FAIL");
    if (listen(ls6, 4) != 0) return fail("listen6 FAIL");

    int cs6 = socket(AF_INET6, SOCK_STREAM, 0);
    if (connect(cs6, (struct sockaddr *)&sa6, sizeof sa6) != 0) return fail("connect6 FAIL");
    if (write(cs6, "hi", 2) != 2) return fail("write6 FAIL");
    int as6 = accept(ls6, NULL, NULL);
    if (as6 < 0) return fail("accept6 FAIL");
    n = read(as6, b, sizeof b);
    outs("tcp6: ");
    if (n > 0) write(1, b, n);
    outs("\n");
    close(as6);
    close(cs6);
    close(ls6);

    outs("inet OK\n");
    return 0;
}
