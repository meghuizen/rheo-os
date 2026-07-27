/* polldead.c - a wait that can never end, and what the kernel says about it
 * (docs/ARCHITECTURE-DEBT.md 2.4)
 *
 * A single-process program that `poll`s **indefinitely** on the read end of its
 * own pipe, having written nothing and with itself as the only writer. Nothing can
 * ever make that descriptor ready: there is no other process, no timer, no console
 * byte and no frame. On Linux this hangs forever.
 *
 * The point is what the kernel does with it. Before this slice `reschedule`
 * **panicked** ("no runnable cell") the moment nothing was runnable, printing a
 * kernel stack trace that named no process and no reason. Now the scheduler
 * classifies the situation - nothing runnable, and every blocked process waiting
 * only on another process - prints which pid is blocked on what, and ends the run
 * with `DEADLOCK_EXIT` (147). The test asserts that exit code and both diagnostic
 * lines.
 *
 * The line before the poll is what proves the program really got that far.
 *
 * Built from source by xtask (static glibc, ET_EXEC, no relink), never committed.
 *
 * Expected stdout:
 *   polldead: polling forever
 * (and then no more output - the process never returns from poll)
 */

#include <poll.h>
#include <string.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }

int main(void) {
    int p[2];
    if (pipe(p) != 0) {
        outs("polldead: pipe FAIL\n");
        return 1;
    }
    outs("polldead: polling forever\n");
    struct pollfd pf = {p[0], POLLIN, 0};
    poll(&pf, 1, -1);
    /* Unreachable: nothing can make an empty pipe with no other writer readable. */
    outs("polldead: poll returned FAIL\n");
    return 1;
}
