/* sig_fp - FP/SIMD registers survive a signal handler (docs/SUBSTRATE.md S4).
 *
 * A signal handler runs on the *live* register file. The kernel is soft-float, so
 * nothing between the trap and delivery touches the vector registers, and the
 * interrupted code's values are still in the hardware. If the kernel does not save
 * them, a handler that executes one FP instruction destroys them and the
 * interrupted code resumes with someone else's numbers - no fault, no log, wrong
 * answers. Fatal for a JIT, where a profiling signal lands mid-vector-loop in
 * generated code and the loop continues with the handler's registers.
 *
 * ---------------------------------------------------------------------------
 * Two earlier versions of this fixture passed with the kernel fix deleted, which
 * means they tested nothing. Both failures are worth stating, because they are
 * what determines the shape below.
 *
 * 1. `raise()` is a *call*. On all three ABIs the caller-saved FP registers are
 *    dead across a call, so the compiler had already spilled every value to the
 *    stack before the signal existed.
 *
 * 2. A fault taken mid-loop fixed that, but the accumulators were still safe: a
 *    signal handler is an ordinary C function, so it **preserves the
 *    callee-saved FP registers itself**, and a register allocator puts values
 *    that live across a loop exactly there. Only the **caller-saved** registers
 *    are genuinely at risk, and C gives no way to pin a value in one.
 *
 * So this pins them in inline asm - the same technique the tree's cross-cell
 * FP proof uses (docs/LIBRHEO.md, the `librheoipc` register-pattern phase): one
 * asm block loads eight known doubles into caller-saved FP registers, performs
 * the faulting store, and writes the registers back out, so the compiler cannot
 * spill around the fault. Per-ISA register names are unavoidable here and
 * legitimate in a fixture - naming a register *is* the experiment.
 *
 * The signal itself arrives with **no call boundary**: a store to a PROT_NONE
 * page faults, SIGSEGV is delivered at that instruction, the handler mprotects
 * the page writable and runs its own FP work, and `rt_sigreturn` re-executes the
 * store - which now succeeds.
 */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define NREG 8

static volatile char *trap_page;
static volatile sig_atomic_t handled;

/* Overwrite the same eight caller-saved FP registers with sentinels.
 *
 * In asm for the same reason the read-back below is: C code in a handler *may*
 * touch these registers, but nothing makes it certain - a compiler is free to use
 * callee-saved ones, which the handler then dutifully preserves, and the
 * experiment quietly stops being an experiment. Writing the clobber by hand is
 * what makes "the handler destroyed them" a fact of the fixture rather than a
 * hope about code generation. */
static void clobber_fp(const double *junk) {
#if defined(__x86_64__)
  __asm__ __volatile__(
      "movsd  0(%[j]), %%xmm0\n\t"
      "movsd  8(%[j]), %%xmm1\n\t"
      "movsd 16(%[j]), %%xmm2\n\t"
      "movsd 24(%[j]), %%xmm3\n\t"
      "movsd 32(%[j]), %%xmm4\n\t"
      "movsd 40(%[j]), %%xmm5\n\t"
      "movsd 48(%[j]), %%xmm6\n\t"
      "movsd 56(%[j]), %%xmm7\n\t"
      :
      : [j] "r"(junk)
      : "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7");
#elif defined(__aarch64__)
  __asm__ __volatile__(
      "ldp d0, d1, [%[j], #0]\n\t"
      "ldp d2, d3, [%[j], #16]\n\t"
      "ldp d4, d5, [%[j], #32]\n\t"
      "ldp d6, d7, [%[j], #48]\n\t"
      :
      : [j] "r"(junk)
      : "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7");
#elif defined(__riscv)
  __asm__ __volatile__(
      "fld ft0,  0(%[j])\n\t"
      "fld ft1,  8(%[j])\n\t"
      "fld ft2, 16(%[j])\n\t"
      "fld ft3, 24(%[j])\n\t"
      "fld ft4, 32(%[j])\n\t"
      "fld ft5, 40(%[j])\n\t"
      "fld ft6, 48(%[j])\n\t"
      "fld ft7, 56(%[j])\n\t"
      :
      : [j] "r"(junk)
      : "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7");
#endif
}

/* Leave the FP file as different from the program's as possible, and make the
 * faulting store succeed so the interrupted code resumes. */
static void on_segv(int sig) {
  (void)sig;
  handled++;
  mprotect((void *)trap_page, 4096, PROT_READ | PROT_WRITE);
  static double junk[NREG];
  for (int i = 0; i < NREG; i++) {
    junk[i] = -1000.0 - (double)i;
  }
  /* Last thing the handler does, so nothing after it reloads the registers. */
  clobber_fp(junk);
}

/* Load `in[0..8)` into eight caller-saved FP registers, store one byte to `page`
 * (which faults), then write the registers to `out[0..8)`. One asm block, so the
 * values are in registers across the fault and nowhere else. */
static void across_fault(const double *in, double *out, volatile char *page) {
#if defined(__x86_64__)
  __asm__ __volatile__(
      "movsd  0(%[i]), %%xmm0\n\t"
      "movsd  8(%[i]), %%xmm1\n\t"
      "movsd 16(%[i]), %%xmm2\n\t"
      "movsd 24(%[i]), %%xmm3\n\t"
      "movsd 32(%[i]), %%xmm4\n\t"
      "movsd 40(%[i]), %%xmm5\n\t"
      "movsd 48(%[i]), %%xmm6\n\t"
      "movsd 56(%[i]), %%xmm7\n\t"
      "movb $1, (%[p])\n\t"
      "movsd %%xmm0,  0(%[o])\n\t"
      "movsd %%xmm1,  8(%[o])\n\t"
      "movsd %%xmm2, 16(%[o])\n\t"
      "movsd %%xmm3, 24(%[o])\n\t"
      "movsd %%xmm4, 32(%[o])\n\t"
      "movsd %%xmm5, 40(%[o])\n\t"
      "movsd %%xmm6, 48(%[o])\n\t"
      "movsd %%xmm7, 56(%[o])\n\t"
      :
      : [i] "r"(in), [o] "r"(out), [p] "r"(page)
      : "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "memory");
#elif defined(__aarch64__)
  __asm__ __volatile__(
      "ldp d0, d1, [%[i], #0]\n\t"
      "ldp d2, d3, [%[i], #16]\n\t"
      "ldp d4, d5, [%[i], #32]\n\t"
      "ldp d6, d7, [%[i], #48]\n\t"
      "strb wzr, [%[p]]\n\t"
      "stp d0, d1, [%[o], #0]\n\t"
      "stp d2, d3, [%[o], #16]\n\t"
      "stp d4, d5, [%[o], #32]\n\t"
      "stp d6, d7, [%[o], #48]\n\t"
      :
      : [i] "r"(in), [o] "r"(out), [p] "r"(page)
      : "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "memory");
#elif defined(__riscv)
  __asm__ __volatile__(
      "fld ft0,  0(%[i])\n\t"
      "fld ft1,  8(%[i])\n\t"
      "fld ft2, 16(%[i])\n\t"
      "fld ft3, 24(%[i])\n\t"
      "fld ft4, 32(%[i])\n\t"
      "fld ft5, 40(%[i])\n\t"
      "fld ft6, 48(%[i])\n\t"
      "fld ft7, 56(%[i])\n\t"
      "sb zero, 0(%[p])\n\t"
      "fsd ft0,  0(%[o])\n\t"
      "fsd ft1,  8(%[o])\n\t"
      "fsd ft2, 16(%[o])\n\t"
      "fsd ft3, 24(%[o])\n\t"
      "fsd ft4, 32(%[o])\n\t"
      "fsd ft5, 40(%[o])\n\t"
      "fsd ft6, 48(%[o])\n\t"
      "fsd ft7, 56(%[o])\n\t"
      :
      : [i] "r"(in), [o] "r"(out), [p] "r"(page)
      : "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "memory");
#else
#error "sig_fp: no caller-saved FP register set named for this ISA"
#endif
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_segv;
  if (sigaction(SIGSEGV, &sa, NULL) != 0) {
    puts("sigfp: sigaction failed");
    return 1;
  }

  trap_page = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (trap_page == MAP_FAILED) {
    puts("sigfp: trap page mmap failed");
    return 1;
  }

  /* Distinctive values, none of which the handler produces. */
  double in[NREG], out[NREG];
  for (int i = 0; i < NREG; i++) {
    in[i] = 100.5 + (double)i;
    out[i] = 0.0;
  }

  across_fault(in, out, trap_page);

  if (handled != 1) {
    printf("sigfp: handler ran %d times, want 1\n", (int)handled);
    return 1;
  }
  if (*trap_page != 1 && *trap_page != 0) {
    puts("sigfp: trap page not written");
    return 1;
  }

  for (int k = 0; k < NREG; k++) {
    if (out[k] != in[k]) {
      printf("sigfp: fp%d = %.1f, want %.1f (the handler's values survived)\n", k,
             out[k], in[k]);
      return 1;
    }
  }

  puts("sigfp: FP registers survived the handler");
  return 0;
}
