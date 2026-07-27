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
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

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

  /* The `kill` half of this fixture was removed before landing, deliberately.
   * Cross-process signalling was implemented (pid lookup + pending + delivery on
   * cell resume) and behaved correctly on riscv64, then failed on x86-64 in a way
   * that was not root-caused inside the slice. Shipping a signal path that is
   * broken on one ISA is worse than shipping neither, so it was reverted and
   * recorded with everything learned - docs/ARCHITECTURE-DEBT.md 4. What remains
   * here is `/proc/self/exe`, which is proven on all three ISAs. */

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
