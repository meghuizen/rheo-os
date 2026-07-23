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
6. PID 1 starts: the host reconciler, holding the initial capability set.

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
