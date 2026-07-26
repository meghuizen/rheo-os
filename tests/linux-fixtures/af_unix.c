/* af_unix.c - the rheo-net Phase N1d / Linux-personality L8 AF_UNIX proof
 * fixture (docs/LINUX-COMPAT.md L8). An unmodified static-glibc C program that
 * exercises Unix domain sockets two ways:
 *
 *   Part 1: socketpair(AF_UNIX, SOCK_STREAM) + fork - parent and child (two
 *           cells after fork) send + recv a known message in both directions.
 *   Part 2: socket/bind/listen/connect/accept over an *abstract* name, all in
 *           one process (a loopback), then send + recv in both directions.
 *
 * Deterministic stdout + exit 0 are asserted by the `linuxunix` test kernel on
 * all three ISAs. Built from source by xtask (static glibc, ET_EXEC, no relink),
 * never committed - mirrors hello.c / cecho.c.
 *
 * Expected stdout:
 *   pair: pong
 *   conn: hello
 *   back: world
 *   af_unix OK
 */

#include <stddef.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }

int main(void) {
    /* ---- Part 1: socketpair + fork ---- */
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        outs("socketpair FAIL\n");
        return 1;
    }
    pid_t pid = fork();
    if (pid == 0) {
        /* child: receive "ping", reply "pong" */
        close(sv[0]);
        char b[16];
        int n = read(sv[1], b, sizeof b);
        if (n != 4 || memcmp(b, "ping", 4) != 0) {
            outs("child recv FAIL\n");
            _exit(1);
        }
        write(sv[1], "pong", 4);
        close(sv[1]);
        _exit(0);
    }
    close(sv[1]);
    write(sv[0], "ping", 4);
    char b[16];
    int n = read(sv[0], b, sizeof b);
    outs("pair: ");
    if (n > 0) write(1, b, n);
    outs("\n");
    int st;
    waitpid(pid, &st, 0);

    /* ---- Part 2: bind/listen/connect/accept over an abstract name ---- */
    int ls = socket(AF_UNIX, SOCK_STREAM, 0);
    if (ls < 0) {
        outs("socket FAIL\n");
        return 2;
    }
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    const char *nm = "rheo-unix";
    a.sun_path[0] = 0; /* abstract namespace */
    memcpy(a.sun_path + 1, nm, strlen(nm));
    socklen_t alen = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(nm);
    if (bind(ls, (struct sockaddr *)&a, alen) != 0) {
        outs("bind FAIL\n");
        return 3;
    }
    if (listen(ls, 4) != 0) {
        outs("listen FAIL\n");
        return 4;
    }
    int cs = socket(AF_UNIX, SOCK_STREAM, 0);
    if (connect(cs, (struct sockaddr *)&a, alen) != 0) {
        outs("connect FAIL\n");
        return 5;
    }
    write(cs, "hello", 5);
    int as = accept(ls, NULL, NULL);
    if (as < 0) {
        outs("accept FAIL\n");
        return 6;
    }
    char c[16];
    int m = read(as, c, sizeof c);
    outs("conn: ");
    if (m > 0) write(1, c, m);
    outs("\n");
    write(as, "world", 5);
    int k = read(cs, c, sizeof c);
    outs("back: ");
    if (k > 0) write(1, c, k);
    outs("\n");

    close(as);
    close(cs);
    close(ls);
    outs("af_unix OK\n");
    return 0;
}
