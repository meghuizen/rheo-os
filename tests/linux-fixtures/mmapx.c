/* mmapx - the mmap region is bounded, and MAP_FIXED cannot replace the kernel's
 * own rings (docs/ARCHITECTURE-DEBT.md 4, blocker 2).
 *
 * `mmap` is placed by a first-fit search over the per-cell VMA list, bounded to a
 * dedicated window (kernel/src/linux/mem.rs: 80..252 GiB, above every fixed region
 * - the image, stack, queue-pair, channels and ELF interpreter all sit below it).
 * The bound is what stops a long run of allocations from walking into the kernel's
 * own rings or the dynamic linker and handing a program addresses that alias them.
 * A request larger than the whole window is refused with ENOMEM rather than placed
 * past the window's end.
 *
 * Both phases print one line from a fixed set so the transcript stays exact.
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* The window is 80..252 GiB (172 GiB), so one 200 GiB request cannot fit and the
 * reservation must be refused. PROT_NONE keeps it a bare reservation - no frames
 * are touched, so this tests the *placement* bound and not the frame budget.
 * (A `MAP_NORESERVE` mapping that *does* fit the window is now demand-filled rather
 * than eagerly committed - the JSC-Gigacage path, GOAL-BUN - which is why the size
 * here must exceed the window, not merely the frame budget.) */
#define TOO_BIG (200ULL * 1024 * 1024 * 1024)

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

  /* 4. W^X is structural, and now honest. `mmap(PROT_WRITE|PROT_EXEC)` used to
   *    return success and silently drop EXEC - so a JIT that maps its code pool
   *    RWX (which is what JavaScriptCore does on Linux) would fault on its first
   *    jump into generated code, with no diagnostic near the cause. `EPERM` is
   *    the answer that lets a caller act. */
  void *wx = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (wx != MAP_FAILED) {
    puts("wx: PROT_WRITE|PROT_EXEC was accepted");
    return 1;
  }
  if (errno != EPERM) {
    puts("wx: PROT_WRITE|PROT_EXEC refused with the wrong errno");
    return 1;
  }
  puts("wx: mmap PROT_WRITE|PROT_EXEC EPERM");

  /* 5. And the fallback a JIT can actually take: map RW, write code bytes, then
   *    flip to RX. That path works, which is why refusing RWX is a choice a
   *    caller can route around rather than a dead end. */
  unsigned char *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (code == MAP_FAILED) {
    puts("wx: RW mapping for the flip path failed");
    return 1;
  }
  memset(code, 0x90, 64); /* filler; not executed - see below */
  if (mprotect(code, 4096, PROT_READ | PROT_EXEC) != 0) {
    puts("wx: RW->RX flip failed");
    return 1;
  }
  /* Deliberately NOT jumping into it: the point being proven is that the
   * permission transition is available, and emitting real instructions for three
   * ISAs from one fixture would prove something else. Reading it back confirms the
   * page is still mapped and readable after the flip. */
  if (code[0] != 0x90) {
    puts("wx: page unreadable after the flip");
    return 1;
  }
  if (mprotect(code, 4096, PROT_READ | PROT_WRITE | PROT_EXEC) == 0 ||
      errno != EPERM) {
    puts("wx: mprotect to RWX was not refused");
    return 1;
  }
  puts("wx: RW->RX flip works, mprotect to RWX EPERM");

  /* 6. A freed span is REUSED. This is the property a bump cursor cannot have:
   *    it only moves forward, so a program that maps and unmaps in a loop walks
   *    to the region's end and then fails with the whole region free behind it.
   *    Map three, free the middle one, map its size again: first fit must hand
   *    back the hole's address, which is an *address* assertion, not a success
   *    code - the only kind that can tell the two designs apart. */
  const size_t span = 64 * 1024;
  char *a = mmap(NULL, span, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
                 -1, 0);
  char *b = mmap(NULL, span, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
                 -1, 0);
  char *c = mmap(NULL, span, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
                 -1, 0);
  if (a == MAP_FAILED || b == MAP_FAILED || c == MAP_FAILED) {
    puts("vma: three-span setup failed");
    return 1;
  }
  /* Not assumed to be contiguous or ordered - only that they are distinct and
   * that freeing the middle one leaves a hole big enough for one more. */
  if (munmap(b, span) != 0) {
    puts("vma: munmap of the middle span failed");
    return 1;
  }
  char *reused = mmap(NULL, span, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (reused == MAP_FAILED) {
    puts("vma: remap after free failed");
    return 1;
  }
  if (reused != b) {
    printf("vma: freed span not reused (got %p, freed %p)\n", (void *)reused,
           (void *)b);
    return 1;
  }
  /* Genuinely usable memory, not just an address: a stale page-table entry from
   * the old mapping would read back the old byte. */
  memset(reused, 0x5A, span);
  if ((unsigned char)reused[0] != 0x5A ||
      (unsigned char)reused[span - 1] != 0x5A) {
    puts("vma: reused span is not writable");
    return 1;
  }
  puts("vma: freed span reused at the same address, and writable");

  /* 7. A partial unmap in the middle of one mapping leaves the two ends alive.
   *    A bump cursor has no record to split, so it cannot answer this at all;
   *    the failure it produces is a mapping that claims to own a hole. Assert
   *    both ends still read back, and that the hole is genuinely gone by mapping
   *    exactly into it. */
  const size_t page = 4096;
  char *three = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (three == MAP_FAILED) {
    puts("vma: split setup failed");
    return 1;
  }
  memset(three, 0x11, 3 * page);
  if (munmap(three + page, page) != 0) {
    puts("vma: partial munmap failed");
    return 1;
  }
  if ((unsigned char)three[0] != 0x11 ||
      (unsigned char)three[2 * page] != 0x11) {
    puts("vma: partial munmap damaged the surviving ends");
    return 1;
  }
  char *hole = mmap(NULL, page, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (hole != three + page) {
    printf("vma: middle hole not reused (got %p, want %p)\n", (void *)hole,
           (void *)(three + page));
    return 1;
  }
  puts("vma: partial unmap split the mapping, both ends intact, hole reused");

  puts("mmapx OK");
  return 0;
}
