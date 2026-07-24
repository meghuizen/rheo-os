# comparison/euroos - Design comparison with EuroOS

A design-level comparison between rheo-os and
[EuroOS](https://github.com/GoTrustbe/Euroos), focused on target
architecture (docs/TARGET-ARCHITECTURES.md) and on what each project can
take from the other. Unlike `comparison/sel4`, this is a paper comparison,
not a measured one - see "A measured comparison is possible" at the end.

**Snapshot:** EuroOS main branch as of 2026-07-24, read from its README,
STATUS.md, and docs/EUROOS-DEEP-TECHNICAL-REFERENCE.md. EuroOS is alpha
and moves fast; re-check before quoting.

## What EuroOS is

EuroOS is a from-scratch, `no_std` Rust operating system for x86-64 UEFI
machines, built by GoTrust with a European-sovereignty framing (EUPL
license, EU Cyber Resilience Act self-assessment, zero telemetry). It is a
*general-purpose desktop/server OS*: it boots to its own windowed desktop,
has its own copy-on-write filesystem (EuroFS), a full in-kernel network
stack up to TLS 1.3, USB/NVMe/AHCI drivers, TPM-backed measured boot and
full-disk encryption, Ed25519-signed binaries, a Linux/musl compatibility
bridge (the original 1993 DOOM binary runs unmodified), and ~1000 host
tests. That is a lot of working, user-visible system for an alpha.

## Fairness first: the two projects aim at different targets

| | rheo-os | EuroOS |
|---|---|---|
| Target | Fleet servers, heterogeneous compute (GPU/NPU/DPU) | Sovereign desktop + general-purpose machines |
| Bet | The kernel's job shrinks to identity, placement, queue setup | Europe needs a full OS stack it controls end to end |
| Posture | Design-first (docs are the spec), emulation-first, measured claims | Ship-first (sprints), real hardware early, feature breadth |

Several differences below follow from the different targets and are not
mistakes on either side. The lessons called out are the ones that hold
*within EuroOS's own stated goals* (security-first, sovereign, CRA-grade).

## Target-architecture comparison

| Axis | rheo-os | EuroOS |
|---|---|---|
| ISAs | x86-64, ARM64, RISC-V 64, CI-gated on every commit; CHERI tracked | x86-64 UEFI only |
| Portability rule | Only `arch/` may differ per ISA; per-ISA code elsewhere is an architecture bug | None; kernel is coupled to long mode, MSRs, APIC throughout |
| Hardware floor | Explicit and binding per profile: IOMMU, measured-boot root, hardware entropy, invariant TSC (TARGET-ARCHITECTURES.md 3) | HARDWARE-COMPAT.md lists tested devices; no stated security floor |
| IOMMU | Non-negotiable; every DMA in the system is IOMMU-mediated | Not mentioned; DMA devices are driven by ring-0 drivers without an IOMMU boundary |
| Kernel contents | Control plane only: no filesystems, no network stack, no data-format parsers, no vendor drivers in kernel (the negative constitution, ARCHITECTURE.md 5) | Filesystem, TCP/IP, TLS 1.3, X.509, SMB/NFS clients, USB, compositor and display protocol all run in ring 0 |
| Capability model | Per-object, unforgeable, typed, delegatable, epoch-revocable, metered handles (a *specific* buffer/queue/device) | Per-process capability *bitmask* by category (CAP_FILE, CAP_NET, ...), checked at the syscall boundary, drop-only |
| Syscall surface | Queue pairs (submission/completion rings); blocking does not exist below the library level | Synchronous SYSCALL/SYSRET; syscalls run with interrupts off (IF=0), flagged in their own docs as a future SMP bottleneck |
| Scheduling | Tickless; EDF admission control; reservations as contracts; two-level (kernel places vcores, runtime schedules ~200-byte strands) | 100 Hz tick, mini-CFS on vruntime with nice levels; per-CPU run queues |
| SMP | Detection/topology done; secondary-core bring-up deferred, honestly scoped | Boots 4 cores via INIT-SIPI-SIPI; finer-grained locking deferred |
| Entropy | ChaCha20 DRBG per cell, fast key erasure, SP 800-90B health tests on the hardware source, entropy in the attestation chain | RDRAND + RDTSC jitter |
| Benchmarks | Deterministic QEMU icount path lengths, CI-run; wall-clock claims only from a hardware lab; seL4 measured under identical harness | None published; correctness-focused (host tests + boot self-tests + fuzzing) |
| Compatibility | POSIX personality + Linux personality as translation layers at the edge; glibc target | Linux/musl bridge in-kernel ("a guest, never the system's identity"); 200+ syscalls verified |

## What EuroOS could take from this design

Ordered by how much it matters for EuroOS's own security-first goals.

**1. An IOMMU requirement - the capability bitmask does not gate DMA.**
EuroOS enforces capabilities at the syscall boundary, but its NVMe, xHCI,
and NIC hardware can DMA anywhere in physical memory, and nothing checks
that. A malicious or buggy device (or a compromised in-kernel driver
programming one) bypasses every capability, signature check, and audit
log. rheo-os makes the IOMMU (VT-d/AMD-Vi) a boot requirement precisely
because isolation claims are empty without it. For a CRA-grade,
security-first OS this is the single highest-leverage change: require
VT-d/AMD-Vi on supported hardware, give every device its own IOMMU
domain, and state the degradation honestly where firmware lacks it.

**2. Object capabilities, not category bitmasks.** `CAP_FILE` means "all
files", `CAP_NET` means "the network". That is coarser than least
privilege: a text editor and a backup tool get identical file authority.
rheo-os capabilities name one object (this buffer, this queue, this
device), can be delegated with attenuation, and are revocable by epoch -
and the runtime tests prove four properties (unforgeability, monotonic
attenuation, revocation soundness, isolation) on every commit. EuroOS
already has the right instinct (drop-only, policy can only reduce);
making the unit of authority an object instead of a category is the step
that makes "capability-based security" load-bearing. Related quick win:
EuroIPC's permission check is currently an open hook (all sends allowed,
by their own docs) - port-send rights are a natural first object
capability.

**3. Get the parsers out of ring 0.** EuroOS runs TLS 1.3, X.509 chain
validation, SMB2/NTLMv2, NFS, DNS, and multiple filesystem parsers in
kernel mode. Rust removes memory-unsafety, not logic bugs - a confused
X.509 validator or SMB state machine in ring 0 is a full-system
compromise. rheo-os's rule is that the kernel never parses data formats;
filesystems and network stacks are userspace cells. EuroOS is unusually
well positioned to adopt this incrementally, because its code is already
factored into ~50 host-tested `crates/euro*` libraries with thin kernel
glue - the same crates could link into ring-3 service processes behind
its existing IPC instead of into the kernel. The TCB shrinks by an order
of magnitude without rewriting the logic.

**4. An architecture boundary, even while single-ISA.** EuroOS's
sovereignty goal sits oddly on an x86-64-only kernel: the two x86 vendors
are American, while Europe's own silicon efforts (SiPearl/EPI, Codasip)
are ARM64 and RISC-V. rheo-os's experience is that portability is cheap
*only* if enforced from the start: one rule ("only `arch/` may differ per
ISA"), one `Arch` trait per concern, and CI that boots every ISA on every
commit. Even before a second ISA exists, drawing that boundary keeps
MSR/APIC/long-mode assumptions from soaking into scheduler and memory
code - and it is the difference between a future ARM64 port being a
recompile or a rewrite. A second lesson from the same experience: two
memory models (x86 TSO vs ARM weak ordering) exercised continuously is
what keeps synchronization code honest; single-ISA kernels silently
accumulate TSO assumptions.

**5. Deterministic path-length benchmarking.** EuroOS publishes no
performance numbers, reasonably avoiding fake QEMU wall-clock figures.
There is a middle path: QEMU `-icount shift=0` makes instruction counts
deterministic (zero variance across runs), so syscall, context-switch,
and IPC *path lengths* can be measured in CI and trend-tracked - a
regression in the syscall path shows up as an instruction-count diff in a
pull request. rheo-os runs this on every commit (`cargo xtask bench`) and
used it to compare against seL4 under an identical harness. Cheap to
adopt, and it would put numbers behind EuroOS's IF=0 syscall-path
concern.

**6. Contracts over priorities, async over blocking - before SMP load
makes it urgent.** EuroOS's own status notes that syscalls running with
interrupts off are a future SMP bottleneck, and its mini-CFS inherits the
priority model rheo-os deliberately rejected (admission-checked
reservations instead of nice levels; failure delivered as events, no OOM
killer). The queue-pair idea - submission/completion rings in shared
memory as the syscall surface - is how rheo-os avoids both the IF=0
serialization and per-call ring crossings. EuroOS need not adopt the
whole model, but an async completion path for I/O-heavy syscalls is the
scalable exit from the bottleneck it already identified.

**7. Entropy with health tests and key erasure.** RDRAND plus timing
jitter is a reasonable start; a per-process ChaCha20 DRBG seeded from a
health-tested hardware source (SP 800-90B continuous tests), with fast
key erasure and reseed-on-restore, is a documented, testable design that
composes with EuroOS's existing TPM/measured-boot story (entropy sources
belong in the attestation chain).

**8. A kernel admission rule.** rheo-os keeps the kernel object model
closed with a three-part test (needs unforgeable enforcement; arbitrates
shared hardware; mechanism with policy outside). EuroOS's sprint-driven
process is visibly effective at shipping, but each sprint lands more code
in ring 0 (`ring3.rs` alone carries the syscall surface, Linux bridge,
/proc synthesizer, and capability checks). A written rule for what may
enter the kernel is how a security-first project keeps its TCB from
growing monotonically.

## What this project can take from EuroOS

The comparison is not one-way. EuroOS demonstrates several things this
project has deferred and should not forget the cost of. Ordered by how
actionable they are for rheo-os today.

**1. Verify-before-execute.** EuroOS refuses to run any binary whose
Ed25519 signature does not verify. rheo-os's ELF loader currently loads
whatever it is handed. Signature verification at load passes the section
6 admission test cleanly (unforgeable enforcement, arbitrates a shared
mechanism, policy - the trust roots - lives outside), composes with the
existing attest-by-measurement engine model, and closes a real gap
between the attestation story on paper and what the loader enforces.

**2. Fuzz every parser of untrusted input.** EuroOS runs a deterministic
fuzz harness (200k inputs per parser, panic-safe) over its filesystem,
network, and format parsers. rheo-os host-fuzzes the heap allocator, but
the ELF loader and the ext4 driver parse *attacker-shaped* bytes (a
binary to run, a disk image to mount) and are only exercised by
well-formed fixtures. Both are `no_std` libraries that already compile
on the host - a fuzz target each is cheap and overdue.

**3. Crash-consistency test discipline for storage.** EuroFS ships
copy-on-write snapshots, A/B superblocks, checksummed data paths with
scrub, and rollback - and tests disk-full recovery, multi-disk
cross-copy, and stress to 64 GiB. rheo-os's write path today is ramfs
(ext4 is read-only) and the native object store is still design. When
write-capable persistence lands, EuroOS's test list (power-cut points,
disk-full, checksum-detected corruption) is the acceptance bar to copy,
and durability-as-typed-completion (docs 4.6) needs exactly this kind of
adversarial testing to be credible.

**4. Real-hardware breadth, early.** USB (hubs, HID, audio), NVMe and
AHCI on physical machines, e1000/CDC-ECM NICs, driverless printing and
scanning, install-to-disk. rheo-os is emulation-first by design and the
hardware lab is scheduled, not skipped - but EuroOS shows a small team
can hold a physical-hardware matrix, and QEMU never shows the bug
classes real firmware, real timing, and real device errata produce. The
longer the lab is deferred, the bigger the surprise bill.

**5. Host-testable crates as the primary test surface.** Roughly a
thousand `cargo test` tests in pure libraries with a thin kernel glue
layer, no VM in the inner loop. rheo-os does this for the allocator,
ext4 parsing, and rheo-json, but the capability core and queue-pair
state machines are tested only as in-QEMU boot kernels. Factoring them
to also run as host property tests would make the proof-property suite
(ARCHITECTURE.md 8.2) seconds instead of boots, without replacing the
in-QEMU tests that gate CI.

**6. Proof-by-running-real-software milestones.** Booting the unmodified
1993 DOOM binary is worth a thousand ABI tables - unarguable, and it
forces syscall breadth honestly. The Linux personality
(docs/LINUX-COMPAT.md L0-L7) currently proves `write`/`exit`; it should
name its DOOM - one specific, well-known, unmodified binary per
milestone (a static busybox is the natural L-series candidate).

**7. Tamper-evident audit and operability plumbing.** EuroOS ships a
hash-chained audit log, structured journal, kernel crash dumps, a
deadman watchdog, and SMART-based disk health. rheo-os's event streams
are the richer observability design (typed, flow-context, capability-
scoped), but they are neither persisted nor tamper-evident, and there is
no crash-dump or watchdog story yet. For the stated production-grade bar
(PRODUCTION.md), hash-chaining the security-relevant event streams and a
minimal crash-dump path are the missing operability pieces EuroOS
already has.

**8. Compliance-shaped deliverables.** CRA self-assessment, SBOM, a
SECURITY.md with coordinated disclosure, a support policy, signed
release artifacts, and one-click demo images. rheo-os has the stronger
primitives (measured boot, attestation chains, capability audit) but
none of the documents adopters and regulators actually ask for. These
are cheap relative to the engineering already done, and the EU
regulatory framing EuroOS targets (CRA) will apply to any OS shipped
into that market.

## A measured comparison is possible

EuroOS boots headless in QEMU x86-64 with a scripted test entry point
(`scripts/run-qemu.sh`), so the `comparison/sel4` methodology applies: the
same QEMU build under `-icount shift=0`, instruction path lengths for a
syscall round trip, a context switch, and an IPC message send on both
systems. That would replace the design-level claims above (queue pair vs
IF=0 syscall, mini-CFS switch vs strand switch) with numbers. Not done
here; left as the natural follow-up.
