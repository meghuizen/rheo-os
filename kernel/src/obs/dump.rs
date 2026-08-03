//! Printing the event plane in the form `cargo xtask trace` parses
//! (docs/OBSERVABILITY.md 11).
//!
//! # Why the console, and why this format
//!
//! The ordinary console is the one channel that already leaves QEMU and works
//! identically on all three ISAs, so a dump needs no device and no host-side
//! cooperation. One line per event, prefixed `@E`, with a fixed field order and
//! nothing else interleaved on those lines - so the host parser is a split rather
//! than a grammar.
//!
//! # This is the edge, so this is where ticks become nanoseconds
//!
//! Records carry a raw counter reading precisely so that the emit path does not pay
//! for a conversion (see [`crate::abi::obs`]). The conversion has to happen
//! somewhere, and here is where: a 128-bit multiply and divide per event, once, off
//! the path being measured, on a machine that is already spending far more than that
//! formatting the line.
//!
//! # Per-CPU blocks, not a merged stream
//!
//! Events are printed one CPU at a time, in each ring's own order. A k-way merge on
//! the tick was considered and refused: it costs a scan of every cursor per emitted
//! line for an ordering the host tool can produce by sorting, and it would put a
//! ~2.5 KiB working set on a kernel stack at exactly the moment the machine is being
//! inspected. Printing per CPU also makes the host's loss detection correct for
//! free, because a sequence number is per-CPU monotone and consecutive lines within
//! a block are consecutive events of one stream.

use crate::abi::obs::ObsEvent;

/// Print every recorded event, and a header a reader can bound the stream with.
///
/// The header carries `written` and `overwritten` because those two together say
/// whether what follows is the whole story: a stream that overwrote something is
/// still useful, but a balance computed from it would be wrong, and a reader must be
/// able to tell.
pub fn dump() {
    let (written, unfunded) = super::counters();
    let over = super::overwritten();
    let hz = crate::arch::obs_tick_hz();
    crate::println!(
        "@E# written={written} overwritten={over} unfunded={unfunded} \
         cap={} tick_hz={hz}",
        super::ring::RING_EVENTS
    );
    let base = crate::obs::root::root().boot_tick;
    for cpu in 0..crate::smp::MAX_CPUS {
        // SAFETY: a read of another CPU's ring. Counters may tear (only upward) and
        // each record is validated by `ObsRing::get`, which is the contract.
        let r = unsafe { super::ring_of(cpu) };
        if !r.funded() {
            continue;
        }
        let end = r.written();
        let mut n = r.oldest();
        while n < end {
            if let Some(e) = r.get(n) {
                line(cpu, &e, base, hz);
            }
            n += 1;
        }
    }
    crate::println!("@E. end");
}

/// One `@E` line.
///
/// The field order is part of the format: `seq`, nanoseconds, CPU, window name,
/// kind, owner, `a`, `b`. Eight whitespace-separated fields, which is what the host
/// parser counts.
fn line(cpu: usize, e: &ObsEvent, base_tick: u64, hz: u64) {
    let name = super::Window::from_u8(e.window)
        .map(|w| w.name())
        .unwrap_or("?");
    crate::println!(
        "@E {} {} {} {} {} {} {} {}",
        e.seq,
        ns_since(base_tick, e.tick, hz),
        cpu,
        name,
        e.kind,
        e.owner,
        e.a,
        e.b
    );
}

/// Nanoseconds from the plane's origin tick to `tick`.
///
/// Relative rather than absolute, because a raw counter reading converted to
/// nanoseconds is an enormous number on a machine whose counter did not start at
/// boot (an x86-64 TSC has been running since power-on), and every question anyone
/// asks of a trace is about intervals. `saturating_sub` because a reader on a
/// secondary can legitimately hold a tick from before the origin was stamped.
fn ns_since(base_tick: u64, tick: u64, hz: u64) -> u64 {
    if hz == 0 {
        return 0;
    }
    let d = tick.saturating_sub(base_tick) as u128;
    ((d * 1_000_000_000) / hz as u128) as u64
}
