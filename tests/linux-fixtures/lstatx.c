/* lstatx - the **legacy path-based `stat`/`lstat` syscall numbers**
 * (docs/LINUX-COMPAT.md).
 *
 * Deliberately a **raw syscall**, not glibc's `stat()`. That distinction is the whole
 * point and was established by measurement: an earlier version of this fixture called
 * `stat()` and passed with the fix reverted, because this glibc routes `stat()` through
 * `newfstatat` even on x86-64. The programs that issue numbers 4 and 6 are the ones that
 * bypass libc - Bun's Zig runtime does, and `ENOSYS nr=4` in its trace is what turned
 * this up. A fixture that cannot fail is worse than no fixture, so this one calls the
 * numbers directly.
 *
 * On arm64/riscv64 those numbers do not exist in the asm-generic table (4 and 6 are
 * `read`/`close` there), so the raw path is x86-64 only and the other ISAs check the
 * ordinary `stat()` instead - which is the honest shape: the trap being fixed is
 * per-ISA, so the proof is too.
 *
 * One line from a fixed set, so the transcript stays exact.
 */

#include <stdio.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
#if defined(__x86_64__)
  /* The legacy numbers, issued directly. `struct stat` here is the kernel's x86-64
   * layout, which glibc's is compatible with for the fields checked below. */
  struct stat sb;
  if (syscall(SYS_stat, "/", &sb) != 0) {
    puts("lstatx: raw stat(/) failed");
    return 1;
  }
  if (!S_ISDIR(sb.st_mode)) {
    puts("lstatx: / is not a directory");
    return 2;
  }
  struct stat lb;
  if (syscall(SYS_lstat, "/", &lb) != 0) {
    puts("lstatx: raw lstat(/) failed");
    return 3;
  }
  /* No symlinks exist in this VFS, so the two must agree - and agreeing on the inode is
   * the check that catches a stat block filled with constants (the `st_ino = 1` defect
   * that made ld.so treat two libraries as one). */
  if (!S_ISDIR(lb.st_mode) || lb.st_ino != sb.st_ino) {
    puts("lstatx: raw lstat disagrees with raw stat");
    return 4;
  }
  /* An absent path must still be refused, so success is not the only answer this code
   * path can give. */
  if (syscall(SYS_stat, "/definitely-absent", &sb) == 0) {
    puts("lstatx: raw stat of an absent path succeeded");
    return 5;
  }
  puts("lstatx: raw stat + lstat OK");
#else
  /* No legacy numbers on this ISA: glibc's `stat()` is `newfstatat`, already covered.
   * Checked anyway so the fixture is not vacuous here either. */
  struct stat sb;
  if (stat("/", &sb) != 0 || !S_ISDIR(sb.st_mode)) {
    puts("lstatx: stat(/) failed");
    return 1;
  }
  puts("lstatx: newfstatat-only ISA, no legacy numbers");
#endif
  return 0;
}
