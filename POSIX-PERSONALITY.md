# POSIX Personality

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.12 and doctrine 10.

Position: POSIX is a **compatibility personality at the edge**, never the
native model (the mistake Hurd made). It is a translation layer - gVisor-
style - implementing POSIX syscalls over cells, capabilities, and queues, so
existing software runs while new software targets the native primitives
directly. You can SSH in and get bash; underneath, your shell is a cell, your
terminal is a queue pair, your filesystem is a per-identity view over the
object store.

## 1. How the translation works

- A **POSIX personality cell** hosts one or more legacy processes and
  implements the syscall surface (`open`, `read`, `write`, `ioctl`, signals,
  `fork`, `mmap`, ...) by mapping each onto native operations. Unmodified
  Linux binaries run inside it.
- The personality holds capabilities on behalf of the processes it hosts;
  those processes never see native handles - they see file descriptors,
  PIDs, and signals, which the personality synthesizes.
- Isolation is still the cell boundary: a POSIX process cannot exceed the
  personality cell's grants, so "legacy software" is contained exactly like
  everything else.

## 2. The hard translations (stated honestly)

- **fork.** Fundamentally hostile to a capability model - duplicating an
  address space plus grants has no clean meaning. Implemented as
  "clone a cell within the same capability bundle," which covers the shell's
  dominant fork+exec pattern well. Copy-on-write fork of a large heap with
  immediate divergence is where it is weakest; documented.
- **mmap.** Anonymous and local-file mappings work. `mmap` of a *remote*
  object is refused transparently (no network paging, doctrine 4) -
  pin-local-first or fail loudly.
- **Signals.** Mapped onto event delivery on the process's queue. Common
  signals (SIGTERM, SIGINT, SIGCHLD, SIGPIPE) behave correctly; exotic
  real-time signal ordering and some `sigaction` corners are approximated
  and documented.
- **PTYs.** Bidirectional byte queues plus a control channel (window size,
  signals). bash never knows the difference.
- **rename/flock/locking.** Per FILESYSTEMS.md 5: atomic within a namespace
  view, lease-based for cross-cell locks (better under failure, not bit-
  identical).
- **/proc, /sys.** Synthesized, read-only, scoped to the caller's own cells,
  format-compatible for common tools, deliberately incomplete. The
  cross-tenant leak surface of a real /proc does not exist.

## 3. The filesystem you see

- The native model has no global tree, so the personality synthesizes a `/`
  **per session** from the identity's capabilities: mounts appear because you
  hold grants, not because a global mount table says so. Two users' `/` can
  legitimately differ.
- This is Plan 9's per-process namespace, now on a security model that
  enforces it (FILESYSTEMS.md 3).

## 4. Login and identity

- SSH authenticates a *person* (key/certificate); sshd (a cell holding a
  bounded minting grant) creates a **session cell** with that identity's
  entitled capability set. There is no root to become.
- `sudo` is grant escalation to a policy service, possibly gated by approval
  or a second factor, and is itself an audit event (SECURITY-IDENTITY.md 4).
- The whole login path is ordinary cells and queues: sshd holds a network
  queue endpoint grant; the terminal is a queue pair; "ssh host7" is just a
  placement constraint on where the session cell spawns (multi-host is the
  same mechanism, doctrine 9).

## 5. Fidelity expectations

- **~99% interactive fidelity:** an SSH shell, editing files, running
  compilers, common CLI tools feel normal.
- **~80% arbitrary-script fidelity:** scripts that assume Linux-exact /proc
  formats, specific signal-ordering, cgroup files, or `/dev` layouts are the
  failure population.
- ARCHITECTURE.md P11 gates this: interactive parity plus >= 80% of a defined
  coreutils/tooling suite, with < 60% suite pass as the kill threshold.

## 6. Relationship to native software

The personality is explicitly a bridge, not a destination:

- New services target native queues, capabilities, and the IDL directly -
  they get the async model, typed memory, and identity for free.
- Legacy services run in the personality until rewritten or replaced, with
  the honest gaps above.
- There is no ambition to be a POSIX-conformant OS; there is an ambition to
  run the software people actually have while they migrate.

## 7. Relationship to emulation

The POSIX personality handles *same-ISA* legacy software (a Linux x86-64
binary on a Lattice x86-64 host). Cross-ISA execution and full-machine
emulation are a separate concern (EMULATION.md) - though the two compose: a
foreign-ISA Linux binary runs under the emulation layer *inside* a POSIX
personality cell.
