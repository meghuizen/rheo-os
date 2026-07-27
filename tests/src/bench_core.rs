//! In-QEMU benchmark kernel: "the three numbers" a control-plane kernel
//! lives or dies on (docs/IO.md 7) - P1 grant check, P2 queue round trip,
//! P3 context switch (docs/ARCHITECTURE.md 8.4).
//!
//! Methodology: run under `cargo xtask bench`, which boots QEMU with
//! `-icount shift=0,align=off,sleep=off`. In that mode the guest counters
//! advance deterministically with executed instructions, so results are
//! *instruction path lengths*, not wall-clock time. That is the only
//! honest microbenchmark QEMU can produce (docs/TOOLING.md 4: absolute
//! performance gates only on the hardware lab; QEMU tracks correctness
//! and trends). The calibration loop measures the tick:instruction ratio
//! per ISA instead of assuming it.
//!
//! Output lines are machine-readable:
//!   BENCH <name> ops=<n> ticks=<total> per_op_milliticks=<avg*1000>

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::abi::{WORKLOAD_CROSSCELL, WORKLOAD_ROUNDTRIP, WORKLOAD_SYSCALL};
use kernel::arch::{self, Context};
use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::cell::Cell;
use kernel::println;
use kernel::queue::{self, OP_NOP, QueuePair, RING_DEPTH, SqEntry};
use kernel::rng::Drbg;
use kernel::user;
use kernel::user_progs::{user_pong, user_worker};

const CAL_ITERS: u64 = 100_000;
const BATCHES: usize = 32;
const BATCH_OPS: usize = 1024;

/// The shared queue-pair region (header + SQ + CQ), page-aligned.
#[repr(C, align(4096))]
struct Region([u8; QueuePair::REGION_SIZE]);
static mut REGION: Region = Region([0; QueuePair::REGION_SIZE]);

static mut MAIN_CTX: Context = Context { sp: 0 };
static mut WORKER_CTX: Context = Context { sp: 0 };
static mut WORKER_STACK: [u8; 16 * 1024] = [0; 16 * 1024];

// RNG bench buffers (kept off the boot stack).
static mut RNG_DRBG: Drbg = Drbg::ZERO;
static mut RNG_BUF32: [u8; 32] = [0; 32];
static mut RNG_KBUF: [u8; 1024] = [0; 1024];

// Heap for the strand-runtime (P4) bench: spawn/join allocate.
#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 512 * 1024] = [0; 512 * 1024];

// The canonical tile kernels (docs/TILES.md) - the same source include the
// kernel engine and librheo executor use, so the P5 path lengths measure
// exactly the shipped code.
#[path = "../../librheo/src/tile/kernels.rs"]
#[allow(dead_code)]
mod tile_kernels;

// P5 tile-bench buffers (off the boot stack, like RNG_KBUF). 64x64 is the
// largest GEMM the benches use; A/B are i8, C is i32.
static mut TA: [i8; 64 * 64] = [0; 64 * 64];
static mut TB: [i8; 64 * 64] = [0; 64 * 64];
static mut TC: [i32; 64 * 64] = [0; 64 * 64];
static mut TRED: [i32; 1024] = [0; 1024];
static mut TQF: [f32; 1024] = [0.0; 1024];
static mut TQI: [i8; 1024] = [0; 1024];
static mut TQS: [f32; 1024 / 32] = [0.0; 1024 / 32];

fn report(name: &str, ops: u64, ticks: u64) {
    let milli = ticks * 1000 / ops;
    println!("BENCH {name} ops={ops} ticks={ticks} per_op_milliticks={milli}");
}

/// Run `body` BATCHES times over BATCH_OPS ops and report the *best*
/// batch (the least-disturbed run; under icount runs are near-identical).
fn bench<F: FnMut()>(name: &str, mut body: F) {
    let mut best = u64::MAX;
    for _ in 0..BATCHES {
        let start = arch::cycles();
        for _ in 0..BATCH_OPS {
            body();
        }
        let elapsed = arch::cycles() - start;
        if elapsed < best {
            best = elapsed;
        }
    }
    report(name, BATCH_OPS as u64, best);
}

extern "C" fn worker_entry() -> ! {
    loop {
        unsafe {
            let worker = &mut *core::ptr::addr_of_mut!(WORKER_CTX);
            let main = &*core::ptr::addr_of!(MAIN_CTX);
            arch::context_switch(worker, main);
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("bench-core: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, 512 * 1024);
    }

    // Calibration: a known loop of exactly 2 instructions per iteration.
    // ticks_per_kilo_insn ~= 1000 means 1 tick = 1 instruction (x86 tsc,
    // riscv rdcycle under icount); ~16000 means 16 insns/tick (aarch64
    // cntvct at 62.5 MHz vs 1 GHz virtual clock). The xtask report uses
    // this line to normalise everything to instructions.
    let mut cal_best = u64::MAX;
    for _ in 0..4 {
        let start = arch::cycles();
        arch::spin_loop(CAL_ITERS);
        let elapsed = arch::cycles() - start;
        if elapsed < cal_best {
            cal_best = elapsed;
        }
    }
    println!(
        "CALIB spin_insns={} ticks={cal_best} ticks_per_kilo_insn={}",
        2 * CAL_ITERS,
        cal_best * 1000 / (2 * CAL_ITERS)
    );

    // Counter read overhead (subtracted by the analyst, not hidden here).
    let mut overhead = u64::MAX;
    for _ in 0..16 {
        let start = arch::cycles();
        let delta = arch::cycles() - start;
        if delta < overhead {
            overhead = delta;
        }
    }
    println!("CALIB counter_read_ticks={overhead}");

    // ---------------------------------------------------------------- P1
    let mut objects = ObjectTable::new();
    let mut cell = Cell::new(1);
    let object = objects.create(ObjectKind::MemoryGrant).unwrap();
    let cap = cell
        .caps
        .mint(&objects, object, READ | WRITE, BUDGET_UNLIMITED)
        .unwrap();

    bench("p1_grant_check", || {
        let result = cell.caps.grant_check(&objects, cap, READ);
        core::hint::black_box(&result);
    });

    let abi_cap = cap.raw_low32();
    bench("p1_grant_check_abi32", || {
        let result = cell.caps.grant_check_low32(&objects, abi_cap, READ);
        core::hint::black_box(&result);
    });

    // The deny path must not be slower than the grant path (it is the
    // DDoS-relevant one): check a revoked capability.
    let revoked_obj = objects.create(ObjectKind::MemoryGrant).unwrap();
    let revoked = cell
        .caps
        .mint(&objects, revoked_obj, READ, BUDGET_UNLIMITED)
        .unwrap();
    objects.revoke_epoch(revoked_obj);
    bench("p1_grant_check_revoked", || {
        let result = cell.caps.grant_check(&objects, revoked, READ);
        core::hint::black_box(&result);
    });

    // ---------------------------------------------------------------- P2
    let qp = unsafe { QueuePair::init(core::ptr::addr_of_mut!(REGION) as *mut u8) };

    // Ring transport alone (no kernel work): push one, pop one.
    bench("p2_ring_push_pop", || {
        qp.sq.push(SqEntry::new(OP_NOP, cap, 1, 1));
        core::hint::black_box(qp.sq.pop());
    });

    // Doorbell floor: one privilege-boundary round trip, nothing else.
    bench("p2_doorbell_trap", arch::doorbell_trap);

    // Single-message round trip: submit -> doorbell -> kernel validates
    // and completes -> reap. This is the number IO.md 6.1 says to compare
    // against seL4's Call+ReplyRecv, because it is where the ring+doorbell
    // trade-off is visible.
    bench("p2_roundtrip_single", || {
        qp.sq.push(SqEntry::new(OP_NOP, cap, 2, 2));
        arch::doorbell_trap();
        queue::kernel_process(&qp, &mut cell.caps, &objects);
        core::hint::black_box(qp.cq.pop());
    });

    // Batched round trip: RING_DEPTH submissions, one doorbell
    // (doorbell coalescing, docs/IO.md 1) - the amortized P2 target.
    let mut batch_best = u64::MAX;
    for _ in 0..BATCHES {
        let start = arch::cycles();
        for _ in 0..BATCH_OPS / RING_DEPTH {
            for i in 0..RING_DEPTH {
                qp.sq.push(SqEntry::new(OP_NOP, cap, i as u128, i as u64));
            }
            arch::doorbell_trap();
            queue::kernel_process(&qp, &mut cell.caps, &objects);
            for _ in 0..RING_DEPTH {
                core::hint::black_box(qp.cq.pop());
            }
        }
        let elapsed = arch::cycles() - start;
        if elapsed < batch_best {
            batch_best = elapsed;
        }
    }
    report("p2_roundtrip_batched64", BATCH_OPS as u64, batch_best);

    // ---------------------------------------------------------------- P3
    unsafe {
        let stack_top = core::ptr::addr_of_mut!(WORKER_STACK)
            .cast::<u8>()
            .add(16 * 1024);
        *core::ptr::addr_of_mut!(WORKER_CTX) = arch::context_init(stack_top, worker_entry);
    }
    // Each iteration is two switches: main -> worker -> main.
    let mut switch_best = u64::MAX;
    for _ in 0..BATCHES {
        let start = arch::cycles();
        for _ in 0..BATCH_OPS {
            unsafe {
                let main = &mut *core::ptr::addr_of_mut!(MAIN_CTX);
                let worker = &*core::ptr::addr_of!(WORKER_CTX);
                arch::context_switch(main, worker);
            }
        }
        let elapsed = arch::cycles() - start;
        if elapsed < switch_best {
            switch_best = elapsed;
        }
    }
    report("p3_context_switch", 2 * BATCH_OPS as u64, switch_best);

    // -------------------------------------------------- user-mode P2 / P5
    // The numbers that actually answer the seL4 comparison: work measured
    // from U-mode, across the real syscall boundary, and (for P5) across a
    // real address-space switch each way. Timing is done by the U-mode
    // worker itself with rdcycle; the kernel reads it back from the shared
    // params page.
    user_mode_benches();

    // ------------------------------------------------------------- P-RNG
    // Cryptographic RNG path length (ChaCha20 DRBG, fast key erasure). The
    // draw is a plain function call over the DRBG's own state - the per-cell
    // "library call, not a syscall" model (TIME-IDENTITY.md 4). Divide the
    // normalised instruction count by the byte count for bytes/instruction;
    // rng_fill_1KiB is steady-state throughput, the u64/32B lines are the
    // small-draw latency (nonce/key sized). Buffers live in statics so the
    // 1 KiB output buffer does not overflow the kernel boot stack.
    let drbg = unsafe { &mut *core::ptr::addr_of_mut!(RNG_DRBG) };
    *drbg = Drbg::from_seed(0x0BADC0DE_CAFEF00D);
    bench("rng_next_u64", || {
        core::hint::black_box(drbg.next_u64());
    });
    let buf32 = unsafe { &mut *core::ptr::addr_of_mut!(RNG_BUF32) };
    bench("rng_fill_32B", || {
        drbg.fill_bytes(buf32);
        core::hint::black_box(&buf32);
    });
    let kbuf = unsafe { &mut *core::ptr::addr_of_mut!(RNG_KBUF) };
    bench("rng_fill_1KiB", || {
        drbg.fill_bytes(kbuf);
        core::hint::black_box(&kbuf);
    });
    // The syscall path an OS with a getrandom-style syscall pays on every
    // draw: the same DRBG work plus one privilege round trip. Subtract
    // rng_next_u64 from this to see the boundary cost the per-cell library-
    // call model avoids.
    bench("rng_u64_plus_doorbell", || {
        core::hint::black_box(drbg.next_u64());
        arch::doorbell_trap();
    });

    // ------------------------------------------------------------- P4
    // Strand spawn/teardown and context switch (docs/CONCURRENCY.md,
    // BUILD-ORDER step 7). These are the "light thread" path lengths: a
    // strand is a slab slot + a boxed state machine, and a switch is a
    // cooperative yield - no syscall, no kernel stack. The host comparison
    // (comparison/threads/) puts real ns on these vs Linux/Go/Python.
    const STRANDS: usize = 256;
    let mut spawn_best = u64::MAX;
    for _ in 0..BATCHES {
        runtime::reset();
        let start = arch::cycles();
        for _ in 0..STRANDS {
            let _ = runtime::spawn(async {});
        }
        runtime::run();
        let elapsed = arch::cycles() - start;
        if elapsed < spawn_best {
            spawn_best = elapsed;
        }
    }
    report("p4_strand_spawn_teardown", STRANDS as u64, spawn_best);

    let mut switch_best = u64::MAX;
    for _ in 0..BATCHES {
        runtime::reset();
        let _ = runtime::spawn(async {
            for _ in 0..STRANDS {
                runtime::yield_now().await;
            }
        });
        let _ = runtime::spawn(async {
            for _ in 0..STRANDS {
                runtime::yield_now().await;
            }
        });
        let start = arch::cycles();
        runtime::run();
        let elapsed = arch::cycles() - start;
        if elapsed < switch_best {
            switch_best = elapsed;
        }
    }
    report("p4_strand_switch", 2 * STRANDS as u64, switch_best);

    // ---------------------------------------------------------------- P5
    // Tile-op path lengths (docs/TILES.md). Custom loops with controlled op
    // counts: a 64^3 GEMM is 262k MACs, far too heavy for the 1024x32
    // `bench()` loop under icount. Each reports per-op instruction length.
    bench_p5();

    println!("bench-core: DONE");
    arch::exit(arch::ExitCode::Success)
}

/// Best of `BATCHES` runs of `body`, reported as `ops` operations.
fn measure(name: &str, ops: u64, iters: usize, mut body: impl FnMut()) {
    let mut best = u64::MAX;
    for _ in 0..BATCHES {
        let start = arch::cycles();
        for _ in 0..iters {
            body();
        }
        let elapsed = arch::cycles() - start;
        if elapsed < best {
            best = elapsed;
        }
    }
    report(name, ops, best);
}

/// One 64^3 int8 GEMM tiled at `block`, calling the canonical kernel per
/// (m,n,k) block - the same loop the executors run, isolated for icount.
fn tiled_gemm64(block: usize) {
    let (a, b, c) = (
        core::ptr::addr_of!(TA) as *const i8,
        core::ptr::addr_of!(TB) as *const i8,
        core::ptr::addr_of_mut!(TC) as *mut i32,
    );
    // Zero C.
    for i in 0..64 * 64 {
        unsafe { *c.add(i) = 0 };
    }
    let mut i0 = 0;
    while i0 < 64 {
        let bm = block.min(64 - i0);
        let mut j0 = 0;
        while j0 < 64 {
            let bn = block.min(64 - j0);
            let mut p0 = 0;
            while p0 < 64 {
                let bk = block.min(64 - p0);
                // SAFETY: block stays inside the 64x64 statics.
                unsafe {
                    tile_kernels::gemm_i8_i32(
                        a.add(i0 * 64 + p0),
                        64,
                        b.add(p0 * 64 + j0),
                        64,
                        c.add(i0 * 64 + j0),
                        64,
                        bm,
                        bn,
                        bk,
                    );
                }
                p0 += block;
            }
            j0 += block;
        }
        i0 += block;
    }
}

fn bench_p5() {
    // Deterministic fills.
    unsafe {
        for i in 0..64 * 64 {
            TA[i] = ((i * 31 + 7) & 0x7F) as i8;
            TB[i] = ((i * 17 + 3) & 0x7F) as i8;
        }
        for i in 0..1024 {
            TRED[i] = i as i32;
            TQF[i] = (i as f32) * 0.1 - 50.0;
        }
    }

    // One 16^3 tile (4096 MACs), the executor's inner block.
    measure("p6_tile_gemm_i8_16", 256, 256, || {
        // SAFETY: 16x16 stays inside the 64x64 statics.
        unsafe {
            tile_kernels::gemm_i8_i32(
                core::ptr::addr_of!(TA) as *const i8,
                64,
                core::ptr::addr_of!(TB) as *const i8,
                64,
                core::ptr::addr_of_mut!(TC) as *mut i32,
                64,
                16,
                16,
                16,
            );
        }
    });

    // Reduce over 4 KiB (1024 i32).
    measure("p6_tile_reduce_4kib", 1024, 1024, || {
        // SAFETY: exactly 1024 i32 in TRED.
        let s = unsafe {
            tile_kernels::reduce_wrapping(core::ptr::addr_of!(TRED) as *const u8, 1024, 2)
        };
        core::hint::black_box(s);
    });

    // Quantize 4 KiB f32 -> i8, block 32.
    measure("p6_tile_quant_4kib", 256, 256, || {
        // SAFETY: TQF/TQI are 1024 elems, TQS is 32 scales.
        unsafe {
            tile_kernels::quant_f32_i8(
                core::ptr::addr_of!(TQF) as *const f32,
                core::ptr::addr_of_mut!(TQI) as *mut i8,
                core::ptr::addr_of_mut!(TQS) as *mut f32,
                1024,
                32,
            );
            // Observe the output so the quantize is not elided.
            core::hint::black_box(TQI[0]);
            core::hint::black_box(TQS[0]);
        }
    });

    // The full svc::graph_submit path for one 32^3 TileGemm node - the
    // kernel engine's tile execution measured end to end (validate +
    // dispatch + FNV receipt), callable directly in kernel context.
    let desc = kernel::abi::TileGemmDesc {
        a_va: core::ptr::addr_of!(TA) as u64,
        b_va: core::ptr::addr_of!(TB) as u64,
        c_va: core::ptr::addr_of_mut!(TC) as u64,
        m: 32,
        n: 32,
        k: 32,
        a_stride: 64,
        b_stride: 64,
        c_stride: 64,
        dtype_in: 0,
        dtype_acc: 2,
    };
    let node = kernel::abi::GraphNode {
        op: 5,
        a_is_node: 0,
        b_is_node: 0,
        _pad: 0,
        a: &desc as *const kernel::abi::TileGemmDesc as u64,
        b: 0,
    };
    let mut result = [0u64; 1];
    measure("p6_graph_tilegemm_32", 64, 64, || {
        let r = kernel::svc::graph_submit(
            &node as *const kernel::abi::GraphNode as u64,
            1,
            result.as_mut_ptr() as u64,
        );
        core::hint::black_box(r);
    });

    // The same 64^3 GEMM at three tilings - the TileSim op-count leg. The
    // MAC count is identical (262144), but finer tiling runs more tile trips
    // (block8: 8^3=512, block16: 4^3=64, block32: 2^3=8), and each trip
    // carries call + zero + loop overhead. So under icount the path length
    // DECREASES as the block grows - and it ranks the tilings in the SAME
    // order as TileSim's `tile_trips` (block32 < block16 < block8), which is
    // the op-count leg validating the sim's overhead model (docs/TILES.md 7).
    // The RESULT is tiling-invariant (proven in `librheotile`); the COST is
    // not, and the sim predicts the cost ordering.
    measure("p6_gemm64_block8", 1, 4, || tiled_gemm64(8));
    measure("p6_gemm64_block16", 1, 4, || tiled_gemm64(16));
    measure("p6_gemm64_block32", 1, 4, || tiled_gemm64(32));
}

const USER_ITERS: u64 = 2048;

#[unsafe(link_section = ".user.bss")]
static mut STORE0: CellStore = CellStore::new();
#[unsafe(link_section = ".user.bss")]
static mut STORE1: CellStore = CellStore::new();
static mut KSTACK: KernelStack = KernelStack::new();
static mut U_OBJECTS: ObjectTable = ObjectTable::new();
static mut U_CAPS0: CapTable = CapTable::new();
static mut U_CAPS1: CapTable = CapTable::new();

/// Run one single-cell user workload and return (ticks, ops) the worker
/// measured across the boundary.
fn run_single(workload: u64) -> (u64, u64) {
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(U_OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(U_CAPS0);
        let store = core::ptr::addr_of_mut!(STORE0);
        let ksp = (*core::ptr::addr_of!(KSTACK)).top();

        let (aspace, _obj, mut frame) = build_cell(
            &mut *store,
            objects,
            caps,
            ksp,
            1,
            user_worker,
            workload,
            USER_ITERS,
        );
        let qp = (*store).qp.qp.as_ptr();

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            qp,
            core::ptr::addr_of_mut!(frame),
        );
        user::run(0);

        let p = &(*store).params;
        (p.ticks, p.ops)
    }
}

/// Run the cross-cell workload: cell 0 (client) switches to cell 1 (a pong
/// peer) and back, USER_ITERS times. One iteration = one round trip = two
/// address-space switches (directly comparable to seL4 Call + ReplyRecv).
fn run_crosscell() -> (u64, u64) {
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(U_OBJECTS);
        let caps0 = &mut *core::ptr::addr_of_mut!(U_CAPS0);
        let caps1 = &mut *core::ptr::addr_of_mut!(U_CAPS1);
        let s0 = core::ptr::addr_of_mut!(STORE0);
        let s1 = core::ptr::addr_of_mut!(STORE1);
        let ksp = (*core::ptr::addr_of!(KSTACK)).top();

        let (aspace0, _o0, mut frame0) = build_cell(
            &mut *s0,
            objects,
            caps0,
            ksp,
            1,
            user_worker,
            WORKLOAD_CROSSCELL,
            USER_ITERS,
        );
        let (aspace1, _o1, mut frame1) =
            build_cell(&mut *s1, objects, caps1, ksp, 2, user_pong, 0, 0);
        let qp0 = (*s0).qp.qp.as_ptr();
        let qp1 = (*s1).qp.qp.as_ptr();

        user::reset();
        user::install(
            0,
            &aspace0,
            caps0,
            objects,
            qp0,
            core::ptr::addr_of_mut!(frame0),
        );
        user::install(
            1,
            &aspace1,
            caps1,
            objects,
            qp1,
            core::ptr::addr_of_mut!(frame1),
        );
        user::run(0);

        let p = &(*s0).params;
        (p.ticks, p.ops)
    }
}

fn report_user(name: &str, ticks: u64, ops: u64) {
    report(name, ops, ticks);
}

fn user_mode_benches() {
    let (t, n) = run_single(WORKLOAD_SYSCALL);
    report_user("p2_user_syscall_floor", t, n);

    let (t, n) = run_single(WORKLOAD_ROUNDTRIP);
    report_user("p2_user_roundtrip", t, n);

    let (t, n) = run_crosscell();
    report_user("p5_crosscell_roundtrip", t, n);
}
