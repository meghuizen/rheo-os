/* Read the kernel's own CPU list and print it.
 *
 * The value under test is `/sys/devices/system/cpu/online`, which was a seeded constant
 * `0-0` and is now synthesized from `smp::online_count()` (docs/ARCHITECTURE-DEBT.md 7.6).
 * The oracle is the LAUNCH: this fixture runs under `linuxsmp`, which QEMU starts with
 * `-smp 4`, so the expected string is `0-3` and it is independent of the number the kernel
 * believes - which is the point, since asserting the kernel's count against itself would
 * prove only that it is self-consistent.
 *
 * Why it matters beyond tidiness: libuv sizes its thread pool from the CPU count, so Node
 * and Bun under-parallelise when this file lies.
 *
 * Raw syscalls, no libc topology helper, so what is asserted is the file's own bytes. */
#include <fcntl.h>
#include <unistd.h>

static void put(const char *s) {
  const char *p = s;
  while (*p) p++;
  (void)!write(1, s, (unsigned long)(p - s));
}

int main(void) {
  int fd = open("/sys/devices/system/cpu/online", O_RDONLY);
  if (fd < 0) {
    put("cpulist: open failed\n");
    return 1;
  }
  char buf[64];
  long n = read(fd, buf, sizeof buf - 1);
  close(fd);
  if (n <= 0) {
    put("cpulist: read failed\n");
    return 2;
  }
  buf[n] = 0;
  /* Strip the trailing newline so the transcript is one line. */
  if (buf[n - 1] == '\n') buf[n - 1] = 0;
  put("cpulist: online=");
  put(buf);
  put("\n");
  return 0;
}
