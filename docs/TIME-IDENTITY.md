# Time, Ordering, Identity, and Randomness

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.5; the clock/entropy object
(section 3, object 9). Boot-time entropy in BOOT.md 4; RNG security in
SECURITY-IDENTITY.md.

**Implemented (section 4):** the ChaCha20 per-cell DRBG with fast key
erasure (`kernel/src/rng/`), hardware seeding with health tests, and
non-blocking draws. See `kernel/src/rng/mod.rs`, the `rng` test kernel, and
the host comparison in `comparison/rng/` (rheo-os ~4.8x faster than Linux
`getrandom` on key/nonce-sized draws). The "library call, not a syscall"
fast path (section 4) is proven at the primitive level - the host benchmark
calls the DRBG directly - but the lsh `rand` builtin currently draws via
`SYS_RANDOM`: linking the DRBG into a U-mode cell needs the strand runtime's
`.user` heap + `mem*` shims (the `.user.text` window forbids the out-of-line
calls a full DRBG emits), which is the same deferred U-mode-runtime
integration noted in CONCURRENCY.md.
Deferred: continuous background reseed scheduling, checkpoint/restore reseed
(no checkpoint yet), attested seed configuration, virtio-rng feeding.

Position: three problems OSes habitually blur, kept strictly apart - **what
time is it** (clock sync), **what order did things happen** (causality), and
**unpredictable bits** (entropy). Each has one owner and one honest contract.
Uncertainty is made explicit rather than hidden behind a single perfect clock.

## 1. Time - intervals, not instants

The kernel never reports "the time is T." It reports "the time is in
[T-e, T+e]" - a bounded interval (Google TrueTime / AWS ClockBound style).
Hardware reality promoted to the API.

- Three clock types are distinct kernel objects:
  - **Monotonic** - never jumps; for durations, timeouts, leases. The default.
  - **Wall** - an *estimate* with error bound e; for humans and cross-system
    timestamps.
  - **Engine clocks** - GPU/NIC timestamp counters mapped with known
    offset/drift, so a NIC-timestamped completion is comparable to CPU time
    (essential for tracing DMA chains, OBSERVABILITY.md).
- **Sync:** PTP-first (NIC hardware timestamping, nearly free given the
  DPU/SmartNIC design), NTS as fallback, ideally a GNSS or atomic reference
  per fabric island. The sync daemon is an ordinary cell holding a capability
  to *discipline* the clock - nothing else can step it.
- **Authenticated time is mandatory** (NTS, PTP with MACsec). Unauthenticated
  time is an attack vector against everything that consumes it: lease expiry,
  certificate validity, capability TTLs.
- **e is queryable.** A cell asks "current bound?" and gets ~50 us on a PTP
  island or ~5 ms on NTP fallback. Leases consume it directly: a lease is
  valid until T minus the joint error bound, so a host with degrading sync
  sees its effective leases shrink and **self-fences** (SECURITY-IDENTITY.md
  3). Clock quality is a safety input, not a dashboard curiosity.

**The rule that follows:** wall-clock timestamps never decide ordering or
uniqueness. They are for humans, logs, and external interop only.

## 2. Ordering - hybrid logical clocks

Causal order is what distributed systems actually need, and physical time
cannot give it. Every cross-host message - queue transfers, state-store
writes, capability grants - carries an **HLC** timestamp (physical component +
logical counter). HLCs stay close to real time but never violate causality.
The state store uses them for versioning and watch ordering
(CONTAINERS-KUBERNETES.md 3). Cheap (64-128 bits), stamped by the transport
layer, invisible to applications unless they want it.

## 3. Identifiers - UUIDv7 as the convention

Every kernel object needs a cluster-unique ID; v7 (48-bit ms timestamp +
random) is the default because IDs are created everywhere without coordination
and land in ordered indexes - the time prefix means B-tree appends instead of
random-insert thrash, which matters enormously for the state store and the
object store's namespace indexes (FILESYSTEMS.md 3). Three caveats built in:

- The timestamp inside a v7 ID is **hint, not truth** - never parsed for
  logic, only exploited for index locality. Ordering truth lives in HLCs.
- v7 IDs leak creation time. IDs are typed (like memory); for tenant-visible
  objects where that is sensitive, a **v4-random** kind is offered. Explicit
  per object class, not a global default.
- Within one host, node-scoped **64-bit handles** (engine/queue handles) are
  cheaper and get promoted to full UUIDs only when they cross the host
  boundary.

## 4. Randomness - per-cell DRBGs

Replaces Linux's global-pool / blocking-lore / random-vs-urandom confusion:

- One **root DRBG** per host, seeded from hardware sources (RDSEED/RNDR, TPM,
  NIC/board TRNGs, jitter entropy as a floor), continuously reseeded, health-
  tested (SP 800-90B). Post-boot on modern hardware, "running out of entropy"
  is a myth; seeding is the only critical moment.
- Every cell gets its **own DRBG instance**, seeded from the root at creation,
  reseedable via a capability. Getting random bytes is a **library call in the
  cell**, not a syscall - fast path costs nothing, no cross-cell side channel
  through a shared pool, and a compromised cell's RNG state reveals nothing
  about siblings'.
- **Fork/clone/restore safety is structural:** checkpoint/restore *must*
  reseed the cell DRBG on every restore, kernel-enforced, because resumed-VM /
  cloned-snapshot RNG reuse is a real bug class (duplicated ECDSA nonces,
  repeated TLS keys). The DRBG state is deliberately excluded from the
  checkpoint image (ARCHITECTURE.md verb set; VIRTUALIZATION.md 8).
- **Boot-time entropy is attested:** the measured-boot chain includes the
  entropy source configuration, so a host proves *what* seeded its root DRBG.
  A diskless node with no TRNG and no sealed seed fails attestation rather
  than silently minting weak keys (the Mining-your-Ps-and-Qs failure class).
  See BOOT.md 4.
- VMs get virtio-rng feeding plus their own jitter + mandatory reseed - trust
  but supplement (VIRTUALIZATION.md, EMULATION.md 3).

## 4a. The entropy pool - built (`kernel/src/rng/`)

Section 4 is the design; this is what is in the tree, and what each part is
allowed to claim.

### The shape

```
  CPU instruction  ─┐
  RNG device       ─┤ credited          ┌──────────────┐      per-CPU
  jitter source    ─┘ (health-tested)   │  input pool  │  ──► root DRBG ──► getrandom
                                        │  (ChaCha20,  │      (ChaCha20,   (no lock)
  NIC/NVMe/disk/UART ─┐                 │   256-bit,   │       fast key
  /dev/urandom write ─┤ mixed,          │   credited)  │       erasure)
  boot cycle counter ─┘ never counted   └──────────────┘
```

`kernel/src/rng/entropy.rs` is the pool, `jitter.rs` the software source,
`health.rs` the boot self test, `kernel/src/hw/virtio_rng.rs` the device
driver. **The output algorithm did not change** - it is still the ChaCha20
fast-key-erasure DRBG, because that is what makes a read cheap, and the read
path still touches only the calling core's own root with no lock.

### Fast key erasure has two rules, and only one was implemented

The construction (cr.yp.to 2017.07.23) says: re-key from the output on every
refill, **and** erase each random byte from the buffer as it is handed out. Rule
1 was here; rule 2 was not, so up to 256 bytes of *already-delivered* output sat
in the buffer until the next refill, and an attacker who captured the DRBG state
recovered them - djb's recording attacker, exactly the case the construction
exists to defeat.

`Drbg::fill_bytes` wipes as it copies now, `refill` wipes its whole keystream
local rather than just the new key, and `reseed` wipes the buffer tail it
abandons. The wipe is word-wide where alignment allows; a per-byte volatile loop
measured about twice as slow on bulk draws, and the guarantee is that the bytes
are gone, not that it took one store each. Honest limit: `Drbg` is `Copy`, so a
caller that copied the struct leaves a stale image no wipe can reach.

### Absorbing cannot reduce entropy

For each 32-byte chunk `C` of input, with `K` the pool's 256-bit state:

```
K_new = ChaCha20_block(key = K, nonce = seq++)[0..32]  XOR  C
```

Both directions hold:

- An attacker who **chose `C`** but does not know `K` cannot predict
  `ChaCha20_block(K, ..)`, and XORing a known value into an unknown one leaves
  it unknown. The pool is no weaker than before.
- An attacker who **knows `K`** (the pool was compromised) gets no help if `C`
  carries real entropy: `K_new` is then as unpredictable as `C`. The pool
  *recovers*.

Non-decreasing in both directions is what "seeding must not reduce entropy"
means. (Up to the collision loss of a random function on 256 bits, which is
negligible - the same caveat Linux's pool carries.)

### Mixing is not counting

Every source is mixed. Only some are counted towards the 256 credited bits a
re-key needs. A source this kernel cannot measure contributes **zero** - still
mixed, because mixing can only help, but unable to declare the pool ready.
There is no entropy estimator here and inventing one would let a predictable
source seed the machine.

| Source | Credited | Why |
|---|---|---|
| CPU instruction (RDSEED / RNDR / Zkr `seed`) | full | after the SP 800-90B health tests |
| RNG device (virtio-rng, TRNG/TPM chip) | full | a device whose whole purpose is randomness |
| Firmware boot seed (`/chosen/rng-seed`) | full | a bootloader that lied had already loaded the kernel |
| Jitter (`rng::jitter`) | 1 bit/sample | **only** when its own health tests pass |
| NIC / NVMe / disk / UART event timing | none | real, but unmeasured |
| A program writing `/dev/urandom` | none | exactly Linux's rule |
| Boot cycle counter | none | deterministic under emulation |

`/dev/urandom` **writes now do something**. They used to be discarded while
returning success - the stub-reporting-success shape ENGINEERING.md 7 rejects.

### The pool cannot run out

Two things could be called exhaustion; only one is real.

- **The pool state** cannot be exhausted. Extraction runs the state through
  ChaCha20 and keeps the first half as the new state, so it is refreshed, not
  consumed. A read of random bytes always succeeds; there is no blocking
  `/dev/random` here and no need for one.
- **The credit counter** can reach zero, which only means "nothing fresh has
  arrived since the last re-key". It never weakens the generator.

Two guards keep the second from mattering: `seeded` is **sticky** (once a full
seed has ever been held, the machine is seeded for the boot, and nothing can
put it back), and `replenish` actively asks every source rather than waiting -
`pump` calls it whenever credit falls below half a seed. The jitter source is
gated on *not yet seeded*, because its job is a machine's first seed, not
steady-state supply, and it is the expensive one.

### The software jitter source

The fallback for a machine with no randomness hardware at all. Times a
data-dependent walk over a scratch buffer, 256 times, and takes the low bits of
each cycle-count delta - the `jitterentropy`/`haveged` idea.

It **cannot fabricate entropy**, which matters here because under QEMU
`-icount` the cycle counter is deterministic. Three checks run before any of it
counts: no long run of identical deltas, no single delta dominating the window,
and enough distinct values. Failing any of them credits **zero** and reports
which one failed; the samples are still mixed. Credit is at most one bit per
sample and never more than the number of distinct values observed.

Measured here: aarch64 refuses (`longest_run=9`, "deltas repeat"), x86-64
credits 42 bits, riscv64 credits 20 - all far below the 256-bit target, so on
these emulated machines jitter contributes but never seeds alone. On real
hardware it is expected to reach the target; that is a lab claim, not one made
here.

### The firmware boot seed

Device-tree platforms hand a kernel entropy before any device is up, in
`/chosen/rng-seed`, filled by the bootloader or hypervisor. `hw::fdt` captures it
during the discovery walk it already performs - matched on the property name, the
same way `distance-matrix` is - and `rng::init` absorbs it first, because the
bytes are already in hand.

It is **credited in full**, for the reason Linux credits it: a bootloader that
lied about the seed had already loaded the kernel, so it could have compromised
the boot far more directly. Trusting it admits no attacker who was not already
inside.

Measured: QEMU's riscv64 `virt` supplies **32 bytes**, credited as 256 bits.
x86-64 has no device tree, and an ARM64 bare-ELF `-kernel` boot is handed no
pointer to one, so both report the seed absent - asserted as absent rather than
assumed, so a platform that starts supplying one cannot go unnoticed.

### Per-ISA seeding, and the hole that is now closed

| ISA | CPU instruction | Device | Seed source reached |
|---|---|---|---|
| x86-64 | RDSEED | virtio-rng-pci | `Hwrng` |
| ARM64 | RNDR | virtio-rng-device | `Hwrng` |
| riscv64 | **none** (Zkr `seed` needs an M-mode `mseccfg` grant this firmware does not give) | virtio-rng-device | `Device` |

RISC-V previously reached `Fallback` - a cycle-counter loop, which is not a
source. `kernel/src/hw/virtio_rng.rs` closes it: the standard paravirtual
randomness device, over the same two transports every other virtio driver here
uses (virtio-mmio on arm/riscv `virt`, virtio-pci through the
`VIRTIO_PCI_CAP_PCI_CFG` tunnel on x86-64 q35, so no BAR is needed). The fix is
a driver, not a per-ISA workaround, which is what TARGET-ARCHITECTURES.md 4
requires.

### The boot health check

`rng::health::check()` runs on **every** boot, right after the root is keyed.
Three integrity tests, each a panic on failure because a broken generator must
not reach a cell:

1. **Known-answer test** - ChaCha20 against the RFC 8439 section 2.3.2 block.
   Catches a miscompiled or mis-linked primitive, which is not hypothetical in
   this tree (NETSTACK.md, the N3a AES miscompile).
2. **Continuous test** - two consecutive live root outputs differ (FIPS 140-2
   CRNGT).
3. **Window test** - 64 words of live output pass the SP 800-90B checks.

It also *reports* whether the pool is seeded and which source paid. That half
is never asserted at boot: a machine may genuinely have no source, and saying
so is the honest answer.

The healthy path prints nothing. A line on every boot would say the same thing
in all ~210 logs, and the failure path is a panic naming the test, which is
louder than a log line.

### Performance

Measured by `cargo xtask bench` (icount, x86-64), per operation:

| Bench | ticks | what it is |
|---|---|---|
| `entropy_mix_event` | **15** | what every NIC / NVMe / disk / UART interrupt pays |
| `rng_next_u64` | 367 | one 64-bit draw from a root DRBG |
| `entropy_absorb_32B` | 1,422 | the thread-context path: lock + ChaCha20 |

The interrupt hook is ~24x cheaper than a single `u64` draw, which is what makes
"two atomic operations, no lock" a measurement rather than a reading of the
source - a handler that quietly grew a lock is a latency bug nothing else here
would catch.

- The read path is unchanged: this core's root DRBG, no lock.
- An interrupt handler calls `absorb_fast` - two atomic operations into **this
  core's own** scratch words. No lock, no ChaCha20. The split exists so a
  handler can never wait on a thread holding the pool lock.
- The pool lock is taken from thread context only: on a `/dev/urandom` write,
  and once every 1024 DRBG derivations when `pump` drains the scratch. Driving
  reseed off the *consume* path rather than a timer costs an increment and a
  compare on the hot path.

### Proof

`cargo xtask test --bin rng`, all three ISAs, with a randomness device attached
to each launch. Six controls observed firing:

| Claim | Control that breaks it |
|---|---|
| A chosen-input source cannot seed the pool (1 MiB of chosen bytes = 0 credited bits) | make `Source::User` creditable -> "user writes credited 256 bits" |
| `seeded` is sticky, so credit can never un-seed a machine | drop the sticky write -> the exhaustion phase fails |
| Uncredited input is still mixed | skip mixing when credit is zero -> "an uncredited write did not change the pool" |
| Jitter never credits without passing its checks | delete the three checks -> "credited 17 bits with a run of 5 identical deltas" |
| The boot KAT catches a broken ChaCha20 | flip one byte of the vector -> every boot panics |
| The device is what seeds RISC-V | detach it -> `seed_source=Fallback seeded=false` |
| A delivered byte is erased from the buffer | drop the wipe in `fill_bytes` -> "delivered bytes still in the buffer" |
| The firmware seed is fed, not just captured | keep the capture, skip the absorb -> "32 seed bytes but none were credited" |

One control is recorded as **self-defeating and replaced**: disabling
`save_rng_seed` also made `rng_seed()` return `None`, so the test took its
"no device tree here" branch and passed. The same switch flipped the source and
the detector. Breaking only the *feed* fires it.

The jitter control earned its keep: its first two versions **passed**, because
removing one check let the next one catch the same window, and then because the
test asserted `distinct` but not `longest_run` when crediting. The assertion is
tighter now for that reason.

Honest remainders: the credited jitter figures above are emulated-machine
numbers; `/dev/urandom` writes are proven at the kernel API (`rng::feed_user`)
and through the fd path by inspection, not yet by a Linux fixture; there is no
`RNDADDENTROPY` ioctl, so a program cannot credit its own writes (which is the
right default); and nothing seals a seed across reboots, so a diskless node
still depends on having a source at boot.

## 5. How it hangs together

Authenticated time bounds make leases and capability TTLs safe; HLCs make the
state store and audit logs causally ordered; UUIDv7 makes indexes fast without
pretending IDs are clocks; per-cell attested DRBGs make every key and nonce in
the identity system trustworthy. Time, order, identity, and entropy each have
one owner and one contract.

## 6. Open question

Whether the state store needs TrueTime-style **external consistency** (commit-
wait on e, Spanner-style, buying strict serializability at the cost of
coupling write latency to clock quality) or **HLC-based causal+** consistency
is enough. For an infrastructure store - low write rate, high read rate - the
current default is HLC without commit-wait, but it is an explicit open
decision (ARCHITECTURE.md 9).
