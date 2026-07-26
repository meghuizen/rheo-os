//! `librheo-gpu` - the Phase H proof program (docs/LIBRHEO.md, docs/DISPLAY.md):
//! **a real GPU scanout over a virtio-gpu 2D driver**. The cell allocates a
//! framebuffer grant, draws a known RGBA pattern into it, and presents it
//! (`display::Scanout::present`) - an `OP_GPU_PRESENT` submission the kernel
//! bridges to the virtio-gpu driver (copy into the resource, `TRANSFER_TO_HOST_2D`,
//! `RESOURCE_FLUSH`). It exits `0x42` only if the present completes OK.
//!
//! QEMU runs headless (`-display none`), so the proof is the genuine driver
//! round-trip: the device accepts the full create-2d -> attach -> set-scanout ->
//! transfer -> flush sequence and returns `RESP_OK_*` (the `librheogpu` test
//! kernel prints which commands returned OK). This does NOT claim a visible pixel
//! - visible output is deferred, honestly (docs/DISPLAY.md).

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::display::Scanout;
use librheo::{println, rt};

/// The framebuffer geometry - matches the driver's fixed 128x128 resource
/// (kernel/src/hw/virtio_gpu.rs), so the whole buffer transfers.
const W: u32 = 128;
const H: u32 = 128;

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// A deterministic, non-trivial pixel pattern (a zero/garbage buffer would give
/// a different checksum, which the cell logs for the reader).
fn pattern(i: usize) -> u32 {
    (i as u32).wrapping_mul(0x9E37_79B1) ^ 0x1234_5678
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    OK_CODE
}

async fn run() {
    let Some(mut sc) = Scanout::new(W, H) else {
        CODE.store(10, Ordering::Relaxed);
        return;
    };
    {
        let px = sc.pixels_mut();
        for (i, p) in px.iter_mut().enumerate() {
            *p = pattern(i);
        }
    }
    let sum = sc.checksum();
    match sc.present().await {
        Ok(bytes) => {
            println!("librheo-gpu: presented {W}x{H} frame ({bytes} B), checksum {sum:#010x}");
        }
        Err(()) => {
            println!("librheo-gpu: GPU present failed");
            CODE.store(11, Ordering::Relaxed);
        }
    }
}
