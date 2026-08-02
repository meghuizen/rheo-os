//! In-QEMU test kernel for the tile BATTLE TIER (docs/TILES.md 10): Loads the
//! `librheo-tilebattle` ELF into a cell with a real mapped queue pair + a minted
//! QueuePair capability and asserts it exits `0x42` - reached only if the
//! whole tile surface passed: TileBuf + checked views, the strand-parallel
//! tiled int8 GEMM asserted bit-exact against an independent naive reference,
//! TileSim's closed-form deterministic counts, the CPU TileContract + the
//! autotune key, copy/requantize/reduce receipts, and the f32<->i8
//! quantization round-trip bound. With the kernel slice (graph ops 4/5) the
//! cell also lowers the same program to the CPU engine and asserts receipt
//! equality across executors. Wiring mirrors `librheocompute`.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/librheo-tilebattle"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/librheo-tilebattle"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/librheo-tilebattle"
));

const EXPECTED_EXIT: u64 = 0x42;

fn c_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        -9
    }
}
fn c_stub_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -38
}
fn c_stub_close(_fd: u64) -> i64 {
    -38
}
fn c_stub_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -38
}
fn c_stub_lseek(_fd: u64, _o: i64, _w: u64) -> i64 {
    -38
}
fn c_stub_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("librheotilebattle: start on {}", arch::NAME);

    svc::init();
    svc::set_file_ops(FileOps {
        open: c_stub_open,
        close: c_stub_close,
        read: c_stub_read,
        write: c_write,
        lseek: c_stub_lseek,
        stat: c_stub_stat,
        fstat: c_stub_fstat,
        getdents: c_stub_getdents,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(DEMO, &mut aspace).expect("load librheo-tilebattle ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);
    println!(
        "librheotilebattle: loaded librheo-tilebattle ({} bytes), entry {entry:#x}",
        DEMO.len()
    );

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let object = objects.create(ObjectKind::QueuePair).unwrap();
        let cap = caps
            .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
        let cap_id = cap.raw_low32();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();

        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, cap_id);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-tilebattle exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!("librheotilebattle: tile framework proof, exit {code:#x} OK");

            // The per-cell grant table has an inline half and funds the rest from the
            // cell's own budget (docs/EXECUTION-MODEL.md 9.6). It was a fixed
            // `[[GrantSlot; 64]; MAX_CELLS]` - 24,576 bytes of `.bss`, already raised
            // 16 -> 64 once for this very kernel, with docs/TILES.md 12 recording
            // "whether 64 suffices for the largest real cell is an open sizing
            // question". It is not a sizing question now, and this is the measurement
            // that says so rather than the absence of a failure: the cell held twelve
            // grants at once, past the inline half, so the table had to grow.
            //
            // Asserted on the **kernel** side because a cell cannot see a frame. A
            // growth count of 0 would mean the inline half absorbed everything and the
            // funded path never ran - which is exactly what happens if `GRANTS_INLINE`
            // is quietly raised back to the old ceiling, and is the control.
            let growths = user::grant_growths();
            assert!(
                growths > 0,
                "the grant table never grew past its inline half - the cell held 12 \
                 grants at once, so either the inline half is back to a fixed ceiling \
                 or the funded path is unreachable"
            );
            // And the frames go back with the cell. A funded table whose slot-handback
            // path is not also a release path leaks until the next boot (the S1' scar,
            // found twice).
            user::free_cell(0);
            assert_eq!(
                user::grant_frames(0),
                0,
                "the grant table kept its funded frames after the cell was freed"
            );
            println!(
                "librheotilebattle: THE GRANT TABLE HAS NO CEILING - 12 grants held at \
                 once past the {} inline slots, {growths} growth(s) into frames charged \
                 to the cell's own budget, all returned when the cell was freed. It was \
                 a fixed 64-slot array (24,576 bytes of .bss) raised 16 -> 64 for this \
                 kernel, with the sizing left open in docs/TILES.md 12 OK",
                user::grants_inline()
            );
        }
        Outcome::Faulted(addr) => panic!("librheo-tilebattle faulted at {addr:#x}"),
    }

    println!("librheotilebattle: PASS");
    arch::exit(arch::ExitCode::Success)
}
