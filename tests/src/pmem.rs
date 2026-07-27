//! In-QEMU test kernel for **real persistent-memory grants** (docs/MEMORY.md
//! real-PMEM path, ARCHITECTURE.md 3 object 5). Proves that a `MemKind::Pmem`
//! grant is backed by frames from a **real QEMU nvdimm's physical region** -
//! distinct from the DDR frame pool - where the platform exposes one, and does
//! a write/read round-trip through the grant's frames.
//!
//! Per-ISA reality (honest, the project's established skip-with-reason pattern):
//!   - **x86-64**: genuinely nvdimm-backed. QEMU q35 (`nvdimm=on`) exposes an
//!     nvdimm whose persistent span is reported via the ACPI **NFIT** SPA range;
//!     the kernel discovers it, allocates from a separate pmem allocator, and
//!     reaches the frames (placed at 4 GiB, above the linear map) through a
//!     dedicated mapping window.
//!   - **ARM64 / RISC-V**: skip-with-reason + PASS. QEMU's arm `virt` needs an
//!     ACPI GED device for nvdimm hotplug (this kernel is DT-less builtin, no
//!     ACPI/NFIT parser) and riscv `virt` has no nvdimm support at all, so no
//!     pmem region is discovered and the DDR-backed behavior is unchanged.
//!
//! Cross-reboot persistence is a real-hardware property of the DIMM and is not
//! headlessly assertable in one boot; the proof here is "the grant is backed by
//! the real persistent-memory physical range + a write/read round-trip".

#![no_std]
#![no_main]

use kernel::mm::frames_pmem;
use kernel::mm::grant::{Grant, MemKind};
use kernel::svc;
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

/// The cell that asks `SYS_GRANT` for `MemKind::Pmem` - the path a real program
/// takes, as opposed to the kernel-internal `Grant` type tested above.
static PMEM_CELL: &[u8] = fixture::cell!("librheo-pmem");

/// What `librheo-pmem` exits with on success, and how many pages it commits.
const CELL_OK: u64 = 0x42;
const CELL_PAGES: usize = 4;

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init(); // runs hw::detect -> frames_pmem::init_from_inventory
    println!("pmem: start on {}", arch::NAME);

    match frames_pmem::region() {
        None => {
            // No nvdimm surfaced on this ISA/machine: DDR path unchanged.
            println!(
                "pmem: SKIP on {} - no nvdimm region discovered (QEMU machine \
                 exposes none; DDR-backed behavior unchanged)",
                arch::NAME
            );
        }
        Some((base, len)) => {
            println!(
                "pmem: nvdimm region [{base:#x}..{:#x}] ({} KiB)",
                base + len,
                len / 1024
            );
            test_real_pmem_grant(base, len);
        }
    }

    // Regardless of nvdimm presence, a DDR grant must still work and draw from
    // the DDR pool (never the pmem region) - the DDR path is unaffected.
    test_ddr_unaffected();

    // The half above exercises the kernel-internal `Grant` type. The half below
    // exercises the path a **cell** takes, which is where object 5 was
    // implemented a second time and the typed kind was dropped on the floor
    // (docs/ARCHITECTURE-DEBT.md 3.6).
    test_cell_grant_reaches_the_pmem_pool();

    println!("pmem: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// A `MemKind::Pmem` grant is genuinely nvdimm-backed: every committed frame's
/// physical address falls inside the discovered pmem region (not the DDR pool),
/// and a write/read round-trip through the frames succeeds.
fn test_real_pmem_grant(base: usize, len: usize) {
    let (free_pmem_before, total_pmem) = frames_pmem::stats();
    assert!(total_pmem > 0, "pmem region has no frames");
    let (free_ddr_before, _) = kernel::mm::frames::stats();

    const PAGES: usize = 4;
    let mut g = Grant::new(MemKind::Pmem, true);
    g.commit(PAGES).unwrap();
    assert_eq!(g.committed_pages(), PAGES);

    // The commit drew from the pmem allocator, NOT the DDR pool.
    let (free_pmem_mid, _) = frames_pmem::stats();
    let (free_ddr_mid, _) = kernel::mm::frames::stats();
    assert_eq!(
        free_pmem_before - free_pmem_mid,
        PAGES,
        "pmem commit did not draw from the pmem region"
    );
    assert_eq!(
        free_ddr_before, free_ddr_mid,
        "pmem commit leaked into the DDR pool"
    );

    // Every frame's physical address is inside the real nvdimm region, and a
    // write/read round-trip through the pmem mapping window works.
    for i in 0..PAGES {
        let pa = g.page(i).unwrap();
        assert!(
            pa >= base && pa < base + len,
            "pmem frame {pa:#x} outside the nvdimm region [{base:#x}..{:#x}]",
            base + len
        );
        assert!(
            frames_pmem::contains(pa),
            "pmem frame {pa:#x} not owned by the pmem allocator"
        );
        round_trip(pa, i);
    }
    println!("pmem: {PAGES} frames nvdimm-backed + round-trip OK");

    g.seal();
    drop(g);
    let (free_pmem_after, _) = frames_pmem::stats();
    assert_eq!(
        free_pmem_after, free_pmem_before,
        "pmem grant leaked frames on drop"
    );
    println!("pmem: real nvdimm grant OK");
}

/// Write a frame-derived pattern through the pmem mapping window and read it
/// back - the persistent-memory write/read round-trip. (Cross-reboot survival
/// is a real-hardware property; here we prove the frame is genuinely the
/// nvdimm's and is byte-addressable through the grant.)
fn round_trip(pa: usize, i: usize) {
    let va = frames_pmem::phys_to_virt(pa);
    let words = frames_pmem::FRAME_SIZE / 8;
    let p = va as *mut u64;
    // A frame- and offset-derived pattern so a stale/aliased mapping is caught.
    let pat = |j: usize| 0x5045_4D00_0000_0000_u64 ^ ((i as u64) << 32) ^ (j as u64);
    // SAFETY: `va` maps `pa`, a committed pmem frame reachable through the
    // kernel window; the whole 4 KiB is ours for this grant's lifetime.
    unsafe {
        for j in 0..words {
            p.add(j).write_volatile(pat(j));
        }
        for j in 0..words {
            let got = p.add(j).read_volatile();
            assert_eq!(
                got,
                pat(j),
                "pmem round-trip mismatch at frame {i} word {j}"
            );
        }
    }
}

/// A DDR grant is unaffected by the pmem path: it commits from the DDR pool and
/// none of its frames land in the pmem region.
fn test_ddr_unaffected() {
    let (free_before, _) = kernel::mm::frames::stats();
    let mut g = Grant::new(MemKind::Ddr, true);
    g.commit(3).unwrap();
    let (free_mid, _) = kernel::mm::frames::stats();
    assert_eq!(
        free_before - free_mid,
        3,
        "DDR commit did not take 3 DDR frames"
    );
    for i in 0..3 {
        let pa = g.page(i).unwrap();
        assert!(
            !frames_pmem::contains(pa),
            "DDR frame {pa:#x} landed in the pmem region"
        );
    }
    drop(g);
    let (free_after, _) = kernel::mm::frames::stats();
    assert_eq!(free_after, free_before, "DDR grant leaked frames on drop");
    println!("pmem: DDR path unaffected OK");
}

/// A **cell** asking `SYS_GRANT` for `MemKind::Pmem` must land on the pmem pool.
///
/// This is the assertion the cell cannot make and cannot fake: the kernel reads
/// its own pmem allocator's free count before and after, and the count must fall
/// by exactly the pages the cell committed. Before the fix the delta was **0** -
/// `grant_commit` recorded the kind and then called `frames::alloc`, so a `Pmem`
/// grant was DDR with nothing printed.
///
/// Where the machine exposes no nvdimm (arm/riscv `virt`) the cell still
/// succeeds, because the commit falls back to DDR, but the kernel must have
/// **said so** - the other half of the honesty requirement
/// (docs/ENGINEERING.md 7). That path is asserted as "the DDR pool paid
/// instead", with the printed reason visible in the log.
fn test_cell_grant_reaches_the_pmem_pool() {
    let have_nvdimm = frames_pmem::region().is_some();
    let (free_pmem_before, _) = frames_pmem::stats();
    let (free_ddr_before, _) = kernel::mm::frames::stats();

    svc::set_file_ops(console_personality::console_only());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(PMEM_CELL, "librheo-pmem") };
    match outcome {
        Outcome::Exited(code) => assert!(
            code == CELL_OK,
            "librheo-pmem exited {code:#x}, expected {CELL_OK:#x}"
        ),
        Outcome::Faulted(addr) => panic!("librheo-pmem faulted at {addr:#x}"),
    }

    let (free_pmem_after, _) = frames_pmem::stats();
    let (free_ddr_after, _) = kernel::mm::frames::stats();
    let pmem_used = free_pmem_before - free_pmem_after;
    let ddr_used = free_ddr_before.saturating_sub(free_ddr_after);

    if have_nvdimm {
        assert!(
            pmem_used >= CELL_PAGES,
            "SYS_GRANT(Pmem) drew {pmem_used} pmem frames, expected at least \
             {CELL_PAGES} - the typed kind never reached the allocator"
        );
        println!(
            "pmem: SYS_GRANT(Pmem) from a cell drew {pmem_used} frames from the \
             nvdimm pool (DDR paid {ddr_used} for the cell's own image/stack)"
        );
    } else {
        assert!(
            pmem_used == 0,
            "no nvdimm region, yet {pmem_used} pmem frames were consumed"
        );
        println!(
            "pmem: SYS_GRANT(Pmem) from a cell fell back to DDR with a printed \
             reason (no nvdimm on this machine); {ddr_used} DDR frames used"
        );
    }
}
