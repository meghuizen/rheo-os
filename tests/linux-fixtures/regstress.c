/* regstress - hammer the Linux personality's GLOBAL registries (docs/SMP.md 10.2).
 *
 * Every other multi-core Linux fixture is a hello: it starts glibc, prints, exits, and
 * barely touches the kernel's shared tables. So "four Linux cells across four cores pass"
 * held equally with the personality lock removed, which makes it evidence of width and not
 * of serialisation. This fixture is the missing half.
 *
 * The two registries it aims at are global fixed arrays whose allocators are
 * find-a-free-slot-then-claim-it (`kernel/src/linux/pipe.rs`, `eventfd.rs`). That shape
 * races directly: two cores can both find the same free index and both claim it, so two
 * processes end up holding one object. The detectable consequence is not a fault, it is
 * *someone else's bytes* - which is why every value written here is derived from this
 * process's own pid, and every read is checked against it.
 *
 * One line of output, so the transcript stays exact:
 *   "regstress OK\n"          - every round agreed
 *   "regstress FAIL <n>\n"    - n rounds disagreed
 * Exit 0 or 1 to match.
 */
#include <stdio.h>
#include <unistd.h>
#include <string.h>
#include <sys/eventfd.h>
#include <stdint.h>

/* Enough rounds that two cores are inside the allocators together many times over,
 * few enough to stay well inside the boot-test budget under emulation. */
#define ROUNDS 256

int main(void) {
    /* This process's own marker. Two cells running this concurrently have different
     * pids, so a byte that comes back wrong came from the other one. */
    unsigned char mark = (unsigned char)(getpid() & 0xff);
    unsigned long bad = 0;

    for (int i = 0; i < ROUNDS; i++) {
        /* --- the pipe registry --- */
        int fds[2];
        if (pipe(fds) != 0) { bad++; continue; }
        unsigned char out = (unsigned char)(mark ^ (unsigned char)i);
        if (write(fds[1], &out, 1) != 1) { bad++; }
        unsigned char in = (unsigned char)~out;
        if (read(fds[0], &in, 1) != 1 || in != out) { bad++; }
        close(fds[0]);
        close(fds[1]);

        /* --- the eventfd registry --- */
        int ev = eventfd(0, 0);
        if (ev < 0) { bad++; continue; }
        uint64_t v = (uint64_t)mark * 1000u + (uint64_t)i + 1u;
        if (write(ev, &v, 8) != 8) { bad++; }
        uint64_t got = 0;
        if (read(ev, &got, 8) != 8 || got != v) { bad++; }
        close(ev);
    }

    if (bad == 0) {
        printf("regstress OK\n");
        fflush(stdout);
        return 0;
    }
    printf("regstress FAIL %lu\n", bad);
    fflush(stdout);
    return 1;
}
