# Identity, Users, and Permissions

**Status:** Design v1.0 - PLANNED, nothing here is implemented yet. This
document is the source of truth for the identity model; where it disagrees with
`SECURITY-IDENTITY.md` or `POSIX-PERSONALITY.md`, this document wins and those
have been amended to point here (section 12 records exactly what changed and
why).

Companions: `ARCHITECTURE.md` §3 (the object model) and §6 (the kernel
admission rule); `SECURITY-IDENTITY.md` (the cross-host, cryptographic half);
`POSIX-PERSONALITY.md` (the compatibility surface); `BOOT.md` (the chain of
trust); `FILESYSTEMS.md` (where file metadata lives).

---

## 1. The model in one sentence

**Identity is attested, never asserted. A name is not authority - it is the
thing authority is issued *to*, for a bounded time.**

Everything below follows from that sentence. The kernel stamps a name it can
prove. A userspace service turns that name into capabilities. POSIX users,
groups and `rwx` are a faithful *projection* of the same machinery, computed
where POSIX belongs in a microkernel - in the file server and the personality,
never in the kernel.

---

## 2. Why the previous position was wrong

`SECURITY-IDENTITY.md` §4 was titled *"No users, no root"* and stated *"UID 0
does not exist"*. `POSIX-PERSONALITY.md` §4 said *"There is no root to
become"*.

That position was correct about the *mechanism* and wrong about the *world*.

- It is right that authority must not come from a number in a process
  structure. Ambient authority is the thing a capability system exists to
  delete, and `uid == 0` as a check scattered through a kernel is the canonical
  example of what not to build.
- It is wrong that the answer is to have no users. An OS that intends to run
  unmodified Linux software - which this one does, as its binding goal - meets
  `getuid`, `chown`, `chmod`, `access`, `setuid`, `/etc/passwd` and a shell
  prompt on its first day. Answering those with a hardcoded constant is exactly
  the stub class `ENGINEERING.md` §7 forbids: today `getuid`/`geteuid`/`getgid`/
  `getegid` all return a literal `1000` (`kernel/src/linux/mod.rs:422`), no
  file has an owner, and **no permission check exists anywhere in the tree**. A
  program that drops privileges here believes it dropped them and did not.
- It is also wrong operationally. A machine needs a way to be repaired when its
  identity service will not start. "No single-user mode" is not a security
  property; it is an availability bug that gets solved with a rescue USB stick
  and a lie about what happened.

So the amendment is not "add POSIX identity on the side". It is: **name the
thing the capability model was always issuing capabilities *to*, and let POSIX
read that name in the format it understands.** A user was always implicit in
"the identity's entitled capability set" - it just had no name, no number, and
no place to live.

---

## 3. Three layers, and why they do not fight

| Layer | Owns | Never does |
|---|---|---|
| **Kernel** | Stamps each cell with a `PrincipalId` it derived itself; reports it truthfully; refuses to let a cell change it | Makes any access decision from it. `grant_check` is untouched |
| **`identityd`** (a service cell) | The `rheo://` name space, the user/group tables, short-lived credentials, privilege transitions | Sit in any data path. It is consulted at bind time, not per access |
| **File server + personality** | POSIX `uid`/`gid`/`rwx` metadata and the mode-bit check; the per-process credential | Hold authority of its own. Its power is exactly the capabilities it was minted |

The reason this composes instead of colliding: **the kernel treats a principal
as evidence, not as permission.** It is in the same class as the measured
throughput recorded at engine attach (`ARCHITECTURE.md` §3 object 4) - an
observed fact the kernel reports and refuses to fabricate, which something
above it uses to make a policy decision. `ENGINEERING.md`'s observe-never-infer
rule is the whole design here.

Concretely, the hot path does not change. A read is still `grant_check` on a
capability the cell already holds. The `rwx` check happened once, at `open`, in
the file server, and produced a capability. That is the same shape as every
other bind-time check in the system.

---

## 4. Does this add a kernel object? No.

`ARCHITECTURE.md` §6 requires three proofs. Run them on "Principal":

1. **Needs unforgeable enforcement?** Yes. A cell must not be able to claim an
   identity it does not have, and no library can establish "the cell at the
   other end of this channel is X". Only the kernel knows, because only the
   kernel created the cell from a measured image.
2. **Arbitrates shared hardware?** **No.** An identity arbitrates nothing
   physical.
3. **Mechanism with policy outside?** Yes, provided the kernel only stamps and
   reports.

Test 2 fails, so a Principal is **not** an eleventh object. It is a field on
**Cell** (object 1), set at creation and immutable thereafter - the same class
of thing as the cell's `Personality` tag, which is also per-cell state the
kernel branches on without it being an object.

**No new verb either.** The verb set in `ARCHITECTURE.md` §3 already contains
**`attest`**. "Report the attested principal of this cell, or of the peer on
this channel" *is* that verb, narrowed to the local case. The reporting
surfaces are:

- a cell asking about itself - the `attest` verb;
- a service asking about its client - an extra field in the `ChannelInfo` that
  `SYS_CONNECT` (43) already returns, which is the unforgeable `SO_PEERCRED`
  the L6 pipe/channel machinery currently lacks.

That is the whole kernel surface. Everything else in this document is a
composition, per §6's closing instruction that new workloads should land as
compositions.

---

## 5. Names: SPIFFE and Azure Managed Identity, unified

Both systems solve the same problem and this design takes the same shape,
because the shape is right: **attest the workload, give it a name, hand it a
short-lived credential, never give it a secret.**

| Concern | SPIFFE / SPIRE | Azure Managed Identity | rheo-os |
|---|---|---|---|
| The name | `spiffe://td/path` | principal id / client id | `rheo://<trust-domain>/<path>` + the numeric `PrincipalId` |
| Who vouches | SPIRE agent, from node + workload attestation | The Azure fabric, via IMDS | **The kernel**, from the image measurement it made when it created the cell |
| The credential | X.509 or JWT SVID, short TTL, auto-rotated | OAuth token, short TTL, auto-refreshed | A **Lease** (object 8): fencing token + epoch revoke locally; a signed document when it crosses a host boundary (`SECURITY-IDENTITY.md` §2) |
| How you fetch it | Workload API over a Unix socket | HTTP to `169.254.169.254` | A channel to `identityd`. No address to guess, no socket path, no metadata endpoint - you have the channel because you are a cell |
| Secret in the workload | none | none | none |
| Rotation | agent re-issues | fabric re-issues | lease renewal; expiry is the default state |

The one place rheo-os is structurally better, and it is worth stating because
it is the reason to build this natively rather than port an agent:

> **SPIFFE and Azure MI must both *infer* which workload is calling** - from a
> pid, a cgroup path, a container id, a network namespace. That inference is a
> race (the pid can exit and be reused between the call and the lookup) and a
> spoofing surface. Here there is nothing to infer. The kernel stamped the
> principal when it created the cell, from the image it measured, and it
> reports that stamp on the channel. **There is no runtime attestation step, so
> there is no runtime attestation race.**

`identityd` is therefore the SPIRE agent and the IMDS endpoint at once, minus
the attestor - because the attestor is the kernel and it already ran.

### The name space

```
rheo://<trust-domain>/system/<service>      identityd, the file server, drivers
rheo://<trust-domain>/system/root           the root principal (uid 0)
rheo://<trust-domain>/user/<name>           a human account
rheo://<trust-domain>/tenant/<t>/<workload> a tenant workload
```

A `PrincipalId` is the kernel's compact handle for one of these. The kernel
never parses the path; `identityd` owns the mapping in both directions.

### How a principal is derived

At cell creation the kernel computes the principal from facts a cell cannot
choose:

- the **measurement of the loaded image** (content hash - so image identity is
  code identity, per `SECURITY-IDENTITY.md` §1);
- the **parent's principal**;
- for the very first cell, the **boot configuration hash** as well (section 8).

A cell can be created with a *narrower* principal than its parent if the parent
holds the capability to assume it (that is how `identityd` gives a login
session its user identity). It can never widen, and it can never set its own.
Same monotonic-attenuation invariant as capabilities (`ARCHITECTURE.md` §8.2),
for the same reason.

---

## 6. The POSIX projection

POSIX identity is **real and enforced**, and it is entirely a projection of
section 5 onto the numbers and bits POSIX programs expect.

### Users and groups

- A POSIX `uid` is a **short numeric alias for a principal**. The mapping lives
  in `identityd` and is served through the usual `/etc/passwd` and `/etc/group`
  files, so `getpwuid`, `id`, `ls -l` and every other tool works unmodified.
- A **group** is a principal too - one that names a set rather than a
  workload - so group membership is a relation `identityd` owns, not a second
  mechanism.
- The per-process credential (`uid`, `euid`, `suid`, `gid`, `egid`,
  supplementary groups) is **per-cell synthesized state in the personality**,
  exactly like pids, fds and signals (`LINUX-COMPAT.md` §1). It adds no kernel
  object. `fork` copies it; `execve` applies the POSIX rules to it.

### `rwx`

- File **owner, group and mode** are ordinary filesystem metadata. They live in
  the filesystem, and the **file server checks them** - which is precisely
  where a microkernel puts POSIX permission checks, and precisely where this
  tree's filesystem code already is (`posix/`, outside the kernel; the kernel
  is filesystem-free by `ARCHITECTURE.md` §5 and stays that way).
- The `svc::FileOps` bridge gains the caller's credential as an argument. That
  is the one real ABI change, and it is the same shape as the flow context the
  queue already propagates.
- `chmod`, `chown`, `umask`, `access`, `faccessat` become real operations
  instead of the missing or always-yes answers they are today.

### Root

`root` is a principal, `rheo://<td>/system/root`, aliased to uid 0. What makes
it powerful is **not** the number:

> At boot, the root principal is minted the maximal capability bundle. "Root
> can do anything" is a true statement about a set of capabilities, not a
> branch in a check.

The one thing that looks like a `uid == 0` test is the file server's permission
bypass - and it is a **capability**, not a number:

- A caller holding **`FsOverride`** bypasses mode bits. Root holds it; nobody
  else does by default.
- Because it is a capability it can be **delegated, attenuated, revoked, and
  audited**. This is `CAP_DAC_OVERRIDE` with the properties Linux's capability
  bits never got.
- A service that needs to read every file gets `FsOverride` and *nothing else* -
  it is not root, it just has the one power root has for that purpose. That is
  the point.

**Dropping privileges is real.** `setuid`/`setgid` do not merely change a
number: the personality asks `identityd` for a credential for the target
principal, which succeeds only if the caller holds an `Assume(principal)`
capability, and the old capabilities are **revoked** on the transition. So a
daemon that starts as root and drops to `www-data` genuinely cannot get back -
`derive_subset` + `revoke` are the mechanism, and `ARCHITECTURE.md` §8.2
property 4 (disjoint capability sets) is what makes it checkable rather than
asserted.

**`sudo`** is a request to `identityd` for a short-lived credential for another
principal, subject to policy, and it is an audit event on the event stream
(object 10) - which is what `SECURITY-IDENTITY.md` §4 already said, now with a
concrete mechanism.

**A setuid binary** falls out of the same machinery and is the nicest
consequence: the file server reports the setuid bit and the image measurement;
`identityd` decides whether *that measured image* may assume *that principal*.
That is SPIFFE workload attestation applied to setuid, and it removes the
oldest privilege-escalation surface in Unix - a setuid bit on an arbitrary file
is not enough, the image itself must be entitled.

### What POSIX fidelity means here

Faithful where it is observable, honest where it is not. `ls -l` shows real
owners; a 0600 file owned by another user is genuinely unreadable; `id` prints
the truth. But ACLs beyond the classic bits, and the full Linux capability-bit
set (`CAP_NET_RAW` and friends), are deliberate deferrals - the native
equivalent is holding the capability, and a fake `capget` would be another
stub reporting success.

---

## 7. What enforces what

Read this table as the answer to "where would I look when a permission
decision surprises me".

| Decision | Enforced by | Mechanism |
|---|---|---|
| May this cell touch this memory / queue / engine? | Kernel | `grant_check` on a capability. Identity is not consulted |
| Is this cell who it says it is? | Kernel | The principal it stamped at creation; reported, never accepted from the cell |
| May this uid open this file? | File server | Mode bits against the caller's credential, or the `FsOverride` capability |
| May this process become that user? | `identityd` | An `Assume` capability, plus policy |
| May this workload reach that service? | Capability issuance | There is no channel to a service you were not connected to (`SECURITY-IDENTITY.md` §6) |
| Is this host allowed in the trust domain? | Attestation chain | `BOOT.md` §1 |

Note the asymmetry, and that it is deliberate: **the kernel enforces
capabilities, userspace enforces identity.** Reversing that - putting a uid
check in the kernel - is the thing this design exists to avoid.

---

## 8. Boot: flags, exposure, and modes

### The flag surface

The kernel parses a boot command line once and keeps it immutable.

- **Source is per-ISA, everything above it is portable** (`TARGET-ARCHITECTURES.md`
  §4): a single `arch::boot_cmdline()` reads x86-64's PVH `hvm_start_info`
  cmdline pointer, and `/chosen/bootargs` from the flattened device tree on
  ARM64 and RISC-V. The firmware plumbing for all three already exists in
  `kernel/src/hw/`. Above that there is one portable `BootConfig` and no
  `cfg(target_arch)` anywhere.
- **Readable in user mode, never writable.** Two surfaces, one source:
  `SYS_BOOTINFO` for native cells, and `/proc/cmdline` for the Linux
  personality - the file real programs actually read. A cell cannot modify it
  and cannot forge it.
- **Measured.** The command line's hash is an input to the initial cell's
  principal derivation (section 5). This matters more than it looks: it means
  *how the machine was booted is attestable*, so a single-user boot cannot be
  hidden from a remote verifier. Adding a privileged boot mode is only
  defensible because of this.

### The modes

| `rheo.boot=` | What it does |
|---|---|
| `normal` (default) | `identityd` starts first; the initial cell gets a **narrowed** bundle; login is required to get a user session |
| `single` | No `identityd`, no login, console only, networking not started. The initial cell **is** the root principal with the full bundle |
| `recovery` | `single`, plus the root filesystem mounted read-only, for repair |

The critical property: **the modes differ only in which capabilities the first
cell is minted.** There is no "am I in single-user mode?" branch in any check,
anywhere. Single-user mode is not a bypass of the security model - it is the
security model, started from a different initial capability set. That is what
makes it complement the architecture instead of punching a hole in it.

Stated plainly, because pretending otherwise would be dishonest: **a
single-user boot is a full-authority boot.** What protects it is what protects
it on any other OS - control of the console and the boot path - plus one thing
this design adds, which is that it is measured, so it is *visible* rather than
silent.

---

## 9. Prerequisites

This cannot be enforced yet, and the reasons are already on the register:

- **`ARCHITECTURE-DEBT.md` §2.1 (task #127) - the capability userspace
  surface.** `derive_subset`, `delegate` and `revoke_epoch` have zero
  production callers. Without them reachable from a cell, "root holds a bundle"
  and "dropping privileges revokes" cannot be implemented - they would be
  claims, not mechanisms. **Hard prerequisite.**
- **§2.3 (also #127) - per-child capability tables.** `install_spawned` copies
  the parent's table pointers verbatim, so every descendant inherits everything.
  A session cell that inherits root's bundle is not a session cell. **Hard
  prerequisite.**
- **§2.6 (task #129) - the ambient-authority sweep.** All 18 `svc` verbs are
  reachable with no capability check. Any identity gate above an ambient
  syscall surface is decoration. **Hard prerequisite.**
- **A writable filesystem with persistent metadata.** ramfs can carry owner and
  mode; the ext4 driver is read-only, so `chown`/`chmod` survive a reboot only
  once a writable backing store exists (`FILESYSTEMS.md`).

Sequencing this after #127 and #129 is not a delay - those two *are* the
foundation this stands on, and the register already orders them next.

---

## 10. Phases

Each phase is docs-first, additive (`ENGINEERING.md` §8 - pre-existing proofs
pass unedited), and carries a three-ISA proof observed to fail when reverted.

- **ID0 - doctrine.** This document; the amendments in section 12.
- **ID1 - boot flags.** `arch::boot_cmdline()` on three ISAs, the portable
  `BootConfig`, `SYS_BOOTINFO`, `/proc/cmdline`. *Proof:* the same flag string
  read on all three ISAs from three different firmware mechanisms, and a cell's
  attempt to write it refused.
- **ID2 - the kernel principal.** `PrincipalId` on Cell, derived from the image
  measurement + parent, immutable; reported by `attest` and in `ChannelInfo`.
  *Proof:* a cell cannot change its own; a service reads its client's principal
  and it matches the measurement of the image the test loaded; two cells from
  different images get different principals.
- **ID3 - POSIX credentials, honestly.** Per-cell `Cred`; real `getuid`/
  `geteuid`/`getgid`/`getegid`/`setuid`/`setgid`/`setgroups`/`getgroups`;
  `fork` copies, `execve` applies the rules. No enforcement yet - but
  `getuid` stops returning a hardcoded 1000. *Proof:* an unmodified glibc
  binary observes a credential that changes when it changes it, and survives
  `fork` and `execve` correctly.
- **ID4 - `rwx` enforcement.** `Metadata` gains uid/gid; `FileOps` carries the
  credential; `open`/`stat`/`mkdir`/`unlink`/`getdents` enforce mode bits;
  `chmod`/`chown`/`umask`/`access` become real; `FsOverride` is the only
  bypass. *Proof:* an unprivileged cell is refused a 0600 file owned by another
  uid, root reads the same file, and the refusal is observed to disappear when
  the check is removed.
- **ID5 - `identityd`.** The service cell, the `rheo://` namespace, `/etc/passwd`
  and `/etc/group`, short-lived credentials over Lease, `setuid`-as-capability,
  login / `su` / `sudo`, the SPIFFE and Azure-MI mappings made real. *Proof:* a
  daemon drops privileges and is then structurally unable to regain them - the
  capability is gone from its table, not merely unused.
- **ID6 - boot modes.** `single` and `recovery` as initial-bundle selection,
  measured into the initial principal. *Proof:* the initial cell's capability
  set differs by mode, the mode is visible in the attested principal, and no
  permission check anywhere gained a mode branch.

---

## 11. Honest deferrals

- **POSIX ACLs, extended attributes, and the Linux capability-bit set.** The
  native answer is holding the capability. A fake `capget`/`setcap` would be a
  stub reporting success, which is worse than `ENOSYS`.
- **Cross-host identity.** The signed-document form, trust-domain federation
  and offline verification are `SECURITY-IDENTITY.md`'s subject and stay
  future work; ID5 builds the single-host half against that shape so it
  extends rather than gets replaced.
- **Hardware root of trust.** `BOOT.md` §1 requires TPM or DICE. A TPM 2.0
  is now present and driven here - `kernel/src/hw/tpm.rs` speaks FIFO/TIS and
  `TPM2_GetRandom` against a real `swtpm` backend on all three ISAs
  (docs/TIME-IDENTITY.md 4a) - so the deferral narrows: the *hardware* exists,
  and what is not built is the trust chain over it (PCR extend/quote, sealed
  keystores). Until that lands the measurement chain stays rooted in the
  kernel's own image hash and that limit is stated wherever a principal is
  reported - a measured principal without a hardware-rooted chain proves
  *what* is running, not that the measurement itself was not tampered with
  before the kernel started.
- **Kernel-mediated signing (the Ethos precedent - comparison/ethos/).**
  Ethos implements signing as a syscall where applications never hold private
  keys and kernel policy names *which application may sign which kind of
  statement*. Three notes bind that precedent to this design for whichever
  phase builds a signing surface: (a) the apps-never-hold-keys shape is
  already proven here in miniature - the per-cell DRBG is a library over
  cell-owned state derived from a root the cell never sees; (b) "which kind
  of statement" should be an IDL **type hash** (the Etypes idea), so signing
  policy composes with the typed-channel work rather than inventing a second
  naming scheme; (c) the keystore's sealing target is the TPM above. None of
  this is buildable before the §9 prerequisites - a signing verb above an
  uncapability-checked `svc` surface would be decoration.
- **Login over the network.** sshd is a much later cell; ID5's login path is
  the console.
- **Quotas and per-user resource accounting.** Reservations (object 7) are
  per-cell today; making them per-principal is a separate design.

---

## 12. What this changes in the existing docs

Recorded explicitly, because these are reversals of written doctrine and
`ENGINEERING.md` requires saying so rather than quietly editing.

| Document | Was | Now |
|---|---|---|
| `SECURITY-IDENTITY.md` §4 | "No users, no root. UID 0 does not exist." | Users and root exist as **names**. uid 0 is an alias for the root principal and confers nothing by itself; root's power is the capability bundle minted to it at boot. The original intent - no ambient authority - is preserved exactly, and is now enforceable rather than achieved by omission |
| `POSIX-PERSONALITY.md` §4 | "There is no root to become." | There is, and becoming it is an `Assume` capability plus policy, not a setuid bit. `sudo` is unchanged in spirit: grant escalation, audited |
| `ARCHITECTURE.md` §3 | Cell = address space + capability set + queues | Cell also carries an immutable, kernel-derived `PrincipalId`. **No new object** (§6 test 2 fails - identity arbitrates no hardware) and **no new verb** (`attest` already exists) |
| `BOOT.md` | No boot-flag or boot-mode surface | A measured, immutable, user-readable `BootConfig`; three boot modes that differ only in the initial capability bundle |

The sentence that survives all of it, and is the reason the amendment is safe:
**a name is not authority.** The previous doctrine got that right and then
concluded, too fast, that names should not exist.
