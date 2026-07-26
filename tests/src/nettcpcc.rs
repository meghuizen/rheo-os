//! In-QEMU test kernel for rheo-net Phase N2b (docs/NETSTACK.md §11 congestion
//! control): the two from-scratch congestion controllers, **Reno** and **CUBIC**
//! (`net::cc`), over the N2a `CongestionControl` seam. A cell loaded from the
//! `nettcpcc-demo` ELF drives **deterministic integer cwnd trajectories** - slow
//! start, AIMD, fast retransmit / fast recovery, RTO collapse, and the CUBIC `W(t)`
//! shape - each pinned against a precomputed oracle, plus a real `Connection<Reno>`
//! fast-retransmit-before-RTO scenario over the in-cell virtual link. It exits
//! `0x42` only if every trajectory matches, so the exit code is the proof.
//!
//! Like `nettcp`, this needs **no netdev**: the proof is entirely in-cell (the CC
//! math + the loopback `VirtualLink`), so a live peer is not required (a live TCP
//! handshake to SLIRP is skipped-with-reason - SLIRP has no TCP responder). The
//! kernel is untouched: `net::cc` + `net::tcp` are portable userspace over the
//! existing reactor ABI. A minimal console `FileOps` backs the cell's `println!`.

#![no_std]
#![no_main]

extern crate alloc;

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/nettcpcc-demo"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/nettcpcc-demo"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/nettcpcc-demo"
));

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

// A console-only FileOps so the cell's `println!` (SYS_WRITE_FD on fd 1/2) reaches
// the serial line; every other file op is unused here.
fn con_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -2
}
fn con_close(_fd: u64) -> i64 {
    0
}
fn con_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -9
}
fn con_write(fd: u64, buf_va: u64, len: u64) -> i64 {
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
fn con_lseek(_fd: u64, off: i64, _w: u64) -> i64 {
    off
}
fn con_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn con_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn con_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

/// **The pacer on the timer arbiter, under continuous re-arm** (docs/NETSTACK.md 21,
/// rheo-net N2e). Kernel-side, deterministic, all three ISAs, no NIC traffic.
///
/// N2h built the arbiter because the hardware has exactly one one-shot deadline and
/// two subsystems were arming it directly, each cancelling the other's deadline on
/// its way out. It reserved a slot for the BBR pacer as "the requester that will make
/// this fatal rather than latent", because a paced flow re-arms a deadline after
/// **every segment**, for the life of the flow.
///
/// This is that client: 40 back-to-back pacer deadlines while a cell sleep and a
/// network (RTO) deadline stay outstanding the whole time. Every one of the 40
/// completions must leave both of the others armed - and then they must still fire at
/// their own times, in order.
fn pacer_arbiter_phase() {
    use kernel::ktimer::{self, TimerClient};

    ktimer::reset();
    let d_net: u64 = 20_000_000; // 20 ms: a TCP RTO
    let d_sleep: u64 = 40_000_000; // 40 ms: the cell's own sleep
    let pace_ns: u64 = 200_000; // 200 us: one paced segment
    const PACES: u64 = 40;

    let t0 = ktimer::now_ns();
    ktimer::register(TimerClient::NetTimer, d_net);
    ktimer::register(TimerClient::CellSleep, d_sleep);

    for i in 0..PACES {
        ktimer::register(TimerClient::Pacer, pace_ns);
        while !ktimer::expired(TimerClient::Pacer) {
            if !ktimer::park(false) {
                arch::spin_loop(1);
            }
        }
        // The two long deadlines survived this release - and every one before it.
        assert!(
            ktimer::pending(TimerClient::NetTimer) && !ktimer::expired(TimerClient::NetTimer),
            "pacer release {i} lost the 20 ms network deadline"
        );
        assert!(
            ktimer::pending(TimerClient::CellSleep) && !ktimer::expired(TimerClient::CellSleep),
            "pacer release {i} lost the 40 ms cell-sleep deadline"
        );
    }
    let paced_span = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        paced_span >= PACES * pace_ns,
        "40 x 200 us of pacing took only {paced_span} ns - the deadlines did not hold"
    );
    assert!(
        ktimer::registrations(TimerClient::Pacer) == PACES,
        "expected {PACES} pacer registrations, got {}",
        ktimer::registrations(TimerClient::Pacer)
    );
    assert!(
        ktimer::preserved() >= PACES,
        "the arbiter never preserved another client's deadline across a pacer release"
    );
    ktimer::cancel(TimerClient::Pacer);

    // And the two deadlines still fire at their own times, in order.
    while !ktimer::expired(TimerClient::NetTimer) {
        if !ktimer::park(false) {
            arch::spin_loop(1);
        }
    }
    let at_net = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_net >= d_net,
        "network deadline fired early ({at_net} ns)"
    );
    assert!(
        ktimer::pending(TimerClient::CellSleep),
        "the network deadline's completion cancelled the cell sleep"
    );
    ktimer::cancel(TimerClient::NetTimer);
    while !ktimer::expired(TimerClient::CellSleep) {
        if !ktimer::park(false) {
            arch::spin_loop(1);
        }
    }
    let at_sleep = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_sleep >= d_sleep && at_sleep > at_net,
        "cell sleep fired early or out of order ({at_sleep} ns)"
    );
    ktimer::cancel(TimerClient::CellSleep);
    assert!(
        ktimer::nearest_ns().is_none(),
        "a deadline is still armed after every client was released"
    );
    println!(
        "nettcpcc: pacer slot re-armed {} times over {} us with a 20 ms RTO and a 40 ms \
         sleep outstanding throughout - none lost ({} preserved, {} arms, {} halts); \
         the RTO then fired at {} us and the sleep at {} us",
        PACES,
        paced_span / 1_000,
        ktimer::preserved(),
        ktimer::arms(),
        ktimer::parks(),
        at_net / 1_000,
        at_sleep / 1_000
    );
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("nettcpcc: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // The pacer's deadlines are real: bring up the per-ISA timer interrupt so
    // `SYS_ARM_TIMER` parks at wfi/hlt through the arbiter rather than falling back
    // to the cooperative deadline check (docs/LIBRHEO.md Phase F, NETSTACK.md 21).
    arch::enable_timer_irq();

    // Kernel-side: the pacer slot under continuous re-arm, beside two other
    // outstanding deadlines (rheo-net N2e over the N2h arbiter).
    pacer_arbiter_phase();
    let pacer_regs_before = kernel::ktimer::registrations(kernel::ktimer::TimerClient::Pacer);

    svc::init();
    svc::set_file_ops(FileOps {
        open: con_open,
        close: con_close,
        read: con_read,
        write: con_write,
        lseek: con_lseek,
        stat: con_stat,
        fstat: con_fstat,
        getdents: con_getdents,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(DEMO, &mut aspace).expect("load nettcpcc-demo ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

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
                "nettcpcc-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "nettcpcc: Reno (slow-start/AIMD/fast-retransmit/RTO) + CUBIC W(t) shape \
                 + integration dup-ACK/RTO + BBRv3 (startup/drain/probe-bw/probe-rtt, \
                 the two filters, loss != congestion) + the pacer, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("nettcpcc-demo faulted at {addr:#x}"),
    }

    // The cell's pacer went through the arbiter's **pacer** slot, once per paced
    // release (14 of its 16 releases; the first two fit the burst allowance). This is
    // the kernel's own count, not the cell's.
    let pacer_regs = kernel::ktimer::registrations(kernel::ktimer::TimerClient::Pacer);
    let from_cell = pacer_regs - pacer_regs_before;
    assert!(
        from_cell >= 14,
        "the cell's pacer registered {from_cell} deadlines in the arbiter's pacer slot, \
         expected at least 14"
    );
    if arch::timer_irq_enabled() {
        assert!(
            kernel::time::timer_did_idle(),
            "the timer interrupt is wired but no pacing deadline ever halted the CPU"
        );
        println!(
            "nettcpcc: the cell's pacer registered {from_cell} deadlines in the arbiter's \
             pacer slot, each a genuine wfi/hlt idle-park ({} halts total)",
            kernel::ktimer::parks()
        );
    } else {
        println!(
            "nettcpcc: the cell's pacer registered {from_cell} deadlines in the arbiter's \
             pacer slot; no verified hardware one-shot on this kernel, so each was an \
             honest cooperative deadline check rather than an idle park"
        );
    }

    println!("nettcpcc: PASS");
    arch::exit(arch::ExitCode::Success)
}
