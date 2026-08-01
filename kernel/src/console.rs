//! The kernel's serial console, and the two things that were wrong with it.
//!
//! It began as "just enough for `println!` during bring-up", and its own comment said "No
//! locking yet - only the boot CPU runs at this stage". That stopped being true when four
//! cores began running cells, and the consequence was **observed, not predicted**: two
//! cores printing a fault report at once produced
//! `linuxT: unhandled TRAP: scaRAP: uscase us0xe 0xfffcff at sepcfc 0x08060,4c0a` - two
//! messages interleaved a byte at a time. It cost a real diagnosis, because the garbled
//! console made a single-core run *look* broken and the first write-up blamed personality
//! state; reproducing in a kernel with no secondaries showed the cells were fine and the
//! noise was the secondaries (docs/SMP.md). A diagnostic that corrupts itself under exactly
//! the conditions you need it for is worse than no diagnostic, because it is believed.
//!
//! Two separate costs, fixed by two separate things:
//!
//! - **Interleaving** is fixed by a lock around a whole `write`, so a line is atomic with
//!   respect to other cores. That is the *correctness* fix and it applies always.
//! - **Blocking** - every byte spins on the UART's transmit-ready bit, inline, on whatever
//!   path emitted it - is fixed by [`crate::telemetry`]'s per-CPU ring, which a boot opts
//!   into. That is the *performance* fix and it is opt-in, because buffering reorders
//!   output relative to a cell's own writes and every existing log in this tree would stop
//!   being comparable with its own history.

use core::fmt;

/// Serialises a whole `write` against other cores.
///
/// **A lock, and not a per-CPU buffer, for the always-on path.** Buffering changes *when*
/// output appears, which is a behaviour change for 210 existing boots; a lock changes only
/// who waits, and a console write is already the slowest thing the kernel does, so the
/// contention is irrelevant next to the UART it is protecting.
static LOCK: crate::smp::SpinLock<()> = crate::smp::SpinLock::new(());

struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Serial consoles expect "\r\n" line endings.
            if byte == b'\n' {
                crate::arch::serial_write_byte(b'\r');
            }
            crate::arch::serial_write_byte(byte);
        }
        Ok(())
    }
}

/// A `fmt::Write` sink that formats into a fixed buffer instead of onto the wire, so a
/// whole record can be handed to the ring in one piece.
///
/// Truncation is recorded rather than hidden: a message longer than the buffer keeps its
/// head, which is where the identifying text is, and is marked - so a short-looking line
/// is distinguishable from a genuinely short one.
struct Buffered {
    buf: [u8; crate::telemetry::PAYLOAD_MAX],
    len: usize,
    overflow: bool,
}

impl fmt::Write for Buffered {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.buf.len() {
                self.overflow = true;
                return Ok(());
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

pub fn write(args: fmt::Arguments) {
    // Asked first, and it answers with one load and a branch when off - the
    // `rearm_remaining` lesson: an early-out placed after the work is not an early-out.
    if buffering() {
        let mut b = Buffered {
            buf: [0; crate::telemetry::PAYLOAD_MAX],
            len: 0,
            overflow: false,
        };
        let _ = fmt::Write::write_fmt(&mut b, args);
        let len = b.len;
        // A record longer than the payload is reported as over-long so the ring marks it,
        // rather than as exactly full - which would be indistinguishable from fitting.
        let claimed = if b.overflow {
            crate::telemetry::PAYLOAD_MAX + 1
        } else {
            len
        };
        // SAFETY: one producer per CPU, and this is that CPU's own ring.
        unsafe {
            crate::telemetry::rings().push_claimed(
                crate::telemetry::Level::Info,
                crate::smp::cpu_index() as u16,
                crate::arch::timer_now_ns(),
                &b.buf[..len],
                claimed,
            );
        }
        return;
    }
    let _g = LOCK.lock();
    let _ = fmt::Write::write_fmt(&mut Console, args);
}

/// Whether this boot buffers console output into the telemetry ring.
#[inline]
fn buffering() -> bool {
    // SAFETY: a plain read of a flag written once, before any secondary runs.
    unsafe { crate::telemetry::rings().buffered() }
}

/// Drain every buffered record to the wire, oldest first across all CPUs.
///
/// Called where blocking is already acceptable and the output has to exist: a test
/// boundary, the idle path, and **the panic handler** - a buffered fault report that is
/// never flushed is this module's own failure mode arrived at from the other direction.
pub fn flush() {
    let _g = LOCK.lock();
    // SAFETY: the drain is single-consumer, and the lock is what makes it the only one.
    let rings = unsafe { crate::telemetry::rings() };
    while let Some(rec) = rings.pop_oldest() {
        for &byte in rec.bytes() {
            if byte == b'\n' {
                crate::arch::serial_write_byte(b'\r');
            }
            crate::arch::serial_write_byte(byte);
        }
        if rec.truncated {
            for &byte in b"[truncated]\r\n" {
                crate::arch::serial_write_byte(byte);
            }
        }
        // **A fold must be rendered, or coalescing is silent information loss.** The ring
        // folds an identical repeat into its predecessor rather than consuming a slot, and
        // folds a lost record into the newest one rather than only counting it globally -
        // both strictly better than dropping, and both only honest if the drain says so.
        if rec.repeats > 0 {
            note(b"  [repeated ", rec.repeats as u64, b" more times]\r\n");
        }
        if rec.lost > 0 {
            note(b"  [", rec.lost as u64, b" records lost here]\r\n");
        }
    }
}

/// Emit `prefix`, a decimal, then `suffix` - for the drain's fold notes.
///
/// Hand-rolled rather than `write!`, because this runs *inside* `flush`, which already
/// holds the console lock: routing through `write` would take it again and deadlock.
fn note(prefix: &[u8], value: u64, suffix: &[u8]) {
    for &b in prefix {
        crate::arch::serial_write_byte(b);
    }
    let mut digits = [0u8; 20];
    let mut n = value;
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &b in &digits[i..] {
        crate::arch::serial_write_byte(b);
    }
    for &b in suffix {
        crate::arch::serial_write_byte(b);
    }
}

/// Write straight to the wire, bypassing both the ring and the lock.
///
/// For the one case where neither can be trusted: a panic taken *while* this core held the
/// lock, where acquiring it again would deadlock and the message is the only thing left
/// that matters.
pub fn write_raw(args: fmt::Arguments) {
    let _ = fmt::Write::write_fmt(&mut Console, args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::write(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
