//! In-QEMU test kernel for librheo Phase D (docs/LIBRHEO.md): load the
//! `librheo-term` ELF into a cell with a real mapped queue pair + a minted
//! QueuePair capability, feed it a scripted keystroke sequence through the
//! kernel's console-input path, and assert it exits with its distinctive code.
//!
//! The cell runs an interactive read-eval loop over the `term` byte-stream
//! discipline: it decodes keys (including an escape sequence - arrow keys),
//! edits a line with history, renders each change, and commits lines - parking
//! on input between keystrokes via `SYS_WAIT_INPUT` (the OS's first
//! block-and-wake). The scripted bytes exercise typing, backspace, cursor-left
//! + insert, and Up-arrow history recall; the cell verifies the committed lines
//!   and exits `0x42` only if every one is exact.
//!
//! On an ISA where the UART RX interrupt is wired (`input::interrupt_driven`),
//! the test also asserts the kernel actually idled at WFI while the cell was
//! parked (`input::did_idle`) - a genuine 0%-CPU park, not a spin. In the poll
//! build both are false and that assertion is skipped (honest).

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, input, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/librheo-term"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/librheo-term"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/librheo-term"
));

/// The demo returns this on full success (see librheo-term.rs).
const EXPECTED_EXIT: u64 = 0x42;

/// Scripted keystrokes played as if typed:
///   "worlq" <Backspace> "d" <Enter>            -> commit "world"
///   "helo" <Left> "l" <Enter>                  -> commit "hello"
///   <Up> <Up> <Enter>                          -> commit "world" (older history)
/// Backspace is DEL (0x7f); Left is CSI D (ESC [ D); Up is CSI A (ESC [ A).
static SCRIPT: &[u8] = b"worlq\x7fd\rhelo\x1b[Dl\r\x1b[A\x1b[A\r";

fn c_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        -9
    }
}
fn c_stub_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -38
}
fn c_stub_close(_fd: u64) -> i64 {
    -38
}
fn c_stub_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -38
}
fn c_stub_lseek(_fd: u64, _o: i64, _w: u64) -> i64 {
    -38
}
fn c_stub_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("librheoterm: start on {}", arch::NAME);

    svc::init();
    svc::set_file_ops(FileOps {
        open: c_stub_open,
        close: c_stub_close,
        read: c_stub_read,
        write: c_write,
        lseek: c_stub_lseek,
        stat: c_stub_stat,
        fstat: c_stub_fstat,
        getdents: c_stub_getdents,
    });

    // Bring up the UART RX interrupt where this ISA supports it (a no-op that
    // leaves the poll path elsewhere), then feed the cell scripted keystrokes
    // through the kernel console-input path.
    arch::enable_uart_rx_irq();
    input::reset();
    input::install_script(SCRIPT);

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(DEMO, &mut aspace).expect("load librheo-term ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);
    println!(
        "librheoterm: loaded librheo-term ({} bytes), entry {entry:#x}, input mode: {}",
        DEMO.len(),
        if input::interrupt_driven() {
            "interrupt-driven (WFI idle)"
        } else {
            "poll"
        }
    );

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let object = objects.create(ObjectKind::QueuePair).unwrap();
        let cap = caps
            .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
        let cap_id = cap.raw_low32();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();

        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, cap_id);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-term exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
        }
        Outcome::Faulted(addr) => panic!("librheo-term faulted at {addr:#x}"),
    }

    // Idle-park proof: where the UART RX interrupt is wired, the kernel must
    // have genuinely idled at WFI while the cell was parked on input (0% CPU,
    // not a spin). In the poll build this is skipped (documented, honest).
    if input::interrupt_driven() {
        assert!(
            input::did_idle(),
            "interrupt-driven ISA but the kernel never idled at WFI"
        );
        println!("librheoterm: idle-park proven (kernel idled at WFI, woke on UART IRQ)");
    } else {
        println!("librheoterm: poll input path (no UART RX interrupt on this ISA)");
    }

    // Device events feed the entropy pool (docs/TIME-IDENTITY.md 4a). Every byte
    // that arrived above went through `input::rx_push`, which mixes the byte and
    // the cycle count into **this core's** scratch words - two atomic operations,
    // no lock, because a handler must never wait on a thread holding the pool.
    //
    // The scratch is drained into the pool by `pump`, so pumping is what makes
    // the contribution visible. Asserted rather than assumed: writing the hook
    // and never checking it is how a wire ends up disconnected.
    let before = kernel::rng::entropy::counters();
    kernel::rng::entropy::pump();
    let after = kernel::rng::entropy::counters();
    let src = kernel::rng::entropy::Source::Interrupt.index();
    assert!(
        after.drains > before.drains,
        "console bytes arrived but no core had interrupt scratch to drain"
    );
    assert!(
        after.bytes[src] > before.bytes[src],
        "the scratch drained but nothing reached the pool"
    );
    println!(
        "librheoterm: {} console bytes fed the entropy pool ({} bytes mixed, {} drain(s), 0 bits credited - timing is real but unmeasured)",
        SCRIPT.len(),
        after.bytes[src],
        after.drains
    );

    println!("librheoterm: PASS");
    arch::exit(arch::ExitCode::Success)
}
