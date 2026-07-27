/* killx - cross-process signalling and /proc/self/exe, from an unmodified
 * static-glibc binary (docs/LINUX-COMPAT.md, docs/ARCHITECTURE-DEBT.md 4).
 *
 * Both were stubs that reported success for work never done, which is the class
 * that puts a failure far from its cause:
 *   - `kill(pid, sig)` refused any pid but our own with ESRCH, and `kill(0, sig)`
 *     / `kill(-1, sig)` **silently delivered to the caller** instead of to the
 *     process group. Subprocess management is the whole job of the program this
 *     personality exists to run, so "signal my children" cannot be a lie.
 *   - `readlinkat` was a hardcoded -ENOENT, `/proc/self/exe` included - the one
 *     link real programs actually read.
 *
 * Every phase prints one line from a fixed set so the transcript stays exact.
 *
 * A note on what discriminates. The first three kill phases written for this
 * fixture (self probe, absent pid, unknown group) all passed **with the fix
 * reverted** - the old stub happened to give the same three answers. They are
 * kept as documentation, but the phases that actually prove something are the
 * two below them: signalling a *child* (the old code answered ESRCH), and
 * `kill(-1)` sparing the top of the tree (the old code delivered it to us).
 */

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t got_usr1;
static volatile sig_atomic_t got_usr2;

static void on_usr1(int signo) {
  (void)signo;
  got_usr1 = 1;
}

static void on_usr2(int signo) {
  (void)signo;
  got_usr2 = 1;
}

/* How many yields the child gives the parent before deciding the signal is not
 * coming. Bounded so a broken kill fails the test instead of hanging it. */
#define CHILD_SPINS 200

int main(int argc, char **argv) {
  /* The `/proc/self/exe` half only means anything in a process that was
   * **execve'd**: a cell the test kernel loaded directly never named a path, and
   * inventing one would be a fabricated answer. So the parent runs the kill
   * phases and then execve's itself; this branch is the re-exec'd child. */
  if (argc > 1 && strcmp(argv[1], "exec") == 0) {
    char buf[256];
    ssize_t n = readlink("/proc/self/exe", buf, sizeof buf - 1);
    if (n <= 0) {
      puts("exe: readlink(/proc/self/exe) failed");
      return 1;
    }
    buf[n] = '\0';
    printf("exe: %s\n", buf);

    /* A path that exists but is not a symlink is EINVAL; one that does not exist
     * is ENOENT. `/bin/killx` is this very program, seeded by the test. */
    if (readlink("/bin/killx", buf, sizeof buf) >= 0 || errno != EINVAL) {
      puts("exe: non-link did not report EINVAL");
      return 1;
    }
    if (readlink("/nope/absent", buf, sizeof buf) >= 0 || errno != ENOENT) {
      puts("exe: absent path did not report ENOENT");
      return 1;
    }
    puts("exe: non-link EINVAL, absent ENOENT");
    puts("killx OK");
    return 0;
  }

  /* Handlers are installed before the fork so the child inherits the
   * dispositions (POSIX: `fork` copies them). */
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_usr1;
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    puts("kill: sigaction SIGUSR1 failed");
    return 1;
  }
  sa.sa_handler = on_usr2;
  if (sigaction(SIGUSR2, &sa, NULL) != 0) {
    puts("kill: sigaction SIGUSR2 failed");
    return 1;
  }

  /* 1. A probe of our own pid succeeds, and 2. a pid that does not exist is
   *    ESRCH. Neither discriminates - see the header - but both must hold. */
  pid_t me = getpid();
  if (kill(me, 0) != 0) {
    puts("kill: self probe failed");
    return 1;
  }
  if (kill(999999, 0) == 0 || errno != ESRCH) {
    puts("kill: absent pid did not report ESRCH");
    return 1;
  }
  /* 3. A negative pid other than -1 names a process group. None exist here, so
   *    it must be refused rather than redirected to the caller. */
  if (kill(-4242, SIGUSR1) == 0 || errno != ESRCH) {
    puts("kill: unknown group did not report ESRCH");
    return 1;
  }
  puts("kill: self probe ok, absent ESRCH, unknown group ESRCH");

  /* 4. THE discriminating phase: signal a *child*. The old code answered ESRCH
   *    for any pid but our own, so both the probe and the delivery below fail
   *    without the fix. The child waits by yielding - bounded, so a signal that
   *    never arrives fails the test rather than hanging it - and reports through
   *    its exit code, which the kernel gives us and the child cannot forge. */
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    puts("kill: fork failed");
    return 1;
  }
  if (child == 0) {
    for (int i = 0; i < CHILD_SPINS && !got_usr1; i++) {
      sched_yield();
    }
    _exit(got_usr1 ? 0 : 4);
  }

  if (kill(child, 0) != 0) {
    puts("kill: probe of a live child failed");
    return 1;
  }
  if (kill(child, SIGUSR1) != 0) {
    puts("kill: signalling a child failed");
    return 1;
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    puts("kill: waitpid failed");
    return 1;
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    printf("kill: child status %d\n", status);
    return 1;
  }
  /* The child is reaped, so its pid must now be gone. */
  if (kill(child, 0) == 0 || errno != ESRCH) {
    puts("kill: reaped child still probed live");
    return 1;
  }
  puts("kill: child signalled, handler ran, reaped pid gone");

  /* 5. THE second discriminating phase: `kill(-1)` means "every process I may
   *    signal, except init". We are the top of the process tree - the stand-in
   *    for init - and the child is reaped, so there is nothing left to signal
   *    and the answer is ESRCH. The old code delivered `-1` to the caller, which
   *    would run our SIGUSR2 handler. */
  got_usr2 = 0;
  int r = kill(-1, SIGUSR2);
  if (r == 0 || errno != ESRCH) {
    puts("kill: -1 with no targets did not report ESRCH");
    return 1;
  }
  if (got_usr2) {
    puts("kill: -1 self-targeted init");
    return 1;
  }
  puts("kill: -1 spared init, ESRCH with no other process");

  /* Hand over to the re-exec'd self for the `/proc/self/exe` phase: the path is
   * only recorded by `execve`, and a directly-loaded cell has none. stdout is a
   * pipe, so glibc block-buffers it and `execve` would discard the buffer -
   * exactly as on Linux; flush first. */
  fflush(stdout);
  char *const cargv[] = {(char *)"killx", (char *)"exec", NULL};
  char *const cenv[] = {NULL};
  execve("/bin/killx", cargv, cenv);
  puts("kill: execve failed");
  return 1;
}
