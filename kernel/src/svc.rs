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
pub fn handle(nr: u64, args: &[u64; 6]) -> Option<u64> {
    let arg = args[0];
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
        SYS_ENGINE_INFO => {
            // Engine introspection (docs/LIBRHEO.md Phase C, object 4): report the
            // CPU engine's kind, attach-time MEASURED cost, and preemption contract.
            let engine = unsafe { &*core::ptr::addr_of!(ENGINE) };
            let info = EngineInfo {
                kind: 0, // CPU (the only real engine here)
                measured_cost_ticks: engine.measured_cost_ticks(),
                preemption: match engine.preemption {
                    crate::engine::Preemption::Instruction => 0,
                    crate::engine::Preemption::OpBoundary => 1,
                },
            };
            // SAFETY: `arg` is a user VA in the running cell's active address
            // space, sized for an `EngineInfo` (the cell passes its own slot).
            unsafe {
                (arg as *mut EngineInfo).write(info);
            }
            Some(0)
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
        // POSIX file syscalls (docs/USERLAND.md M2): forwarded to the
        // registered personality handler. The handler runs in kernel context
        // with user-memory access enabled, so it takes raw user VAs. Returns
        // None (faults the cell) if no personality is installed.
        SYS_OPEN => file_ops().map(|o| (o.open)(args[0], args[1], args[2]) as u64),
        SYS_CLOSE => file_ops().map(|o| (o.close)(args[0]) as u64),
        SYS_READ => file_ops().map(|o| (o.read)(args[0], args[1], args[2]) as u64),
        SYS_WRITE_FD => file_ops().map(|o| (o.write)(args[0], args[1], args[2]) as u64),
        SYS_LSEEK => file_ops().map(|o| (o.lseek)(args[0], args[1] as i64, args[2]) as u64),
        SYS_STAT => file_ops().map(|o| (o.stat)(args[0], args[1], args[2]) as u64),
        SYS_FSTAT => file_ops().map(|o| (o.fstat)(args[0], args[1]) as u64),
        SYS_GETDENTS => file_ops().map(|o| (o.getdents)(args[0], args[1], args[2], args[3]) as u64),
        _ => None,
    }
}

/// The POSIX personality's file operations (docs/USERLAND.md M2). In the full
/// design these live in a service cell reached over a queue pair; for M2 they
/// are function pointers a test/service registers, keeping the kernel free of
/// any filesystem dependency. Each runs in kernel context during the trap and
/// takes raw user VAs (readable/writable there). A negative return is
/// `-errno`.
#[derive(Copy, Clone)]
pub struct FileOps {
    pub open: fn(path_va: u64, path_len: u64, flags: u64) -> i64,
    pub close: fn(fd: u64) -> i64,
    pub read: fn(fd: u64, buf_va: u64, len: u64) -> i64,
    pub write: fn(fd: u64, buf_va: u64, len: u64) -> i64,
    pub lseek: fn(fd: u64, off: i64, whence: u64) -> i64,
    pub stat: fn(path_va: u64, path_len: u64, statbuf_va: u64) -> i64,
    pub fstat: fn(fd: u64, statbuf_va: u64) -> i64,
    pub getdents: fn(path_va: u64, path_len: u64, buf_va: u64, buf_len: u64) -> i64,
}

static mut FILE_OPS: Option<FileOps> = None;

/// Install the POSIX personality handler (called once at boot by the cell
/// that provides the filesystem view).
pub fn set_file_ops(ops: FileOps) {
    unsafe {
        *core::ptr::addr_of_mut!(FILE_OPS) = Some(ops);
    }
}

/// The installed POSIX personality handler, if any. Public so the Linux
/// personality's fd table can forward file I/O through the same VFS
/// (docs/LINUX-COMPAT.md L2).
pub fn file_ops() -> Option<&'static FileOps> {
    // SAFETY: set once at boot, read-only afterwards.
    unsafe { (*core::ptr::addr_of!(FILE_OPS)).as_ref() }
}

// ----------------------------------------------------- the remote-INET bridge
//
// rheo-net **N4b** (docs/NETSTACK.md N4b, docs/LINUX-COMPAT.md L8-INET remote).
// `FileOps` above keeps the kernel **filesystem-free** while still serving the
// POSIX file syscalls: a service registers function pointers, policy lives
// outside. `SocketOps` is that pattern applied to the network, and for exactly the
// same reason: doctrine (docs/ARCHITECTURE.md 6, docs/NETWORKING.md) puts IP/UDP/
// TCP in **userspace**, and the kernel is allocation-free, so it can hold no
// network stack. The Linux personality's INET sockets therefore keep their
// loopback fast path in-kernel (a reliable local byte stream is just the L6 ring,
// `linux::inetsock`) and forward every **non-loopback** operation to whatever
// registered this table.
//
// Adds **no kernel object**: a socket is still a per-cell synthesized fd, and the
// bridge is a table of `fn` pointers - the same mechanism-only shape as `FileOps`.
// Today the registrant is a test kernel (`tests/src/inet_personality.rs`) that
// links the `rheo-net` **codec** posture and drives `hw::virtio_net`; the
// documented end state is a network **service cell** reached over a queue pair
// (rheo-net N4a), which this table is deliberately shaped to accept.

/// The remote (NIC-backed) INET datapath's operations. Every handler runs in
/// kernel context during the calling cell's trap, so buffer arguments are raw VAs
/// in the active address space - the `FileOps` convention. A negative return is
/// `-errno`. `timeout_ns` of 0 means "do not block".
#[derive(Copy, Clone)]
pub struct SocketOps {
    /// The local IPv4 address the datapath sends from (for `getsockname`).
    pub local_ip: fn() -> [u8; 4],
    /// Bind a remote UDP endpoint on local `port` (never 0 - the personality
    /// allocates an ephemeral port first). Returns an opaque handle or `-errno`.
    pub udp_bind: fn(port: u16) -> i64,
    /// Release a UDP endpoint handle.
    pub udp_close: fn(ep: u64),
    /// Send `len` bytes at `buf_va` to `dst_ip:dst_port`. Returns bytes or `-errno`.
    pub udp_send: fn(ep: u64, dst_ip: [u8; 4], dst_port: u16, buf_va: u64, len: u64) -> i64,
    /// Receive one datagram into `buf_va` (up to `len`), blocking up to
    /// `timeout_ns`. Writes the sender's IPv4 to `src_ip_va` (4 bytes) and its port
    /// to `src_port_va` (a `u16`), when non-zero. Returns bytes or `-errno`
    /// (`-EAGAIN` when nothing arrived).
    pub udp_recv: fn(
        ep: u64,
        buf_va: u64,
        len: u64,
        src_ip_va: u64,
        src_port_va: u64,
        timeout_ns: u64,
    ) -> i64,
    /// Whether a datagram is already queued for `ep` (poll/epoll readiness).
    pub udp_pending: fn(ep: u64) -> bool,
    /// Active-open a TCP connection to `dst_ip:dst_port` from `src_port`, waiting
    /// up to `timeout_ns` for the handshake. Returns an opaque handle, or
    /// `-ECONNREFUSED` / `-ETIMEDOUT` / another `-errno`.
    pub tcp_connect: fn(dst_ip: [u8; 4], dst_port: u16, src_port: u16, timeout_ns: u64) -> i64,
    /// Send `len` bytes at `buf_va` on a connected handle. Returns bytes or `-errno`.
    pub tcp_send: fn(h: u64, buf_va: u64, len: u64) -> i64,
    /// Receive up to `len` bytes into `buf_va`, blocking up to `timeout_ns`.
    /// Returns bytes (0 = peer closed) or `-errno`.
    pub tcp_recv: fn(h: u64, buf_va: u64, len: u64, timeout_ns: u64) -> i64,
    /// Close a connected handle (sends a FIN, releases the slot).
    pub tcp_close: fn(h: u64),
}

static mut SOCKET_OPS: Option<SocketOps> = None;

/// Install the remote-INET datapath (called once at boot by the cell/test kernel
/// that owns the network stack). Without it, a non-loopback destination keeps
/// returning `ENETUNREACH` exactly as before N4b.
pub fn set_socket_ops(ops: SocketOps) {
    unsafe {
        *core::ptr::addr_of_mut!(SOCKET_OPS) = Some(ops);
    }
}

/// The installed remote-INET datapath, if any. Read by the Linux personality's
/// socket calls (`linux::fd`) for non-loopback addresses.
pub fn socket_ops() -> Option<&'static SocketOps> {
    // SAFETY: set once at boot, read-only afterwards.
    unsafe { (*core::ptr::addr_of!(SOCKET_OPS)).as_ref() }
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

/// Run a **userspace-built** dependency graph on the CPU engine (docs/LIBRHEO.md
/// Phase C, docs/ARCHITECTURE.md 3 objects 4/6). `nodes_va` points at `count`
/// [`GraphNode`]s in the submitting cell's own memory (its address space is
/// active during the drain, so the VAs are directly readable); the graph is
/// validated (topological edges, node cap), executed, and each node's `u64`
/// result written back to `results_va`. Returns `(status, count)` on success or
/// a `(STATUS_*, 0)` failure - the queue completion the reactor delivers.
///
/// # Safety
/// `nodes_va`/`results_va` are trusted to be the calling cell's mapped buffers;
/// the caller (`queue::run_opcode`) invokes this only during that cell's trap.
pub fn graph_submit(nodes_va: u64, count: u32, results_va: u64) -> (u32, u32) {
    use crate::queue::{STATUS_BAD_OPCODE, STATUS_DENIED, STATUS_OK};
    let count = count as usize;
    if count == 0 || count > crate::graph::MAX_NODES {
        return (STATUS_BAD_OPCODE, 0);
    }
    let mut g = Graph::new();
    for i in 0..count {
        // SAFETY: `nodes_va` is the cell's mapped buffer; each 32-byte node is
        // read unaligned to tolerate any buffer alignment.
        let node = unsafe { (nodes_va as *const GraphNode).add(i).read_unaligned() };
        let op = match node.op {
            0 => Op::Const(node.a),
            1 => Op::Add,
            2 => Op::Mul,
            3 => Op::Select,
            _ => return (STATUS_BAD_OPCODE, 0),
        };
        let a = wire_input(node.a_is_node, node.a);
        let b = wire_input(node.b_is_node, node.b);
        if g.push(op, a, b).is_err() {
            // BadEdge (a forward reference) or Full - a malformed graph.
            return (STATUS_DENIED, 0);
        }
    }
    let mut results = [0u64; crate::graph::MAX_NODES];
    let engine = unsafe { &*core::ptr::addr_of!(ENGINE) };
    g.run(engine, &mut results);
    for (i, &r) in results.iter().take(count).enumerate() {
        // SAFETY: `results_va` is the cell's mapped buffer, `count` u64s wide.
        unsafe {
            (results_va as *mut u64).add(i).write_unaligned(r);
        }
    }
    (STATUS_OK, count as u32)
}

/// Decode one graph-node input: an immediate value or a reference to an earlier
/// node's result (validated as topological by `Graph::push`).
fn wire_input(is_node: u32, val: u64) -> Input {
    if is_node != 0 {
        Input::Node(val as usize)
    } else {
        Input::Imm(val)
    }
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
