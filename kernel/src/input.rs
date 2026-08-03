//! Kernel-side console **input**: an RX byte ring plus the park-until-input
//! primitive `SYS_WAIT_INPUT` (docs/LIBRHEO.md Phase D). This is the OS's first
//! block-and-wake - a native cell with nothing to do calls `SYS_WAIT_INPUT` and
//! the kernel waits until a byte is available, then hands it back.
//!
//! Two things live here, both portable (per-ISA interrupt-controller code stays
//! in `kernel/src/arch`, per the portability rule):
//!
//! - **an RX ring** (`RxRing`): bytes received from the UART are buffered here,
//!   so a keystroke typed while a cell computes is not lost - the 16-byte UART
//!   FIFO is the only other buffer and overflows silently. The producer is the
//!   UART RX interrupt handler where one is wired, or the poll path otherwise.
//! - **`wait_input`**: drain the ring into the cell's buffer; if empty, make one
//!   byte available (idle at WFI where the UART RX interrupt is wired - a genuine
//!   0%-CPU park; poll the UART otherwise - honest, the CPU spins) and retry.
//!
//! Input can also come from a fixed **script** (headless tests), played as if
//! typed. This module deliberately does **not** build on `pty.rs` (the legacy
//! cooked line discipline): librheo owns the terminal discipline in userland
//! (docs/SHELL.md 1), and this is only the raw byte substrate under it.
//!
//! Interrupt status is per-ISA (docs/LIBRHEO.md Phase D names which ISA is
//! interrupt-driven vs poll); [`interrupt_driven`] reports it and [`did_idle`]
//! records whether the kernel actually idled at WFI (for the test's idle-park
//! assertion). In the poll build both are false and honest.

use core::ptr::{addr_of, addr_of_mut};

/// RX ring capacity (power of two so the index mask is a bit-and).
const RING_CAP: usize = 512;

/// A single-producer/single-consumer byte ring. Producer: the UART RX interrupt
/// handler (or the poll path). Consumer: `wait_input` on the syscall path. One
/// CPU, so no atomics are needed yet (SMP is task #27).
struct RxRing {
    buf: [u8; RING_CAP],
    head: usize, // producer index (monotonic, masked on access)
    tail: usize, // consumer index
}

impl RxRing {
    const fn new() -> RxRing {
        RxRing {
            buf: [0; RING_CAP],
            head: 0,
            tail: 0,
        }
    }
    /// Push one received byte; drop it if the ring is full (backpressure is a
    /// terminal-level concern, not the byte substrate's).
    fn push(&mut self, b: u8) {
        if self.head.wrapping_sub(self.tail) >= RING_CAP {
            return;
        }
        self.buf[self.head & (RING_CAP - 1)] = b;
        self.head = self.head.wrapping_add(1);
    }
    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let b = self.buf[self.tail & (RING_CAP - 1)];
        self.tail = self.tail.wrapping_add(1);
        Some(b)
    }
}

/// Where raw input bytes come from.
///
/// This is the **only** definition of a scripted keystroke source in the kernel:
/// `pty`'s line discipline used to carry its own byte-for-byte copy - same enum,
/// same `install_script`, its own `static mut SOURCE` and its own cursor - with
/// consumers split between the two (docs/ARCHITECTURE-DEBT.md 3.6). It now reads
/// the script through [`script_next_byte`].
///
/// Note what is deliberately *not* merged: each module keeps its own **live**
/// path. `input`'s serial arm feeds the interrupt-driven RX ring that
/// `SYS_WAIT_INPUT` parks on; `pty`'s polls and blocks inside the cooked line
/// read. Those differ in behaviour, not just in spelling, so unifying them is a
/// console-path change with its own proof, not a de-duplication.
enum Source {
    /// Live serial console (`cargo xtask run`).
    Serial,
    /// A fixed script played byte by byte (headless tests). The usize is the
    /// read cursor; end of script is end of input.
    Script(&'static [u8], usize),
}

static mut RING: RxRing = RxRing::new();
static mut SOURCE: Source = Source::Serial;
static mut IDLED: bool = false;

fn ring() -> &'static mut RxRing {
    // SAFETY: single CPU, synchronous traps; no concurrent access.
    unsafe { &mut *addr_of_mut!(RING) }
}

/// Reset the input state (call before installing a fresh set of cells).
pub fn reset() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(RING) = RxRing::new();
        *addr_of_mut!(SOURCE) = Source::Serial;
        *addr_of_mut!(IDLED) = false;
    }
    // The pump-tier counts live in the observability plane (S4). The byte
    // sequence (`CTR_CONSOLE_BYTES`) is deliberately *not* cleared, matching the
    // old `RX_SEQ`: it is an entropy sequence number, and monotonicity across
    // runs costs nothing.
    crate::obs::cpu_counter_clear(crate::obs::cpu::CTR_PUMP_FIFO_TAKES);
    crate::obs::cpu_counter_clear(crate::obs::cpu::CTR_PUMP_DIRECT_PUSHES);
}

/// Install a scripted input source (deterministic headless tests). The script
/// is played as if typed; end of script is end of input.
pub fn install_script(script: &'static [u8]) {
    // SAFETY: single CPU, before any cell runs.
    unsafe {
        *addr_of_mut!(SOURCE) = Source::Script(script, 0);
    }
}

/// The next byte of the installed script, or `None` on a live-serial source or at
/// end of script. The single reader of the scripted-input cursor; `pty`'s cooked
/// line discipline calls this instead of keeping a second copy of it.
pub fn script_next_byte() -> Option<u8> {
    // SAFETY: single CPU, synchronous; SOURCE is written only by `reset` /
    // `install_script`, both before any cell runs.
    let src = unsafe { &mut *addr_of_mut!(SOURCE) };
    match src {
        Source::Serial => None,
        Source::Script(data, pos) => {
            if *pos >= data.len() {
                None
            } else {
                let b = data[*pos];
                *pos += 1;
                Some(b)
            }
        }
    }
}

/// Whether a script (rather than the live console) is installed - what lets a
/// caller with its own live path tell the two apart.
pub fn scripted() -> bool {
    // SAFETY: as above.
    matches!(unsafe { &*core::ptr::addr_of!(SOURCE) }, Source::Script(..))
}

/// Push a received byte into the RX ring - the UART RX interrupt handler's sink
/// (and the poll path's). Portable so per-ISA trap code never names the ring.
pub fn rx_push(b: u8) {
    // **When** a byte arrived is unpredictable while a human is typing; mixed
    // into the entropy pool, never counted (docs/TIME-IDENTITY.md 4a). The byte
    // itself is deliberately *not* passed: `feed_hid` reads the cycle counter and
    // takes only a sequence number, so console input cannot leak into the entropy
    // path as content. The timing is the entropy; the character is what a person
    // typed. The count is this core's `CTR_CONSOLE_BYTES` slot (S4), and the
    // returned running count is the sequence number.
    let seq = crate::obs::cpu_bump(crate::obs::cpu::CTR_CONSOLE_BYTES, 1);
    crate::rng::feed_hid(seq);
    ring().push(b);
}

/// Whether the running ISA delivers console input by interrupt (a genuine
/// 0%-CPU park at WFI) rather than by polling. docs/LIBRHEO.md Phase D names
/// which ISA is which; the poll build reports false everywhere (honest).
pub fn interrupt_driven() -> bool {
    crate::arch::uart_irq_enabled()
}

/// Whether the kernel actually idled at WFI/HLT during the last `wait_input`
/// block (the test's idle-park assertion; meaningful only when
/// [`interrupt_driven`]). False in the poll build.
pub fn did_idle() -> bool {
    // SAFETY: single CPU.
    unsafe { *addr_of!(IDLED) }
}

/// Record that the kernel genuinely halted waiting for a console byte. Called by
/// the **scheduler idle state** ([`crate::idle`]) when the park it performed on
/// behalf of a cell blocked on console input really stopped the CPU: since the
/// docs/ARCHITECTURE-DEBT.md 2.4 slice, that park may happen in the scheduler
/// rather than inside `SYS_WAIT_INPUT`, and it is the same halt either way.
pub fn mark_idle() {
    // SAFETY: single CPU.
    unsafe {
        *addr_of_mut!(IDLED) = true;
    }
}

/// Whether the RX ring already holds at least one byte - the non-destructive peek
/// the scheduler needs to decide that a cell blocked on console input is now
/// satisfiable (docs/ARCHITECTURE-DEBT.md 2.4).
pub fn has_data() -> bool {
    let r = ring();
    r.head != r.tail
}

/// Whether input has **ended**: a scripted source is exhausted. A live serial
/// console never ends, so this is false for it. A cell blocked on console input
/// with `at_eof()` is satisfiable and its read completes with 0 (end of input).
pub fn at_eof() -> bool {
    // SAFETY: single CPU, synchronous trap.
    match unsafe { &*addr_of!(SOURCE) } {
        Source::Script(data, pos) => *pos >= data.len(),
        Source::Serial => false,
    }
}

// How [`pump`]'s injected byte actually reached the ring is counted per tier in
// the observability plane (`obs::cpu`, S4). **Measured, not claimed**: a non-zero
// fifo-take count means the interrupt did not deliver it and a non-zero
// direct-push count means the wire had nothing either - precisely the facts the
// old code inferred away.

/// Times [`pump`] had to recover an injected byte from the UART RX FIFO because
/// the interrupt did not deliver it. Zero on a healthy path.
pub fn pump_fifo_takes() -> u64 {
    crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PUMP_FIFO_TAKES)
}

/// Times [`pump`] had to push an injected byte into the ring itself, because
/// neither the interrupt nor the FIFO produced it. Zero on a healthy path, and one
/// console line per occurrence says so.
pub fn pump_direct_pushes() -> u64 {
    crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PUMP_DIRECT_PUSHES)
}

/// What [`pump`] achieved.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Pump {
    /// At least one byte is now in the ring.
    Data,
    /// Input has ended (a scripted source is exhausted).
    Eof,
    /// Nothing available yet; the caller must halt/re-check. Only a live serial
    /// source reaches this.
    Wait,
}

/// Make console input progress **without** committing to an unbounded wait - the
/// entry point the scheduler idle state uses (docs/ARCHITECTURE-DEBT.md 2.4).
///
/// This is [`refill`] split into its bounded and unbounded halves
/// (docs/ENGINEERING.md 11, "a state machine must not serve both a bounded sequence
/// and an unbounded steady state from one entry point"): a scripted source
/// *produces* its next byte here (through the real UART RX interrupt where one is
/// wired, so the byte takes a live keystroke's path and the halt is genuine); a live
/// serial source is polled **once** and otherwise reports [`Pump::Wait`], leaving
/// the halt to the caller, which may have other deadlines to honour at the same
/// time.
pub fn pump() -> Pump {
    if has_data() {
        return Pump::Data;
    }
    // SAFETY: single CPU, synchronous trap.
    let src = unsafe { &mut *addr_of_mut!(SOURCE) };
    match src {
        Source::Script(data, pos) => {
            if *pos >= data.len() {
                return Pump::Eof;
            }
            let b = data[*pos];
            *pos += 1;
            if !crate::arch::uart_irq_enabled() {
                rx_push(b);
                return Pump::Data;
            }
            // Interrupt-driven: take the byte through the real UART RX interrupt -
            // the path a live keystroke takes - and then **check that it arrived**.
            //
            // The check is the fix. This used to be `uart_inject_and_wait` followed
            // by an unconditional `Pump::Data`: the arrival was inferred from having
            // halted. A halt ends on *any* enabled interrupt, and since the timer
            // one-shot became real on every ISA (docs/SMP.md 5) a competing deadline
            // can end it with the UART handler never having run - `pump` then
            // claimed data, the ring was empty, and `SYS_WAIT_INPUT` returned 0.
            // Seen as an intermittent `schedidle` failure that passed on every
            // re-run, which is the worst kind: a proof whose result depends on
            // ordering is not a proof (docs/ENGINEERING.md 1, 11).
            //
            // Note what is *not* changed: `uart_inject_and_wait` stays one arch
            // operation. Its per-ISA sequence - raise the controller line, halt
            // (which returns at once *because* the interrupt is already pending),
            // then unmask so it is taken and serviced - is coherent, and splitting
            // the halt out of it produced a second halt with nothing left to wake
            // it, wedging the machine. Verified by trying it.
            unsafe {
                *addr_of_mut!(IDLED) = true;
            }
            crate::arch::uart_inject_and_wait(b);
            if has_data() {
                return Pump::Data;
            }
            // The interrupt did not deliver it. On a 16550 the loopback byte is
            // still sitting in the RX FIFO, so recover it from the wire.
            crate::obs::cpu_bump(crate::obs::cpu::CTR_PUMP_FIFO_TAKES, 1);
            if let Some(got) = crate::arch::serial_read_byte() {
                rx_push(got);
                return Pump::Data;
            }
            // Neither. On ARM64 the injected byte is carried *for* the handler
            // rather than placed in the PL011's receiver (its loopback does not feed
            // the RX FIFO), so there is nothing for the tier above to find and only
            // this one can recover it. Dropping the keystroke would be a silent
            // wrong answer; the printed line plus `pump_direct_pushes` is how a
            // degraded interrupt path stays visible instead of being papered over.
            crate::obs::cpu_bump(crate::obs::cpu::CTR_PUMP_DIRECT_PUSHES, 1);
            crate::println!(
                "input: the UART RX interrupt did not deliver an injected byte and the FIFO was \
                 empty - delivered directly (interrupt path degraded)"
            );
            rx_push(b);
            Pump::Data
        }
        Source::Serial => match crate::arch::serial_read_byte() {
            Some(b) => {
                rx_push(b);
                Pump::Data
            }
            None => Pump::Wait,
        },
    }
}

/// Copy whatever is already buffered into `[buf_va, buf_va+len)` and return the
/// count (0 if the ring is empty). The **non-blocking** half of
/// [`wait_input`]: the scheduler uses it to complete a cell's parked
/// `SYS_WAIT_INPUT` once the ring has data (docs/ARCHITECTURE-DEBT.md 2.4).
///
/// # Safety
/// `buf_va` must be a writable `len`-byte buffer in the **active** address space
/// (the blocked cell's, which the scheduler activates before completing its block).
pub unsafe fn drain(buf_va: u64, len: usize) -> usize {
    drain_into(buf_va, len)
}

/// `SYS_WAIT_INPUT`: block until at least one input byte is available, copy up
/// to `len` bytes into the cell buffer at `buf_va`, and return the count.
/// Returns 0 only at end of input (a script source exhausted). The cell's
/// address space is active during the trap, so `buf_va` is written directly.
///
/// This is the OS's first block-and-wake. Where the UART RX interrupt is wired
/// ([`interrupt_driven`]) the idle path halts the CPU until the interrupt
/// delivers a byte; otherwise it polls the UART (the CPU spins - honest, not
/// 0%-idle).
pub fn wait_input(buf_va: u64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    loop {
        let n = drain_into(buf_va, len);
        if n > 0 {
            return n;
        }
        // Nothing buffered: make a byte available, or report end of input.
        if !refill() {
            return 0;
        }
    }
}

/// Copy whatever the ring already holds into `[buf_va, buf_va+len)`; returns the
/// count copied (0 if the ring was empty).
fn drain_into(buf_va: u64, len: usize) -> usize {
    let r = ring();
    let mut n = 0;
    while n < len {
        match r.pop() {
            // SAFETY: `buf_va + n` is within the cell's `len`-byte buffer in its
            // active address space (the cell passed its own buffer).
            Some(b) => unsafe {
                (buf_va as *mut u8).add(n).write(b);
                n += 1;
            },
            None => break,
        }
    }
    n
}

/// Make at least one byte available in the ring, or return false at end of
/// input. This is where the block/idle happens when `SYS_WAIT_INPUT` has nobody
/// to hand the CPU to - the bounded [`pump`] wrapped in the unbounded wait.
fn refill() -> bool {
    loop {
        match pump() {
            Pump::Data => return true,
            Pump::Eof => return false, // script exhausted = end of input
            Pump::Wait => {
                if crate::arch::uart_irq_enabled() {
                    unsafe {
                        *addr_of_mut!(IDLED) = true;
                    }
                    // Halt until the UART RX interrupt pushes a byte into the ring.
                    crate::arch::idle_wait();
                } else {
                    // Poll the UART until a byte arrives (the honest spin).
                    core::hint::spin_loop();
                }
            }
        }
    }
}
