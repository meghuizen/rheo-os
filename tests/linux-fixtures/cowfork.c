/* cowfork - a fork must share pages, not copy them
 * (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
 *
 * `fork` used to duplicate every committed page, so a process paid its whole
 * resident set to fork - which for a large program is more than its image ever
 * cost. Now the pages are shared read-only in both address spaces and privated
 * one at a time, on write.
 *
 * The program's half proves the *semantics*; the kernel's half (the frame-pool
 * delta across the fork) proves the *saving*, and is the number the program
 * cannot fake.
 *
 * The semantics that matter, and the mistake each one catches:
 *  - the child sees the parent's pre-fork values          (sharing happened)
 *  - the child's writes are invisible to the parent       (the child privated)
 *  - the parent's writes are invisible to the child       (the PARENT was
 *    write-protected too - the half that produces wrong values, not a fault)
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096u
/* 256 pages = 1 MiB of dirty heap. Large enough that copy-vs-share is
 * unmistakable in the frame count, small enough for the test pool. */
#define PAGES 256u

/* Page N carries this byte before the fork. Not N: a handler that lost the
 * offset and always touched page 0 would still look plausible. */
static unsigned char pre(unsigned n) { return (unsigned char)(0x40 + (n & 0x3f)); }

int main(void) {
  unsigned char *m = malloc((size_t)PAGES * PAGE);
  if (!m) {
    puts("cow: malloc failed");
    return 1;
  }
  /* Dirty every page, so all of it is committed and writable before the fork. */
  for (unsigned n = 0; n < PAGES; n++) memset(m + (size_t)n * PAGE, pre(n), PAGE);
  puts("cow: 256 pages dirtied");

  pid_t kid = fork();
  if (kid == 0) {
    /* 1. The child must see what the parent wrote before the fork. */
    for (unsigned n = 0; n < PAGES; n++) {
      if (m[(size_t)n * PAGE + 7] != pre(n)) _exit(11);
    }
    /* 2. The child writes its own marker into a few pages. */
    m[0] = 0xC1;
    m[37u * PAGE] = 0xC2;
    m[(PAGES - 1) * PAGE] = 0xC3;
    /* 3. Give the parent a turn, then check the parent's writes did NOT reach
     *    us. `sched_yield` is enough: the scheduler is cooperative here. */
    sched_yield();
    if (m[1u * PAGE] != pre(1)) _exit(12); /* the parent wrote here */
    if (m[0] != 0xC1) _exit(13);           /* our own write must survive */
    _exit(0);
  }

  /* The parent writes different markers, into different pages. */
  m[1u * PAGE] = 0xD1;
  m[38u * PAGE] = 0xD2;
  int wst = 0;
  if (waitpid(kid, &wst, 0) != kid) {
    puts("cow: waitpid failed");
    return 1;
  }
  if (!WIFEXITED(wst) || WEXITSTATUS(wst) != 0) {
    printf("cow: child failed, status %d\n", wst);
    return 1;
  }
  /* 4. The child's writes must not have reached the parent. */
  if (m[0] != pre(0) || m[37u * PAGE] != pre(37) || m[(PAGES - 1) * PAGE] != pre(PAGES - 1)) {
    puts("cow: the child's writes reached the parent");
    return 1;
  }
  /* 5. And the parent's own writes are still there. */
  if (m[1u * PAGE] != 0xD1 || m[38u * PAGE] != 0xD2) {
    puts("cow: the parent lost its own writes");
    return 1;
  }
  puts("cow: parent and child are isolated after a shared fork");
  puts("cowfork OK");
  return 0;
}
