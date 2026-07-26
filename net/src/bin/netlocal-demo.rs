//! `netlocal-demo` - the rheo-net Phase N1d proof cell for the **native local
//! fast path** (docs/NETSTACK.md §10). ONE binary, run as TWO cells that exchange
//! a payload over `net::local` (a thin typed API over `librheo::ipc::Channel` +
//! sealed grants) with **no IP/Ethernet** - the zero-copy local datapath.
//!
//! - **client** (channel role 0): picks the datapath (`Datapath::Local` for a
//!   same-host peer, `Datapath::Wire` for a remote IP - the selector), draws a
//!   known payload into a buffer grant, seals + `share`s it (zero-copy delegation
//!   to the peer), sends the peer VA + length over the local stream, and awaits
//!   the peer's completion carrying its checksum of the shared buffer.
//! - **server** (channel role 1): receives the handle, maps the shared grant
//!   read-only (the SAME frames - no copy), checksums it, replies.
//!
//! The client asserts the server's checksum equals its own known value - proving
//! the server read the exact client bytes over the shared mapping (zero-copy
//! cell-to-cell transfer over `net::local`) - and exits `0x42`. No kernel object,
//! no `cfg(target_arch)`, no IP stack.

#![no_std]
#![no_main]

use librheo::mem::{Grant, MemKind};
use librheo::sys;
use rheo_net::ip::{IpAddr, Ipv4Addr};
use rheo_net::local::{Datapath, LocalStream, Target, select};

/// Payload size: 4 KiB (one frame), enough to be a real shared buffer.
const LEN: usize = 4096;
/// The queue opcode carrying the shared-buffer handle.
const OP_SHARE: u8 = 1;

/// The known byte pattern the client writes (deterministic, non-trivial, so a
/// zero or mismatched buffer would checksum differently).
fn fill(i: usize) -> u8 {
    ((i as u32).wrapping_mul(2654435761).rotate_left(3) ^ 0xA5) as u8
}

/// A simple order-sensitive checksum both ends compute identically.
fn checksum(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h
}

fn client(s: &LocalStream) -> ! {
    // The datapath selector: a local peer is zero-copy IPC, a remote IP is wire.
    let local = select(&Target::Local);
    let wire = select(&Target::Remote(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2))));
    if local != Datapath::Local || wire != Datapath::Wire || s.datapath() != Datapath::Local {
        librheo::println!("client: datapath selector wrong");
        sys::exit(1);
    }

    // Draw the known payload into a buffer grant, then seal it (immutable =
    // shareable). alloc reserves + commits the whole grant.
    let mut grant = match Grant::alloc(MemKind::Ddr, LEN) {
        Some(g) => g,
        None => {
            librheo::println!("client: grant alloc failed");
            sys::exit(2);
        }
    };
    // SAFETY: the grant is fully committed and unsealed - writable.
    let buf = unsafe { grant.slice_mut(0, LEN) };
    for (i, b) in buf.iter_mut().enumerate() {
        *b = fill(i);
    }
    let expect = checksum(buf);
    if grant.seal().is_err() {
        librheo::println!("client: seal failed");
        sys::exit(3);
    }

    // Share the sealed grant zero-copy, then hand the peer VA + len to the server.
    let shared = match LocalStream::share(&grant) {
        Some(sb) => sb,
        None => {
            librheo::println!("client: share failed");
            sys::exit(4);
        }
    };
    let mut payload = [0u8; 24];
    payload[0..4].copy_from_slice(&(LEN as u32).to_le_bytes());
    if !s.send(OP_SHARE, shared.peer_va, &payload) {
        librheo::println!("client: send failed");
        sys::exit(5);
    }
    librheo::println!("client: shared {LEN}-byte buffer, checksum {expect:#010x}");

    let cq = s.await_completion();
    if cq.result == expect {
        librheo::println!("client: net::local zero-copy verified (server checksum matches)");
        sys::exit(0x42);
    }
    librheo::println!(
        "client: checksum MISMATCH server={:#010x} expect={expect:#010x}",
        cq.result
    );
    sys::exit(6)
}

fn server(s: &LocalStream) -> ! {
    let msg = s.recv();
    let peer_va = msg.user_data;
    let len = u32::from_le_bytes([
        msg.payload[0],
        msg.payload[1],
        msg.payload[2],
        msg.payload[3],
    ]) as usize;
    // Zero-copy: read the SAME frames the client sealed + shared.
    // SAFETY: `peer_va`/`len` are the shared grant the client delegated to us.
    let buf = unsafe { LocalStream::recv_buffer(peer_va, len) };
    let sum = checksum(buf);
    librheo::println!("server: mapped {len}-byte shared buffer zero-copy, checksum {sum:#010x}");
    s.complete(msg.user_data, 0, sum);
    s.switch_to_peer();
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // The channel role decides which end this cell is (the test kernel wires
    // cell 0 = client role 0, cell 1 = server role 1).
    if let Some(s) = LocalStream::connect() {
        client(&s)
    } else if let Some(s) = LocalStream::accept() {
        server(&s)
    } else {
        librheo::println!("netlocal-demo: no cross-cell channel wired");
        sys::exit(7)
    }
}
