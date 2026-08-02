/* sysx - the seven syscalls the real Claude Code binary issues that the Linux
 * personality did not dispatch (docs/ARCHITECTURE-DEBT.md 4.0, blocker 3).
 *
 * They were measured, not guessed: `strace` on the real binary running
 * `claude --version` to completion. Six are advisory (a program keeps going
 * without them, or glibc has a documented fallback); `eventfd2` is not - it is
 * the epoll event loop's only wakeup path.
 *
 * Each phase prints one line from a fixed set so the transcript stays exact.
 * Where the honest answer is a refusal, the *refusal* is what is asserted - a
 * call that reported success while doing nothing is the defect class this whole
 * programme is removing (docs/ENGINEERING.md 7).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

/* A path the harness seeds for this fixture, used only as something openable. */
#define OWN_FILE "/etc/sysx.txt"

int main(void) {
  /* 0. The **legacy** `open`. On x86-64 glibc issues syscall 2 for `open()` in
   *    preference to `openat`, so a personality implementing only `openat` refused
   *    every `open` on that ISA and nowhere else - the two-numbers hazard
   *    (docs/ENGINEERING.md 11). The plain `open()` calls in phase 6 already ride
   *    that path; this phase names it, raw, where the number exists at all. The
   *    asm-generic ISAs have no such syscall, so there is nothing to test there and
   *    the line says so rather than pretending. */
#ifdef SYS_open
  int lo = (int)syscall(SYS_open, OWN_FILE, O_RDONLY, 0);
  if (lo < 0) {
    puts("open: legacy open refused");
    return 1;
  }
  close(lo);
  puts("open: legacy open(2) works");
#else
  puts("open: no legacy open on this ABI (openat only)");
#endif

  /* 1. eventfd2 - the load-bearing one. A counter, readable only when non-zero,
   *    and pollable, which is what makes it a wakeup. Assert the whole contract:
   *    an empty one is NOT readable (a poll that claims it is would wake an event
   *    loop forever), a write makes it readable, a read drains it, and the
   *    counter accumulates across writes. */
  int efd = eventfd(0, EFD_NONBLOCK);
  if (efd < 0) {
    puts("eventfd: create failed");
    return 1;
  }
  struct pollfd pf = {.fd = efd, .events = POLLIN};
  if (poll(&pf, 1, 0) != 0) {
    puts("eventfd: empty counter reported readable");
    return 1;
  }
  uint64_t v = 0;
  if (read(efd, &v, sizeof v) != -1 || errno != EAGAIN) {
    puts("eventfd: empty read did not report EAGAIN");
    return 1;
  }
  uint64_t one = 1, six = 6;
  if (write(efd, &one, sizeof one) != 8 || write(efd, &six, sizeof six) != 8) {
    puts("eventfd: write failed");
    return 1;
  }
  pf.revents = 0;
  if (poll(&pf, 1, 0) != 1 || !(pf.revents & POLLIN)) {
    puts("eventfd: written counter not reported readable");
    return 1;
  }
  if (read(efd, &v, sizeof v) != 8 || v != 7) {
    printf("eventfd: read %llu, expected 7\n", (unsigned long long)v);
    return 1;
  }
  /* Drained: not readable again. */
  pf.revents = 0;
  if (poll(&pf, 1, 0) != 0) {
    puts("eventfd: drained counter still readable");
    return 1;
  }
  /* A short buffer is EINVAL, not a truncated read. */
  uint32_t half = 0;
  if (read(efd, &half, sizeof half) != -1 || errno != EINVAL) {
    puts("eventfd: 4-byte read did not report EINVAL");
    return 1;
  }
  puts("eventfd: empty not readable, 1+6 read as 7, drained, short read EINVAL");

  /* 2. A dup'd eventfd must share the counter. This is the property that a
   *    counter stored per descriptor would silently break: writing one fd and
   *    reading the other has to work, or an event loop that dup'd its wakeup fd
   *    stops being woken. */
  int dup_fd = dup(efd);
  if (dup_fd < 0) {
    puts("eventfd: dup failed");
    return 1;
  }
  if (write(dup_fd, &one, sizeof one) != 8) {
    puts("eventfd: write to dup failed");
    return 1;
  }
  if (read(efd, &v, sizeof v) != 8 || v != 1) {
    puts("eventfd: dup did not share the counter");
    return 1;
  }
  puts("eventfd: dup shares the counter");

  /* 3. EFD_SEMAPHORE decrements by one instead of draining. */
  int sem = eventfd(3, EFD_NONBLOCK | EFD_SEMAPHORE);
  if (sem < 0 || read(sem, &v, sizeof v) != 8 || v != 1) {
    puts("eventfd: semaphore mode did not yield 1");
    return 1;
  }
  if (read(sem, &v, sizeof v) != 8 || v != 1) {
    puts("eventfd: semaphore mode second read wrong");
    return 1;
  }
  puts("eventfd: semaphore mode yields 1 per read");
  close(sem);
  close(dup_fd);
  close(efd);

  /* 4. sysinfo - real numbers. Bun sizes its heap from totalram/freeram, so a
   *    zeroed answer is worse than a refusal. Assert only what must be true
   *    (never the exact figures, which are a property of the build): memory is
   *    non-zero, free never exceeds total, the unit is 1 byte, and at least this
   *    process is counted. */
  struct sysinfo si;
  memset(&si, 0xaa, sizeof si);
  if (sysinfo(&si) != 0) {
    puts("sysinfo: call failed");
    return 1;
  }
  if (si.totalram == 0 || si.freeram > si.totalram) {
    puts("sysinfo: memory figures not plausible");
    return 1;
  }
  if (si.mem_unit != 1) {
    puts("sysinfo: mem_unit is not 1");
    return 1;
  }
  if (si.procs < 1) {
    puts("sysinfo: no processes counted");
    return 1;
  }
  puts("sysinfo: real totals, free <= total, mem_unit 1, procs >= 1");

  /* 5. Scheduling policy, honestly. There is one class - cooperative
   *    round-robin - so asking for SCHED_OTHER at priority 0 succeeds because
   *    that is already true, and asking for SCHED_FIFO is refused rather than
   *    accepted and dropped: a program told it got real-time scheduling here
   *    would be lied to. */
  struct sched_param sp = {.sched_priority = 0};
  if (sched_setscheduler(0, SCHED_OTHER, &sp) != 0) {
    puts("sched: SCHED_OTHER refused");
    return 1;
  }
  struct sched_param rt = {.sched_priority = 50};
  if (sched_setscheduler(0, SCHED_FIFO, &rt) != -1 || errno != EPERM) {
    puts("sched: SCHED_FIFO not refused with EPERM");
    return 1;
  }
  if (sched_getscheduler(0) != SCHED_OTHER) {
    puts("sched: getscheduler is not SCHED_OTHER");
    return 1;
  }
  if (sched_get_priority_max(SCHED_OTHER) != 0 ||
      sched_get_priority_min(SCHED_OTHER) != 0) {
    puts("sched: SCHED_OTHER priority range is not 0..0");
    return 1;
  }
  puts("sched: SCHED_OTHER ok, SCHED_FIFO EPERM, range 0..0");

  /* 6. close_range. glibc has a close-loop fallback, so this is a performance
   *    call - but it must actually close. Open three fds, close the middle range,
   *    and check exactly those went. */
  int a = open(OWN_FILE, O_RDONLY);
  int b = open(OWN_FILE, O_RDONLY);
  int c = open(OWN_FILE, O_RDONLY);
  if (a < 0 || b < 0 || c < 0 || b != a + 1 || c != b + 1) {
    puts("close_range: setup failed");
    return 1;
  }
  long r = syscall(SYS_close_range, (unsigned)a, (unsigned)b, 0u);
  if (r != 0) {
    puts("close_range: call failed");
    return 1;
  }
  if (fcntl(a, F_GETFD) != -1 || errno != EBADF) {
    puts("close_range: first fd still open");
    return 1;
  }
  if (fcntl(b, F_GETFD) != -1 || errno != EBADF) {
    puts("close_range: second fd still open");
    return 1;
  }
  if (fcntl(c, F_GETFD) < 0) {
    puts("close_range: fd past the range was closed");
    return 1;
  }
  close(c);
  puts("close_range: closed the range and nothing beyond it");

  /* 7. clone3 is now *implemented* (GOAL-BUN: Bun's JavaScriptCore issues clone3
   *    directly, with no glibc clone fallback, so refusing it is a hard failure).
   *    It decodes `struct clone_args` and routes to the same thread/process path as
   *    legacy `clone`. Probed here with a null cl_args + size 0, which a working
   *    clone3 rejects with EINVAL (a too-small struct) - NOT ENOSYS. So the honest
   *    assertion flipped: EINVAL proves the number is known *and handled*, where
   *    ENOSYS would now be the regression.
   *
   *    rseq stays refused ENOSYS deliberately - glibc's fallback is "no restartable
   *    sequences", so ENOSYS is the correct answer and a success would mislead. */
  if (syscall(SYS_clone3, NULL, 0ul) != -1 || errno != EINVAL) {
    puts("clone3: not implemented (want EINVAL for a null cl_args)");
    return 1;
  }
  if (syscall(SYS_rseq, NULL, 0u, 0u, 0u) != -1 || errno != ENOSYS) {
    puts("rseq: not refused with ENOSYS");
    return 1;
  }
  puts("clone3: implemented (EINVAL on bad args); rseq: refused ENOSYS");

  /* 8. capget - a non-root process's capability query (Node.js probes it nine
   *    times at startup). The honest answer for our unprivileged identity (uid
   *    1000, no caps) is empty capability sets, not a stub that claims caps the
   *    process does not have. The kernel also answers the version-probe protocol:
   *    an unknown version returns EINVAL with the supported version written back. */
  {
    struct {
      uint32_t version;
      int pid;
    } hdr;
    struct {
      uint32_t eff, perm, inh;
    } data[2];
    hdr.version = 0x20080522; /* _LINUX_CAPABILITY_VERSION_3 */
    hdr.pid = 0;
    memset(data, 0xff, sizeof data);
    if (syscall(SYS_capget, &hdr, data) != 0) {
      puts("capget: v3 query failed");
      return 1;
    }
    if (data[0].eff | data[0].perm | data[0].inh | data[1].eff | data[1].perm |
        data[1].inh) {
      puts("capget: non-empty capabilities");
      return 1;
    }
    hdr.version = 0xdeadbeef;
    if (syscall(SYS_capget, &hdr, (void *)0) != -1 || errno != EINVAL) {
      puts("capget: unknown version not refused");
      return 1;
    }
    if (hdr.version != 0x20080522) {
      puts("capget: version probe not answered");
      return 1;
    }
    puts("capget: empty caps, version probe answered");
  }

  /* 9. io_uring - refused ENOSYS deliberately, the clone3/rseq class. Node 22's
   *    libuv probes io_uring_setup at startup and falls back to epoll+threadpool
   *    when it is ENOSYS (observed in the real `node` trace); our async path is
   *    the queue-pair reactor, not io_uring, so the refusal is a design
   *    statement. The number is *known* and answered ENOSYS - not the
   *    unknown-number log. (A real Linux would answer EINVAL for 0 entries; this
   *    fixture only ever runs under the rheo-os personality.) */
  if (syscall(SYS_io_uring_setup, 0u, (void *)0) != -1 || errno != ENOSYS) {
    puts("io_uring: not refused with ENOSYS");
    return 1;
  }
  puts("io_uring: refused ENOSYS deliberately");

  /* 9a. Writing to /dev/urandom seeds the kernel entropy pool
   *     (docs/TIME-IDENTITY.md 4a). It used to be discarded while returning
   *     success - the stub-reporting-success shape docs/ENGINEERING.md 7
   *     rejects. From in here all that can be checked is that the write is
   *     accepted and the device still reads; that the bytes reached the pool is
   *     asserted kernel-side by the counters, which this program cannot see and
   *     therefore cannot fake. Exactly URANDOM_WRITE bytes, because the kernel
   *     side asserts that number. */
  {
    unsigned char seed[64];
    for (unsigned i = 0; i < sizeof seed; i++) {
      seed[i] = (unsigned char)(0x5a ^ i);
    }
    int uf = open("/dev/urandom", O_RDWR);
    if (uf < 0) {
      puts("urandom: open failed");
      return 1;
    }
    if (write(uf, seed, sizeof seed) != (ssize_t)sizeof seed) {
      puts("urandom: write not accepted");
      return 1;
    }
    unsigned char a[16], b[16];
    if (read(uf, a, sizeof a) != (ssize_t)sizeof a ||
        read(uf, b, sizeof b) != (ssize_t)sizeof b) {
      puts("urandom: read failed");
      return 1;
    }
    if (memcmp(a, b, sizeof a) == 0) {
      puts("urandom: two reads returned the same bytes");
      return 1;
    }
    close(uf);
    puts("urandom: 64-byte write accepted, reads still vary");
  }

  /* 10. The legacy clock reads the real `node` binary calls at startup (V8 +
   *     libuv): gettimeofday, clock_getres, and (x86-64 only) time. libuv's
   *     uv_gettimeofday *asserts* gettimeofday returns 0, so a stub that refused
   *     it aborted Node (docs/LINUX-COMPAT.md). Assert each returns success with a
   *     plausible, monotone-consistent value - never the exact figure. */
  {
    struct timeval tv1 = {0, 0}, tv2 = {0, 0};
    if (gettimeofday(&tv1, NULL) != 0 || tv1.tv_sec <= 0 || tv1.tv_usec < 0 ||
        tv1.tv_usec >= 1000000) {
      puts("gettimeofday: implausible");
      return 1;
    }
    struct timespec res = {-1, -1};
    if (clock_getres(CLOCK_MONOTONIC, &res) != 0 || res.tv_sec != 0 ||
        res.tv_nsec <= 0) {
      puts("clock_getres: implausible");
      return 1;
    }
    /* gettimeofday must not run backwards on a second read. */
    if (gettimeofday(&tv2, NULL) != 0 || tv2.tv_sec < tv1.tv_sec) {
      puts("gettimeofday: went backwards");
      return 1;
    }
#ifdef SYS_time
    time_t t = (time_t)syscall(SYS_time, NULL);
    time_t tstore = 0;
    if (t <= 0 || (time_t)syscall(SYS_time, &tstore) <= 0 || tstore < t) {
      puts("time: implausible");
      return 1;
    }
    puts("clocks: gettimeofday + clock_getres + time OK");
#else
    puts("clocks: gettimeofday + clock_getres OK (no legacy time on this ABI)");
#endif
  }

  puts("sysx OK");
  return 0;
}
