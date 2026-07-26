//! librheo Phase J proof: **symmetric async IPC** (docs/LIBRHEO.md). ONE binary,
//! run as TWO cells that share a typed cross-cell queue pair and ping-pong N
//! typed messages over the async [`AsyncSender`]/[`AsyncReceiver`] - neither cell
//! busy-switching.
//!
//! - **producer** (client, role 0): for each round, `send`s a `Message` then
//!   `recv`s the consumer's ack. Its `recv` (of the ack) parks on the reactor.
//! - **consumer** (server, role 1): for each round, `recv`s a `Message`, checks
//!   it against the expected sequence, and `send`s an ack. Its `recv` parks on
//!   the reactor and is woken by the reactor's channel service - never a spin.
//!
//! Because each round's `recv` finds an empty ring (the peer is parked awaiting
//! this side), every receive genuinely parks and is resumed by the reactor's
//! cross-cell hand-off. The consumer asserts (a) it received the exact expected
//! sequence and (b) `rt::chan_wakeups() == N` - i.e. all N messages arrived via a
//! reactor park+wake, not a busy switch. On success it exits `0x42`. The test
//! kernel wires cell 0 = consumer (role 1), cell 1 = producer (role 0), and
//! starts the consumer; its exit is the asserted outcome.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::ipc::{Channel, Message};
use librheo::{println, rt, sys};

/// Rounds of ping-pong (typed messages exchanged).
const N: u32 = 8;
/// The consumer's success sentinel.
const OK: u64 = 0x42;
/// Exit code a cell uses when the FP/SIMD register file did **not** survive a
/// cross-cell yield (see [`fp_phase`]). Distinct from the generic failure `1`.
const FP_FAIL: u64 = 0x1F;

/// The deterministic payload the producer sends for message `i` (non-trivial so
/// a dropped/reordered message would fail the exact-sequence check).
fn payload(i: u32) -> u32 {
    i.wrapping_mul(0x9E37_79B1) ^ 0x5A5A_1234
}

/// FP/SIMD register-file preservation across the cross-cell yield.
///
/// librheo cells are built **hard-float** (docs/TILES.md 4), so a cell can hold
/// live values in its vector registers when it hands the CPU to a peer. The
/// kernel is soft-float, which means at a switch the physical register file still
/// holds the *outgoing* cell's values - so the switch must save them and load the
/// incoming cell's, or a yielding cell silently reads back its peer's numbers.
///
/// [`round_trip`] is the proof: **one** `asm!` block loads the vector register
/// file from a caller-supplied pattern, executes `SYS_YIELD` (which runs the peer
/// cell, whose own pattern differs in every byte), and only then stores the
/// register file back out. Because the load, the syscall and the store are inside
/// a single block, the compiler cannot spill or reload around the switch - what
/// comes back out came out of the physical registers. If the switch does not swap
/// FP state, the bytes read back are the **peer's** pattern, which is why
/// [`Verdict`] distinguishes that case from ordinary corruption.
///
/// The register names are necessarily per-ISA (there is no portable spelling for
/// `xmm0`); this is a proof program, and it follows the precedent of
/// `librheo::tile::simd`'s per-ISA dispatch rather than the kernel's arch layer.
mod fpcheck {
    use librheo::sys::SYS_YIELD;

    /// Bytes of vector register file patterned and checked: the low 128 bits of
    /// 16 SIMD registers on x86-64/ARM64, and 16 64-bit `f` registers on RISC-V.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub const BYTES: usize = 16 * 16;
    #[cfg(target_arch = "riscv64")]
    pub const BYTES: usize = 16 * 8;

    /// What a round of [`round_trip`] observed.
    #[derive(PartialEq, Eq)]
    pub enum Verdict {
        /// The register file came back bit-identical to this cell's pattern.
        Preserved,
        /// It came back holding the **peer's** pattern - the switch did not swap
        /// FP state (the defect this phase exists to catch).
        PeerPattern,
        /// Neither: corrupted some other way.
        Corrupted,
    }

    /// 16-byte-aligned pattern/result buffer (`movaps`-friendly, and the natural
    /// alignment for `ldp q`).
    #[repr(align(16))]
    pub struct Buf(pub [u8; BYTES]);

    impl Buf {
        pub const fn zeroed() -> Self {
            Buf([0; BYTES])
        }
    }

    /// A per-(role, round) pattern with no byte in common with the other role's
    /// pattern for the same round, so a mixed-up register file is unambiguous.
    pub fn pattern(role: u8, round: u32) -> Buf {
        let mut b = Buf::zeroed();
        let base = role.wrapping_mul(0x5B).wrapping_add(round as u8);
        for (i, slot) in b.0.iter_mut().enumerate() {
            // Odd multiplier over a byte is a bijection, so distinct `base`
            // values give patterns that differ in *every* byte.
            *slot = base
                .wrapping_add(i as u8)
                .wrapping_mul(0x4D)
                .wrapping_add(0x1F);
        }
        b
    }

    /// Load the vector register file from `pattern`, `SYS_YIELD` to the peer
    /// cell, then store the register file into `out`. See the module docs for why
    /// this must be a single `asm!` block.
    ///
    /// # Safety
    /// `pattern` and `out` must each point to [`BYTES`] readable/writable bytes.
    /// The block clobbers the low 128 bits of the 16 vector registers it names.
    #[cfg(target_arch = "x86_64")]
    pub unsafe fn round_trip(pattern: *const u8, out: *mut u8) {
        unsafe {
            core::arch::asm!(
                "movups xmm0,  [{p}]",      "movups xmm1,  [{p} + 16]",
                "movups xmm2,  [{p} + 32]", "movups xmm3,  [{p} + 48]",
                "movups xmm4,  [{p} + 64]", "movups xmm5,  [{p} + 80]",
                "movups xmm6,  [{p} + 96]", "movups xmm7,  [{p} + 112]",
                "movups xmm8,  [{p} + 128]","movups xmm9,  [{p} + 144]",
                "movups xmm10, [{p} + 160]","movups xmm11, [{p} + 176]",
                "movups xmm12, [{p} + 192]","movups xmm13, [{p} + 208]",
                "movups xmm14, [{p} + 224]","movups xmm15, [{p} + 240]",
                // The cross-cell yield. Between here and the stores below, the
                // peer cell runs and loads its own pattern into these registers.
                "mov rax, {nr}",
                "xor edi, edi",
                "syscall",
                "movups [{o}],       xmm0",  "movups [{o} + 16],  xmm1",
                "movups [{o} + 32],  xmm2",  "movups [{o} + 48],  xmm3",
                "movups [{o} + 64],  xmm4",  "movups [{o} + 80],  xmm5",
                "movups [{o} + 96],  xmm6",  "movups [{o} + 112], xmm7",
                "movups [{o} + 128], xmm8",  "movups [{o} + 144], xmm9",
                "movups [{o} + 160], xmm10", "movups [{o} + 176], xmm11",
                "movups [{o} + 192], xmm12", "movups [{o} + 208], xmm13",
                "movups [{o} + 224], xmm14", "movups [{o} + 240], xmm15",
                p = in(reg) pattern,
                o = in(reg) out,
                nr = const SYS_YIELD,
                out("rax") _, out("rcx") _, out("rdi") _, out("r11") _,
                out("xmm0") _,  out("xmm1") _,  out("xmm2") _,  out("xmm3") _,
                out("xmm4") _,  out("xmm5") _,  out("xmm6") _,  out("xmm7") _,
                out("xmm8") _,  out("xmm9") _,  out("xmm10") _, out("xmm11") _,
                out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
                options(nostack),
            );
        }
    }

    /// ARM64: `q0`-`q15` (the low half of `v0`-`v15`) around `svc #0`.
    ///
    /// # Safety
    /// As the x86-64 arm.
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn round_trip(pattern: *const u8, out: *mut u8) {
        unsafe {
            core::arch::asm!(
                "ldp q0,  q1,  [{p}]",       "ldp q2,  q3,  [{p}, #32]",
                "ldp q4,  q5,  [{p}, #64]",  "ldp q6,  q7,  [{p}, #96]",
                "ldp q8,  q9,  [{p}, #128]", "ldp q10, q11, [{p}, #160]",
                "ldp q12, q13, [{p}, #192]", "ldp q14, q15, [{p}, #224]",
                "mov x8, {nr}",
                "mov x0, xzr",
                "svc #0",
                "stp q0,  q1,  [{o}]",       "stp q2,  q3,  [{o}, #32]",
                "stp q4,  q5,  [{o}, #64]",  "stp q6,  q7,  [{o}, #96]",
                "stp q8,  q9,  [{o}, #128]", "stp q10, q11, [{o}, #160]",
                "stp q12, q13, [{o}, #192]", "stp q14, q15, [{o}, #224]",
                p = in(reg) pattern,
                o = in(reg) out,
                nr = const SYS_YIELD,
                out("x0") _, out("x8") _,
                out("v0") _,  out("v1") _,  out("v2") _,  out("v3") _,
                out("v4") _,  out("v5") _,  out("v6") _,  out("v7") _,
                out("v8") _,  out("v9") _,  out("v10") _, out("v11") _,
                out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                options(nostack),
            );
        }
    }

    /// RISC-V: `f0`-`f15` (double precision) around `ecall`.
    ///
    /// # Safety
    /// As the x86-64 arm.
    #[cfg(target_arch = "riscv64")]
    pub unsafe fn round_trip(pattern: *const u8, out: *mut u8) {
        unsafe {
            core::arch::asm!(
                "fld f0,  0({p})",  "fld f1,  8({p})",  "fld f2,  16({p})",
                "fld f3,  24({p})", "fld f4,  32({p})", "fld f5,  40({p})",
                "fld f6,  48({p})", "fld f7,  56({p})", "fld f8,  64({p})",
                "fld f9,  72({p})", "fld f10, 80({p})", "fld f11, 88({p})",
                "fld f12, 96({p})", "fld f13, 104({p})","fld f14, 112({p})",
                "fld f15, 120({p})",
                "li a7, {nr}",
                "li a0, 0",
                "ecall",
                "fsd f0,  0({o})",  "fsd f1,  8({o})",  "fsd f2,  16({o})",
                "fsd f3,  24({o})", "fsd f4,  32({o})", "fsd f5,  40({o})",
                "fsd f6,  48({o})", "fsd f7,  56({o})", "fsd f8,  64({o})",
                "fsd f9,  72({o})", "fsd f10, 80({o})", "fsd f11, 88({o})",
                "fsd f12, 96({o})", "fsd f13, 104({o})","fsd f14, 112({o})",
                "fsd f15, 120({o})",
                p = in(reg) pattern,
                o = in(reg) out,
                nr = const SYS_YIELD,
                out("a0") _, out("a7") _,
                out("f0") _,  out("f1") _,  out("f2") _,  out("f3") _,
                out("f4") _,  out("f5") _,  out("f6") _,  out("f7") _,
                out("f8") _,  out("f9") _,  out("f10") _, out("f11") _,
                out("f12") _, out("f13") _, out("f14") _, out("f15") _,
                options(nostack),
            );
        }
    }

    /// Run one round for `role` and classify what came back.
    pub fn check(role: u8, peer_role: u8, round: u32) -> Verdict {
        let mine = pattern(role, round);
        let mut out = Buf::zeroed();
        // SAFETY: both buffers are exactly `BYTES` long and live for the call.
        unsafe { round_trip(mine.0.as_ptr(), out.0.as_mut_ptr()) };
        if out.0 == mine.0 {
            Verdict::Preserved
        } else if (0..=u8::MAX as u32).any(|r| out.0 == pattern(peer_role, r).0) {
            Verdict::PeerPattern
        } else {
            Verdict::Corrupted
        }
    }
}

/// Rounds of the FP/SIMD preservation phase per cell (so `2 * FP_ROUNDS`
/// cross-cell yields with live vector state).
const FP_ROUNDS: u32 = 4;

/// Run the FP phase for this cell; `true` if every round came back
/// bit-identical. Prints one line from a fixed set either way.
fn fp_phase(is_producer: bool) -> bool {
    let (role, peer_role, name) = if is_producer {
        (0u8, 1u8, "producer")
    } else {
        (1u8, 0u8, "consumer")
    };
    let mut preserved = 0u32;
    let mut peer_seen = 0u32;
    let mut corrupted = 0u32;
    for round in 0..FP_ROUNDS {
        match fpcheck::check(role, peer_role, round) {
            fpcheck::Verdict::Preserved => preserved += 1,
            fpcheck::Verdict::PeerPattern => peer_seen += 1,
            fpcheck::Verdict::Corrupted => corrupted += 1,
        }
    }
    if preserved == FP_ROUNDS {
        println!(
            "librheo-ipc: {name} FP/SIMD preserved across {FP_ROUNDS} cross-cell yields \
             ({} bytes of vector register file, bit-identical)",
            fpcheck::BYTES
        );
        return true;
    }
    println!(
        "librheo-ipc: {name} FP/SIMD FAIL - {preserved}/{FP_ROUNDS} preserved, \
         {peer_seen} read back the PEER's pattern (switch did not swap FP state), \
         {corrupted} otherwise corrupted"
    );
    false
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let ch = Channel::open().expect("librheo-ipc: no cross-cell channel wired");
    let is_producer = ch.is_client();

    // FP/SIMD register-file preservation across the cross-cell yield, before any
    // channel traffic so the message protocol below is untouched (its
    // `chan_wakeups() == N` proof still counts exactly N parks). A producer that
    // fails ends the run with this code, which the test kernel's exit assertion
    // rejects; the consumer folds its own verdict into its success sentinel.
    let fp_ok = fp_phase(is_producer);
    if is_producer && !fp_ok {
        sys::exit(FP_FAIL);
    }

    let (tx, rx) = ch.split();

    if is_producer {
        rt::block_on(async move {
            for i in 0..N {
                tx.send(Message {
                    tag: i as u64,
                    val: payload(i),
                })
                .await;
                // Await the consumer's ack (this parks on the reactor).
                let _ack = rx.recv().await;
            }
        });
        // Unreached: the consumer exits first (it is the started/top cell).
        sys::exit(0)
    } else {
        rt::block_on(async move {
            let mut ok = true;
            for i in 0..N {
                let m = rx.recv().await;
                if m.tag != i as u64 || m.val != payload(i) {
                    ok = false;
                }
                // Ack the message so the producer advances to the next round.
                tx.send(Message {
                    tag: i as u64,
                    val: 1,
                })
                .await;
            }
            let wakeups = rt::chan_wakeups();
            if ok && fp_ok && wakeups == N as u64 {
                println!(
                    "librheo-ipc: consumer received {N} typed msgs, all reactor-parked \
                     ({wakeups} wakeups) - symmetric async IPC OK"
                );
                sys::exit(OK);
            }
            println!("librheo-ipc: FAIL (ok={ok} fp_ok={fp_ok} wakeups={wakeups}, expected {N})");
            sys::exit(1);
        });
        sys::exit(1)
    }
}
