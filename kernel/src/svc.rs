//! The system-service syscall layer: shell I/O (PTY) plus the resource
//! queries the shell surfaces. In the full design these live in service
//! *cells* reached over queue pairs; here they are kernel-side handlers so
//! the shell has something real to talk to before the service framework
//! exists. Each one exercises a genuine kernel object (clock, entropy,
//! events, memory grants, reservations, leases, the dependency graph on an
//! engine), so the shell's builtins report real state, not mock-ups.

use crate::abi::*;
use crate::engine::{Engine, Op};
use crate::event::{self, EventStream};
use crate::graph::{Graph, Input};
use crate::lease::Lease;
use crate::rng::{self, Drbg};
use crate::sched::Admission;
use crate::time;
use crate::{mm, pty, user};

static mut DRBG: Drbg = Drbg::ZERO;
static mut EVENTS: EventStream = EventStream::new();
static mut ADMISSION: Admission = Admission::new();
static mut ENGINE: Engine = Engine::cpu();
static mut READY: bool = false;

/// One-time init: seed the per-cell DRBG and attach (measure) the engine.
pub fn init() {
    unsafe {
        *core::ptr::addr_of_mut!(DRBG) = rng::derive_cell_drbg();
        (*core::ptr::addr_of_mut!(ENGINE)).attach();
        *core::ptr::addr_of_mut!(READY) = true;
    }
}

fn events() -> &'static mut EventStream {
    unsafe { &mut *core::ptr::addr_of_mut!(EVENTS) }
}

/// Handle a shell/resource syscall. Returns Some(ret) if this module owns
/// the number, None otherwise (the caller faults the cell).
pub fn handle(nr: u64, arg: u64) -> Option<u64> {
    match nr {
        SYS_READLINE => Some(read_line(arg)),
        SYS_WRITE => Some(write(arg)),
        SYS_UPTIME => Some(time::uptime_ticks()),
        SYS_RANDOM => Some(unsafe { (*core::ptr::addr_of_mut!(DRBG)).next_u64() }),
        SYS_MEMINFO => {
            let (free, total) = mm::frames::stats();
            Some(((free as u64) << 32) | total as u64)
        }
        SYS_PS => Some(user::cell_count() as u64),
        SYS_CAPS => Some(user::current_caps_live() as u64),
        SYS_EVENT_EMIT => {
            let kind = if arg == 0 { event::EV_USER } else { arg as u16 };
            events().emit(kind, 0, arg);
            Some(events().total())
        }
        SYS_EVENT_COUNT => Some(((events().buffered() as u64) << 32) | events().total()),
        SYS_GRAPH => Some(run_demo_graph(arg)),
        SYS_RESERVE => {
            let budget = arg >> 32;
            let period = arg & 0xFFFF_FFFF;
            let admission = unsafe { &mut *core::ptr::addr_of_mut!(ADMISSION) };
            match admission.admit(budget, period, period) {
                Ok(_) => Some(admission.committed_ppm()),
                Err(_) => Some(u64::MAX),
            }
        }
        SYS_LEASE => {
            // A short lease; return its fencing token (docs SECURITY 3).
            let lease = Lease::acquire(1 << 40, 0);
            Some(lease.token)
        }
        SYS_CPUINFO => {
            print_cpuinfo();
            Some(0)
        }
        SYS_LSPCI => {
            print_lspci();
            Some(0)
        }
        SYS_NUMA => {
            print_numa();
            Some(0)
        }
        SYS_DEBUG_WRITE => Some(debug_write(arg)),
        _ => None,
    }
}

/// Copy `len` bytes from a loaded program's buffer to the console
/// (docs/USERLAND.md M1). The kernel runs in the cell's address space during
/// the trap with supervisor access to user pages enabled, so the user VAs are
/// directly readable. `len` is capped so a bad request cannot run away.
fn debug_write(req_va: u64) -> u64 {
    // SAFETY: the program passes the VA of a `DebugWrite` in its own mapped
    // pages; we read the descriptor, then `len` bytes from `ptr`.
    unsafe {
        let req = (req_va as *const DebugWrite).read();
        let len = (req.len as usize).min(4096);
        let src = req.ptr as *const u8;
        for i in 0..len {
            crate::arch::serial_write_byte(src.add(i).read());
        }
        len as u64
    }
}

// -- console helpers for the hardware builtins --
//
// These format straight to the PTY (the same UART the shell writes to), so
// the per-ISA feature-name table and PCI classification stay kernel-side
// instead of being duplicated into the U-mode shell. Lines end in "\r\n" to
// match the cooked terminal the shell talks to.

fn pty_str(s: &[u8]) {
    pty::write(s);
}

fn pty_u64(mut v: u64) {
    if v == 0 {
        pty::put_byte(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        pty::put_byte(tmp[n]);
    }
}

fn pty_hex_pad(v: u64, nibbles: usize) {
    pty_str(b"0x");
    let mut i = nibbles;
    while i > 0 {
        i -= 1;
        let nib = ((v >> (i * 4)) & 0xF) as u8;
        pty::put_byte(if nib < 10 {
            b'0' + nib
        } else {
            b'a' + nib - 10
        });
    }
}

/// vendor, core count, then each instruction-set feature by name.
fn print_cpuinfo() {
    let inv = crate::hw::inventory();
    pty_str(b"cpu vendor: ");
    for &c in inv.cpu.vendor.iter() {
        if c == 0 {
            break;
        }
        pty::put_byte(c);
    }
    pty_str(b"\r\ncpu cores: ");
    pty_u64(inv.ncpus as u64);
    pty_str(b"\r\ncpu features:");
    let names = crate::arch::cpu_feature_names();
    for (i, name) in names.iter().enumerate() {
        if inv.cpu.features & (1 << i) != 0 {
            pty::put_byte(b' ');
            pty_str(name.as_bytes());
        }
    }
    pty_str(b"\r\n");
}

/// Short engine label for the lspci line.
fn engine_label(k: crate::hw::EngineKind) -> &'static [u8] {
    use crate::hw::EngineKind::*;
    match k {
        Display => b"display",
        Gpu => b"gpu",
        Nic => b"nic",
        Storage => b"storage",
        Nvme => b"nvme",
        Accelerator => b"accelerator",
        Bridge => b"bridge",
        Other => b"other",
    }
}

/// One line per PCIe function: bus:dev.func vendor:device -> engine.
fn print_lspci() {
    let inv = crate::hw::inventory();
    if inv.npci == 0 {
        pty_str(b"no pci devices\r\n");
        return;
    }
    for d in &inv.pci[..inv.npci] {
        pty_hex_pad(d.bus as u64, 2);
        pty::put_byte(b':');
        pty_hex_pad(d.dev as u64, 2);
        pty::put_byte(b'.');
        pty_u64(d.func as u64);
        pty::put_byte(b' ');
        pty_hex_pad(d.vendor as u64, 4);
        pty::put_byte(b':');
        pty_hex_pad(d.device as u64, 4);
        pty_str(b" -> ");
        pty_str(engine_label(d.engine));
        pty_str(b"\r\n");
    }
}

/// Per-node RAM (MiB) and the CPU count homed on that node.
fn print_numa() {
    let inv = crate::hw::inventory();
    pty_str(b"numa nodes: ");
    pty_u64(inv.nnodes as u64);
    pty_str(b"\r\n");
    for node in 0..inv.nnodes as u8 {
        let cpus = inv.cpus[..inv.ncpus]
            .iter()
            .filter(|c| c.node == node)
            .count();
        pty_str(b"node ");
        pty_u64(node as u64);
        pty_str(b": ram ");
        pty_u64(inv.node_ram_bytes(node) / (1024 * 1024));
        pty_str(b" MiB, cpus ");
        pty_u64(cpus as u64);
        pty_str(b"\r\n");
    }
}

/// Read one PTY line into ShellIo.in_buf. Returns 1 for a line, 0 at EOF.
fn read_line(io_va: u64) -> u64 {
    let io = io_va as *mut ShellIo;
    // SAFETY: the shell passes the VA of its own ShellIo (mapped RW, and
    // reachable by the kernel through its identity map).
    unsafe {
        let buf = core::ptr::addr_of_mut!((*io).in_buf) as *mut u8;
        match pty::read_line(buf, SHELL_BUF) {
            pty::Line::Read(len) => {
                (*io).in_len = len as u64;
                1
            }
            pty::Line::Eof => {
                (*io).in_len = 0;
                0
            }
        }
    }
}

/// Write ShellIo.out_buf[..out_len] to the PTY.
fn write(io_va: u64) -> u64 {
    let io = io_va as *const ShellIo;
    // SAFETY: as above; out_len is clamped to the buffer.
    unsafe {
        let len = ((*io).out_len as usize).min(SHELL_BUF);
        let base = core::ptr::addr_of!((*io).out_buf) as *const u8;
        for i in 0..len {
            pty::put_byte(base.add(i).read());
        }
    }
    0
}

/// A small dependency graph run on the compute engine, modelling
/// "a pipeline is a graph submitted to the kernel" (docs/SHELL.md 1):
///   n0 = const(arg); n1 = n0 + 1; n2 = n1 * n0   -> result
fn run_demo_graph(arg: u64) -> u64 {
    let mut g = Graph::new();
    let n0 = g
        .push(Op::Const(arg), Input::Imm(0), Input::Imm(0))
        .unwrap();
    let n1 = g.push(Op::Add, Input::Node(n0), Input::Imm(1)).unwrap();
    let _ = g.push(Op::Mul, Input::Node(n1), Input::Node(n0)).unwrap();
    let mut results = [0u64; crate::graph::MAX_NODES];
    let engine = unsafe { &*core::ptr::addr_of!(ENGINE) };
    g.run(engine, &mut results)
}
