/* fcntlx.c - did `fcntl` tell the truth? (docs/LINUX-COMPAT.md, the `fcntl` row).
 *
 * An **unmodified static-glibc C program** asserting the four things `fcntl` used
 * to get wrong. Before this fixture the personality's `fcntl` ended in `_ => 0`:
 * every command it did not implement reported **success while doing nothing**, so
 * a feature probe asking "can you lock this file?" was told yes; `F_SETFL` never
 * looked at its argument (`O_NONBLOCK` accepted and dropped); `F_GETFL` returned
 * a literal `O_RDWR`; and `FD_CLOEXEC` was not tracked at all, so `execve` kept
 * every descriptor.
 *
 * Phases (all deterministic, network-free):
 *
 *   1. An **unimplemented** command fails. `F_SETLK` (there is no lock manager)
 *      reports ENOLCK, and a nonsense command number reports EINVAL - two
 *      distinct errors, not one vague one (docs/ENGINEERING.md 6).
 *   2. `F_SETFL(O_NONBLOCK)` is honoured. On an **empty pipe** a read returns
 *      -1/EAGAIN; clearing the flag and reading again after a byte is written
 *      returns that byte, so the flag is what changed the behaviour. On **stdin**
 *      an empty console returns -1/EAGAIN where it used to return 0, which a
 *      caller reads as end-of-input.
 *   3. `F_GETFL` reflects reality: the access mode the descriptor was really
 *      opened with, plus O_NONBLOCK exactly while it is set.
 *   4. `FD_CLOEXEC` is honoured by `execve`: this program marks one pipe-read
 *      descriptor close-on-exec, leaves a second one alone, and `execve`s itself
 *      with `child <cloexec-fd> <plain-fd>`. The child asserts the first is
 *      **gone** (EBADF) and the second **survived**.
 *
 * Built from source by xtask (static glibc, ET_EXEC, no relink), never committed.
 *
 * Expected stdout:
 *   fcntl: setlk ENOLCK
 *   fcntl: badcmd EINVAL
 *   fcntl: nonblock EAGAIN
 *   fcntl: stdin EAGAIN
 *   fcntl: getfl ok
 *   fcntl: blocking read ok
 *   fcntl: exec child
 *   fcntl: cloexec closed
 *   fcntl: plain survived
 *   fcntl OK
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }
static int fail(const char *s) {
    outs(s);
    outs("\n");
    return 1;
}

/* The path this program is seeded at, so it can `execve` itself. */
#define SELF "/bin/fcntlx"

/* A command number no Linux fcntl defines. */
#define F_BOGUS 4242

static int child(int cloexec_fd, int plain_fd) {
    outs("fcntl: exec child\n");
    errno = 0;
    if (fcntl(cloexec_fd, F_GETFD) != -1 || errno != EBADF)
        return fail("fcntl: cloexec fd survived execve FAIL");
    outs("fcntl: cloexec closed\n");
    errno = 0;
    if (fcntl(plain_fd, F_GETFD) == -1) return fail("fcntl: plain fd lost across execve FAIL");
    outs("fcntl: plain survived\n");
    outs("fcntl OK\n");
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "child") == 0) return child(atoi(argv[2]), atoi(argv[3]));

    int p[2];
    if (pipe(p) != 0) return fail("fcntl: pipe FAIL");
    char b[8];

    /* ---- 1. unimplemented commands fail, each with its own errno ---- */
    struct flock fl;
    memset(&fl, 0, sizeof fl);
    fl.l_type = F_WRLCK;
    fl.l_whence = SEEK_SET;
    errno = 0;
    if (fcntl(p[0], F_SETLK, &fl) != -1) return fail("fcntl: F_SETLK reported success FAIL");
    if (errno != ENOLCK) return fail("fcntl: F_SETLK errno FAIL");
    outs("fcntl: setlk ENOLCK\n");

    errno = 0;
    if (fcntl(p[0], F_BOGUS, 0L) != -1) return fail("fcntl: unknown cmd reported success FAIL");
    if (errno != EINVAL) return fail("fcntl: unknown cmd errno FAIL");
    outs("fcntl: badcmd EINVAL\n");

    /* ---- 2. O_NONBLOCK is honoured ---- */
    if (fcntl(p[0], F_SETFL, O_NONBLOCK) != 0) return fail("fcntl: F_SETFL FAIL");
    errno = 0;
    if (read(p[0], b, sizeof b) != -1) return fail("fcntl: nonblocking read returned data FAIL");
    if (errno != EAGAIN) return fail("fcntl: nonblocking read errno FAIL");
    outs("fcntl: nonblock EAGAIN\n");

    /* stdin: an empty console used to answer 0, i.e. "end of input". */
    if (fcntl(0, F_SETFL, O_NONBLOCK) != 0) return fail("fcntl: F_SETFL stdin FAIL");
    errno = 0;
    if (read(0, b, 1) != -1) return fail("fcntl: nonblocking stdin returned data FAIL");
    if (errno != EAGAIN) return fail("fcntl: nonblocking stdin errno FAIL");
    if (fcntl(0, F_SETFL, 0) != 0) return fail("fcntl: F_SETFL stdin clear FAIL");
    outs("fcntl: stdin EAGAIN\n");

    /* ---- 3. F_GETFL reports the access mode + the flag we just set ---- */
    int fl0 = fcntl(p[0], F_GETFL);
    if (fl0 < 0) return fail("fcntl: F_GETFL FAIL");
    if ((fl0 & O_ACCMODE) != O_RDONLY) return fail("fcntl: F_GETFL accmode FAIL");
    if (!(fl0 & O_NONBLOCK)) return fail("fcntl: F_GETFL missing O_NONBLOCK FAIL");
    int fl1 = fcntl(p[1], F_GETFL);
    if (fl1 < 0 || (fl1 & O_ACCMODE) != O_WRONLY)
        return fail("fcntl: F_GETFL write-end accmode FAIL");
    if (fl1 & O_NONBLOCK) return fail("fcntl: F_GETFL leaked O_NONBLOCK FAIL");
    /* Clear it again and confirm F_GETFL follows. */
    if (fcntl(p[0], F_SETFL, 0) != 0) return fail("fcntl: F_SETFL clear FAIL");
    if (fcntl(p[0], F_GETFL) & O_NONBLOCK) return fail("fcntl: O_NONBLOCK not cleared FAIL");
    outs("fcntl: getfl ok\n");

    /* The same descriptor, now blocking, returns real data - so phase 2's EAGAIN
     * came from the flag, not from a broken pipe. */
    if (write(p[1], "z", 1) != 1) return fail("fcntl: pipe write FAIL");
    if (read(p[0], b, sizeof b) != 1 || b[0] != 'z') return fail("fcntl: pipe read FAIL");
    outs("fcntl: blocking read ok\n");

    /* ---- 4. FD_CLOEXEC across execve ---- */
    int q[2];
    if (pipe(q) != 0) return fail("fcntl: second pipe FAIL");
    /* q[0] is close-on-exec; p[0] is not. */
    if (fcntl(q[0], F_SETFD, FD_CLOEXEC) != 0) return fail("fcntl: F_SETFD FAIL");
    if (fcntl(q[0], F_GETFD) != FD_CLOEXEC) return fail("fcntl: F_GETFD FAIL");
    if (fcntl(p[0], F_GETFD) != 0) return fail("fcntl: F_GETFD plain FAIL");

    char a2[12], a3[12];
    snprintf(a2, sizeof a2, "%d", q[0]);
    snprintf(a3, sizeof a3, "%d", p[0]);
    char *args[] = {(char *)SELF, (char *)"child", a2, a3, NULL};
    char *env[] = {NULL};
    execve(SELF, args, env);
    return fail("fcntl: execve FAIL");
}
