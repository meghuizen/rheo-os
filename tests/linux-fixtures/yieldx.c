/* yieldx - `sched_yield()` must hand the CPU to another *process*, not only to
 * another thread of the caller (docs/ARCHITECTURE-DEBT.md 4).
 *
 * The personality's scheduler is cooperative: a cell keeps the CPU until it
 * blocks, exits, or yields. `sched_yield` only ever rescheduled among a cell's
 * own L4 contexts, so a single-threaded process yielding had **no ready sibling
 * context** and the call returned immediately. A yield that keeps running is not
 * a yield: a forked child looping `sched_yield()` ran to completion before its
 * parent was scheduled at all.
 *
 * The witness is an **ordering record neither side can fake**. Parent and child
 * both write one marker byte to the same pipe and then yield, eight times each.
 * A pipe is a single cross-cell ring (L6), so the byte order in the ring *is* the
 * interleaving. The parent drains it after reaping the child and checks the
 * order against a hand-computed oracle:
 *
 *   fork() returns into the parent first, so the parent writes 'P', yields, the
 *   child writes 'C', yields, ... = "PCPCPCPCPCPCPCPC" - 16 bytes, alternating.
 *
 * Without a cross-cell yield the parent's yields do nothing, so it writes all
 * eight P's, then blocks in wait4, and only then does the child run:
 * "PPPPPPPPCCCCCCCC". The two orders differ on the first transition, so this
 * discriminates.
 *
 * One line from a fixed set is printed on success; a failure prints the order it
 * actually saw, because that is the useful diagnostic.
 */

#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROUNDS 8
#define TOTAL (2 * ROUNDS)

/* Write one marker then yield, ROUNDS times. Both sides run the identical loop -
 * the asymmetry in the result comes from the scheduler, not from the program. */
static void mark_and_yield(int fd, char c) {
  for (int i = 0; i < ROUNDS; i++) {
    if (write(fd, &c, 1) != 1) {
      _exit(3);
    }
    sched_yield();
  }
}

int main(void) {
  int pfd[2];
  if (pipe(pfd) != 0) {
    puts("yield: pipe failed");
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    puts("yield: fork failed");
    return 1;
  }
  if (child == 0) {
    close(pfd[0]);
    mark_and_yield(pfd[1], 'C');
    close(pfd[1]);
    _exit(0);
  }

  mark_and_yield(pfd[1], 'P');

  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    puts("yield: waitpid failed");
    return 1;
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    printf("yield: child status %d\n", status);
    return 1;
  }

  /* Both write ends are closed now (the child closed its copy before exiting),
   * so the drain sees EOF rather than blocking once the ring is empty. */
  close(pfd[1]);

  char order[TOTAL + 1];
  size_t got = 0;
  while (got < TOTAL) {
    ssize_t n = read(pfd[0], order + got, TOTAL - got);
    if (n <= 0) {
      break;
    }
    got += (size_t)n;
  }
  order[got] = '\0';

  if (got != TOTAL) {
    printf("yield: got %zu of %d bytes: %s\n", got, TOTAL, order);
    return 1;
  }

  /* The hand-computed oracle. Anything else - including the pre-fix
   * "PPPPPPPPCCCCCCCC" - is a failure, printed so it is diagnosable. */
  if (strcmp(order, "PCPCPCPCPCPCPCPC") != 0) {
    printf("yield: order %s\n", order);
    return 1;
  }
  puts("yield: parent and child alternated PCPCPCPCPCPCPCPC");

  puts("yieldx OK");
  return 0;
}
