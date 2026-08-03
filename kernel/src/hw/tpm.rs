//! A **TPM 2.0 driver** - the platform security chip as a randomness device
//! (docs/TIME-IDENTITY.md 4a).
//!
//! # Why a TPM belongs in the entropy path
//!
//! A TPM is required by its own specification to contain a hardware random
//! number generator, and it is the one randomness source on a server that is
//! neither the CPU vendor's instruction nor a paravirtual device. It is
//! therefore worth having for exactly the reason the pool exists: independent
//! sources, so no single one has to be trusted alone. It plugs in as a
//! **registered device source** (`rng::entropy::register_device_source`), so
//! adding it changed no line of the pool.
//!
//! # What this implements
//!
//! The **FIFO / TIS** register interface from the TCG *PC Client Platform TPM
//! Profile (PTP) Specification*, and one command from the TPM 2.0 Library
//! specification, `TPM2_GetRandom`. Nothing else: this driver's whole job is to
//! ask the chip for random bytes.
//!
//! Discovery is firmware's, never a guess:
//!
//! - **x86-64**: the ACPI `TPM2` table. Its *start method* says which interface
//!   the chip speaks - 6 is FIFO/TIS, whose locality 0 the PTP fixes at
//!   `0xFED4_0000`; 7 and 8 are the Command Response Buffer interface, whose
//!   control area the table points at.
//! - **ARM64 / RISC-V**: a device-tree node whose `compatible` list contains
//!   `tcg,tpm-tis-mmio`, with its register base in `reg`.
//!
//! A machine whose firmware describes no TPM is never probed and never has a
//! window mapped for one.
//!
//! # The TIS command protocol (PTP section 5.5)
//!
//! 1. Ask for locality 0: write `requestUse` to `TPM_ACCESS`, wait for
//!    `activeLocality`.
//! 2. Write `commandReady` to `TPM_STS`, wait for it to read back set.
//! 3. Write the command into `TPM_DATA_FIFO`, at most `burstCount` bytes at a
//!    time - the chip says how much room it has and overrunning it is a
//!    protocol error.
//! 4. Write `tpmGo`.
//! 5. Wait for `stsValid | dataAvail`, then read the response out of the same
//!    FIFO, again respecting `burstCount`.
//! 6. Write `commandReady` to abort any leftover state, and release the
//!    locality.
//!
//! Every wait is a **deadline**, never an iteration count
//! (docs/ENGINEERING.md): a chip that stops answering must make the boot slower,
//! not hang it.
//!
//! # CRB is recognised and not driven
//!
//! The Command Response Buffer interface is a different register file with a
//! different handshake. Firmware that reports it is recorded as
//! [`super::TpmInterface::Crb`] and the boot says a TPM is present and undriven,
//! which is a truer answer than reporting no TPM at all.
//!
//! # What is proven
//!
//! QEMU models the chip but not its behaviour: `tpm-tis` / `tpm-crb` on x86-64
//! and `tpm-tis-device` on arm/riscv all need a **backend**. `xtask` starts one
//! (`swtpm`, a software TPM speaking the same protocol over a socket) per ISA for
//! the `rng` kernel, so the command path really executes: `TPM2_Startup` (the
//! chip arrives unstarted, so the `TPM_RC_INITIALIZE` retry below runs every
//! time), then `TPM2_GetRandom` giving 32 bytes and then 32 different ones, on
//! **all three ISAs**, vendor/device `0x00011014`.
//!
//! Where `swtpm` is not installed no TPM is attached, firmware describes none,
//! the probe finds none, and the boot says so. That is a true statement about
//! that machine rather than a gap in the driver.

use crate::arch;

// ---- TIS register offsets, from locality 0 (PTP table 19).

/// Access and locality arbitration.
const TPM_ACCESS: usize = 0x00;
/// Status, `tpmGo`, and the burst count.
const TPM_STS: usize = 0x18;
/// The command/response byte FIFO.
const TPM_DATA_FIFO: usize = 0x24;
/// Vendor id (low half) and device id (high half).
const TPM_DID_VID: usize = 0xF00;

// ---- TPM_ACCESS bits.
const ACCESS_ACTIVE_LOCALITY: u8 = 1 << 5;
const ACCESS_REQUEST_USE: u8 = 1 << 1;
const ACCESS_VALID: u8 = 1 << 7;

// ---- TPM_STS bits.
const STS_DATA_AVAIL: u32 = 1 << 4;
const STS_GO: u32 = 1 << 5;
const STS_COMMAND_READY: u32 = 1 << 6;
const STS_VALID: u32 = 1 << 7;

/// `burstCount` lives in bits 8..24 of `TPM_STS`.
fn burst_of(sts: u32) -> usize {
    ((sts >> 8) & 0xFFFF) as usize
}

// ---- TPM 2.0 command encoding (TPM 2.0 Library, part 2).

/// `TPM_ST_NO_SESSIONS` - a command carrying no authorisation session.
const TAG_NO_SESSIONS: u16 = 0x8001;
const CC_GET_RANDOM: u32 = 0x0000_017B;
const CC_STARTUP: u32 = 0x0000_0144;
/// `TPM_SU_CLEAR`.
const SU_CLEAR: u16 = 0x0000;
/// `TPM_RC_INITIALIZE` - the chip is powered but `TPM2_Startup` has not run.
const RC_INITIALIZE: u32 = 0x0000_0100;

/// Bytes asked of the chip per request. A TPM's RNG is not fast and its reply
/// is bounded by the digest size it implements, so this asks for two 256-bit
/// seeds' worth and accepts whatever comes back.
pub const CHUNK: usize = 64;

/// Longest wait for any single handshake step, in the timer's own nanosecond
/// domain. The PTP's own timeouts are of the order of tens to hundreds of
/// milliseconds; 2 seconds is generous and is a **bound**, so a wedged chip
/// costs a slow boot and a printed reason rather than a hang.
const STEP_TIMEOUT_NS: u64 = 2_000_000_000;

/// The mapped register window, 0 when no TPM was found.
static mut BASE: usize = 0;
/// Set once the chip has answered a command, so the boot can report a TPM that
/// is present *and working* rather than merely described by firmware.
static mut ANSWERED: bool = false;
/// Vendor and device id read from `TPM_DID_VID`, for the boot report.
static mut DID_VID: u32 = 0;

unsafe fn r8(off: usize) -> u8 {
    // SAFETY: `BASE` is a mapped device window and `off` is a PTP register
    // offset inside locality 0.
    unsafe { ((*core::ptr::addr_of!(BASE) + off) as *const u8).read_volatile() }
}
unsafe fn w8(off: usize, v: u8) {
    // SAFETY: as above.
    unsafe { ((*core::ptr::addr_of!(BASE) + off) as *mut u8).write_volatile(v) }
}
unsafe fn r32(off: usize) -> u32 {
    // SAFETY: as above.
    unsafe { ((*core::ptr::addr_of!(BASE) + off) as *const u32).read_volatile() }
}
unsafe fn w32(off: usize, v: u32) {
    // SAFETY: as above.
    unsafe { ((*core::ptr::addr_of!(BASE) + off) as *mut u32).write_volatile(v) }
}

/// Wait until `f` reads true, or the deadline passes. Returns whether it did.
fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    let deadline = arch::timer_now_ns().saturating_add(STEP_TIMEOUT_NS);
    loop {
        if f() {
            return true;
        }
        if arch::timer_now_ns() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Take locality 0. Returns false if the chip never grants it.
fn claim_locality() -> bool {
    // SAFETY: register access on the mapped window; see `r8`/`w8`.
    unsafe {
        if r8(TPM_ACCESS) & ACCESS_ACTIVE_LOCALITY != 0 {
            return true;
        }
        w8(TPM_ACCESS, ACCESS_REQUEST_USE);
        wait_until(|| {
            let a = r8(TPM_ACCESS);
            a & ACCESS_VALID != 0 && a & ACCESS_ACTIVE_LOCALITY != 0
        })
    }
}

/// Hand locality 0 back, so firmware or another agent can use the chip.
fn release_locality() {
    // SAFETY: as above.
    unsafe { w8(TPM_ACCESS, ACCESS_ACTIVE_LOCALITY) }
}

/// Send `cmd` and read the reply into `out`. Returns the reply length.
///
/// The whole TIS handshake; see the module docs for the numbered steps.
fn transact(cmd: &[u8], out: &mut [u8]) -> Option<usize> {
    // SAFETY: every access is to the mapped register window, and the driver is
    // called from thread context with one command outstanding at a time.
    unsafe {
        // Step 2: ask for a fresh command buffer.
        w32(TPM_STS, STS_COMMAND_READY);
        if !wait_until(|| r32(TPM_STS) & STS_COMMAND_READY != 0) {
            crate::println!("tpm: chip never reported commandReady");
            return None;
        }

        // Step 3: write the command, never more than burstCount at a time.
        let mut sent = 0;
        while sent < cmd.len() {
            let mut burst = 0;
            if !wait_until(|| {
                burst = burst_of(r32(TPM_STS));
                burst > 0
            }) {
                crate::println!(
                    "tpm: burstCount stayed 0 with {} bytes left",
                    cmd.len() - sent
                );
                return None;
            }
            let n = burst.min(cmd.len() - sent);
            for &b in &cmd[sent..sent + n] {
                w8(TPM_DATA_FIFO, b);
            }
            sent += n;
        }

        // Step 4: execute.
        w32(TPM_STS, STS_GO);

        // Step 5: wait for a reply and read it out.
        if !wait_until(|| {
            let s = r32(TPM_STS);
            s & STS_VALID != 0 && s & STS_DATA_AVAIL != 0
        }) {
            crate::println!("tpm: no response before the deadline");
            return None;
        }
        let mut got = 0;
        while got < out.len() {
            let s = r32(TPM_STS);
            if s & STS_DATA_AVAIL == 0 {
                break;
            }
            let burst = burst_of(s);
            if burst == 0 {
                continue;
            }
            let n = burst.min(out.len() - got);
            for slot in out[got..got + n].iter_mut() {
                *slot = r8(TPM_DATA_FIFO);
            }
            got += n;
        }

        // Step 6: leave the chip in a clean state whatever happened.
        w32(TPM_STS, STS_COMMAND_READY);
        Some(got)
    }
}

/// Build `TPM2_GetRandom(bytes)` into `buf`, returning its length.
fn build_get_random(buf: &mut [u8; 12], bytes: u16) -> usize {
    buf[0..2].copy_from_slice(&TAG_NO_SESSIONS.to_be_bytes());
    buf[2..6].copy_from_slice(&12u32.to_be_bytes()); // commandSize
    buf[6..10].copy_from_slice(&CC_GET_RANDOM.to_be_bytes());
    buf[10..12].copy_from_slice(&bytes.to_be_bytes());
    12
}

/// Build `TPM2_Startup(TPM_SU_CLEAR)` into `buf`, returning its length.
fn build_startup(buf: &mut [u8; 12]) -> usize {
    buf[0..2].copy_from_slice(&TAG_NO_SESSIONS.to_be_bytes());
    buf[2..6].copy_from_slice(&12u32.to_be_bytes());
    buf[6..10].copy_from_slice(&CC_STARTUP.to_be_bytes());
    buf[10..12].copy_from_slice(&SU_CLEAR.to_be_bytes());
    12
}

/// The response header every TPM 2.0 command returns: tag, size, code.
fn response_code(r: &[u8]) -> Option<u32> {
    if r.len() < 10 {
        return None;
    }
    Some(u32::from_be_bytes([r[6], r[7], r[8], r[9]]))
}

/// Ask the chip for up to `dst.len()` random bytes. Returns how many it gave.
///
/// A TPM that has been powered but not started answers `TPM_RC_INITIALIZE`; on
/// real hardware firmware has already run `TPM2_Startup`, but a chip handed
/// straight to us has not, so that one case is handled and retried once. Any
/// other non-zero response code is reported and returns nothing - a TPM that
/// refuses must not look like a TPM that returned zeros.
pub fn get_random(dst: &mut [u8]) -> usize {
    if !present() || dst.is_empty() {
        return 0;
    }
    if !claim_locality() {
        crate::println!("tpm: could not take locality 0");
        return 0;
    }
    let want = dst.len().min(CHUNK) as u16;
    let mut cmd = [0u8; 12];
    let n = build_get_random(&mut cmd, want);
    // Header (10) + the TPM2B_DIGEST size field (2) + the bytes.
    let mut reply = [0u8; 12 + CHUNK];

    let mut got = transact(&cmd[..n], &mut reply);
    if let Some(len) = got
        && response_code(&reply[..len]) == Some(RC_INITIALIZE)
    {
        // Not started. Start it, once, then ask again.
        let mut su = [0u8; 12];
        let sn = build_startup(&mut su);
        let mut sr = [0u8; 16];
        transact(&su[..sn], &mut sr);
        got = transact(&cmd[..n], &mut reply);
    }
    release_locality();

    let Some(len) = got else { return 0 };
    match response_code(&reply[..len]) {
        Some(0) => {}
        Some(rc) => {
            crate::println!("tpm: GetRandom returned rc {rc:#x}");
            return 0;
        }
        None => return 0,
    }
    // TPM2B_DIGEST: a big-endian size, then that many bytes.
    if len < 12 {
        return 0;
    }
    let size = u16::from_be_bytes([reply[10], reply[11]]) as usize;
    let avail = size.min(len - 12).min(dst.len());
    dst[..avail].copy_from_slice(&reply[12..12 + avail]);
    // SAFETY: boot/thread context, this driver's own flag.
    unsafe {
        ANSWERED = true;
    }
    avail
}

/// Whether a TPM register window is mapped and driveable.
pub fn present() -> bool {
    // SAFETY: written once at boot, read-only after.
    unsafe { *core::ptr::addr_of!(BASE) != 0 }
}

/// Whether the chip has actually answered a command. Distinct from
/// [`present`]: firmware describing a TPM and a TPM replying are two facts, and
/// conflating them is how a boot claims a source it does not have.
pub fn answered() -> bool {
    // SAFETY: as above.
    unsafe { *core::ptr::addr_of!(ANSWERED) }
}

/// `TPM_DID_VID` as read at probe: vendor id in the low half, device id in the
/// high half. 0 when no TPM was found.
pub fn did_vid() -> u32 {
    // SAFETY: as above.
    unsafe { *core::ptr::addr_of!(DID_VID) }
}

/// Probe for the TPM firmware described, map its registers, and register it
/// with the entropy pool. Called from the boot sequencer after `hw::detect`.
///
/// Returns a short description, or `None` when firmware described no TPM.
pub fn init() -> Option<&'static str> {
    let inv = crate::hw::inventory();
    // Firmware first. Where no firmware table exists at all - an ARM64 bare-ELF
    // boot gets no device tree - fall back to this ISA's built-in candidate,
    // which the vendor-id read-back below turns into an observation.
    let (base, iface) = if inv.tpm_iface == super::TpmInterface::None {
        if arch::TPM_TIS_CANDIDATE == 0 {
            return None;
        }
        (arch::TPM_TIS_CANDIDATE, super::TpmInterface::Tis)
    } else {
        (inv.tpm_base, inv.tpm_iface)
    };
    match iface {
        super::TpmInterface::None => return None,
        super::TpmInterface::Crb => {
            // Recognised, not driven. Said out loud, because "no TPM" and "a TPM
            // this driver does not speak to" are different facts.
            crate::println!(
                "tpm: CRB interface at {base:#x} - recognised, not driven (this driver speaks FIFO/TIS)"
            );
            return Some("crb (undriven)");
        }
        super::TpmInterface::Tis => {}
    }
    // One page covers locality 0's whole register file (0x000..0xFFF).
    let va = arch::mmio_map_window(base as usize, 0x1000);
    if va == 0 {
        crate::println!("tpm: could not map the register window at {base:#x}");
        return None;
    }
    // SAFETY: boot, single-threaded, before any cell runs.
    unsafe {
        BASE = va;
        // A window with no chip behind it reads all-ones (unassigned MMIO) or
        // all-zeros. Either way the vendor id is not a real one, so this is the
        // check that keeps a described-but-absent TPM from being claimed.
        // A **guarded** read: on ARM64 the base came from a built-in machine
        // profile, and an address nothing decodes raises an external abort that
        // would kill the boot. Catching it turns a wrong constant into "no TPM".
        DID_VID = arch::mmio_probe_u32(va + TPM_DID_VID).unwrap_or(0);
        let vid = (*core::ptr::addr_of!(DID_VID)) & 0xFFFF;
        if vid == 0x0000 || vid == 0xFFFF {
            // Silent on the *candidate* path: an ARM64 machine with no TPM
            // attached is the common case and printing it in every one of ~210
            // logs would be noise. Loud where firmware said there was one,
            // because that is a disagreement worth seeing.
            if inv.tpm_iface != super::TpmInterface::None {
                crate::println!(
                    "tpm: TIS window at {base:#x} reads vendor id {vid:#x} - no chip behind it"
                );
            }
            BASE = 0;
            return None;
        }
    }
    crate::rng::entropy::register_device_source("tpm", refill);
    // Draw once now, through the same path a later top-up takes.
    refill();
    Some("tis")
}

/// Pull a chunk from the chip into the entropy pool. Returns bytes fed.
/// This is the [`crate::rng::entropy::DeviceSource`] the driver registers.
pub fn refill() -> usize {
    let mut buf = [0u8; CHUNK];
    let got = get_random(&mut buf);
    if got > 0 {
        crate::rng::feed_device(&buf[..got]);
    }
    for b in buf.iter_mut() {
        // SAFETY: a plain write, volatile so it is not optimised away.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    got
}
