//! `rheo-abi` - the on-wire user/kernel ABI, defined once.
//!
//! Everything a separately-compiled cell and the kernel must agree on
//! *byte-for-byte* lives here: syscall numbers, queue opcodes and status codes,
//! the shared ring header and entry layouts, and the `repr(C)` blocks a syscall
//! writes into a cell's memory. Both sides re-export this crate rather than
//! restating it (`kernel::abi`, `kernel::queue`, `librheo::sys`), so a change on
//! one side is a compile error on the other instead of a wrong number at
//! runtime (docs/ARCHITECTURE-DEBT.md 3.1).
//!
//! Design references: docs/ARCHITECTURE.md 3 (the closed object list),
//! docs/IO.md 1 (the queue-pair contract), docs/LIBRHEO.md (the cell side).
//!
//! **What does not belong here**: anything either side can change alone. The
//! kernel's `ShellIo`/`Params` blocks are shared only with U-mode programs
//! compiled into the same crate; the `svc` bridge tables are kernel-internal
//! function pointers. This crate is the *cross-crate* contract, nothing more.
//!
//! Conventions (all three ISAs): the syscall number goes in the ISA's
//! syscall-number register and arguments in the argument registers, with the
//! return value back in the first one - see each arch's `decode_syscall` /
//! `set_syscall_ret`. A syscall returning `i64` reports `>= 0` on success and a
//! negated errno on failure; one returning `u64` reports `u64::MAX` on failure.

#![no_std]

use core::sync::atomic::AtomicU32;

// =========================================================================
// Syscall numbers
// =========================================================================
//
// One numbering space, so a new verb cannot collide with an existing one. Not
// every number is issued by every caller: 1-20 are the native/shell surface
// (`kernel/src/user_progs.rs`), 21-30 the POSIX personality (docs/USERLAND.md
// M2), 31-49 the librheo foundation (docs/LIBRHEO.md, docs/NETSTACK.md).

/// Process the calling cell's queue pair (drain submissions, grant-check,
/// complete), then return to the caller. The doorbell.
pub const SYS_DOORBELL: u64 = 1;

/// Directed switch to the peer cell (cross-cell round trip, P5). Saves the
/// caller's user context, switches address space, resumes the peer where it
/// last switched. Argument and return value are ignored.
pub const SYS_SWITCH: u64 = 2;

/// Leave U-mode and return to the kernel run loop. Argument is the cell's
/// self-reported result code (0 = ok).
pub const SYS_EXIT: u64 = 3;

/// Read the shared cycle counter into the return register. Used so the
/// benchmark's user side measures the same clock the kernel calibrates, without
/// depending on per-ISA U-mode counter-enable quirks.
pub const SYS_CYCLES: u64 = 4;

// ---- shell / resource syscalls (arg is a *const ShellIo unless noted) ----

/// Read one PTY line into `ShellIo.in_buf` (blocking). Returns 1 if a line was
/// read, 0 at end of input.
pub const SYS_READLINE: u64 = 5;
/// Write `ShellIo.out_buf[..out_len]` to the PTY. Returns 0.
pub const SYS_WRITE: u64 = 6;
/// Monotonic uptime ticks.
pub const SYS_UPTIME: u64 = 7;
/// Next per-cell random u64.
pub const SYS_RANDOM: u64 = 8;
/// Frame-pool stats: `(free << 32) | total`.
pub const SYS_MEMINFO: u64 = 9;
/// Number of runnable cells the kernel is tracking.
pub const SYS_PS: u64 = 10;
/// Live capability count in the calling cell's table.
pub const SYS_CAPS: u64 = 11;
/// Emit a user event (arg = event kind). Returns total events emitted.
pub const SYS_EVENT_EMIT: u64 = 12;
/// Event counts: `(buffered << 32) | total`.
pub const SYS_EVENT_COUNT: u64 = 13;
/// Run the demo dependency graph with input `arg`; returns its result.
pub const SYS_GRAPH: u64 = 14;
/// Admit a reservation (arg = `(budget << 32) | period`). Returns the committed
/// utilization in parts-per-million, or `u64::MAX` if refused.
pub const SYS_RESERVE: u64 = 15;
/// Acquire a lease; returns its fencing token.
pub const SYS_LEASE: u64 = 16;
/// `cpuinfo(out_va) -> 0`. With `out_va == 0`, print the CPU report (vendor,
/// core count, instruction-set features) to the console (the shell builtin).
/// With `out_va != 0`, write a [`CpuFeatures`] there instead - the machine-
/// readable form a cell reads to pick its SIMD path (docs/TILES.md 4). Mechanism
/// only: it exposes the already-discovered `hw::Inventory` CPU report + the FP
/// widths the kernel validated at boot; no new object (ARCHITECTURE.md 6).
pub const SYS_CPUINFO: u64 = 17;
/// Print the enumerated PCIe devices and their engine classification.
pub const SYS_LSPCI: u64 = 18;
/// Print the NUMA topology: per-node RAM and CPU counts.
pub const SYS_NUMA: u64 = 19;

/// Write bytes to the console from a loaded userland program (docs/USERLAND.md
/// M1). The argument is the VA of a [`DebugWrite`] in the cell; the kernel
/// copies `len` bytes from `ptr` to the console. A bring-up primitive - the real
/// fd-based `write(2)` is [`SYS_WRITE_FD`]. Returns the number of bytes written.
pub const SYS_DEBUG_WRITE: u64 = 20;

// ---- POSIX personality syscalls (docs/USERLAND.md M2) ----
//
// Multi-argument, fd-based: arguments come from a0..a5 (see each arch's
// `decode_syscall`). The memory/process calls are handled in the kernel; the
// file calls are forwarded to a registered `svc::FileOps`, which is what keeps
// the kernel filesystem-free.

/// `mmap_anon(len)` -> base VA of `len` bytes of fresh zeroed RW pages (0 fails).
pub const SYS_MMAP: u64 = 21;
/// `exit_group(code)`: leave U-mode, like [`SYS_EXIT`].
pub const SYS_EXIT_GROUP: u64 = 22;
/// `open(path_va, path_len, flags)` -> fd or -errno.
pub const SYS_OPEN: u64 = 23;
/// `close(fd)` -> 0 or -errno.
pub const SYS_CLOSE: u64 = 24;
/// `read(fd, buf_va, len)` -> bytes read or -errno.
pub const SYS_READ: u64 = 25;
/// `write(fd, buf_va, len)` -> bytes written or -errno.
pub const SYS_WRITE_FD: u64 = 26;
/// `lseek(fd, offset, whence)` -> new offset or -errno.
pub const SYS_LSEEK: u64 = 27;
/// `stat(path_va, path_len, statbuf_va)` -> 0 or -errno. Fills a [`Stat`] at
/// `statbuf_va` (docs/USERLAND.md M5).
pub const SYS_STAT: u64 = 28;
/// `fstat(fd, statbuf_va)` -> 0 or -errno. Like [`SYS_STAT`] but by open fd.
pub const SYS_FSTAT: u64 = 29;
/// `getdents(path_va, path_len, buf_va, buf_len)` -> bytes written or -errno.
/// Packs directory entries into `buf` (docs/USERLAND.md M5): each record is
/// `[u32 kind][u32 name_len][name bytes]` with no padding, read sequentially by
/// the std `ReadDir`. `kind`: 0 regular, 1 dir, 2 symlink, 3 other.
pub const SYS_GETDENTS: u64 = 30;

// ---- librheo native foundation (docs/LIBRHEO.md Phase A) ----

/// `queue_info(out_va)` -> 0, or `u64::MAX` if the cell has no queue pair.
/// Writes a [`QueueInfo`] at `out_va`. librheo's reactor calls this once at
/// startup to bind the ring and address the doorbell (a native, explicit
/// alternative to an auxv entry).
pub const SYS_QUEUE_INFO: u64 = 31;

// ---- typed memory grants exposed to userspace (docs/LIBRHEO.md Phase B,
// docs/MEMORY.md, docs/ARCHITECTURE.md 3 object 5) ----
//
// These EXPOSE the existing memory-grant object as mechanism (ARCHITECTURE.md 6
// admission rule); they add no new object. A grant is a typed reservation of
// address space that is demand-committed with frames. Each `SYS_GRANT` mints a
// real MemoryGrant capability into the cell's table, and commit/decommit/seal
// grant-check it (MAP right) before touching pages.

/// `grant(out_va, len, kind, flags)` -> 0, or `u64::MAX` on failure. Reserves
/// `len` bytes of address space of typed `kind` (no frames yet - demand commit),
/// mints a MemoryGrant capability, and writes a [`GrantInfo`] at `out_va`.
pub const SYS_GRANT: u64 = 32;
/// `commit(cap_id, offset, len)` -> 0 or -errno. Backs `[offset, offset+len)` of
/// the grant with fresh zeroed RW frames (demand paging without a fault
/// handler). Refused on a sealed grant.
pub const SYS_COMMIT: u64 = 33;
/// `decommit(cap_id, offset, len)` -> 0 or -errno. Returns the frames backing
/// `[offset, offset+len)` to the pool (the reservation and cap stay).
pub const SYS_DECOMMIT: u64 = 34;
/// `seal(cap_id)` -> 0 or -errno. Makes the grant immutable (its committed pages
/// become read-only, shareable) - the zero-copy-buffer / dmabuf precursor.
pub const SYS_SEAL: u64 = 35;
/// `munmap(va, len)` -> 0. Unmaps whole pages in `[va, va+len)` and frees their
/// frames (the real unmap the anon [`SYS_MMAP`] lacked - fixes the frame leak).
pub const SYS_MUNMAP: u64 = 36;
/// `mmap_file(fd, offset, len, flags)` -> base VA (0 fails). Maps `len` bytes of
/// the file open on `fd` (via the registered `svc::FileOps`) into the cell, read
/// into fresh frames (MAP_PRIVATE semantics), for mmap-ing a dataset.
pub const SYS_MMAP_FILE: u64 = 37;

// ---- compute & QoS exposed to userspace (docs/LIBRHEO.md Phase C,
// docs/ARCHITECTURE.md 3 objects 4/6/7) ----

/// `engine_info(out_va, index)` -> engine count. Writes an [`EngineInfo`] for
/// engine `index`: index 0 is the CPU engine the kernel runs graphs on
/// (throughput MEASURED at attach - attest-by-measurement, object 4); indices
/// beyond it are GPUs enumerated from PCIe (docs/GPU-HARDWARE.md), registered
/// with their declared op-boundary preemption contract and a zero measured cost
/// until a driver cell can execute on them. Out-of-range indices write nothing;
/// the returned count is the enumeration bound.
pub const SYS_ENGINE_INFO: u64 = 38;
/// `reserve_admit(out_va, budget, period, deadline, mem_floor_pages)` -> 0 on
/// success, else a rejection code (1=BadParams, 2=Overcommit, 3=MemoryFloor).
/// Runs the EDF schedulability test (object 7); on success mints a Reservation
/// capability and writes a [`ReserveInfo`] at `out_va`.
pub const SYS_RESERVE_ADMIT: u64 = 39;
/// `reserve_query()` -> the cell's committed CPU utilization in parts-per-million.
pub const SYS_RESERVE_QUERY: u64 = 40;
/// `reserve_release(cap_id)` -> 0. Releases an admitted reservation, freeing its
/// utilization (the RAII drop path). `u64::MAX` if the handle is not live.
pub const SYS_RESERVE_RELEASE: u64 = 41;

// ---- console input / the first block-and-wake (docs/LIBRHEO.md Phase D) ----

/// `wait_input(buf_va, len)` -> nbytes. **Block** until at least one console
/// input byte is available, copy up to `len` bytes into the cell buffer at
/// `buf_va`, and return the count (0 = end of input). The OS's first
/// block-and-wake: the cell is descheduled and a sibling runs; the kernel idles
/// (`wfi`/`hlt` where the ISA's UART RX interrupt is wired) only when nothing
/// else is runnable (docs/ARCHITECTURE-DEBT.md 2.4). The RX bytes come from a
/// kernel-side ring (`kernel/src/input.rs`) so keystrokes typed while a cell
/// computes are not lost. librheo's `term` builds its async input on this.
pub const SYS_WAIT_INPUT: u64 = 42;

/// `connect_info(out_va, slot)` -> 0, or `u64::MAX` if the cell has no
/// cross-cell channel in `slot`. Writes a [`ChannelInfo`]. librheo's
/// `ipc::Channel` calls this once per end to bind a shared ring (IO.md 6:
/// connect = capability exchange yielding a typed queue pair).
///
/// A cell holds up to [`MAX_CELL_CHANNELS`] ends (docs/NETSTACK.md 17, rheo-net
/// N4a) - a **service** holds one per client, a client holds one (slot 0). Slot
/// 0 is the Phase E/J channel, so every pre-N4a caller passes `slot = 0` and
/// behaves exactly as before.
pub const SYS_CONNECT: u64 = 43;

/// `grant_share(grant_cap_id, out_va)` -> 0, or `u64::MAX` on failure. Delegate
/// a **sealed** memory grant to the peer cell: the kernel maps the grant's
/// frames into the peer read-only, mints a MemoryGrant capability there
/// referencing the same kernel object (so an epoch revoke kills the peer's copy
/// too), and writes a [`ShareInfo`] at `out_va`. Requires the grant cap to carry
/// DELEGATE and the grant to be sealed (immutable = shareable, object 5). This
/// is zero-copy cross-cell buffer passing (the dmabuf equivalent).
pub const SYS_GRANT_SHARE: u64 = 44;

// ---- native process model + timers (docs/LIBRHEO.md Phase F,
// docs/ARCHITECTURE.md 3 object 1 Cell, verb set "create/destroy cell" +
// "arm timer/doorbell") ----

/// `spawn(path_va, path_len, argv_va, envp_va, chan_spec)` -> child handle
/// (>= 0), or `u64::MAX` on failure (no spawn capability, ELF not found, cell
/// table full). Loads the ELF at `path` from the VFS into a NEW native cell,
/// builds its initial stack from the NUL-terminated C-string arrays at
/// `argv_va`/`envp_va`, maps it a queue pair + mints a queue capability, and
/// records the caller as its parent. Gated by a **cell-spawn capability** (an
/// `ObjectKind::Cell` cap carrying WRITE) - no ambient authority. The child is
/// runnable but does not run until the parent [`SYS_WAIT`]s (or exits).
///
/// `chan_spec` selects **which of the caller's channel ends the child inherits**
/// (docs/NETSTACK.md 17): 0 = the Phase J default (inherit slot 0 if the caller
/// has one), otherwise [`SPAWN_CHAN_SLOT`] with the slot number in bits 15:8
/// ([`spawn_chan_spec`]). The child always receives the inherited end at its own
/// **slot 0** with the opposite role, so a client binary is slot-agnostic.
pub const SYS_SPAWN: u64 = 45;

/// `wait(handle)` -> the child's exit code (0..=255). **Blocks** cooperatively
/// (the parent's other strands run meanwhile, driven by librheo's reactor) until
/// the child named by `handle` exits, then reaps it and frees its slot. Returns
/// `u64::MAX` if `handle` names no child of the caller. A native child that
/// faults is reaped with [`FAULT_EXIT`], never a signal.
pub const SYS_WAIT: u64 = 46;

/// `arm_timer(deadline_ns, client)` -> 0. **Blocks** until `deadline_ns`
/// nanoseconds of monotonic time elapse from the call. The "arm timer" verb: a
/// one-shot deadline. Honors docs/POWER.md - the kernel only waits when a real
/// deadline was requested. The deadline is held by the **timer arbiter**
/// (`kernel/src/ktimer.rs`), never armed on the hardware directly, so it
/// coexists with every other outstanding deadline; the caller is descheduled so
/// siblings run, and the kernel halts only when nothing else can.
///
/// `client` (argument 1) selects **which arbiter slot** holds it:
/// [`TIMER_CLIENT_CELL_SLEEP`] (0 - the pre-N2e shape, so an old caller passing
/// nothing is unchanged) or [`TIMER_CLIENT_PACER`] (1 - a paced transport's
/// send-release deadline, continuously re-armed; docs/NETSTACK.md 21). It
/// transfers no authority and adds no object: it names a slot in a fixed table.
pub const SYS_ARM_TIMER: u64 = 47;

/// `wait_net(buf_va, len, timeout_ns)` -> frame_len. **Block** until a received
/// Ethernet frame is available on the NIC, copy up to `len` bytes of it into the
/// cell buffer at `buf_va`, and return the frame length (0 = the wait gave up:
/// no NIC, the timeout elapsed, or the bounded poll fallback expired).
/// `timeout_ns` of 0 waits indefinitely; a non-zero deadline is what a transport
/// needs for a retransmission timeout ("a frame, or the RTO, whichever comes
/// first"). The network twin of [`SYS_WAIT_INPUT`] (docs/NETSTACK.md 16,
/// rheo-net N2d).
///
/// Mechanism only - it adds **no kernel object** (ARCHITECTURE.md 6): it exposes
/// the same virtio-net driver the `OP_NET_*` opcodes bridge to. The per-ISA
/// interrupt wiring lives in `kernel/src/arch`; the portable wait + counters are
/// `kernel/src/net_rx.rs`.
pub const SYS_WAIT_NET: u64 = 48;

/// `yield_cell()` -> 0. Hand the CPU to the **next runnable native cell** in
/// round-robin order and resume there; the caller stays runnable and is reached
/// again on a later pass (docs/NETSTACK.md 17, rheo-net N4a). Where the caller
/// has no native process tree (two cells wired by a test kernel, never spawned)
/// this degenerates to the [`SYS_SWITCH`] `cur^1` peer hand-off, so the Phase
/// E/J two-cell behaviour is unchanged byte-for-byte.
///
/// It exists because [`SYS_SWITCH`] is a *directed* `cur^1` hand-off: with a
/// service cell and N>1 client cells, `cur^1` cannot reach client 3 from client
/// 2, so a fan-out would livelock between siblings. Mechanism over the Cell
/// object (1), **no new kernel object**, no authority transfer (a scheduling
/// hint within one capability bundle). Cooperative and single-CPU: concurrency,
/// not parallelism (SMP is task #27).
pub const SYS_YIELD: u64 = 49;

// -------------------------------------------------------------------------
// The capability verbs (docs/ARCHITECTURE.md 3 "mint/delegate/revoke
// capability", docs/ARCHITECTURE-DEBT.md 2.1)
// -------------------------------------------------------------------------
//
// Object 2 is described as "simultaneously the security model, the audit log,
// and the metering system", and `ARCHITECTURE.md` 3's verb set has named
// mint/delegate/revoke from the start - but none of them was reachable from a
// cell, so `derive_subset`/`delegate`/`revoke_epoch` had **zero production
// callers** and every kernel-side mint passed `BUDGET_UNLIMITED`. A cell could
// not narrow a capability before passing it on, and the promise that "an epoch
// revoke kills the peer's copy too" was unreachable code.
//
// These four verbs implement verbs the design already admitted; they are not a
// section 6 extension. They are also the hard prerequisite for the identity
// model (docs/IDENTITY.md 9): "root holds a maximal bundle" and "dropping
// privileges revokes" are claims, not mechanisms, without them.

/// Derive a **narrower** capability from one this cell holds, into this cell's
/// own table: `(handle, rights, budget, out_va) -> 0 | -errno`.
///
/// `rights` must be a subset of the parent's - widening is refused, which is
/// the monotonic-attenuation invariant of ARCHITECTURE.md 8.2 made reachable
/// rather than merely tested. `budget` may be [`BUDGET_UNLIMITED`] or any
/// finite count; a finite budget is decremented by every successful grant check
/// and exhausts. Writes a `u64` handle to `out_va`.
pub const SYS_CAP_DERIVE: u64 = 50;

/// Revoke every outstanding capability to a capability's object, in **every**
/// cell, by bumping the object epoch: `(handle) -> 0 | -errno`.
///
/// Requires [`RIGHT_REVOKE`] on the handle, so revocation is itself an
/// authority that can be withheld when a capability is delegated. O(1) - no
/// table is walked; a stale epoch fails the next grant check.
pub const SYS_CAP_REVOKE: u64 = 51;

/// Report what a handle actually carries: `(handle, out_va) -> 0 | -errno`,
/// writing a [`CapInfo`].
///
/// Introspection is what lets a cell *prove* an attenuation happened rather
/// than assume it, which is the difference between this being a mechanism and
/// being decoration (docs/ENGINEERING.md 1).
pub const SYS_CAP_INFO: u64 = 52;

/// Release a capability from this cell's table: `(handle) -> 0 | -errno`.
///
/// The object is untouched; only this cell's reference goes away. Dropping the
/// last handle does not destroy the object - object reclamation is separate and
/// still future work (docs/TILES.md 12).
pub const SYS_CAP_DROP: u64 = 53;

/// Report the calling **vcore's** index and how many its cell holds:
/// `(out_va: *mut VcoreInfo) -> 0 | -errno` (docs/SUBSTRATE.md pillar 3,
/// docs/CONCURRENCY.md).
///
/// A cell whose runtime schedules strands over several vcores has to know which
/// context it is running on, because every per-vcore structure - the executor, the
/// local run queue, the ring `SYS_QUEUE_INFO` reports - is indexed by it. Nothing in
/// userspace can work that out: there is no register a cell may read that says "you
/// are context 1 of your cell". Only the kernel knows, because the kernel decided.
///
/// **Admission audit** (docs/ARCHITECTURE.md 6). This adds **no kernel object**: a
/// vcore is an execution context of the Cell object (object 1), and this is a verb
/// over it, exactly as [`SYS_QUEUE_INFO`] is a verb over the QueuePair and
/// [`SYS_CONNECT`] over the shared channel. Against the three tests:
///
/// 1. **Unforgeable enforcement** - a cell cannot derive its own vcore index and must
///    not be able to claim a different one, since every per-vcore structure keys on
///    it. It cannot be a library.
/// 2. **Arbitrates shared hardware** - the answer *is* the kernel's own placement
///    decision about which core runs which context. No other cell knows it either, so
///    it cannot be a cell.
/// 3. **Mechanism with policy outside** - it reports two integers. What the runtime
///    does with them (one executor per vcore, work stealing, affinity) is entirely
///    the runtime's, and the kernel neither knows nor cares.
pub const SYS_VCORE_INFO: u64 = 54;

// =========================================================================
// ABI constants that are not syscall numbers
// =========================================================================

// -------------------------------------------------------------------------
// Capability rights (docs/KERNEL-RUST.md 2)
// -------------------------------------------------------------------------
//
// Defined here because both sides name them: the kernel checks them and, since
// [`SYS_CAP_DERIVE`], a **cell** chooses them. They were previously written out
// twice by hand - `kernel::capability` and `runtime::rights` - which is the
// duplication class this crate exists to delete (docs/ARCHITECTURE-DEBT.md 3.1).

/// Read the object's contents.
pub const RIGHT_READ: u32 = 1 << 0;
/// Modify the object's contents.
pub const RIGHT_WRITE: u32 = 1 << 1;
/// Execute from the object.
pub const RIGHT_EXECUTE: u32 = 1 << 2;
/// Hand this capability to another cell. Without it a capability is
/// cell-local, which is what makes "give it away" a decision rather than a
/// default.
pub const RIGHT_DELEGATE: u32 = 1 << 3;
/// Map the object into an address space.
pub const RIGHT_MAP: u32 = 1 << 4;
/// Revoke the object's epoch, killing **every** outstanding capability to it,
/// including copies held by other cells ([`SYS_CAP_REVOKE`]).
///
/// A separate right rather than an implied property of holding the capability:
/// delegating read access to a buffer must not hand over the power to
/// invalidate it for everyone. Minted to a creator, withheld from a derivation
/// unless asked for and already held.
pub const RIGHT_REVOKE: u32 = 1 << 5;

/// Every defined right. Not "whatever the creator happened to get" - a named
/// set, so a widening check has something exact to compare against.
pub const RIGHT_ALL: u32 =
    RIGHT_READ | RIGHT_WRITE | RIGHT_EXECUTE | RIGHT_DELEGATE | RIGHT_MAP | RIGHT_REVOKE;

/// "No budget metering" sentinel. A finite budget is decremented by every
/// successful grant check and exhausts.
pub const BUDGET_UNLIMITED: u64 = u64::MAX;

/// [`SYS_ARM_TIMER`] argument 1: hold the deadline in the cell-sleep slot (the
/// default, and the only behaviour before rheo-net N2e).
pub const TIMER_CLIENT_CELL_SLEEP: u64 = 0;
/// [`SYS_ARM_TIMER`] argument 1: hold the deadline in the **pacer** slot - a
/// paced transport's "release the next segment at `bytes/rate`" deadline,
/// re-armed after every send (docs/NETSTACK.md 21, rheo-net N2e).
pub const TIMER_CLIENT_PACER: u64 = 1;

/// How many cross-cell channel ends one cell can hold (docs/NETSTACK.md 17,
/// rheo-net N4a). Slot 0 is the Phase E/J channel; slots 1.. let a **service
/// cell** hold one end per client (the fan-out). Fixed, so the per-cell table
/// stays a static array - the kernel allocates nothing.
pub const MAX_CELL_CHANNELS: usize = 4;

/// [`SYS_SPAWN`] `chan_spec` flag: inherit the caller's channel **slot** named
/// in bits 15:8 rather than slot 0 (docs/NETSTACK.md 17). Spawn fails
/// (`u64::MAX`) if that slot holds no channel.
pub const SPAWN_CHAN_SLOT: u64 = 1 << 0;

/// Build a [`SYS_SPAWN`] `chan_spec` naming channel `slot` of the caller.
pub const fn spawn_chan_spec(slot: usize) -> u64 {
    SPAWN_CHAN_SLOT | ((slot as u64) << 8)
}

/// The exit code a native child is reaped with when it faults (native cells have
/// no signal delivery, docs/LIBRHEO.md Phase F): 128 + a SIGSEGV-shaped 11.
pub const FAULT_EXIT: u64 = 139;

/// The exit code a run ends with when the scheduler finds **nothing runnable and
/// no wake source left** - a genuine deadlock (docs/ARCHITECTURE-DEBT.md 2.4).
/// The scheduler prints which cell is blocked on what and ends the run with
/// this, rather than panicking with a kernel stack trace that names no cell.
/// 128 + a SIGSTOP-shaped 19, so it cannot collide with [`FAULT_EXIT`] or a real
/// exit code.
pub const DEADLOCK_EXIT: u64 = 147;

// Portable SIMD-tier bits reported in `CpuFeatures::simd` - the widths the
// kernel actually **enabled and validated** for U-mode (not just what CPUID
// claims), so a cell dispatches only to a path whose register state the kernel
// saves across cell switches (docs/TILES.md 4). x86 tiers are cumulative
// (AVX-512 implies AVX2 implies SSE2); ARM64 reports NEON; RISC-V reports none
// (scalar F/D only until RVV). A cell still runs its own functionality +
// benchmark probe before trusting a tier.

/// x86 SSE2 (the hard-float x86 baseline).
pub const SIMD_SSE2: u64 = 1 << 0;
/// x86 AVX2 (x86-64-v3).
pub const SIMD_AVX2: u64 = 1 << 1;
/// x86 AVX-512F (x86-64-v4).
pub const SIMD_AVX512F: u64 = 1 << 2;
/// x86 AVX-512-VNNI (Zen4 / Sapphire Rapids int8 `dpbusd`).
pub const SIMD_AVX512VNNI: u64 = 1 << 3;
/// ARM64 NEON (Advanced SIMD).
pub const SIMD_NEON: u64 = 1 << 4;

// =========================================================================
// The queue-pair on-wire layout (docs/IO.md 1, docs/KERNEL-RUST.md 3)
// =========================================================================
//
// A queue pair is a single contiguous shared region a separately-compiled
// library can bind to: a `repr(C)` [`QueueHeader`] with the ring indices at
// fixed offsets, followed by the SQ entry array and the CQ entry array. Both the
// kernel and a loaded cell overlay the *same physical frames* at their own VAs;
// the head/tail atomics live in the region, not in either Rust struct.

/// On-wire ABI version carried in the ring header. A cell binding the region
/// checks this before trusting the layout.
pub const QUEUE_ABI_VERSION: u32 = 1;

/// Ring depth (entries per ring). A power of two: the index masks depend on it.
pub const RING_DEPTH: usize = 64;
const _: () = assert!(RING_DEPTH.is_power_of_two());

/// Submission opcodes.
pub const OP_NOP: u8 = 0;
/// Echo `payload[0..4]` back through the completion's `result` field - the null
/// round trip with a data touch, used by the librheo async proof.
pub const OP_ECHO: u8 = 1;

// ---- async I/O opcodes (docs/LIBRHEO.md Phase B, docs/IO.md 1) ----
// Each reads its arguments from the `SqEntry.payload` (24 bytes) and completes
// through the CQ carrying the submission's `user_data` (the strand token). File
// work is performed via the registered `svc::FileOps` (the same VFS the POSIX
// personality uses), so the kernel stays filesystem-free. During a cell's
// `SYS_DOORBELL` trap its address space is active, so a `buf_va` in the payload
// is the cell's own mapped memory: the read/write lands there directly (no
// kernel bounce), which is the IO.md zero-copy-by-reference path.

/// `open(path_va, path_len, flags)`: payload `[path_va u64@0][path_len u32@8]
/// [flags u32@12]`; `result` = fd (or an I/O error status).
pub const OP_OPEN: u8 = 2;
/// `read(fd, buf_va, len, offset)`: payload `[buf_va u64@0][offset u64@8]
/// [len u32@16][fd u32@20]`; `result` = bytes read.
pub const OP_READ: u8 = 3;
/// `write(fd, buf_va, len, offset)`: same layout as [`OP_READ`]. With
/// [`FLAG_INLINE`] and `len <= INLINE_MAX`, the bytes ride in the payload
/// instead of a `buf_va` (the IO.md sub-threshold inline path): payload
/// `[fd u32@0][len u32@4][data @8..8+len]`. `result` = bytes written.
pub const OP_WRITE: u8 = 4;
/// `close(fd)`: payload `[fd u32@0]`.
pub const OP_CLOSE: u8 = 5;
/// `fstat(fd, statbuf_va)`: payload `[statbuf_va u64@0][fd u32@8]`.
pub const OP_FSTAT: u8 = 6;
/// Submit a userspace-built dependency graph to the CPU engine (docs/LIBRHEO.md
/// Phase C, docs/ARCHITECTURE.md 3 objects 4/6). Payload `[nodes_va u64@0]
/// [count u32@8][results_va u64@12]`: `count` [`GraphNode`]s live at
/// `nodes_va`; the kernel validates the edges, runs the graph on the CPU engine,
/// writes each node's `u64` result to `results_va`, and completes with `result`
/// = the node count.
pub const OP_GRAPH_SUBMIT: u8 = 7;

// ---- raw-frame networking opcodes (docs/NETWORKING.md, LIBRHEO.md Phase G) ----
// A cell's async `net::send`/`recv`/`mac` bridged to the NIC during the
// `SYS_DOORBELL` trap. Networking above raw frames (IP/TCP/QUIC) is a
// **service**, not a kernel object (docs/NETWORKING.md 1-2); the kernel owns
// only the queue plumbing, and reaches the device through `svc::NicOps` rather
// than naming a driver (docs/ARCHITECTURE-DEBT.md 3.2).

/// `net_tx(buf_va, len)`: payload `[buf_va u64@0][len u32@8]`; `result` = bytes
/// sent. Sends the `len` bytes at `buf_va` as one Ethernet frame.
pub const OP_NET_TX: u8 = 8;
/// `net_rx(buf_va, len)`: payload `[buf_va u64@0][len u32@8]`; `result` = the
/// received frame length (0 = no packet available; a cell that wants to *wait*
/// uses [`SYS_WAIT_NET`] rather than re-submitting in a spin).
pub const OP_NET_RX: u8 = 9;
/// `net_mac(buf_va)`: payload `[buf_va u64@0]`; writes the 6-byte MAC at
/// `buf_va`, `result` = 6.
pub const OP_NET_MAC: u8 = 10;

/// `gpu_present(buf_va, w, h)`: payload `[buf_va u64@0][w u32@8][h u32@12]`. The
/// `w x h` RGBA framebuffer at `buf_va` (the cell's own mapped memory) is copied
/// into the display resource, transferred to the host, and flushed to scanout 0.
/// `result` = bytes presented. Writes to the device, so it needs WRITE (the
/// `opcode_right` default). Extends the queue object (3) with a mechanism - no
/// new kernel object (ARCHITECTURE.md 6). docs/LIBRHEO.md Phase H, docs/DISPLAY.md.
pub const OP_GPU_PRESENT: u8 = 11;

/// A cell-to-cell message on a **cross-cell** channel (docs/LIBRHEO.md Phase E,
/// docs/IO.md 6). The kernel initialises such a channel's header but **never
/// drains it** - the two cells drive the SPSC rings directly over the shared
/// frames - so this opcode is deliberately absent from the kernel's dispatch:
/// it is a tag the peer interprets, not a verb the kernel serves.
pub const OP_CHAN_MSG: u8 = 12;

/// `SqEntry.flags` bit: the op's data rides inline in the payload rather than by
/// reference at `buf_va` (docs/IO.md 1 - the inline-vs-by-reference threshold).
/// librheo sets it for writes at or below [`INLINE_MAX`] bytes.
pub const FLAG_INLINE: u8 = 1 << 0;
/// `SqEntry.flags` bit: the IO.md durability contract "flush before completing".
/// Carried on the wire and understood by librheo's `io::Contract`; the current
/// kernel-side file bridge does not distinguish it (docs/IO.md - a named
/// deferral, not a silent one).
pub const FLAG_DUR_FLUSH: u8 = 1 << 4;
/// `SqEntry.flags` bit: the IO.md durability contract "force unit access".
/// Same status as [`FLAG_DUR_FLUSH`].
pub const FLAG_DUR_FUA: u8 = 1 << 5;
/// Largest inline write payload: what fits after the `[fd u32][len u32]` header
/// in the 24-byte payload. Above this, an op is by-reference (zero-copy).
pub const INLINE_MAX: usize = 16;

/// Completion status: the op succeeded.
pub const STATUS_OK: u32 = 0;
/// The opcode is not one this kernel serves.
pub const STATUS_BAD_OPCODE: u32 = 1;
/// The capability check refused the op (wrong rights, or a bounds/validation
/// refusal on a cell-supplied address, docs/ENGINEERING.md 12).
pub const STATUS_DENIED: u32 = 2;
/// The capability's object epoch was revoked (docs/SECURITY-IDENTITY.md 3).
pub const STATUS_REVOKED: u32 = 3;
/// The capability's metered budget is exhausted.
pub const STATUS_EXHAUSTED: u32 = 4;
/// The submission's `cap_id` names no live capability in the cell's table.
pub const STATUS_BAD_HANDLE: u32 = 5;
/// The op failed (no registered bridge, or the bridge returned -errno).
pub const STATUS_IO: u32 = 6;

/// A submission queue entry - exactly 64 bytes, one cache line, so producer and
/// consumer never false-share (docs/KERNEL-RUST.md 3).
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct SqEntry {
    pub opcode: u8,
    pub flags: u8,
    pub engine_id: u16,
    pub cap_id: u32,
    /// 16 bytes - the distributed trace handle.
    pub flow_id: u128,
    /// Returned in the [`CqEntry`] unchanged; librheo uses it as the strand token.
    pub user_data: u64,
    /// 24, not 32: header (8) + `flow_id` at its 16-alignment (16..32) +
    /// `user_data` (8) leaves exactly 24 bytes in a 64-byte line.
    pub payload: [u8; 24],
}
const _: () = assert!(core::mem::size_of::<SqEntry>() == 64);

impl SqEntry {
    /// All-zero entry, for static ring storage.
    pub const ZERO: SqEntry = SqEntry {
        opcode: 0,
        flags: 0,
        engine_id: 0,
        cap_id: 0,
        flow_id: 0,
        user_data: 0,
        payload: [0; 24],
    };

    /// One submission with no payload. `cap` is the capability's 32-bit ABI id;
    /// the kernel passes a `Handle` (which converts) and a cell passes the
    /// `cap_id` it was given by `SYS_QUEUE_INFO`/`SYS_CONNECT`.
    pub fn new(opcode: u8, cap: impl Into<u32>, flow_id: u128, user_data: u64) -> SqEntry {
        SqEntry {
            opcode,
            cap_id: cap.into(),
            flow_id,
            user_data,
            ..SqEntry::ZERO
        }
    }
}

/// A completion queue entry - 32 bytes.
#[repr(C, align(32))]
#[derive(Copy, Clone)]
pub struct CqEntry {
    pub flow_id: u128,
    pub user_data: u64,
    pub status: u32,
    pub result: u32,
}
const _: () = assert!(core::mem::size_of::<CqEntry>() == 32);

impl CqEntry {
    /// All-zero entry, for static ring storage.
    pub const ZERO: CqEntry = CqEntry {
        flow_id: 0,
        user_data: 0,
        status: 0,
        result: 0,
    };
}

/// The shared ring header (docs/IO.md 1): version + geometry + the four ring
/// indices, at fixed `repr(C)` offsets so an independently-compiled cell binds
/// to the same words the kernel does. Exactly one cache line (64 B).
///
/// Every field is public because the two overlays live in different crates; the
/// discipline that keeps it sound is *ownership of each index* (one producer and
/// one consumer per ring), not privacy.
#[repr(C)]
pub struct QueueHeader {
    /// ABI version ([`QUEUE_ABI_VERSION`]).
    pub version: u32,
    /// Ring depth (entries per ring); [`RING_DEPTH`].
    pub depth: u32,
    /// Byte offset of the SQ entry array from the region base.
    pub sq_off: u32,
    /// Byte offset of the CQ entry array from the region base.
    pub cq_off: u32,
    pub sq_head: AtomicU32,
    pub sq_tail: AtomicU32,
    pub cq_head: AtomicU32,
    pub cq_tail: AtomicU32,
    pub reserved: [u32; 8],
}
/// Asserted on **both** sides now that there is only one definition - the
/// pre-`rheo-abi` asymmetry (the kernel checked, librheo did not) is gone.
const _: () = assert!(core::mem::size_of::<QueueHeader>() == 64);

// =========================================================================
// Result blocks: `repr(C)` structs a syscall writes into a cell's memory
// =========================================================================

/// Machine-readable CPU feature report a cell reads via `SYS_CPUINFO(out_va)`
/// (docs/TILES.md 4). `features` is the raw per-ISA CPUID bitmask
/// (`arch::cpu_feature_names()`-indexed, for diagnostics); `simd` is the
/// portable `SIMD_*` tier mask of kernel-enabled+validated widths a cell
/// dispatches on; `vendor` is the ASCII vendor string, NUL-padded.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct CpuFeatures {
    pub features: u64,
    pub simd: u64,
    pub vendor: [u8; 16],
}

/// The [`SYS_DEBUG_WRITE`] argument block.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct DebugWrite {
    pub ptr: u64,
    pub len: u64,
}

/// The [`SYS_QUEUE_INFO`] result block.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QueueInfo {
    /// Base VA of the cell's queue-pair region (the [`QueueHeader`]).
    pub qp_va: u64,
    /// 32-bit ABI id of the cell's QueuePair capability (`Handle::raw_low32`).
    pub cap_id: u64,
}

/// The [`SYS_VCORE_INFO`] result block.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct VcoreInfo {
    /// This vcore's index within its cell, `0..count`.
    pub index: u64,
    /// How many vcores the cell holds. 1 for a cell that was given no extra ones,
    /// which is every cell that predates vcores.
    pub count: u64,
}

/// The [`SYS_CAP_INFO`] result block: what a handle actually carries.
///
/// `kind` is the [`ObjectKind`](../kernel/capability/enum.ObjectKind.html)
/// discriminant as a small integer; the constants are `CAP_KIND_*` below. A
/// cell reads this to *check* an attenuation rather than assume it.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct CapInfo {
    /// The object this capability names. Two handles with the same `object`
    /// reach the same thing, which is how a cell can tell a derivation from an
    /// unrelated capability.
    pub object: u32,
    /// `CAP_KIND_*`.
    pub kind: u32,
    /// The rights actually stored, which is the point: a derivation that asked
    /// for more than the parent held was refused, so what comes back here is
    /// what the kernel will enforce.
    pub rights: u32,
    pub _pad: u32,
    /// Remaining uses, or [`BUDGET_UNLIMITED`].
    pub budget: u64,
}

/// [`CapInfo::kind`] values. Stable numbers rather than the Rust enum's
/// layout, because this crosses the ABI.
pub const CAP_KIND_MEMORY_GRANT: u32 = 0;
pub const CAP_KIND_QUEUE_PAIR: u32 = 1;
pub const CAP_KIND_FILE: u32 = 2;
pub const CAP_KIND_STREAM: u32 = 3;
pub const CAP_KIND_RESERVATION: u32 = 4;
pub const CAP_KIND_CELL: u32 = 5;

/// The [`SYS_GRANT`] result block.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GrantInfo {
    /// Base VA of the reserved grant region.
    pub base: u64,
    /// 32-bit ABI id of the minted MemoryGrant capability.
    pub cap_id: u64,
}

/// The [`SYS_CONNECT`] result block.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ChannelInfo {
    /// Base VA of the cell's mapped shared-channel region (the [`QueueHeader`]).
    pub chan_va: u64,
    /// 32-bit ABI id of the cell's QueuePair capability for the channel.
    pub cap_id: u64,
    /// 0 = initiator (client: SQ producer, CQ consumer); 1 = acceptor (server:
    /// SQ consumer, CQ producer). The two ends drive opposite sides of the SPSC.
    pub role: u64,
    /// How many channel slots this cell holds in total (docs/NETSTACK.md 17). 1
    /// for a Phase E/J cell; N for a service serving N clients, which uses it to
    /// size its per-client fan-out.
    pub count: u64,
}

/// The [`SYS_GRANT_SHARE`] result block.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ShareInfo {
    /// Base VA at which the grant's frames were mapped read-only in the peer.
    pub peer_va: u64,
    /// 32-bit ABI id of the MemoryGrant capability minted in the peer's table.
    pub peer_cap_id: u64,
}

/// The [`SYS_ENGINE_INFO`] result block. `kind`: 0=CPU, 1=GPU (enumerated from
/// PCIe - recognised and registered, executable only via its future driver cell,
/// docs/GPU-HARDWARE.md 5). `preemption`: 0=per-instruction, 1=per-op-boundary
/// (the declared accelerator contract). `vendor` is the PCI vendor ID for a
/// device engine (0x10DE NVIDIA, 0x1002 AMD, 0x8086 Intel, 0x1AF4 virtio), 0 for
/// the CPU.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct EngineInfo {
    pub kind: u64,
    pub measured_cost_ticks: u64,
    pub preemption: u64,
    pub vendor: u64,
}

/// The [`SYS_RESERVE_ADMIT`] success block. `handle` is the 32-bit ABI id of the
/// minted Reservation capability; `committed_ppm` is the cell's total committed
/// utilization after admission.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ReserveInfo {
    pub handle: u64,
    pub committed_ppm: u64,
}

/// Graph-node op 4 (BufReduce) descriptor (docs/TILES.md 6): `node.a` is the
/// cell VA of this struct. The engine returns the wrapping u64 sum over `elems`
/// elements of `dtype` (0=I8, 1=U8, 2=I32; signed sign-extends). Validation
/// caps: `va != 0`, `0 < elems <= 1<<20`, `dtype <= 2` - anything else completes
/// [`STATUS_DENIED`].
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BufReduceDesc {
    pub va: u64,
    pub elems: u64,
    pub dtype: u32,
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<BufReduceDesc>() == 24);

/// Graph-node op 5 (TileGemm) descriptor (docs/TILES.md 6): `node.a` is the cell
/// VA of this struct. The engine zeroes C, runs the int8->i32 GEMM whole (the
/// node is the tile loop), and returns the FNV-1a hash of C's logical m x n
/// window - the deterministic receipt; the buffer carries the real output.
/// Strides are in elements. Validation caps: VAs non-zero, `1 <= m,n,k <= 256`,
/// strides >= the matching dims, `dtype_in == 0` (I8) and `dtype_acc == 2`
/// (I32) exactly - the kernel path is integer-only (the aarch64 kernel target is
/// soft-float). Worst node = 16.7M MACs, worst graph = 32 nodes: the documented
/// drain bound.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TileGemmDesc {
    pub a_va: u64,
    pub b_va: u64,
    pub c_va: u64,
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub a_stride: u32,
    pub b_stride: u32,
    pub c_stride: u32,
    pub dtype_in: u32,
    pub dtype_acc: u32,
}
const _: () = assert!(core::mem::size_of::<TileGemmDesc>() == 56);

/// One node of a userspace-built dependency graph (docs/LIBRHEO.md Phase C,
/// docs/ARCHITECTURE.md 3 object 6). A cell writes an array of these into one of
/// its buffers and submits it with [`OP_GRAPH_SUBMIT`]; the kernel validates the
/// edges (topological), runs it on the CPU engine, and writes each node's `u64`
/// result back. `op`: 0=Const (value in `a`), 1=Add, 2=Mul, 3=Select, 4=BufReduce
/// ([`BufReduceDesc`] VA in `a`), 5=TileGemm ([`TileGemmDesc`] VA in `a`). For
/// Add/Mul/Select each input is an immediate (`*_is_node == 0`, value in
/// `a`/`b`) or an earlier node's result (`*_is_node == 1`, node index in `a`/`b`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GraphNode {
    pub op: u32,
    pub a_is_node: u32,
    pub b_is_node: u32,
    pub _pad: u32,
    pub a: u64,
    pub b: u64,
}
const _: () = assert!(core::mem::size_of::<GraphNode>() == 32);

/// The `stat`/`fstat` result block (kept in sync with the std `fs` arm in
/// targets/std-rheo/fs.rs, which is outside this workspace and so cannot depend
/// on this crate - the one remaining hand-kept copy, named here on purpose).
/// `kind`: 0 regular, 1 dir, 2 symlink, 3 other.
///
/// `ino` is the filesystem's inode number (the VFS `NodeId`), distinct per file.
/// It is load-bearing, not decorative: glibc's `ld.so` dedups shared libraries by
/// `(st_dev, st_ino)`, so two different libraries reporting the same inode make the
/// loader treat the second as already-loaded and never map it - which broke every
/// multi-library dynamic binary until this field carried a real value
/// (docs/LINUX-COMPAT.md, docs/ENGINEERING.md 11).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Stat {
    pub size: u64,
    pub kind: u64,
    pub ino: u64,
}
const _: () = assert!(core::mem::size_of::<Stat>() == 24);
