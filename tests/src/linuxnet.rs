//! In-QEMU test kernel for rheo-net **N4b** (docs/NETSTACK.md N4b,
//! docs/LINUX-COMPAT.md L8-INET remote): **real remote networking for unmodified
//! Linux binaries**. Where `linuxinet` proved the AF_INET socket *ABI* over
//! loopback, this proves the *wire*: an unmodified static-glibc C program
//! (`inetremote`) runs as a `Personality::Linux` cell and
//!
//!   1. `sendto`s a hand-built **DNS query** to QEMU SLIRP's built-in responder at
//!      `10.0.2.3:53` and `recvfrom`s the reply, checking its structure (our
//!      transaction id echoed, the QR bit set, the sender being `10.0.2.3:53`);
//!   2. `connect()`s to a **closed port** on the SLIRP gateway (`10.0.2.2:9`) - a
//!      real three-way handshake goes out and SLIRP's reset comes back as
//!      `ECONNREFUSED`.
//!
//! ## The architecture being proven
//! The kernel gains a **bridge, not a network stack** (and no kernel object): the
//! Linux personality forwards every **non-loopback** socket operation to a
//! `svc::SocketOps` table - the `svc::FileOps` pattern that already keeps the kernel
//! filesystem-free. This kernel registers it (`inet_personality`), backed by the
//! `rheo-net` stack in its librheo-free **codec posture** plus `hw::virtio_net`.
//! `kernel/` stays allocation-free and network-stack-free, and **loopback INET
//! behaviour is byte-for-byte unchanged** (`linuxinet` still passes).
//!
//! ## Honest / deterministic
//! The DNS *answer count* and whether the gateway port is truly closed depend on
//! the host, so the fixture prints one line from a small fixed set for each phase
//! (`dns answers yes` / `dns answers none` / `dns no reply`; `tcp refused` /
//! `tcp timeout` / `tcp connected`) and this kernel accepts exactly those
//! transcripts, reporting which one occurred. Nothing is ever faked: a reply that
//! did not arrive is printed as such. With no netdev attached the kernel
//! skips-with-reason (the loopback coverage lives in `linuxinet`).
//!
//! A remote receive is a genuine **park**, not a spin: the bridge blocks in
//! `net_rx::wait_frame_slice` (the N2d primitive), so on riscv64/aarch64 the kernel
//! halts at WFI until the NIC's RX interrupt fires. That evidence is asserted
//! kernel-side below.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::hw::virtio_net;
use kernel::linux::{self, stack as linux_stack};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome, Personality};
use kernel::{arch, load, net_rx, println};
use posix::{RamFs, mount};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[path = "inet_personality.rs"]
mod inet_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

/// The static-glibc remote-INET fixture (built by `xtask::build_linux_fixtures`).
macro_rules! fixture {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/x86_64/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/aarch64/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/riscv64/",
                $name
            ))
        }
    }};
}

static INETREMOTE: &[u8] = fixture!("inetremote");

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

// -- stdout capture, wired to the Linux personality's stdout tap --
const CAP_MAX: usize = 4 * 1024;
static mut STDOUT_CAP: [u8; CAP_MAX] = [0; CAP_MAX];
static mut STDOUT_LEN: usize = 0;

fn tap(bytes: &[u8]) {
    // SAFETY: single-threaded; the tap runs only during a cell run.
    unsafe {
        for &b in bytes {
            if STDOUT_LEN < CAP_MAX {
                STDOUT_CAP[STDOUT_LEN] = b;
                STDOUT_LEN += 1;
            }
        }
    }
}

fn captured() -> &'static [u8] {
    unsafe { &STDOUT_CAP[..STDOUT_LEN] }
}

fn run(image: &[u8], argv: &[&[u8]]) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);
    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, img.image_end);
        user::run(0).1
    }
}

/// The accepted transcripts. The UDP phase reports one of three honest outcomes
/// and the TCP phase one of three, so the exact stdout is one of the products -
/// enumerated here rather than pattern-matched, so nothing vague is asserted.
const UDP_LINES: [&[u8]; 3] = [
    b"inetremote: udp sent\ninetremote: dns reply ok\ninetremote: dns answers yes\n",
    b"inetremote: udp sent\ninetremote: dns reply ok\ninetremote: dns answers none\n",
    b"inetremote: udp sent\ninetremote: dns no reply\n",
];
const TCP_LINES: [&[u8]; 3] = [
    b"inetremote: tcp refused\n",
    b"inetremote: tcp timeout\n",
    b"inetremote: tcp connected\n",
];
const TAIL: &[u8] = b"inetremote OK\n";

/// Match the captured stdout against `UDP_LINES x TCP_LINES + TAIL`, returning the
/// `(udp, tcp)` variant indices.
fn classify(got: &[u8]) -> Option<(usize, usize)> {
    for (ui, u) in UDP_LINES.iter().enumerate() {
        let Some(rest) = got.strip_prefix(*u) else {
            continue;
        };
        for (ti, t) in TCP_LINES.iter().enumerate() {
            let Some(tail) = rest.strip_prefix(*t) else {
                continue;
            };
            if tail == TAIL {
                return Some((ui, ti));
            }
        }
    }
    None
}

const UDP_WHAT: [&str; 3] = [
    "DNS reply with answers (host has outbound DNS)",
    "DNS reply, zero answers (SLIRP answered, no outbound resolution)",
    "no DNS reply (send went out on the wire, nothing came back)",
];
const TCP_WHAT: [&str; 3] = [
    "ECONNREFUSED from a real reset (remote handshake attempted)",
    "ETIMEDOUT (SYN sent, no answer within the budget)",
    "connected (a real remote three-way handshake completed)",
];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("linuxnet: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC. Without one there is no wire, so
    // this kernel skips with a reason (loopback INET stays covered by linuxinet).
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("linuxnet: no virtio-net device attached - skipping");
            println!("linuxnet: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let mac = dev.mac();
    virtio_net::install(dev);

    // The NIC RX interrupt + the one-shot timer, so a remote receive is a genuine
    // 0%-CPU park with a real deadline (docs/NETSTACK.md N2d). Opt-in, like every
    // other interrupt user: the other kernels boot untouched.
    net_rx::reset();
    let irq = net_rx::enable_irq();
    arch::enable_timer_irq();
    println!(
        "linuxnet: virtio-net MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, receive wait: {}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        if irq {
            "interrupt-driven (WFI idle)"
        } else {
            "kernel poll (no NIC RX interrupt on this ISA)"
        }
    );

    // A ramfs at / + the VFS personality (glibc startup wants a working VFS
    // surface; the fixture opens no files).
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    svc::set_file_ops(vfs_personality::ops());

    // The N4b bridge: the remote-INET datapath, over the rheo-net codec + the NIC.
    // The kernel itself gains no stack - only this table of function pointers.
    assert!(inet_personality::init(), "remote INET datapath init failed");
    svc::set_socket_ops(inet_personality::ops());
    println!(
        "linuxnet: remote INET datapath registered (svc::SocketOps over rheo-net + virtio-net), \
         local {}.{}.{}.{}",
        inet_personality::LOCAL_IP.octets()[0],
        inet_personality::LOCAL_IP.octets()[1],
        inet_personality::LOCAL_IP.octets()[2],
        inet_personality::LOCAL_IP.octets()[3],
    );

    println!("linuxnet: inetremote={} bytes", INETREMOTE.len());

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(INETREMOTE, &[b"inetremote"]);
    linux::set_stdout_tap(None);

    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                code == 0,
                "inetremote: exit {code}, expected 0; stdout {:?}",
                core::str::from_utf8(got)
            );
            let Some((ui, ti)) = classify(got) else {
                panic!(
                    "inetremote: stdout not an accepted transcript\n  got: {:?}",
                    core::str::from_utf8(got)
                );
            };
            println!("linuxnet: udp  -> {}", UDP_WHAT[ui]);
            println!("linuxnet: tcp  -> {}", TCP_WHAT[ti]);
            // The UDP phase must have reached the wire in both directions: a
            // structurally valid DNS reply from 10.0.2.3:53 is the deliverable.
            assert!(
                ui != 2,
                "no DNS reply arrived - SLIRP's resolver did not answer, so the remote \
                 UDP round trip is unproven on this host"
            );
        }
        Outcome::Faulted(addr) => panic!("inetremote: faulted at {addr:#x}"),
    }
    println!(
        "linuxnet: unmodified static-glibc binary did a REAL remote UDP round trip \
         (DNS to 10.0.2.3:53) + a real remote TCP connect over the NIC"
    );

    // Kernel-side evidence that the remote receive genuinely parked. The interrupt
    // count can only be incremented from the ISA's interrupt vector, so it cannot
    // be faked; where no NIC interrupt exists the wait is a bounded kernel poll,
    // reported rather than claimed.
    if net_rx::interrupt_driven() {
        assert!(
            net_rx::irq_count() > 0,
            "interrupt-driven ISA but the kernel never took a NIC interrupt"
        );
        println!(
            "linuxnet: NIC interrupts taken: {} (genuine device interrupt); idle-park: {}",
            net_rx::irq_count(),
            net_rx::did_idle()
        );
    } else {
        // No NIC RX interrupt here, but that does not by itself mean a spin: the
        // wait has three modes and only the last one spins (docs/NETSTACK.md 16).
        // Report the one that was actually taken, plus whether it halted.
        println!(
            "linuxnet: no NIC RX interrupt on this ISA - the remote receive parked in {:?} mode \
             (halted: {}, {} timer slice(s), {} spin poll(s))",
            net_rx::idle_mode(),
            net_rx::did_idle(),
            net_rx::timer_slices(),
            net_rx::spin_polls()
        );
    }

    println!("linuxnet: PASS");
    arch::exit(arch::ExitCode::Success)
}
