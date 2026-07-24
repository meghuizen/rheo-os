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
use crate::sched::Admission;
use crate::time::{self, Drbg};
use crate::{mm, pty, user};

static mut DRBG: Drbg = Drbg::ZERO;
static mut EVENTS: EventStream = EventStream::new();
static mut ADMISSION: Admission = Admission::new();
static mut ENGINE: Engine = Engine::cpu();
static mut READY: bool = false;

/// One-time init: seed the per-cell DRBG and attach (measure) the engine.
pub fn init() {
    unsafe {
        *core::ptr::addr_of_mut!(DRBG) = time::derive_cell_drbg();
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
        _ => None,
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
