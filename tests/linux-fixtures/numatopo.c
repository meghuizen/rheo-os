/* Read the kernel's memory-locality files the way libnuma and hwloc do.
 *
 * `/sys/devices/system/node/{online,nodeN/cpulist,nodeN/distance,nodeN/meminfo}` is what
 * `numactl --hardware` and every NUMA-aware runtime read to decide where to put a worker and
 * its memory (docs/RESOURCE-GRAPH.md 6.3). `distance` in particular is the file the whole SLIT
 * parse exists to feed.
 *
 * This fixture only reports what it read; the assertion is in the test kernel, whose oracle is
 * QEMU's `-numa` lines. */
#include <fcntl.h>
#include <unistd.h>

static void put(const char *s) {
  const char *p = s;
  while (*p) p++;
  (void)!write(1, s, (unsigned long)(p - s));
}

/* Read `path` into `buf`, NUL-terminated, trailing newline stripped. 0 on success. */
static int slurp(const char *path, char *buf, unsigned long cap) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) return -1;
  long got = read(fd, buf, cap - 1);
  close(fd);
  if (got <= 0) return -2;
  buf[got] = 0;
  if (buf[got - 1] == '\n') buf[got - 1] = 0;
  return 0;
}

static void show(const char *label, const char *path) {
  char buf[128];
  put("numatopo: ");
  put(label);
  put("=");
  if (slurp(path, buf, sizeof buf))
    put("<none>");
  else
    put(buf);
  put("\n");
}

int main(void) {
  show("online", "/sys/devices/system/node/online");
  show("n0cpus", "/sys/devices/system/node/node0/cpulist");
  show("n0dist", "/sys/devices/system/node/node0/distance");
  show("n1cpus", "/sys/devices/system/node/node1/cpulist");
  show("n1dist", "/sys/devices/system/node/node1/distance");
  /* Node 9 does not exist. It must be refused rather than answered - the index comes out of a
   * path this program chose, and `distance` indexes a fixed matrix. */
  show("n9dist", "/sys/devices/system/node/node9/distance");
  return 0;
}
