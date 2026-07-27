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

**The distinction that keeps this rule usable.** "Never tune a test parameter"
and "make the proof deterministic" can look like the same move, so the difference
must be stated. An idle-park assertion on x86-64 failed intermittently: the
orchestrator slept 2 ms and the kernel was asserted to have genuinely halted. The
boot tests run **without** `-icount`, so the guest's monotonic clock *is* host
wall-clock - a host scheduling hiccup between arming the deadline and reaching the
park consumes guest time the guest never spent, and a millisecond-scale deadline
can already be elapsed when the run loop gets there. An elapsed deadline
completes immediately and nothing idles, so the outcome was decided by host load
rather than by the kernel.

Raising the sleep to 50 ms is the *opposite* of the forbidden move, and the test
is which way the claim moves:
- **Forbidden:** the assertion is weakened, or a parameter is shrunk until the
  defect stops showing. The system still misbehaves; the proof stopped looking.
- **Required:** the assertion is untouched - it still demands a genuine halt - and
  the *stimulus* is enlarged until emulator timing noise can no longer decide the
  result. Nothing is routed around, because there was no defect to route around:
  the kernel halts correctly on any deadline still in the future.

It has now happened twice from the same cause, which is why it is a rule and not
an anecdote. The second was a timer-arbiter proof: 40 back-to-back pacer
deadlines had to land inside a 20 ms RTO to show that a continuously re-armed
client cannot destroy another client's deadline. The per-release assertions were
already written correctly - their author had reasoned explicitly that a loaded
host can deschedule QEMU for milliseconds, so a release running *past* the RTO
makes marking it due the correct answer - but a final `checked >= PACES / 2`
guard reintroduced the dependency the per-release logic had just excused. Under a
full matrix run only 10 of 40 landed inside the window. 40 releases are ~8 ms of
guest time, so the deadlines are now 200 ms and 400 ms: a 15x margin, measured
(13.46 ms observed), with every assertion unchanged.

Before enlarging a stimulus, prove which of the two you are doing by naming the
mechanism. If you cannot say *why* the old value was marginal, you are guessing,
and the answer is the forbidden one. And check the *whole* proof: reasoning
correctly about emulator timing in one assertion does not help if a summary guard
three lines later quietly depends on it.

**Required practice.**
- Never tune a test to route around a behavioural defect. If a parameter must
  shrink to pass, find out why.
- If a proof's outcome depends on host timing, fix the *proof's determinism* and
  say what the mechanism was - never the claim.
- Performance claims from the emulator are **deterministic instruction path
  lengths** only (TOOLING.md 4). Wall-clock throughput, jitter and line rate are
  hardware-lab measurements and are labelled as such - or as someone else's
  published result, with attribution.

## 11. Recorded hazards

Specific traps this codebase has hit, kept here so they are not re-learned.

- **Two encodings of the same thing that are *usually* equal.** A `Handle` is
  `(generation << 16) | slot`; its 32-bit ABI form is
  `((generation & 0xFFFF) << 16) | slot`. Those are the **same number** while a
  slot's generation stays below 2^16 - which it does in every test, every
  fixture, and every boot. A capability verb written against the 64-bit form
  would therefore have accepted the 32-bit form a cell actually holds, worked
  perfectly, and begun failing after 65536 reuses of one slot. The fix was not
  to convert carefully at each call site but to **delete the choice**: the verbs
  take the form the rest of the ABI already uses, and the wide form never
  crosses the boundary. When two encodings agree on all reachable inputs, that
  is not safety - it is a test that cannot fail.
- **A read must not be a write.** `grant_check` decrements a metered
  capability's budget, so the obvious way to answer "what does this handle
  carry?" - `grant_check(h, 0)` - *consumes* the thing being inspected. Any
  accessor built on an enforcement path inherits that path's side effects; give
  the read-only question its own function (`inspect_low32`) rather than passing
  a zero to the enforcing one.
- **An optimisation added at the same time as the feature can silently delete
  it.** The `mmap` bump cursor was replaced with first fit over a VMA list, whose
  whole point is that a freed span becomes reusable - and the same commit kept
  the old cursor as a "hint" for the search to start from, so that an
  allocation-heavy program would not rescan the low end of the region. A search
  that starts past every live mapping can never find a hole behind one, so the
  feature was present, documented, and did nothing. Two lessons, and the second
  is the load-bearing one:
  - Land the mechanism and the optimisation in **separate steps**, so the proof
    runs against the mechanism alone at least once.
  - **Assert the property, not the success.** The fixture asked for a specific
    *address* and got a different one, which is what caught it. Had it asserted
    only that the second `mmap` succeeded, it would have passed with the feature
    inert - and there is no amount of code review that reliably catches a
    correct-looking cache in front of a correct-looking search.

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
- **A good pattern gets copied instead of imported.** The measured shape of most
  structural debt in this tree is not bad judgement: it is an abstraction invented
  correctly, used once, and then re-typed at the next site. The on-wire ABI was
  hand-written twice (28 syscall numbers, 12 opcodes, 12 `repr(C)` structs) with a
  "keep in sync" comment as the only enforcement; the `svc` bridge that keeps the
  kernel filesystem-free sat twenty lines above four opcodes that named device
  drivers directly; ten separate `macro_rules!` were written for one per-ISA
  `include_bytes!`, two of them byte-identical *and on the same line number*. Each
  copy is individually cheap, which is why it happens - and each one is a place a
  future change can diverge silently. When you find yourself writing something
  that already exists, the correct cost to pay is the import, once.
- **A sentinel scheme needs a uniqueness check, and "clippy was clean" has a
  timestamp.** The Linux personality represents each x86-64-only verb by an
  unreachable `u64::MAX - n` sentinel in the asm-generic table, so one portable
  `match` compiles on all three ISAs. Adding `READLINK` as `MAX - 7` collided with
  `EPOLL_CREATE` and made `epoll_create` **dead code on two ISAs** - silently,
  because the boot tests only exercise `epoll_create1`. Clippy does report it as an
  unreachable pattern; I had run clippy in that slice *before* adding the
  constant, and reported the slice as lint-clean on the strength of that run. Two
  rules follow: re-run the gate after the last edit, not after the last edit you
  remember; and where a scheme's correctness is "all these values differ", assert
  it (`const _: () = { ... }` over the whole set) rather than leaving it to review
  - the guard here was verified to fire by reintroducing the collision.
- **A mechanical rename is not an edit to a proof.** Moving `arch::init` to
  `kernel::boot::init` touched 61 call sites, 59 of them test kernels - which
  looks like it violates "re-run the *old* proofs unchanged" (section 8). It does
  not: what section 8 protects is the **assertion set**, not the spelling of a
  call. State which one you changed. Renaming a boot call in 59 files while
  asserting exactly what was asserted before is additive; quietly relaxing one
  `assert!` in one file is not, however few lines it touches.
- **A state machine must not serve both a bounded sequence and an unbounded
  steady state from one entry point.** An announce routine that also served
  post-claim defence returned a frame forever once claimed, so
  `while let Some(x) = next()` never terminated. Split bounded from unbounded and
  assert the boundedness.
- **A build tool that ignores an option it does not understand turns a typo into
  a silent wrong answer.** The `PT_GNU_STACK` proof needs a fixture linked with a
  larger stack request. GNU `ld` spells that `-z stack-size=N`; `lld` spells it
  `-z stacksize=N`. Given the wrong spelling, `ld` **warns and links anyway**, so
  the fixture built cleanly with a `p_memsz` of **0** - and the test then failed
  against the *fixed* kernel, which is the worst possible direction to be wrong
  in: it accuses correct code. The general rule is that a proof's *inputs* need
  the same evidence discipline as its assertions. Assert the property you needed
  the tool to produce (here: `readelf` the fixture and check `p_memsz`), and be
  suspicious of any build step whose failure mode is a warning.
- **A wait must report what it observed, not that it waited.** `input::pump`
  injected a scripted byte, halted once for its UART RX interrupt, and returned
  "there is data" - on the strength of *having halted*. A halt ends on **any**
  enabled interrupt, so the moment the timer one-shot became real on every ISA
  (docs/SMP.md 5) a competing deadline could end it with the UART handler never
  having run: the ring was empty and `SYS_WAIT_INPUT` returned 0. It surfaced as a
  `schedidle` failure that passed on the next five runs, which is the worst
  possible shape - **a proof whose result depends on ordering is not a proof**, and
  a flaky proof also poisons the evidence for every unrelated change that runs
  after it. "I waited for X, therefore X happened" is false as soon as more than
  one thing can end the wait; check the condition, and count the times the
  fallback fired so a degraded path stays visible instead of being inferred away.
- **When a fix does not reproduce on demand, build the reproduction rather than
  shipping the reasoning.** The flake above depended on QEMU's interrupt-model
  timing, so there was nothing to revert that would fail reliably. The
  reproduction was to *suppress the interrupt line in the arch layer* - "the
  interrupt did not deliver" made deterministic - and then run both directions:
  with the fix, the byte is recovered from the UART FIFO and the run passes; with
  the inference restored, it fails with the exact original message. That is the
  difference between "reasoned and code-reviewed" (section 7's honest but weaker
  label) and proven.
- **A `fork` must add a reference to every refcounted thing it shares, and the cost of
  missing one is paid by the *other* process.** `dup_state` copies a whole `LinuxState`
  with `copy_nonoverlapping`, which duplicates every record while touching no counter -
  so each shared resource needs an explicit inherit step. Pipes had one. When
  demand-paged mappings arrived, their backing stores did not: the child's records named
  entries it held no reference to, the child's exit released one per record and drove the
  count to zero, and the **parent** then faulted against a freed entry and got a zero
  page - a SIGILL long after the fork, in the process that did nothing wrong. Put the
  inherit steps adjacent so the rule is visible as a list rather than remembered as a
  habit.
- **"Am I allowed to address this?" and "am I about to touch it?" are different
  questions.** Demand paging needed the kernel to make a user page present before
  dereferencing it. The tree already had one choke point for cell-supplied pointers, so
  the guard went there - into `user_read_ok`/`user_write_ok`. It cost a ~2,900x
  amplification, because `unmap_range` calls the *same predicate* purely to bound a
  range, and so materialised every page in it immediately before freeing it. The guard
  belongs on the helpers that hand back something to dereference, not on the predicate
  that answers whether an address is permissible. When one function answers two
  questions, a change to it lands on both callers.
- **A correctness fix that works must still be measured before it ships.** Adding
  "ensure the page is present" to the kernel's user-pointer checks made unmodified
  static *and* dynamic glibc run with demand-paged images - the functional proof was
  unambiguous. Then the counters said 11,516 of 11,520 demand fills came from the
  *kernel* pre-faulting and only 4 from the program's own faults, an amplification of
  ~2,900x in the hottest path in the kernel. Functionally right, and not shippable.
  The temptation at that point is to widen the test's bound until it passes, which is
  the same defect as an oracle that cannot fail - so the honest move was to revert and
  write down the one measurement that would explain it (which call site asks for pages
  the program never touches).
- **Demand paging makes every kernel touch of a user buffer a fault site.** Making
  file mappings demand-paged was safe because a program reads its own mapping before
  passing it anywhere. Extending it to the ELF *image* was not: a program hands a
  pointer into its own rodata straight to `write`, and the kernel then dereferenced a
  page nothing had faulted in yet - a load fault at a kernel PC, which is not
  resumable here. This is precisely why Linux has `copy_from_user` and a fixup table.
  The lesson is general: when you make a page's presence lazy, every *other* reader
  of that page becomes a caller that must be able to make it present. Enumerate those
  readers before shipping the laziness, not after.
- **State established before a reset that clears it is a silent zero, not an error.**
  The loader registered a backing store, then the harness called `user::reset()`,
  which cleared the registry - so the mapping records named a freed entry and every
  page of the image came back zeroed. On RISC-V a page of zeros is an illegal
  instruction, so the symptom was `exit 132` at the entry point with nothing in
  between. Two fixes, and both were needed: order the reset before the thing that
  populates, and make the consumer *check* the handle is live so the ordering mistake
  can never again present as blank memory.
- **A return path that "works" may be working for the wrong reason.** The x86-64
  ring-3 fault resume used `sysretq`, which takes RIP from RCX and RFLAGS from R11 -
  it *consumes* both registers. Correct for a syscall return (SYSCALL is defined to
  clobber them); wrong for a fault, where the interrupted instruction re-executes and
  needs the register file it had. It was invisible for as long as signal delivery was
  the only ring-3 fault resume, because a handler entry is a fresh function boundary
  that does not care about two caller-saved registers. The first path that genuinely
  re-executed - demand paging - broke immediately. When a mechanism has exactly one
  caller, "it passes" is evidence about that caller, not about the mechanism.
- **Bisect the layers before theorising about either.** That defect looked like
  wrong bookkeeping (a mis-computed file offset, a bad merge) and there were four
  plausible stories. Keeping the new bookkeeping and restoring the *old eager fill*
  through the new fault path made x86-64 pass, which located the bug in the resume in
  one run. When a change has an independent "what" and "when", hold one fixed.
- **Measure the artifact before choosing which half to build.** The plan was to
  demand-page anonymous memory first, on the assumption that a large image is mostly
  `.bss`. `readelf` on the actual target says `filesz == memsz` for every `PT_LOAD` -
  no `.bss` at all - so anonymous demand paging would have covered none of it. Ten
  minutes of measurement inverted a week's ordering.
- **A proof whose oracle cannot fail is not an oracle.** The first demand-paging
  fixture "verified" the present-page check by re-reading a filled page 100 times.
  Removing the check entirely still passed: a filled page never faults again on a
  read, so that phase never reached the code it claimed to test. The discriminating
  case was a *write* to a filled read-only page. Before trusting a phase, delete the
  thing it guards and watch it fail.
- **Do not split a sequence you have not understood.** The first attempt at that
  fix split the arch injector into "inject" and "halt" so the portable code could
  re-halt until the byte arrived. It wedged the machine: the per-ISA sequence is
  *raise the controller line, halt - which returns immediately **because** the
  interrupt is already pending - then unmask so it is taken*, and a second halt has
  nothing left to wake it. The refactor was reverted and the fix became four lines
  at the call site. A seam that looks like two steps may be one; the way to find
  out is to run it, not to read it.

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
12. Verification scaled to the change (see 13); every pre-existing test still
    passing; formatter and linter clean across the documented target set.

## 13. Verification costs what it costs - so spend it where it buys something

**Rule.** Match the verification to the blast radius of the change. Measure the
cost model before optimising it, and never run two tree-mutating agents at once.

**The measured cost model** (this tree, this emulator): a **warm** no-op build is
~3 s per ISA; a test kernel boots in **2-5 s** (`bench-core` is the outlier at
~32 s). So the full matrix - 57 kernels x 3 ISAs - is **~10-12 minutes**, and the
boots, not the build, dominate.

**The case.** Runs were taking 40-70 minutes, and the cause was not the matrix.
Two agents were mutating one working tree concurrently: they invalidated each
other's build fingerprints (24 cargo invocations, several with different
`RUSTFLAGS` against a shared `target/`), and they collided on the shared
`target/qemu-<arch>-<bin>.log` filenames, producing **spurious failures** that
forced kernel-by-kernel reruns with retries. One of them also had to fix a
non-compiling tree it did not create, and once stashed the other's in-progress
work. The serialised cost was several hours of wall clock for no additional
assurance.

**Required practice.**
- **One tree-mutating worker at a time.** Read-only analysis may run in parallel;
  anything that edits, builds, or boots may not.
- Scale the matrix to the blast radius:
  - a **kernel** change owes the **full matrix** - `.bss` motion has broken an
    unrelated kernel before (11), so "unrelated" is not a safe assumption there;
  - a change confined to a **userspace crate** (`net/`, `librheo/`, `json/`,
    `posix/`) owes the kernels that embed it plus one canary from another family;
  - a **docs-only** change owes formatter/linter, nothing more.
- Iterate with `cargo xtask test --arch <isa> --bin <k1>,<k2>,...` - it boots only
  those kernels. Reach for the full matrix to *confirm*, not to iterate.
- CI is the backstop for cross-cutting regressions. A green subset plus CI beats
  an hour of local matrix that a concurrent agent has already invalidated.
- If a run is unexpectedly slow, **measure where the time goes** before changing
  anything. The assumption that "the tests are slow" was wrong; the tests were
  fine and the process around them was not.

**Reporting it**
13. State scope precisely: built / proven / partially proven / deferred, with
    what would close each gap. Attribute external numbers. Correct the record -
    including the docs - when measurement contradicts an earlier claim.
