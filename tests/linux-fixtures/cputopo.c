/* Read the kernel's per-CPU topology files the way hwloc does.
 *
 * `/sys/devices/system/cpu/cpu<N>/topology/{core_id,physical_package_id,thread_siblings_list}`
 * is what every topology-aware runtime reads to decide how many workers to start and where to
 * put them (docs/RESOURCE-GRAPH.md 2.4a). This fixture only *reports* what it read; the
 * assertion lives in the test kernel, whose oracle is QEMU's `-smp` line.
 *
 * Raw open/read, no libc topology helper, so what is printed is the file's own bytes. */
#include <fcntl.h>
#include <unistd.h>

static void put(const char *s) {
  const char *p = s;
  while (*p) p++;
  (void)!write(1, s, (unsigned long)(p - s));
}

/* Read a topology file for CPU `n` into `buf`, NUL-terminated with any trailing newline
 * stripped. Returns 0 on success. */
static int slurp(int n, const char *file, char *buf, unsigned long cap) {
  char path[128];
  const char *a = "/sys/devices/system/cpu/cpu";
  unsigned long w = 0;
  while (*a) path[w++] = *a++;
  if (n >= 10) path[w++] = (char)('0' + n / 10);
  path[w++] = (char)('0' + n % 10);
  const char *b = "/topology/";
  while (*b) path[w++] = *b++;
  while (*file) path[w++] = *file++;
  path[w] = 0;

  int fd = open(path, O_RDONLY);
  if (fd < 0) return -1;
  long got = read(fd, buf, cap - 1);
  close(fd);
  if (got <= 0) return -2;
  buf[got] = 0;
  if (buf[got - 1] == '\n') buf[got - 1] = 0;
  return 0;
}

static void report(int n) {
  char core[32], pkg[32], sibs[64];
  if (slurp(n, "core_id", core, sizeof core) || slurp(n, "physical_package_id", pkg, sizeof pkg)
      || slurp(n, "thread_siblings_list", sibs, sizeof sibs)) {
    put("cputopo: read failed\n");
    return;
  }
  put("cputopo: cpu");
  char d[3] = {0, 0, 0};
  if (n >= 10) {
    d[0] = (char)('0' + n / 10);
    d[1] = (char)('0' + n % 10);
  } else {
    d[0] = (char)('0' + n);
  }
  put(d);
  put(" core=");
  put(core);
  put(" pkg=");
  put(pkg);
  put(" threads=");
  put(sibs);
  put("\n");
}

int main(void) {
  /* CPU 0 and CPU 2: on a two-core, two-thread machine they are on *different* cores, so a
   * synthesis that returned one constant for every CPU is visible in the transcript. */
  report(0);
  report(2);
  /* Two CPUs that do not exist, and they are refused for two different reasons - both must
   * be refused rather than answered.
   *
   * CPU 9 is inside the kernel's fixed CPU array but past the CPUs it discovered, so what
   * stops it is the unknown-topology sentinel in that slot. CPU 99 is past the array itself,
   * so what stops it is the index bound - and without that bound the kernel would index out
   * of range, which is a panic rather than a wrong answer. */
  report(9);
  report(99);
  return 0;
}
