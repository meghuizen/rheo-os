/* mmapdp - a file mapping must cost what the program touches, not what it
 * reserved (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
 *
 * `mmap` of a file used to read EVERY page into a fresh frame before returning.
 * That is not a size problem to be answered with a bigger pool - it is the wrong
 * design at any size. Measured on the binary this is aimed at, all three PT_LOADs
 * have filesz == memsz (no .bss), so the whole 262 MiB image is file-backed and
 * this is the path that decides whether it can run at all.
 *
 * The program's half of the proof: map a file far larger than it reads, touch
 * exactly three pages at known offsets, and report the bytes. The kernel's half is
 * the oracle that cannot be faked from here - the number of pages demand paging
 * actually filled, which must be 3 and not PAGES.
 *
 * Each page of the file is filled with a distinct byte, so reading page N proves
 * the fault used the right file offset - the arithmetic a split or a high page
 * would get wrong.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096u
/* 64 pages = 256 KiB. Large enough that eager-vs-demand is unmistakable in the
 * fault count, small enough to write into a ramfs. */
#define PAGES 64u

/* The byte page N is filled with. Deliberately not N: a handler that lost the
 * offset and always read page 0 would still produce a plausible-looking byte. */
static unsigned char page_byte(unsigned n) { return (unsigned char)(0x40 + n); }

int main(void) {
  const char *path = "/mmapdp.bin";

  /* 1. Build the backing file: PAGES pages, page N filled with page_byte(N). */
  int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
  if (fd < 0) {
    puts("dp: create failed");
    return 1;
  }
  static unsigned char buf[PAGE];
  for (unsigned n = 0; n < PAGES; n++) {
    memset(buf, page_byte(n), PAGE);
    if (write(fd, buf, PAGE) != (ssize_t)PAGE) {
      puts("dp: write failed");
      return 1;
    }
  }
  puts("dp: backing file written");

  /* 2. Map the whole thing read-only, private. Nothing should be read yet. */
  unsigned char *m = mmap(NULL, (size_t)PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
  if (m == MAP_FAILED) {
    printf("dp: mmap failed errno %d\n", errno);
    return 1;
  }
  /* Closing the fd must NOT break the mapping - the mapping references the file,
   * not the descriptor. This is exactly what ld.so does, and a mapping that kept
   * the caller's fd would fault on a closed one. */
  close(fd);
  puts("dp: mapped 64 pages, fd closed");

  /* 3. Touch exactly three pages: the first, one in the middle, and the last.
   *    The last is the one that catches offset arithmetic that only works near 0. */
  unsigned char first = m[0];
  unsigned char mid = m[37u * PAGE + 11];
  unsigned char last = m[(PAGES - 1) * PAGE + PAGE - 1];
  if (first != page_byte(0) || mid != page_byte(37) || last != page_byte(PAGES - 1)) {
    printf("dp: wrong bytes %02x %02x %02x, want %02x %02x %02x\n", first, mid, last,
           page_byte(0), page_byte(37), page_byte(PAGES - 1));
    return 1;
  }
  puts("dp: pages 0, 37 and 63 read the right bytes");

  /* 4. Re-touching a page already filled must not fill it again - the fault
   *    handler must recognise a present page rather than repopulating it. If it
   *    did repopulate, the kernel-side fault count would exceed 3. */
  for (int i = 0; i < 100; i++) {
    if (m[0] != page_byte(0)) {
      puts("dp: reread changed");
      return 1;
    }
  }
  puts("dp: 100 rereads of a filled page cost nothing");

  /* 5. A write to a **filled** read-only page must still be a SIGSEGV.
   *
   *    This is the phase that discriminates the fault handler's second check.
   *    Reading page 5 first *fills* it (read-only); the write then faults on
   *    PERMISSION, not absence. A handler that could not tell the two apart would
   *    allocate another frame, map it read-only again, retry, and fault forever -
   *    a hang with no diagnostic, and the reason the check consults the page tables
   *    rather than `FaultCause` (which carries no read/write bit).
   *
   *    Run in a forked child so the fixture survives to report it. */
  volatile unsigned char probe = m[5u * PAGE]; /* fills page 5 read-only */
  if (probe != page_byte(5)) {
    puts("dp: page 5 read wrong");
    return 1;
  }
  pid_t kid = fork();
  if (kid == 0) {
    m[5u * PAGE] = 0xFF; /* permission fault: must not be refilled */
    _exit(0);            /* reached only if the write wrongly succeeded */
  }
  int wst = 0;
  if (waitpid(kid, &wst, 0) != kid) {
    puts("dp: waitpid failed");
    return 1;
  }
  if (!WIFSIGNALED(wst) || WTERMSIG(wst) != SIGSEGV) {
    puts("dp: writing a read-only page was not SIGSEGV");
    return 1;
  }
  puts("dp: writing a filled read-only page is SIGSEGV, not a refill");

  /* 6. The mapping must still work now that a process sharing it has exited.
   *
   *    `fork` duplicates the mapping records, and the kernel counts references to
   *    the file behind them. If the fork did not add a reference for the child, the
   *    child's exit gives back one the child never took, the count reaches zero, the
   *    file is closed - and the loser is THIS process, which did nothing wrong. The
   *    damage shows up only on the next page it has not touched yet, so read one:
   *    page 20, untouched by every phase above.
   *
   *    A closed backing store does not fault; it fills the page with zeros. So the
   *    symptom of the bug is a plausible-looking read that returns 0x00, which is
   *    why the file is filled with per-page bytes and this checks the byte. */
  unsigned char after_child = m[20u * PAGE + 3];
  if (after_child != page_byte(20)) {
    printf("dp: page 20 reads %02x after a sharer exited, want %02x\n", after_child,
           page_byte(20));
    return 1;
  }
  puts("dp: a page still faults from the file after a forked sharer exited");

  /* 7. And the W->X-style flip a caller is entitled to still works. */
  if (mprotect(m, PAGE, PROT_READ | PROT_WRITE) != 0) {
    puts("dp: mprotect to RW failed");
    return 1;
  }
  m[1] = 0xAB; /* now legitimately writable; a private write, not seen by the file */
  if (m[1] != 0xAB) {
    puts("dp: private write did not stick");
    return 1;
  }
  puts("dp: mprotect RW then a private write works");

  if (munmap(m, (size_t)PAGES * PAGE) != 0) {
    puts("dp: munmap failed");
    return 1;
  }
  puts("mmapdp OK");
  return 0;
}
