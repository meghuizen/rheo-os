//! librheo Phase E proof: a Wayland-class compositor demo (docs/LIBRHEO.md).
//! ONE binary, run as TWO cells that share a typed cross-cell queue pair and
//! pass ownership of a sealed buffer grant, with a flip/present completion.
//!
//! - **client** (role 0): allocates a buffer grant, draws a known pattern into
//!   it, seals it, and `commit`s it - which delegates the sealed buffer to the
//!   compositor (zero-copy) and sends the handle + geometry over the channel,
//!   then awaits the flip completion (the frame callback).
//! - **server / compositor** (role 1): receives the buffer handle, maps the
//!   sealed grant read-only (same frames - no copy), composites it into its
//!   in-memory framebuffer, checksums it, and replies with the flip completion
//!   carrying that checksum.
//!
//! The client asserts the compositor's checksum equals its own known value -
//! proving the compositor read the exact client bytes over the shared mapping
//! (zero-copy cross-cell buffer sharing) - and exits `0x42` on success. Which
//! cell is which is decided by the role `SYS_CONNECT` reports (the test kernel
//! wires cell 0 = client, cell 1 = server).

#![no_std]
#![no_main]

use librheo::display::{Compositor, Surface};
use librheo::ipc::Channel;
use librheo::sys;

const W: u32 = 64;
const H: u32 = 64;

/// The known pixel pattern the client draws (deterministic, non-trivial, so a
/// zero or mismatched buffer would produce a different checksum).
fn pattern(i: usize, frame_id: u32) -> u32 {
    (i as u32)
        .wrapping_mul(0x9E37_79B1)
        .rotate_left(frame_id & 31)
        ^ 0x1234_5678
}

/// The client cell: draw a surface, commit it, verify the flip round-trip.
fn client(ch: &Channel) -> ! {
    let mut surface = Surface::new(W, H).expect("librheo-wl: surface alloc");
    // Frame 1: fill with the known pattern.
    {
        let px = surface.pixels_mut();
        for (i, p) in px.iter_mut().enumerate() {
            *p = pattern(i, 1);
        }
    }
    let expect = surface.checksum();
    librheo::println!("client: drew {W}x{H} surface, checksum {expect:#010x}");

    // Commit: seal + delegate the buffer to the compositor, send the frame, and
    // await the flip completion (the compositor's checksum of the shared buffer).
    let got = surface.commit(ch).expect("librheo-wl: commit/flip");
    librheo::println!("client: flip completion, compositor checksum {got:#010x}");

    if got == expect {
        librheo::println!("client: zero-copy cross-cell buffer share verified");
        sys::exit(0x42);
    }
    librheo::println!("client: checksum MISMATCH (share not zero-copy)");
    sys::exit(1)
}

/// The compositor cell: receive the committed frame, composite the shared buffer
/// into the framebuffer, reply with the flip completion.
fn server(ch: &Channel) -> ! {
    let mut comp = Compositor::new(W, H).expect("librheo-wl: compositor alloc");
    let (frame_id, sum, peer_va) = comp.present(ch);
    librheo::println!(
        "compositor: composited frame {frame_id} into framebuffer, checksum {sum:#010x}"
    );
    // Security regression (docs/ENGINEERING.md 12, finding F3). The client's
    // sealed grant is mapped read-only *here*, but its frames belong to the
    // client: freeing them would be a cross-cell use-after-free, and a second
    // free would panic the kernel. The peer's capability on that grant is
    // READ-only, so `SYS_MUNMAP`'s grant check (which requires MAP) must refuse
    // it. Same for this cell's own shared channel ring and its queue region -
    // one is shared with the client, the other the kernel still holds an overlay
    // onto. The evidence is unfakeable: after all four refusals the compositor
    // re-reads the shared buffer and re-checksums it, and the client asserts
    // that checksum against its own known value.
    let attempts = [
        ("peer sealed grant", peer_va),
        ("shared channel ring", ch.base() as u64),
        (
            "own queue region",
            sys::queue_info().map(|q| q.qp_va).unwrap_or(0),
        ),
    ];
    for (what, va) in attempts {
        let r = sys::munmap_checked(va as usize, 4096);
        if r == 0 {
            librheo::println!("compositor: SYS_MUNMAP of the {what} was ACCEPTED - F3 regression");
            sys::exit(2)
        }
        librheo::println!("compositor: SYS_MUNMAP of the {what} refused OK");
    }
    let sum2 = comp.rechecksum();
    if sum2 != sum {
        librheo::println!("compositor: framebuffer changed after the refusals ({sum2:#010x})");
        sys::exit(3)
    }
    librheo::println!("compositor: framebuffer intact after all refusals, checksum {sum2:#010x}");
    // Deliver the flip completion to the client (which reaps it, verifies, and
    // exits - ending the run; the compositor is not resumed after this).
    ch.switch_to_peer();
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let ch = Channel::open().expect("librheo-wl: no cross-cell channel wired");
    if ch.is_client() {
        client(&ch)
    } else {
        server(&ch)
    }
}
