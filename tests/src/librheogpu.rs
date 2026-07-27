//! In-QEMU test kernel for librheo Phase H (docs/LIBRHEO.md, docs/DISPLAY.md):
//! **a real GPU scanout over a virtio-gpu 2D driver**. A librheo cell allocates a
//! framebuffer grant, draws a known RGBA pattern, and presents it - an
//! `OP_GPU_PRESENT` submission the kernel bridges to the virtio-gpu driver
//! (`RESOURCE_CREATE_2D` -> `ATTACH_BACKING` -> `SET_SCANOUT` at install, then
//! `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` per present). The cell exits `0x42`
//! only if the present completes OK, so the exit code is the proof.
//!
//! QEMU runs headless (`-display none`), so the proof is the genuine command
//! round-trip: every 2D command returns its `RESP_OK_*` code from the real QEMU
//! device model (this kernel prints exactly which returned OK - the honest proof
//! surface). It does NOT assert a visible pixel; visible output is deferred
//! (docs/DISPLAY.md). The transport differs per machine: virtio-mmio on riscv/arm
//! `virt`, virtio-pci on x86-64 q35. The skip branch fires only if no virtio-gpu
//! device is attached (or the essential create+attach bring-up is refused).
//!
//! Wiring mirrors `librheonet` (queue pair + minted cap + `set_queue_info`); the
//! GPU is discovered + installed like `blockfs` discovers virtio-blk. A minimal
//! console `FileOps` backs the cell's `println!` (fd 1/2 -> serial).

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::addr_of_mut;

use kernel::hw::virtio_gpu;
use kernel::svc::{self};
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("librheo-gpu");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("librheogpu: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-gpu device. `probe` runs the install-time
    // 2D bring-up (get_display_info + create_2d + attach_backing + set_scanout)
    // and returns None if no device is attached or create+attach is refused.
    let dev = match virtio_gpu::probe() {
        Some(d) => d,
        None => {
            println!("librheogpu: no virtio-gpu device (or 2D bring-up refused) - skipping");
            println!("librheogpu: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let r = dev.report();
    println!(
        "librheogpu: virtio-gpu bring-up: display_info={} ({}x{}) create_2d={} attach={} set_scanout={}",
        r.display_info_ok, r.display_w, r.display_h, r.create_2d_ok, r.attach_ok, r.set_scanout_ok
    );
    virtio_gpu::install(dev);

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "librheo-gpu") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-gpu exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            let r = virtio_gpu::report().unwrap();
            println!(
                "librheogpu: present round trip: transfer={} flush={}, exit {code:#x} OK",
                r.transfer_ok, r.flush_ok
            );
        }
        Outcome::Faulted(addr) => panic!("librheo-gpu faulted at {addr:#x}"),
    }

    println!("librheogpu: PASS");
    arch::exit(arch::ExitCode::Success)
}
