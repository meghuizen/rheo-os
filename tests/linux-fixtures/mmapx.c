/* mmapx - the mmap region is bounded, and MAP_FIXED cannot replace the kernel's
 * own rings (docs/ARCHITECTURE-DEBT.md 4, blocker 2).
 *
 * `mmap` is a forward bump cursor with no accounting. It used to run without a
 * limit, so a long enough run of allocations walked out of the 12 GiB mmap region,
 * through the cell's queue-pair region at 16 GiB, its channel regions at 24 GiB,
 * and into the ELF interpreter at 64 GiB - where `ld.so` and `libc.so.6` live. A
 * program would be handed addresses aliasing its own dynamic linker, with no
 * error. Against a ~100 MB binary that is not a remote possibility: 4 GiB of
 * mappings is enough to reach the queue.
 *
 * Both phases print one line from a fixed set so the transcript stays exact.
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* The region is 12..16 GiB, so one 8 GiB request cannot fit and the reservation
 * must be refused. PROT_NONE keeps it a bare reservation - no frames are touched,
 * so this tests the *placement* bound and not the frame budget. */
#define TOO_BIG (8ULL * 1024 * 1024 * 1024)

/* The cell's queue-pair region (kernel/src/load.rs USER_QUEUE_VA). A program has
 * no business mapping here; the point is that it is refused rather than allowed to
 * replace the kernel's frames. */
#define QUEUE_VA 0x400000000ULL

int main(void) {
  /* 1. A modest anonymous mapping still works - the bound is a bound, not a
   *    break. Write and read it back so it is genuinely usable memory. */
  const size_t small = 64 * 1024;
  char *p = mmap(NULL, small, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (p == MAP_FAILED) {
    puts("mmap: small anonymous mapping failed");
    return 1;
  }
  memset(p, 0xA5, small);
  for (size_t i = 0; i < small; i += 4096) {
    if ((unsigned char)p[i] != 0xA5) {
      puts("mmap: small mapping did not read back");
      return 1;
    }
  }
  puts("mmap: small anonymous mapping usable");

  /* 2. A request larger than the whole region is refused with ENOMEM - an answer
   *    glibc acts on - instead of silently landing past the region's end. */
  void *big = mmap(NULL, (size_t)TOO_BIG, PROT_NONE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (big != MAP_FAILED) {
    puts("mmap: oversized reservation was accepted");
    return 1;
  }
  if (errno != ENOMEM) {
    puts("mmap: oversized reservation refused with the wrong errno");
    return 1;
  }
  puts("mmap: oversized reservation ENOMEM");

  /* 3. MAP_FIXED onto the cell's queue-pair region is refused. This is the case
   *    the bump cursor cannot protect against, because the caller chooses the
   *    address. */
  void *ring = mmap((void *)QUEUE_VA, 4096, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
  if (ring != MAP_FAILED) {
    puts("mmap: MAP_FIXED over the queue region was accepted");
    return 1;
  }
  if (errno != EINVAL) {
    puts("mmap: MAP_FIXED over the queue region refused with the wrong errno");
    return 1;
  }
  puts("mmap: MAP_FIXED over the queue region EINVAL");

  puts("mmapx OK");
  return 0;
}
