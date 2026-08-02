/* Count the `cpuN` lines in /proc/stat and print the count.
 *
 * `/proc/stat` predates the sysfs CPU tree, so counting its `cpuN` lines is how the
 * portable readers count CPUs - every libc's `get_nprocs` falls back to it. It was a
 * seeded static file with a single `cpu0` line whatever the boot's CPU count was, the
 * same defect `/sys/devices/system/cpu/online` had as the constant `0-0`
 * (docs/ARCHITECTURE-DEBT.md 7.6); it is synthesized from `smp::online_count()` now.
 *
 * The oracle is the LAUNCH: this fixture runs under `linuxsmp`, which QEMU starts with
 * `-smp 4`, so the expected count is 4 and it is independent of the number the kernel
 * believes. Asserting the kernel's count against itself would prove only that it is
 * self-consistent.
 *
 * The aggregate `cpu ` line is deliberately NOT counted: it is the machine total, and a
 * counter that included it would report N+1 - the off-by-one every naive reader makes,
 * and the reason `cpu` is followed by a space and `cpuN` by a digit.
 *
 * Raw syscalls, so what is counted is the file's own bytes. */
#include <fcntl.h>
#include <unistd.h>

static void put(const char *s) {
  const char *p = s;
  while (*p) p++;
  (void)!write(1, s, (unsigned long)(p - s));
}

static void putn(int v) {
  char b[12];
  int i = 12;
  b[--i] = 0;
  if (v == 0) b[--i] = '0';
  while (v > 0) {
    b[--i] = (char)('0' + v % 10);
    v /= 10;
  }
  put(&b[i]);
}

int main(void) {
  int fd = open("/proc/stat", O_RDONLY);
  if (fd < 0) {
    put("procstat: open failed\n");
    return 1;
  }
  char buf[4096];
  long n = read(fd, buf, sizeof buf - 1);
  close(fd);
  if (n <= 0) {
    put("procstat: read failed\n");
    return 2;
  }
  buf[n] = 0;

  /* A `cpuN` line: at the start of a line, "cpu" followed by a digit. */
  int cpus = 0;
  int at_line_start = 1;
  for (long i = 0; i < n; i++) {
    if (at_line_start && i + 3 < n && buf[i] == 'c' && buf[i + 1] == 'p' &&
        buf[i + 2] == 'u' && buf[i + 3] >= '0' && buf[i + 3] <= '9') {
      cpus++;
    }
    at_line_start = (buf[i] == '\n');
  }
  put("procstat: cpus=");
  putn(cpus);
  put("\n");
  return 0;
}
