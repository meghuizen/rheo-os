//! Display / compositor primitives (docs/LIBRHEO.md Phase E, docs/GRAPHICS.md,
//! docs/DISPLAY.md): the Wayland-class building blocks over [`ipc`](crate::ipc)
//! and [`mem`](crate::mem). A [`Surface`] is a client-side drawable backed by a
//! **sealed buffer grant** (the `wl_buffer` / dmabuf equivalent); `commit` seals
//! it, delegates it to the compositor, sends the handle + damage over the
//! channel, and awaits the **flip/present completion** (the frame callback). A
//! [`Compositor`] is the server end: it receives a committed frame, maps the
//! shared buffer read-only (zero-copy - same frames), composites it into an
//! in-memory framebuffer, and replies with the flip completion.
//!
//! An input event stream is the *same* typed channel carrying HID events; a
//! compositor delivers [`InputEvent`]s (reusing the Phase D [`term`](crate::term)
//! key type) to the focused client over an [`ipc::Channel`](crate::ipc::Channel).
//!
//! **Phase H adds a real GPU scanout path** (docs/DISPLAY.md, docs/LIBRHEO.md
//! Phase H): [`Scanout`] / [`Gpu`] drive a real virtio-gpu 2D resource. A cell
//! draws into a framebuffer grant and [`Scanout::present`]s it - an
//! `OP_GPU_PRESENT` submission the kernel bridges to the virtio-gpu driver
//! (copy into the resource, transfer to host, flush to scanout 0). The Phase E
//! in-memory compositor path is unchanged (it still proves zero-copy cross-cell
//! sharing); the GPU present is the added real-hardware step. QEMU runs headless
//! in CI, so the proof is the genuine command round-trip (every 2D command
//! returns `RESP_OK_*` from the real device model), not a visible pixel -
//! visible output stays deferred, honestly (docs/DISPLAY.md).

use crate::ipc::{self, Channel};
use crate::mem::{Grant, MemKind};
use crate::rt;
use crate::sys;

/// Bytes per pixel (packed 32-bit RGBA/XRGB - the compositor treats a pixel as
/// an opaque `u32`).
pub const BYTES_PER_PIXEL: usize = 4;

/// The channel message opcode for a committed frame (client -> compositor). The
/// payload is `[peer_va u64@0][frame_id u32@8][w u32@12][h u32@16]`.
pub const OP_FRAME: u8 = 1;

/// A HID/input event a compositor delivers to the focused client over an
/// [`ipc::Channel`](crate::ipc::Channel). Reuses the Phase D typed key
/// (docs/LIBRHEO.md Phase D): escape/keymap decoding stays userland, and the
/// input stream is the same cross-cell typed queue pair as the frame channel.
pub type InputEvent = crate::term::input::Key;

/// Sum the pixels of `px` (wrapping) - a cheap content fingerprint used to prove
/// the compositor read the client's exact bytes over the shared mapping.
pub fn checksum(px: &[u32]) -> u32 {
    let mut s: u32 = 0;
    for &p in px {
        s = s.wrapping_add(p);
    }
    s
}

/// A client-side drawable backed by a sealed buffer grant (the `wl_buffer` /
/// dmabuf equivalent). Draw into [`pixels_mut`](Surface::pixels_mut), then
/// [`commit`](Surface::commit) to hand the buffer to the compositor and await the
/// flip completion.
pub struct Surface {
    grant: Grant,
    w: u32,
    h: u32,
    frame_id: u32,
    committed: bool,
}

impl Surface {
    /// Allocate a `w x h` RGBA surface (a committed DDR grant). `None` if the
    /// kernel refuses the grant.
    pub fn new(w: u32, h: u32) -> Option<Surface> {
        let len = w as usize * h as usize * BYTES_PER_PIXEL;
        let grant = Grant::alloc(MemKind::Ddr, len)?;
        Some(Surface {
            grant,
            w,
            h,
            frame_id: 0,
            committed: false,
        })
    }

    pub fn width(&self) -> u32 {
        self.w
    }
    pub fn height(&self) -> u32 {
        self.h
    }

    /// The pixel buffer as a mutable `u32` slice (writable until `commit` seals
    /// it).
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let n = self.w as usize * self.h as usize;
        // SAFETY: the committed grant holds `n` u32s at its base; unsealed => RW.
        unsafe { core::slice::from_raw_parts_mut(self.grant.base() as *mut u32, n) }
    }

    /// The pixel buffer as a shared `u32` slice.
    pub fn pixels(&self) -> &[u32] {
        let n = self.w as usize * self.h as usize;
        // SAFETY: the committed grant holds `n` u32s at its base.
        unsafe { core::slice::from_raw_parts(self.grant.base() as *const u32, n) }
    }

    /// The content fingerprint of the current pixels (the client's known value).
    pub fn checksum(&self) -> u32 {
        checksum(self.pixels())
    }

    /// Commit the frame: seal the buffer immutable, delegate it to the
    /// compositor, send the buffer handle + geometry over `ch`, and await the
    /// flip/present completion - returning the checksum the compositor computed
    /// over the shared buffer (the frame-callback payload). `None` on any
    /// failure. A surface commits **once** (single-buffered); double-buffering
    /// with a per-frame buffer pool is the documented refinement.
    pub fn commit(&mut self, ch: &Channel) -> Option<u32> {
        if self.committed {
            return None;
        }
        self.frame_id += 1;
        // Seal the buffer immutable (object 5), then delegate it to the peer.
        self.grant.seal().ok()?;
        let shared = ipc::share(&self.grant)?;
        self.committed = true;
        // Frame message: [peer_va u64][frame_id u32][w u32][h u32].
        let mut p = [0u8; 24];
        p[0..8].copy_from_slice(&shared.peer_va.to_le_bytes());
        p[8..12].copy_from_slice(&self.frame_id.to_le_bytes());
        p[12..16].copy_from_slice(&self.w.to_le_bytes());
        p[16..20].copy_from_slice(&self.h.to_le_bytes());
        ch.send(OP_FRAME, self.frame_id as u64, &p);
        // Await the flip completion (the compositor runs, composites, replies).
        let cqe = ch.await_completion();
        Some(cqe.result)
    }
}

/// A committed frame the compositor received: where the shared buffer is mapped
/// in this cell, its geometry, and the frame id.
#[derive(Copy, Clone)]
pub struct Frame {
    pub peer_va: u64,
    pub frame_id: u32,
    pub w: u32,
    pub h: u32,
}

/// The server end: an in-memory framebuffer that composites client surfaces
/// (docs/DISPLAY.md). Receives a committed frame, maps the shared buffer
/// read-only (zero-copy), composites it into the framebuffer, and replies with
/// the flip completion.
pub struct Compositor {
    fb: Grant,
    w: u32,
    h: u32,
}

impl Compositor {
    /// A `w x h` in-memory framebuffer (a committed DDR grant the server owns).
    pub fn new(w: u32, h: u32) -> Option<Compositor> {
        let len = w as usize * h as usize * BYTES_PER_PIXEL;
        let fb = Grant::alloc(MemKind::Ddr, len)?;
        Some(Compositor { fb, w, h })
    }

    /// The framebuffer pixels.
    pub fn framebuffer(&self) -> &[u32] {
        let n = self.w as usize * self.h as usize;
        // SAFETY: the committed framebuffer grant holds `n` u32s at its base.
        unsafe { core::slice::from_raw_parts(self.fb.base() as *const u32, n) }
    }

    fn framebuffer_mut(&mut self) -> &mut [u32] {
        let n = self.w as usize * self.h as usize;
        // SAFETY: unsealed RW framebuffer grant, `n` u32s at its base.
        unsafe { core::slice::from_raw_parts_mut(self.fb.base() as *mut u32, n) }
    }

    /// Decode a frame message from a channel submission payload.
    fn decode(payload: &[u8; 24]) -> Frame {
        let mut va = [0u8; 8];
        va.copy_from_slice(&payload[0..8]);
        let rd32 = |o: usize| {
            u32::from_le_bytes([payload[o], payload[o + 1], payload[o + 2], payload[o + 3]])
        };
        Frame {
            peer_va: u64::from_le_bytes(va),
            frame_id: rd32(8),
            w: rd32(12),
            h: rd32(16),
        }
    }

    /// Receive one committed frame over `ch`, composite the shared buffer into
    /// the framebuffer (the single compositing copy, like a real server blends a
    /// surface into the scanout), and send the flip completion carrying the
    /// framebuffer checksum. Returns the `(frame_id, checksum)`. The caller then
    /// [`switch_to_peer`](Channel::switch_to_peer)s to deliver the completion.
    pub fn present(&mut self, ch: &Channel) -> (u32, u32) {
        let msg = ch.recv();
        let frame = Self::decode(&msg.payload);
        let n = (frame.w as usize * frame.h as usize).min(self.w as usize * self.h as usize);
        // SAFETY: the client's sealed buffer was mapped read-only into this cell
        // at `peer_va`; `n` u32s lie within it. Reading it is zero-copy - the
        // same physical frames the client filled.
        let src = unsafe { core::slice::from_raw_parts(frame.peer_va as *const u32, n) };
        let dst = self.framebuffer_mut();
        dst[..n].copy_from_slice(src); // the one compositing copy
        let sum = checksum(&dst[..n]);
        // The flip/present completion (the frame callback): status ok, checksum
        // in `result`, frame id echoed in `user_data`.
        ch.complete(frame.frame_id as u64, crate::sys::STATUS_OK, sum);
        (frame.frame_id, sum)
    }
}

// ============================================================================
// Real GPU scanout (docs/LIBRHEO.md Phase H, docs/DISPLAY.md). The virtio-gpu
// 2D driver is a single-instance kernel resource; `OP_GPU_PRESENT` bridges a
// cell's present to it. This is the added real-hardware step over the Phase E
// in-memory compositor - the compositor can present its composited framebuffer
// to a real display surface after building it.
// ============================================================================

/// The GPU present verb: submit one framebuffer to the virtio-gpu driver over
/// the queue (`OP_GPU_PRESENT`) and await the completion. Low-level; most cells
/// use [`Scanout`].
pub struct Gpu;

impl Gpu {
    /// Present the `w x h` RGBA framebuffer at `buf_va` (a live cell VA): the
    /// kernel copies it into the virtio-gpu resource, transfers it to the host,
    /// and flushes it to scanout 0. Returns the byte count on success. `Err` if
    /// no GPU is installed or a 2D command failed.
    #[allow(clippy::result_unit_err)]
    pub async fn present(buf_va: u64, w: u32, h: u32) -> Result<u32, ()> {
        let mut a = [0u8; 24];
        a[0..8].copy_from_slice(&buf_va.to_le_bytes());
        a[8..12].copy_from_slice(&w.to_le_bytes());
        a[12..16].copy_from_slice(&h.to_le_bytes());
        let cqe = rt::submit_and_await(sys::OP_GPU_PRESENT, a).await;
        if cqe.status == sys::STATUS_OK {
            Ok(cqe.result)
        } else {
            Err(())
        }
    }
}

/// A client-side scanout surface backed by a framebuffer grant, presented to a
/// real virtio-gpu resource. Draw into [`pixels_mut`](Scanout::pixels_mut), then
/// [`present`](Scanout::present) to push the frame to the device. Unlike
/// [`Surface`] (which delegates a sealed buffer to a compositor cell), this goes
/// straight to the GPU driver.
pub struct Scanout {
    grant: Grant,
    w: u32,
    h: u32,
}

impl Scanout {
    /// Allocate a `w x h` RGBA scanout (a committed DDR grant). `None` if the
    /// kernel refuses the grant.
    pub fn new(w: u32, h: u32) -> Option<Scanout> {
        let len = w as usize * h as usize * BYTES_PER_PIXEL;
        let grant = Grant::alloc(MemKind::Ddr, len)?;
        Some(Scanout { grant, w, h })
    }

    pub fn width(&self) -> u32 {
        self.w
    }
    pub fn height(&self) -> u32 {
        self.h
    }

    /// The pixel buffer as a mutable `u32` slice.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let n = self.w as usize * self.h as usize;
        // SAFETY: the committed, unsealed grant holds `n` u32s at its base.
        unsafe { core::slice::from_raw_parts_mut(self.grant.base() as *mut u32, n) }
    }

    /// The content fingerprint of the current pixels.
    pub fn checksum(&self) -> u32 {
        let n = self.w as usize * self.h as usize;
        // SAFETY: the committed grant holds `n` u32s at its base.
        let px = unsafe { core::slice::from_raw_parts(self.grant.base() as *const u32, n) };
        checksum(px)
    }

    /// Present the current frame to the real GPU scanout (transfer + flush).
    /// Returns the byte count on success.
    #[allow(clippy::result_unit_err)]
    pub async fn present(&self) -> Result<u32, ()> {
        Gpu::present(self.grant.base() as u64, self.w, self.h).await
    }
}
