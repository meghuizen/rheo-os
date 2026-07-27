# Boot

**Status:** Draft v0.1. Expands ARCHITECTURE.md sections 3 (clock/entropy),
4.8 (cluster identity).

Boot has one job beyond starting the kernel: producing a **provable host
identity**. Everything above (capability tokens, leases, secrets, cluster
membership) roots in what boot measures. A host that cannot prove what it is
running does not join a trust domain.

## 1. The chain

```
Hardware root of trust (TPM 2.0 or DICE)
  -> Platform firmware (UEFI, measured)
    -> Lattice bootloader (signed, measured)
      -> Kernel image + system manifest (signed, content-addressed, measured)
        -> Entropy source configuration (measured - see 4)
          -> Kernel init
            -> System pool carve-out
              -> PID 1: the host reconciler
                -> Attestation report -> host identity issued
                  -> Cluster join
```

- **UEFI + Secure Boot** on x86-64/ARM64; the RISC-V flow uses an equivalent
  measurement chain. There is no legacy BIOS path.
- Every stage extends PCRs (TPM) or the DICE certificate chain. The
  attestation report binds: firmware versions, kernel image hash, system
  manifest hash, entropy configuration, and platform features (IOMMU present
  and enabled, MTE available, and so on).
- **Hard refusals:** no TPM/DICE, IOMMU absent or disabled, or no provable
  entropy source means the host halts with a diagnostic. These are not
  degraded modes (TARGET-ARCHITECTURES.md section 3); booting anyway would
  make the security model a lie.

## 2. The system image

- The system is a **content-addressed manifest**: a signed list of sealed
  objects (kernel, system service cells, driver cells, personalities) by
  hash. There is no initramfs assembly step and no package-database drift -
  the manifest hash *is* the system version.
- **A/B slots with automatic rollback:** a new manifest boots into slot B; if
  the reconciler fails its health gate within a deadline, firmware falls back
  to slot A. Updates are therefore atomic and reversible.
- Driver cells and personalities load lazily from the manifest as engines and
  workloads demand them; each load is a hash verification, not a trust
  decision.

## 3. Kernel init sequence

1. Arch bring-up: page tables, per-core state, one-shot timers, IOMMU on.
2. **Root DRBG seeding** from measured entropy sources; health tests run
   before any key material exists.
3. Clock objects created; sync daemon starts later as a cell - until then
   wall time reports a wide honest error bound e.
4. **System pool carve-out:** cores, memory, and queue capacity reserved for
   system services. This floor is set before any tenant admission math runs
   and can never be granted away (ARCHITECTURE.md 4.2).
5. Engine enumeration: devices discovered, IOMMU domains created, firmware
   measured. Engines are *not yet* schedulable.
6. PID 1 starts: the host reconciler, holding the initial capability set -
   which set depends on the boot mode (section 3.1).

## 3.1 Boot flags and boot modes

**PLANNED** (docs/IDENTITY.md 8, phases ID1 and ID6). Nothing here is
implemented yet.

The kernel parses a boot command line once, at init, and keeps it immutable for
the life of the boot.

- **The source is per-ISA; everything above it is portable**
  (TARGET-ARCHITECTURES.md 4). One `arch::boot_cmdline()` reads x86-64's PVH
  `hvm_start_info` cmdline pointer and `/chosen/bootargs` from the flattened
  device tree on ARM64 and RISC-V. The firmware plumbing for all three already
  exists in `kernel/src/hw/`. Above it there is one portable `BootConfig` and no
  `cfg(target_arch)`.
- **Readable in user mode, never writable.** `SYS_BOOTINFO` for native cells,
  `/proc/cmdline` for the Linux personality - the file real programs read. A
  cell cannot modify it and cannot forge it.
- **Measured.** The command line's hash is an input to the initial cell's
  principal derivation, so *how the machine was booted* is part of what the host
  attests. This is what makes a privileged boot mode defensible: it cannot be
  hidden from a remote verifier.

| `rheo.boot=` | Effect |
|---|---|
| `normal` (default) | `identityd` starts first; the initial cell gets a **narrowed** bundle; a login is required for a user session |
| `single` | No `identityd`, no login, console only, networking not started. The initial cell **is** the root principal with the full bundle |
| `recovery` | `single`, plus the root filesystem read-only, for repair |

The modes differ **only in which capabilities the first cell is minted**. There
is no "am I in single-user mode?" branch in any check anywhere, so single-user
mode is not a hole in the security model - it is the same model started from a
different initial capability set.

Stated plainly: a single-user boot is a **full-authority boot**. What protects
it is control of the console and the boot path, exactly as on any other OS, plus
the one thing this design adds - it is measured, so it is visible rather than
silent.

## 4. Entropy at boot

- Sources (RDSEED/RNDR, platform TRNG, jitter floor) and their configuration
  are part of the measured state - a host attests *what seeded it*, closing
  the classic weak-key-at-first-boot failure (the Mining-your-Ps-and-Qs class
  of incident).
- Diskless/first-boot nodes without a hardware TRNG must be provisioned with
  a sealed seed bound to the TPM; otherwise attestation fails by design.
- VMs: virtio-rng feed accepted, jitter supplemented, and the guest's
  attestation report marks entropy class as "hypervisor-fed" so cluster
  policy can decide what such hosts may run.

## 5. Engine attach and benchmark phase

Before an engine becomes schedulable:

1. Firmware measured and signature-checked; result enters the host's
   attestation evidence.
2. Trust class assigned: **exclusive** (Lattice fully owns it) or
   **shared-with-firmware** (a secure world also touches it) - the latter
   gets no secrets and no multi-tenant grants. This is the structural answer
   to the QCE "races with the secure world" failure.
3. **Attach-time benchmark:** measured throughput/latency per op class
   recorded into the topology graph. Offload that loses to the CPU is never
   routed to (ARCHITECTURE.md doctrine 6).
4. Preemption contract and partitioning capability declared; only then may
   the placement engine hand out engine grants.

## 6. Cluster join

1. The reconciler presents the attestation report to the trust domain's
   registration service.
2. Policy evaluates: firmware allow-list, entropy class, platform features.
3. Host identity issued (SPIFFE-shaped, see SECURITY-IDENTITY.md); short-
   lived credentials begin rotating.
4. The host appears in the membership consensus; desired state starts
   flowing; the reconciler converges the node.

A single-host deployment is the same flow with a local trust domain of one -
no mode switch (doctrine 9).

## 7. Failure and recovery

- Boot failures are events too: the bootloader writes a structured failure
  record to a reserved region readable by the next successful boot or by
  out-of-band management (Redfish/BMC), so "why did it not come up" is a
  query, not serial-cable archaeology.
- **Fleet recovery:** because host state is desired-state driven and cells
  are checkpoint/restorable-or-restartable, reimaging a node is manifest
  download + reboot; there is no per-node mutable configuration to lose.
