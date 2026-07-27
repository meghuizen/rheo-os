//! In-QEMU test kernel for rheo-net Phase N4c (docs/NETSTACK.md §20,
//! docs/NETWORKING.md): **host configuration** - DHCP, zeroconf (IPv4 link-local +
//! mDNS), NTP, and the host-config store the rest of the stack reads. A cell loaded
//! from the `nethostcfg-demo` ELF asks the driver for the NIC MAC, then runs a
//! deterministic, **network-free** suite and finally four bonus live attempts.
//!
//! The deterministic assertions (each with its own exit code, so a failure names
//! itself - see the demo's module docs for the full list):
//! 1. **DHCP**: a complete byte oracle for an encoded DISCOVER (every field pinned
//!    at its wire offset, every uncovered byte asserted zero, at the padded 300-byte
//!    length); the DISCOVER -> OFFER -> REQUEST -> ACK -> BOUND walk driven by OFFER
//!    and ACK crafted with the crate's **own** encoder (so encode and decode are
//!    both exercised); a decode oracle on the ACK; the extracted lease and the
//!    armed T1/T2/expiry deadlines; the T1/T2 defaults and the `T1 > T2` clamp; a
//!    **renewal** at T1 (unicast, `ciaddr` set, requested-IP and server-id absent -
//!    RFC 2131 §4.4.5) re-arming the deadlines; **rebinding** at T2 and **expiry**
//!    back to SELECTING with a fresh transaction id; NAK; seven malformed/hostile
//!    shapes each rejected with their own error; DECLINE and RELEASE.
//! 2. **hostcfg**: the lease populates the store, `prefix_len`/`broadcast`/
//!    `netmask_is_valid`/on-link-vs-gateway `next_hop`/search-domain `qualify` are
//!    exact, and **two real stack paths read it back** - a `dns::Config` whose
//!    resolvers are the leased DNS servers, and a `udp::UdpEndpoint` whose source
//!    address is the leased address and which routes an off-link destination to the
//!    leased gateway.
//! 3. **IPv4 link-local** (RFC 3927): a candidate-generator known answer, the
//!    `0.0.0.0`-sender ARP probe decoded back off the wire, a synthetic conflicting
//!    ARP reply forcing a re-pick, a racing probe from another MAC also conflicting
//!    while our own frame does not, 3 probes + 2 announcements reaching Claimed,
//!    `announce()` **bounded** thereafter (so a driver's drain loop terminates) with
//!    defending as its own explicit act, and defend-once-then-yield.
//! 4. **mDNS** (RFC 6762) over the **DNS codec, unchanged**: byte oracles for the
//!    `.local` query with and without the QU bit and for a cache-flushing response,
//!    decode of the cache-flush bit / TTL / class / goodbye, the RFC 1112
//!    `224.0.0.251 -> 01:00:5e:00:00:fb` multicast-MAC mapping, `.local` scoping,
//!    and a responder that answers only its own name.
//! 5. **NTP** (RFC 5905 client subset): a byte oracle for the 48-byte request and a
//!    **hand-computed known-answer test** - T1..T4 of `S / S+1 / S+1.5 / S+2` giving
//!    an offset of exactly **+250 ms** and a delay of exactly **1.5 s**, expressed as
//!    a **bounded interval** of half-width exactly `delay/2` = **750 ms** (and
//!    **1.75 s** once the server declares a 1 s root delay and 0.5 s root
//!    dispersion) - plus nine rejections and the Kiss-o'-Death backoff.
//!
//! Every **live** phase is bounded by a duration, never a drain count (docs/NETSTACK.md
//! 20.2): 1 s per DHCP attempt, 1 s for the NTP reply, 500 ms for an mDNS response,
//! 200 ms of listening after each ARP probe. After the run this kernel also asserts
//! **which wait mode** the kernel's receive wait used - NIC-interrupt-driven 0%-CPU
//! parks on riscv64/aarch64, a timer-backed idle (a real halt between polls, never
//! claimed as a NIC interrupt) on x86-64 - see docs/NETSTACK.md 16.
//!
//! The cell exits `0x42` only if every one of those passes, so the exit code is the
//! proof.
//!
//! Then four **bonus live** attempts over SLIRP, none of them fatal and none of
//! them permitted to fake a result: a real DHCP DISCOVER, an NTP request to the
//! gateway, an mDNS query to `224.0.0.251:5353`, and a link-local ARP probe. SLIRP
//! **does** run a DHCP server on the emulated link, so that exchange is normally
//! answered and the lease is genuine - reported, never asserted (it is a property of
//! the QEMU backend, and a link with no server prints the skip instead). SLIRP runs
//! **no NTP service and hosts no mDNS peer**, so those two **skip with a reason**
//! printed on the serial line; the link-local probe transmits and observes no
//! conflict, which the cell reports as absence of evidence rather than as proof the
//! address is free.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35. The probe tries both, so all three ISAs
//! exercise the same NIC path. The skip branch fires only if no virtio-net device
//! is attached at all.
//!
//! Wiring mirrors `netdns` (queue pair + minted cap + `set_queue_info`); the NIC
//! is discovered + installed like `blockfs` discovers virtio-blk. A minimal
//! console `FileOps` backs the cell's `println!` (fd 1/2 -> serial).

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::addr_of_mut;

use kernel::hw::virtio_net;
use kernel::svc::{self};
use kernel::user::Outcome;
use kernel::{arch, net_rx, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("nethostcfg-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nethostcfg: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("nethostcfg: no virtio-net device attached - skipping");
            println!("nethostcfg: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    println!(
        "nethostcfg: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    virtio_net::install(dev);

    // Every live phase below waits for a frame with a **deadline** rather than a drain
    // count, so bring up both interrupt sources the wait can idle on (opt-in, like
    // `netwait`): the NIC's RX interrupt where the ISA delivers it, and the timer,
    // which every ISA has. Which of the two is available decides the wait mode
    // (kernel/src/net_rx.rs), asserted after the run.
    net_rx::reset();
    let nic_irq = net_rx::enable_irq();
    arch::enable_timer_irq();
    println!(
        "nethostcfg: NIC RX interrupt {}, timer interrupt {} - receive waits idle on {}",
        if nic_irq { "wired" } else { "not available" },
        if arch::timer_irq_enabled() {
            "wired"
        } else {
            "not available"
        },
        if nic_irq {
            "the NIC interrupt (0%-CPU park)"
        } else if arch::timer_irq_enabled() {
            "short timer slices (a real halt between polls)"
        } else {
            "a bounded poll (the CPU spins)"
        }
    );

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "nethostcfg-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "nethostcfg-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "nethostcfg: host config (DHCP + zeroconf + mDNS + NTP + the hostcfg store), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("nethostcfg-demo faulted at {addr:#x}"),
    }

    // Kernel-side evidence for the **wait mode**. The live phases all waited on a
    // deadline, so a wait must have run; which mode it chose follows from the two
    // interrupt predicates, so this is deterministic per ISA and not a guess.
    let mode = net_rx::idle_mode();
    assert!(
        mode != net_rx::IdleMode::None,
        "no receive wait ran - the live phases did not reach the NIC"
    );
    let expected = if nic_irq {
        net_rx::IdleMode::NicInterrupt
    } else if arch::timer_irq_enabled() {
        net_rx::IdleMode::TimerIdle
    } else {
        net_rx::IdleMode::Poll
    };
    assert!(
        mode == expected,
        "receive wait used {mode:?}, expected {expected:?} for this ISA's interrupt set"
    );
    match mode {
        net_rx::IdleMode::NicInterrupt => {
            assert!(
                net_rx::interrupt_driven() && net_rx::did_idle(),
                "NIC-interrupt mode but the wait never halted the CPU"
            );
            println!(
                "nethostcfg: receive waits were NIC-interrupt-driven 0%-CPU parks ({} genuine \
                 device interrupt(s) taken, CPU halted inside the wait)",
                net_rx::irq_count()
            );
        }
        net_rx::IdleMode::TimerIdle => {
            assert!(
                !net_rx::interrupt_driven(),
                "timer-backed mode must never report itself as NIC-interrupt-driven"
            );
            assert!(
                net_rx::did_idle(),
                "timer-backed mode but the wait never halted the CPU - it spun"
            );
            let p = net_rx::policy();
            println!(
                "nethostcfg: receive waits were timer-backed idles - no NIC RX interrupt on this \
                 ISA, so the kernel halted between polls (a real halt, not a spin, and not a NIC \
                 interrupt): {:?} profile, {} us warm slices then {} us cold, {} slice(s) halted \
                 for, {} escalation(s)",
                net_rx::profile(),
                p.warm_slice_ns / 1_000,
                p.cold_slice_ns / 1_000,
                net_rx::timer_slices(),
                net_rx::escalations()
            );
        }
        _ => println!(
            "nethostcfg: receive waits used the bounded poll fallback (neither interrupt \
             available) - deadline-honouring, but the CPU spins"
        ),
    }

    println!("nethostcfg: PASS");
    arch::exit(arch::ExitCode::Success)
}
