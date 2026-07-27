# Engineering Standard - How Work Lands Here

**Status:** v1.0. The unified engineering standard for this repository: the
principles every change obeys, the evidence every claim needs, and the patterns
that have proven to work. Complements the other method docs rather than
repeating them - ARCHITECTURE.md 6 governs *what may enter the kernel*,
VALIDATION.md *what each profile must prove*, TOOLING.md *how performance is
measured*, DEVELOPMENT.md *the day-to-day mechanics*. This document is about
**quality**: how to be sure a thing works, and how to say what is true about it.

Every principle below was forced by a real defect in this codebase. The case is
cited with each rule, because a rule without its scar is easy to talk past.

## 1. Observe, never infer

**Rule.** A capability may never be claimed from a feature bit, a register
write, or a successful-looking return. It must be **observed working**, and the
observation must be **unfakeable** by the code under test.

**The case.** x86-64 drove the LAPIC through the x2APIC MSR block for several
phases. QEMU-TCG with `-cpu max` reports `CPUID.01H:ECX[21] = 0`, so `EXTD`
never latched and `TMCCT` read `0` - permanently "already expired". Nothing
checked. Consequences: `SYS_ARM_TIMER` returned **immediately** (a `sleep` that
never slept), a documented "timer-backed idle at ~1% duty cycle" was in fact a
spin that reported `did_idle() == true`, and `docs/` claimed the timer was
interrupt-driven on all three ISAs. It surfaced only when a later phase made the
halt *measured* instead of asserted, and it invalidated a conclusion drawn about
the x86 UART/IOAPIC path, which had rested on the same inert registers.

**How it ended.** The wording was corrected first (an honest fallback), then the
*capability* was fixed: the LAPIC is now reached over the **xAPIC MMIO** page, which
QEMU does model, with the access mode chosen by probe - x2APIC requested, `EXTD`
read back, kept only if it latched. The timer is genuinely interrupt-driven on all
three ISAs (docs/SMP.md 5). Enabling it immediately exposed a second instance of the
same rule: the LAPIC tick-rate calibration busy-spun a fixed window *inside the
first `timer_arm`*, so the first `sleep` on a fresh kernel had its whole deadline
consumed by bring-up cost and reported no park. Bring-up cost belongs at bring-up.

**And the invalidated conclusion was re-tested, not just flagged.** The x86
UART/IOAPIC verdict turned out to be wrong: with a working EOI that path
re-delivers, so the UART RX line is now interrupt-driven there too, probed by a
loopback byte and a handler-only counter (docs/SMP.md 8). x86 AP bring-up was
blocked by the *same* inert registers, since INIT-SIPI-SIPI goes through the
interrupt command register - one root cause behind three "separate" blockers. When
a capability lie is found, every conclusion that touched it is a suspect.

**The good pattern, already in the tree.** The x86 FP/XSAVE bring-up enables
`XCR0`, **reads it back, and records only the bits that stuck** - graceful
fallback to `FXSAVE`/SSE when a component is dropped. Enable, verify, keep what
survived.

**Required practice.**
- Probe at bring-up: arm/inject/enable, then confirm the effect **from the other
  side** - a counter incremented *only* inside the interrupt vector, a value read
  back out of the device, a byte that actually arrived.
- Prefer evidence the implementation cannot manufacture: handler-only counters
  (`irq_count`), pointer-range checks (a parsed slice must lie *inside* the input
  buffer), park/wake counters that must equal the message count (`net_wakeups`,
  `chan_wakeups`), instruction counters from the emulator.
- Record the *validated* capability, not the requested one, and let every
  downstream decision read the validated value.
- Boot-time verification is the cheapest place to do this: see the boot
  hardware-features/health/bench probe. A silent capability lie found at boot
  costs minutes; found three phases later it invalidates conclusions.

## 2. Express time, not iterations

**Rule.** Waiting APIs take a **duration or deadline**. Never a poll count, spin
count, or retry count.

**The case.** `zeroconf::claim(polls_per_probe)` took a drain count. Under one
wait mechanism (interrupt park) a "poll" meant one blocking wait; under another
(bounded spin) it meant one cheap loop - so the same argument produced wildly
different real durations per ISA, and the parameter leaked the mechanism into a
protocol API. Separately, the network wait's exit condition was a spin budget
rather than the caller's deadline, so `timeout_ns` did not mean the same thing on
every ISA. Both were fixed by making time the unit: five APIs moved from counts
to durations (`zeroconf::claim`, `mdns::query`, `dhcp::configure`, `ntp::query`,
plus new `recv_*_timeout` entry points), and the spin budget was demoted to a
backstop that cannot fire before the deadline.

**Required practice.**
- Deadlines are monotonic and absolute internally; durations at the API edge.
- A budget/limit may exist only as a **safety backstop** that provably cannot
  truncate a caller's stated deadline.
- If two implementations of a wait cannot honour the same parameter identically,
  the parameter is wrong - not the implementations.

## 3. One owner per shared resource

**Rule.** A shared hardware resource has exactly **one** software owner. Other
subsystems register intent with that owner; they never touch the device.

**The case.** `arch::timer_arm`/`timer_disarm` drive a single hardware one-shot.
Both the network receive wait (deadlines *and* poll slices) and `SYS_ARM_TIMER`
(every cell `sleep`/`timeout`/`interval`) armed it directly. Last-armer-wins, and
`timer_disarm()` on the way out **cancelled whichever deadline another subsystem
had armed**. It was latent only because the system is single-CPU cooperative and
no test ran both concurrently; it would have become a silent lost-RTO /
overslept-timeout bug class the moment a pacer re-armed the timer continuously.
Fixed with a kernel timer arbiter: fixed slots per client, nearest-deadline
arming, and on fire *every* due client marked before the next is armed.

**Required practice.**
- Enforce the invariant **by construction**, not by convention: the
  arm-wait-disarm helper that made the bad pattern expressible was **deleted from
  all three ISAs** and replaced by a halt-only primitive, so no per-ISA path can
  own the device behind the arbiter's back.
- Document the invariant at the owner, at each per-ISA primitive it wraps, and in
  the subsystem doc - a future subsystem must trip over it.
- Prove it with a **conflict regression test** that reproduces the old pattern
  and asserts it fails the property (see 4).

## 4. Prove deterministically; live paths are a bonus

**Rule.** The core proof of a feature must be deterministic and independent of
any external service or network. Live/integration paths are additive, and must
**degrade with a printed reason** rather than fail or fake.

**The case.** This is the pattern that made two dozen network phases landable in
an emulator with no peers: TCP's full lifecycle (handshake, bidirectional data,
a **dropped segment retransmitted after RTO**, teardown) proven over an in-cell
`VirtualLink` driven by a logical clock; multi-hop traceroute proven by feeding a
state machine synthetic Time-Exceeded replies, since the emulator has no
intermediate routers; DHCP proven by crafted OFFER/ACK packets built with our own
codec (exercising encode *and* decode) plus timer-wheel-driven renewal and
expiry; congestion control proven by scripted loss/ACK sequences against
hand-computed cwnd trajectories.

**Required practice.**
- Build the deterministic harness first: a virtual link, a logical clock, and
  synthetic peer responses. It is faster to iterate against and it is what CI can
  rely on.
- **Hand-compute the oracle.** Assert against a value derived independently of
  the implementation - never against what the code currently produces.
- Live phases print one line from a **fixed set** of accepted outcomes, and the
  test asserts membership in that set. This keeps transcripts exact while
  admitting genuine environmental variation.
- Never synthesise a result the environment did not produce. "Skipped, and why"
  is a passing outcome; a fabricated success is not.

## 5. Known-answer provenance

**Rule.** Test vectors come from an authoritative source, verified. Never from
memory.

**The case.** An Ed25519 test seed recalled from memory was **wrong**; the phase
caught it by fetching RFC 8032 from an authoritative source instead of trusting
recall. For HPACK, the RFC editor's host was unreachable, so RFC 7541 was pulled
from a mirror and its **git blob SHA cross-checked against five independent
repositories**; the 257-row Huffman table and the static table were then
**generated by script**, with the generator asserting every code fits its
declared length, the code is prefix-free, and codes of equal length are
consecutive. No hand-typed hex.

**Required practice.**
- Fetch the spec text; cite it; cross-check the artefact when the canonical host
  is unavailable.
- Generate large tables mechanically and assert their **structural invariants**,
  which is what lets a decoder be a lookup rather than a tree.
- A vector computed by the implementation under test proves nothing.

## 6. Rejections are deliverables

**Rule.** For any parser or protocol that faces hostile input, the **negative**
behaviour is a first-class feature with its own tests, and each rejection has its
own distinguishable error.

**The case.** The HTTP codec asserts **22 request-smuggling shapes** rejected -
`Content-Length` with `Transfer-Encoding` in either order, duplicate
`Content-Length` even when the values agree, `5, 5` / `+5` / `0x5`, a non-final
`chunked` coding, bare LF where CRLF is required, obs-fold, non-token names,
control bytes in values, oversized/too-many headers, a double space in the
request line, a non-1.x version - each with a distinct error, plus four chunked
framing rejections where a non-empty trailer is refused rather than dropped. The
DNS name parser bounds pointer jumps, rejects out-of-range offsets and caps
assembled length, with three crafted attack packets (self-pointer, mutual cycle,
out-of-bounds) each asserted to *error* rather than hang.

**Required practice.**
- Enumerate the attack shapes for the format and assert each one individually.
- Distinct errors, not a single "invalid" - a filter that cannot tell shapes
  apart cannot be reasoned about, and request smuggling is a *parser* property,
  not something a downstream filter can add.
- Prove termination on adversarial input (bounded jumps, bounded recursion,
  bounded buffers) as explicitly as you prove the happy path.

## 7. Say exactly what is true

**Rule.** Scope language is part of the deliverable. Distinguish built, proven,
partially proven, and deferred - in code comments, docs, and reports.

**The case.** The vocabulary this program settled on, each earned:
- **"implemented but unproven"** - remote TCP data transfer exists and is
  reachable, but the emulator offers no TCP responder, so there is no
  deterministic round trip to assert. Stated in three places rather than implied
  as covered.
- **"reported, not asserted"** - the emulator *does* answer DHCP, so a real lease
  is obtained and printed, but never asserted: a lease is a property of the
  backend, not of this code.
- **"concurrent, not parallel"** - one CPU, cooperative scheduling. Every
  fan-out claim is a concurrency claim; parallelism awaits SMP.
- **Three wait modes, not two** - device-interrupt park, timer-backed idle, and
  bounded poll are named separately, because collapsing the last two into "idle"
  is how the x86 spin passed for a halt (see 1).
- **Attribution** - external published measurements are cited as *theirs*.
  Numbers this repository cannot reproduce are never restated as ours.

**Required practice.**
- A fallback must never be described in the vocabulary of the real thing.
- When a phase is partial, name what is missing and what would close it. A
  precise partial result is a good outcome; an overstated complete one is a bug
  in the documentation.
- Correct the record when measurement contradicts an earlier claim, including in
  the docs that carried it.

## 8. Compose before extending; change additively

**Rule.** Prefer composing existing mechanisms. When new mechanism is genuinely
required, prove composition impossible first, then add the minimum - and add it
so that existing proofs stay byte-for-byte valid.

**The cases.**
- *Compose:* Linux socket support gained a `svc::SocketOps` function-pointer
  table mirroring `svc::FileOps`, so the kernel gained **a bridge, not a network
  stack** - staying allocation-free and stack-free exactly as it stays
  filesystem-free. AF_UNIX and loopback INET reuse the existing cross-cell ring;
  no new kernel object.
- *Prove impossibility first:* service fan-out looked composable until tracing
  showed a cell held **one** channel end at a fixed address (so N spawns would
  share one SPSC ring - a race) and the directed switch is `cur ^ 1` (so clients
  ping-pong and the service starves - a livelock). Both were found by reading the
  code before designing, and only then was minimal mechanism added.
- *Additively:* ALPN was added so that with an empty list the ClientHello is
  **byte-identical**, leaving the TLS known-answer test untouched. The congestion
  control trait was extended with **default methods** so existing controllers
  behave identically. New dependencies and SMP live behind cargo features so
  pre-existing kernels link an **unchanged** library.

**Required practice.**
- Read and trace before designing; report what you found, not what you assumed.
- Gate new dependencies and risky subsystems behind features so the default build
  is provably unaffected.
- After an additive change, re-run the *old* proofs unchanged. If they needed
  editing, the change was not additive.

## 9. Profiles, not magic numbers

**Rule.** A tuning constant that must differ by deployment belongs to a named
profile, with the trade-off documented.

**The case.** A single fixed 500 µs receive poll slice is simultaneously too much
latency for latency-critical work, wasteful for battery-powered deployments, and
mediocre for batch throughput. Replaced with tiered, profile-scoped constants
(`hft` / `edge` / `warehouse` / `embedded`) covering the hot-window, spin count,
slice length and count. The same applies to congestion-control selection,
forward-error-correction on/off, and buffer sizing.

**Required practice.** Name the profile, state the trade-off each constant makes,
and give the default profile's reasoning explicitly.

## 10. Emulation is a target, not an excuse

**Rule.** The system must behave **correctly** under emulation - that is a
supported platform, not a test rig. Equally, never claim from emulation what it
cannot measure.

**The case.** A receive wait that hot-spun on one ISA was first "fixed" by
shrinking a test parameter. That was wrong twice over: it did not fix the hang
(the real bug was an infinite loop in an announce state machine that returned a
frame forever once claimed), and it treated correct behaviour under emulation as
optional. The right fix made the wait honour deadlines on every ISA and idle
wherever *any* wake source exists.

**Required practice.**
- Never tune a test to route around a behavioural defect. If a parameter must
  shrink to pass, find out why.
- Performance claims from the emulator are **deterministic instruction path
  lengths** only (TOOLING.md 4). Wall-clock throughput, jitter and line rate are
  hardware-lab measurements and are labelled as such - or as someone else's
  published result, with attribution.

## 11. Recorded hazards

Specific traps this codebase has hit, kept here so they are not re-learned.

- **Layout sensitivity is a real bug class.** A `static` stack used as the
  syscall dispatch stack had alignment 1. `SYSCALL` does not adjust `RSP` (unlike
  a hardware trap, which the CPU aligns), so correctness depended on where the
  linker happened to place it; any code motion broke SSE spills in the formatting
  path and produced a corrupted-length string. Two separate phases were derailed
  before the root cause was found. Fix alignment at the definition
  (`repr(align(16))`); never work around code motion.
- **Clock domains are not interchangeable.** A cycle counter (instructions
  retired, under emulation) is not a wall-clock or timer counter. Using one for
  the other yields deadlines that are wrong by an arbitrary factor. Use the
  timer's own domain for deadlines, and cross-check the two counters at boot.
- **Cargo feature unification can break a build that works per-crate.** Building
  two packages in one invocation can re-enable a feature that must stay off for
  one of them (here: a cell-side crate supplying `_start`/panic handler/allocator
  cannot be linked into a kernel binary). Keep such builds in separate
  invocations and record the constraint in both manifests.
- **A state machine must not serve both a bounded sequence and an unbounded
  steady state from one entry point.** An announce routine that also served
  post-claim defence returned a frame forever once claimed, so
  `while let Some(x) = next()` never terminated. Split bounded from unbounded and
  assert the boundedness.

## 12. Never dereference an address the caller chose

**Rule.** A raw address supplied by a **cell** may never be dereferenced in
kernel mode. It must first be bounded: non-null, correctly aligned for the type,
`base + len` free of overflow, and wholly inside the calling cell's user VA
range. A rejected address is a **refused syscall** - the appropriate error, no
write, no read, no fault, no panic. A `SAFETY` comment on such a dereference must
cite the check that ran, never the caller's good behaviour.

**The case.** There was no `access_ok` equivalent anywhere in the tree. Every
out-parameter syscall, every queue-payload VA and every buffer handed to a
personality handler wrote or read straight through a cell-supplied address, while
the kernel serviced the trap in S-mode/EL1/ring 0 **with the calling cell's root
active** - and every cell root maps all of kernel RAM supervisor-RWX through the
linear map. So `SYS_GRANT(out_va = <any kernel VA>, ...)` was a 16-byte arbitrary
kernel write with a cell-steerable first word, reachable from one line of
unprivileged code. The `SAFETY` comments said "`out_va` is a user VA in the
running cell's active address space" - an assumption about the caller, written as
if it were a fact about the code. Two length arguments (`readv`'s `iovcnt`,
`poll`'s `nfds`) were likewise walked unbounded.

Three consequences worth separating, because each needs its own kind of check:

- **A cell-supplied address** needs a range check (this rule).
- **A cell-supplied length** needs a *budget*: `SYS_MMAP` computed
  `len.div_ceil(4096)` pages and looped the frame allocator, which ended in
  `panic!("frame pool exhausted")` - so `mmap(1 << 40)` took the machine down.
  ARCHITECTURE.md 5 forbids an OOM killer; an OOM **panic** is strictly worse.
  Allocation on a path a cell can drive must be *refusable*, must be charged
  against a per-cell limit, and must leave a global reserve the kernel's own
  allocations draw from. Where a partial result is meaningless the failure rolls
  back (a fresh `mmap` frees what it took and returns 0); where it is not, say so
  - a `mprotect` that commits some pages and then cannot commit the rest keeps
  them, because reversing a *reprotect* would discard page contents the cell
  already had. Both behaviours are fine; leaving which one applies unstated is
  not.
- **A cell-supplied address that names a resource** needs an *ownership* check,
  not just a range check. `SYS_MUNMAP` freed whatever the page tables gave back;
  three frame sets in a cell's address space are not its own (a shared channel
  ring, a peer's shared sealed grant, its own queue-pair region), so this was a
  cross-cell use-after-free, and the second free tripped a "double free"
  assertion: a kernel panic from unprivileged code.

**The good pattern, already in the tree.** `grant_resolve` - the gate on
`SYS_COMMIT`/`DECOMMIT`/`SEAL` - resolves a cell-supplied handle by *checking a
live capability with the right it needs* before touching anything. F3 was not a
failure of that pattern but a failure to **extend** it: `SYS_MUNMAP` now goes
through the same call, and a peer's shared grant is refused for free, because the
capability minted into the peer carries READ and not MAP.

**Required practice.**
- One validation helper, portable, allocation-free, on the syscall path: a null
  test, an alignment test, an overflow-checked add and one or two compares. No
  page-table walk per syscall (the P1 grant-check budget is < 50 ns p99).
- Derive the bound from the ISA with the **narrowest** user half, so "below the
  kernel half" holds everywhere, and pin every region base below it with a
  `const` assertion, so moving a region cannot silently escape the bound.
- Validate *before* any state changes. A refused call must consume no capability,
  no table slot, no frame, and no admission budget.
- Bound the whole surface at **one** place per ABI. Spreading the
  number-to-pointer-argument map over dozens of handlers is exactly what let this
  go unchecked; the Linux personality is bounded at its single dispatch point.
- Where a length is itself an argument, use it: that bounds the array walk too.
- A **NUL-terminated** argument is the one shape whose length the caller never
  states, so the entry check can only bound its first byte. The scan for the
  terminator must carry its own bound - how much of the cell's range remains at
  that pointer - or a string placed at the top of the range walks the scan into
  the kernel half. Same for a NULL-terminated *pointer* array (`execve`'s argv).
- Prove it from an unprivileged cell, against evidence the cell cannot fake - a
  canary word in memory it has no mapping for, a frame-pool count, a resource
  that still works after the refusal - and prove the legitimate path still works
  in the same test.

## 13. The checklist

Applied to every slice of work, in order. This is the unified practice the rest
of this document argues for.

**Before writing code**
1. Read the seams and *trace* the constraint. Report what the code actually does.
2. If new kernel mechanism seems needed, prove composition impossible, then meet
   ARCHITECTURE.md 6 and document before implementing.
3. Choose the deterministic proof and its **hand-computed oracle** up front.

**While building**
4. Express waits as deadlines; give shared resources a single owner.
5. Put tuning constants in profiles; put new dependencies behind features.
6. Keep changes additive so existing proofs remain valid unchanged.
7. Concentrate `unsafe` in small blocks with a `SAFETY` comment; keep per-ISA
   code inside the arch layer.
8. **Never dereference a cell-supplied address, and never allocate or free on a
   cell-supplied length or handle, without a check** - range, budget, ownership
   (section 12). A `SAFETY` comment states the check that ran, not the caller's
   good behaviour.

**Proving it**
9. Deterministic core assertions, unfakeable evidence, adversarial rejections
   each with a distinct error.
10. Probe every capability claim; record the validated value, not the requested
    one.
11. Live paths additive, degrading with a printed reason; nothing synthesised.
12. Full matrix green on all three ISAs; every pre-existing test still passing;
    formatter and linter clean across the documented target set.

**Reporting it**
13. State scope precisely: built / proven / partially proven / deferred, with
    what would close each gap. Attribute external numbers. Correct the record -
    including the docs - when measurement contradicts an earlier claim.
