/* stackx - the loader must size the stack from PT_GNU_STACK, not from a fixed
 * constant (docs/ARCHITECTURE-DEBT.md 4.0, blocker 1).
 *
 * The loader ignored PT_GNU_STACK entirely and gave every Linux cell the same
 * 8 MiB. A binary that recorded a larger request silently got less and overran,
 * and the failure landed wherever the recursion happened to be deep enough -
 * far from the cause.
 *
 * The measured case that forced this: the real Claude Code binary's
 * PT_GNU_STACK p_memsz is 0xc35000 = 12.8 MiB. Bumping the constant to 16 MiB
 * would have fixed that one binary and left the next one to fail the same way,
 * which is why the fix reads the header.
 *
 * This fixture is linked with `-Wl,-z,stacksize=SIZE` (see xtask) so its own
 * PT_GNU_STACK asks for more than the old fixed default, then it actually
 * touches that much stack. Pre-fix it faults; post-fix it returns.
 *
 * Each phase prints one line from a fixed set so the transcript stays exact.
 */

#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

/* Must match the -z stacksize the link uses. 12 MiB: above the old fixed 8 MiB
 * default, and the same order as the real binary's 12.8 MiB request. */
#define WANT_BYTES (12u * 1024 * 1024)

/* Touch the stack in chunks, recursing so the compiler cannot hoist the frames
 * away and the pages are dirtied in address order - the same shape as a deep
 * call chain in a JIT, which is what overruns a stack in practice.
 *
 * `volatile` and the returned checksum keep every frame live: without them the
 * optimiser is entitled to delete the whole thing, and the test would pass by
 * doing nothing. */
#define CHUNK 65536

static unsigned long descend(unsigned depth) {
  volatile unsigned char pad[CHUNK];
  memset((void *)pad, (int)(depth & 0xff), CHUNK);
  unsigned long sum = pad[0] + pad[CHUNK - 1];
  if (depth == 0) {
    return sum;
  }
  return sum + descend(depth - 1);
}

int main(void) {
  /* 1. RLIMIT_STACK must report what was actually mapped. glibc sizes THREAD
   *    stacks from this number, so reporting more than is mapped hands every
   *    thread a stack that faults, and reporting the old default when more was
   *    mapped wastes it. Either way the number has to be the truth. */
  struct rlimit rl;
  if (getrlimit(RLIMIT_STACK, &rl) != 0) {
    puts("stack: getrlimit failed");
    return 1;
  }
  if (rl.rlim_cur < WANT_BYTES) {
    printf("stack: RLIMIT_STACK %lu < requested %u\n", (unsigned long)rl.rlim_cur,
           WANT_BYTES);
    return 1;
  }
  puts("stack: RLIMIT_STACK covers the PT_GNU_STACK request");

  /* 2. And the stack is genuinely there. Leave a margin: argv/envp/auxv and the
   *    frames above main also live on it, so touching the full request from here
   *    would be off by however much the entry path used. Three quarters is well
   *    clear of the old 8 MiB default and well inside the 12 MiB request. */
  const unsigned depth = (WANT_BYTES * 3u / 4u) / CHUNK;
  unsigned long sum = descend(depth);
  if (sum == 0) {
    puts("stack: descend produced nothing");
    return 1;
  }
  printf("stack: touched %u KiB of stack in %u frames\n",
         (depth + 1) * CHUNK / 1024, depth + 1);

  puts("stackx OK");
  return 0;
}
