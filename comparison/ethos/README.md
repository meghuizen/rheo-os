# comparison/ethos/ - the Ethos operating system, judged

Ethos is the comparison that matters for a different reason than seL4 or
Linux. Those measure *mechanisms* this tree also has; Ethos is the other
team that made the same opening bet - a clean-slate OS that refuses POSIX as
its foundation because the API's shape is where the bugs come from - and then
spent its budget on different things. Comparing against it is comparing two
answers to one question, which is where a design learns.

## Provenance, stated first

**The Ethos source is unreachable from this environment, and this document
does not pretend otherwise.** The canonical repository is
`gitolite3@gitolite.ethos-os.org:/ethos`, which requires an authorized SSH
key (this container additionally has no `ssh` binary and an HTTPS-only
network proxy that refuses the ethos-os.org domains). The one public GitHub
mirror (`jagooding/ethos`) was cloned and inspected: it is an empty
placeholder - a single commit holding a LICENSE and a two-line README.

So every claim below about Ethos comes from its **published papers**, cited
inline, and the comparison is **design-level**: it says what Ethos's papers
say the system does, never what its code does. One paper - MinimaLT - was
read in full; the others through their published texts and abstracts. This
is the same rule `comparison/README.md` applies to numbers ("a published
reference, never a fabricated local number"), applied to designs.

Sources:

- **[Etypes]** Petullo, Fei, Solworth, Gavlin - *Ethos' Distributed Types*
  (2013) and *Ethos' Deeply Integrated Distributed Types* (IEEE S&P
  Workshops 2014).
- **[Auth]** Petullo, Solworth - *Authentication in Ethos* (2013) and
  *Digital Identity Security Architecture in Ethos* (2011).
- **[MinimaLT]** Petullo, Zhang, Solworth, Bernstein, Lange - *MinimaLT:
  Minimal-latency Networking Through Better Security*, CCS 2013
  (permanent ID 4613afc97aa0c7b8c45bc08f46c2e120). Read in full.
- **[Lazy]** Petullo, Solworth - *The Lazy Kernel Hacker and Application
  Programmer* (2013).

## What Ethos is

A clean-slate, security-first research OS from Jon Solworth's group at the
University of Illinois at Chicago (2007 onward). It runs as a guest on the
Xen hypervisor in a "paired-OS" arrangement: Ethos implements processes,
its own system-call surface, types and authentication, and **delegates
drivers, filesystems and the network device to a Linux dom0** [Lazy]. Its
stated goal is not performance but making it *easy to write robust
applications* - the system call layer removes whole bug classes instead of
documenting them.

Its three signature ideas:

1. **Etypes - distributed types checked at the OS boundary.** Every object a
   process sends anywhere - IPC, filesystem, network - is typed. A notation
   (ETN) compiles to a machine-readable *type graph*; the type's identity is
   a **type hash**, a UUID derived from a cryptographic hash of the type's
   description; one wire encoding (ETE); code generators for multiple
   languages. The kernel checks the data against the type **before the
   application sees it**, so a program never reads ill-formed input - the
   papers call Ethos "the first OS to subject its system calls to a
   language-agnostic system-wide type checker" [Etypes].

2. **Kernel-mediated signing - applications never hold private keys.** The
   kernel implements signing as a system call and enforces **per-application
   policy over what may be signed** ("only a banking application may request
   signatures for withdrawal requests"); keystores are TPM-protected;
   processes never change owner, so authentication never escalates privilege
   [Auth].

3. **MinimaLT - encrypted-by-default transport that is *faster* than
   unencrypted TCP at connection setup.** Host pairs share a *tunnel*
   identified only by a tunnel ID; **everything else, including headers, is
   encrypted**. Connections multiplex inside tunnels. The server's
   *ephemeral* public key is published through the directory service, so the
   client's **first packet is already encrypted** - no handshake round trip
   at all. Rekeying is a hash chain (`nextTid0` / `rekeyNow0`): hash the
   tunnel key to derive the next one, erase the old - **fast key erasure**
   at the transport. IP mobility is "rekey with a fresh tunnel ID", which
   also makes a moved connection unlinkable across addresses. Under load a
   server sends a **puzzle and then forgets the request** - the client
   brute-forces `w` bits to continue, so the cost asymmetry favors the
   defender [MinimaLT].

## Where the two designs already agree

Not adoption candidates - lineage. Each of these is independently arrived
at, which is worth knowing because agreement between independent derivations
is evidence the position is load-bearing.

| Idea | Ethos | rheo-os |
|---|---|---|
| Clean slate over POSIX-as-foundation | refuses POSIX entirely | native API is the queue/capability surface; POSIX is a *personality*, not the foundation (docs/ARCHITECTURE.md) |
| No privilege escalation path | processes never change owner; authentication never raises privilege [Auth] | no setuid anywhere; a cell's capability bundle is fixed at spawn and can only narrow (docs/SECURITY.md) |
| Fast key erasure | tunnel rekeying hashes the old key into the new and erases it [MinimaLT 5.5] | the DRBG implements the same construction (Bernstein 2017), both rules, proven with erase-on-read asserted (kernel/src/rng/mod.rs) |
| Identity from image measurement | user/service identity rooted in keys, host attestation | `PrincipalId` derived from the cell's image content hash - designed, PLANNED (docs/IDENTITY.md 5) |
| Semantic syscalls over byte-shoveling | say/signing/typed-IO calls | grants/queues/leases/reservations - typed verbs over eight objects |

## Judged items

Each judged by docs/GREENFIELD.md's three tests - does it beat what this
tree already has; does it fit, or does it fight; can it be proven here - and
each take carries a gate, because a research idea adopted without a gate is
a decoration.

### 1. The type hash as wire-level identity (Etypes) - take, gated

**The idea.** A protocol's identity is a cryptographic hash of its type
description, carried on the wire and checked at the boundary, so two ends
that disagree about the protocol find out *at connection time* rather than
mid-conversation.

**Does it beat what we have?** Yes, because today nothing checks anything on
a cross-cell channel. `ChannelInfo` is four words - VA, capability id, role,
slot count (abi/src/lib.rs) - with no protocol identity; a channel message's
`opcode` is "a tag the peer interprets, not a verb the kernel serves"; and
`net::service` answers an unknown op with the `REPLY_NONE` sentinel - a
silent wrong answer, not a refusal. A type hash bound when the channel is
minted (by the launcher - the same authority that mints the queue pair and
the W^X exception) and checked at `SYS_CONNECT` turns protocol mismatch into
a refusal at setup.

**Does it fit?** Yes: one word on an existing struct, no new kernel object,
and it is the *same* `ChannelInfo` extension docs/IDENTITY.md already plans
for peer credentials - two fields, one precedent.

**Gate.** `netservice` with a deliberately mismatched hash observed refused;
the whole existing suite unchanged. Recorded in docs/ARCHITECTURE-DEBT.md
7.5 as **G**. **Not built in this pass** - recorded, gated, deliberate.

**What to refuse from it:** per-message kernel type checking - see the
refusals table.

### 2. ETN / type graphs / multi-language codegen (Etypes) - already owned

The full Etypes pipeline - notation, type graph, wire format, generated
encoders - is this tree's `idl/` (BUILD-ORDER step 6), which is a stub, and
the debt row that owns it is "Contract-checked channels (Singularity)"
(docs/ARCHITECTURE-DEBT.md 7.5). What Ethos adds to that row is one
observation Singularity does not: Singularity's contracts are compile-time
and single-language by construction (Sing#), while a **runtime type-hash
check at the boundary is language-agnostic** - it protects a Rust cell from
a C cell. The IDL should generate both: the compile-time contract for the
languages it emits, and the type hash for the boundary. Recorded on that
debt row; nothing new to build beyond what the row already gates.

### 3. Kernel-mediated, policy-restricted signing (say) - design notes taken

**The idea.** Applications never hold private keys; the kernel signs on
their behalf, under policy naming *which application may sign which kind of
statement* [Auth].

**Does it beat what we have?** There is no signing surface here at all yet,
so the comparison is against docs/IDENTITY.md's plan. Three Ethos specifics
are worth folding into that plan (and now are - see docs/IDENTITY.md 11):
(a) the apps-never-hold-keys shape is already proven here in miniature - the
per-cell DRBG is a library over cell-owned state derived from a root the
cell never sees; (b) "which kind of statement" is exactly a type hash, so
the signing policy and item 1 *compose* - a signing capability names the
type hashes it may sign; (c) TPM-protected keystores stopped being
hypothetical here when `hw/tpm.rs` landed.

**Does it fit?** As design notes, yes. As code, not yet, by IDENTITY.md's
own prerequisite analysis: all 18 `svc` verbs are reachable with no
capability check, and an identity gate above an ambient syscall surface is
decoration. The prerequisites (debt tasks #127, #129) come first.

**Gate.** Owned by IDENTITY.md's phased gates (ID0-ID6). No new row.

### 4. Cell identity from image measurement - convergent, gated

Ethos roots identity in keys and measurement; docs/IDENTITY.md 5 - written
before this comparison - derives `PrincipalId` from the cell's image content
hash plus the parent's principal. Independent convergence, noted above. The
missing piece here is mechanical and now has a debt row (**G**): the kernel
has no collision-resistant hash (only an inline FNV receipt and the
KAT-tested ChaCha20), so image measurement needs a zero-dependency kernel
SHA-256 plus a measure-at-load step. The gate is the strongest kind
available: the kernel-reported hash must equal an **xtask-computed sha256 of
the same ELF** - an oracle the kernel cannot fake, the same shape as the
`sysx` entropy assertion. **Not built in this pass.**

### 5. MinimaLT's transport lessons - taken into the N7 roadmap

The N7 entry (QUIC + WireGuard/IPsec) carried BBR and FEC field learnings
but named nothing about connection mobility, header protection, or
address-validation cost asymmetry - three problems MinimaLT solved in 2013,
two years before QUIC's first draft. The N7 entry in docs/NETSTACK.md 5's
roadmap now names them as requirements (with this directory as the citation):

- **directory-published ephemeral keys**: encrypted 0-RTT without TLS
  early-data's replay semantics - the key arrives with the name lookup;
- **full header encryption**: only a connection/tunnel id visible on the
  wire (QUIC adopted a weaker form as header protection);
- **mobility = rekey under a fresh id**, unlinkable across IP changes
  (QUIC's connection migration, plus the unlinkability QUIC gets only with
  care);
- **server-stateless puzzles**: the server spends a hash and *forgets*; the
  client spends a brute-force - the DoS asymmetry QUIC approximates with
  Retry tokens;
- **hash-chain rekeying is fast key erasure at the transport** - the
  primitive is already in this tree, proven, in the DRBG.

Also worth naming: MinimaLT's tunnel/connection split - one host-pair link
carrying many user connections - is the same picture as N4a's service cell
(one channel per client over one link), independently derived.

### 6. Encrypted-by-default networking - take the default, refuse the rule

Ethos encrypts all network traffic and gives applications no plaintext
socket. The *default* is right and N7 adopts it: the native stack's remote
transport defaults to encrypted, and plaintext is a compat surface. The
*hard rule* is refused, because this tree's measured workload is unmodified
Linux binaries (`linuxnet`'s glibc DNS, `linuxclaude`), and a personality
that cannot open a plaintext TCP socket does not run them. Ethos gets purity
by refusing compatibility; this tree's whole bet is that both surfaces can
be carried honestly.

## Refusals, with reasons

The same discipline as docs/GREENFIELD.md 3: each of these is a good idea
somewhere and wrong here.

| Refused | Why |
|---|---|
| **Paired-OS: delegate drivers/FS/network to Linux over Xen** [Lazy] | The motivation is real and acknowledged - the paper's numbers (driver code is ~50% of Linux kernel LOC and 3-7x buggier) are the same evidence behind this tree's driver story. But the answer here is different and already chosen: drivers as unprivileged *cells* behind IOMMU domains (docs/DRIVERS.md D2), with the LKL lane as the compatibility harvest. Delegation to a Linux dom0 imports the entire Linux TCB to guard against Linux's bug rate, and makes Xen load-bearing |
| **Xen as the hardware layer** | This tree boots bare on three ISAs, emulation-first; a hypervisor dependency replaces the hardware-bring-up problem with a hypervisor-API problem and gives up the multikernel/SMP story built in docs/SMP.md |
| **Go (garbage-collected) as *the* userland language** | Ethos wrote its userspace in Go. The librheo bet is `no_std` Rust: a strand switch is ~12ns measured, and a GC pause is a latency cliff the reservation/EDF math (object 7) cannot admit honestly |
| **Wholesale POSIX refusal** | Ethos gains purity; this tree needs `linuxclaude` to print `2.1.220 (Claude Code)`. The Linux personality is the measured gate for real workloads, and it is a *personality* - kernel-resident today, a documented bridge to a cell - not the foundation. Purity is kept where it pays: the native API |
| **Per-message in-kernel type checking on the data plane** | The kernel deliberately **never drains a cross-cell channel** - a message is a pure shared-memory write, which is what makes IPC cheap here. Putting a type check in that path re-inserts the kernel into every message. The Etypes property worth having lands at the edges instead: the type hash at connect (item 1) and generated codecs in the IDL library (item 2) |

## What this comparison cannot say

No performance claims in either direction: Ethos's papers benchmark on 2013
hardware over Xen, nothing here has been run against an Ethos build, and the
rule of `comparison/linux/README.md` applies - "outperforms" must cite a run
of a harness in this directory, and there is no harness because there is no
runnable artifact to compare. If the source ever becomes reachable, the
first harness worth writing is connection establishment: MinimaLT's 0-RTT
claim against the N7 transport, same QEMU, same icount rules as the seL4
comparison.
