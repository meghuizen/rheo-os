//! `librheo-data` - the Phase B proof program (docs/LIBRHEO.md): a **mini-DuckDB
//! columnar scan** over a dataset read off the live virtio-blk disk. It
//! exercises the whole Phase B surface:
//!
//! - **typed memory grants** (`mem::Grant`): reserve a DDR grant, commit/write/
//!   read/decommit it, seal it immutable, request an emulated HBM grant, and
//!   confirm a device-BAR grant is refused.
//! - **async I/O** (`io::File`/`store::Dataset`): open the dataset, `fstat` it,
//!   and async-read the header - each an OP_* submission parked on a completion.
//! - **batched async I/O**: N strands async-read the partitions of column A into
//!   a grant concurrently; one doorbell drains all N completions (zero-copy read
//!   straight into the grant).
//! - **zero-copy mmap scan**: mmap the dataset (`mem::Mapping`), fan a columnar
//!   scan across N strands (each computes a partial SUM/COUNT/MAX under a
//!   predicate over mapped memory - no syscall per access), reduce, and assert
//!   the exact aggregate. The async-read column and the mmap column must agree.
//!
//! It exits `0x42` only if every stage passed and the aggregate is exact; the
//! `librheodata` test kernel asserts that code.
//!
//! Dataset layout (written to the disk by xtask, read back here): a 16-byte
//! header `[magic u32][nrows u32][ncols u32][reserved u32]` then column A
//! (`col_a[i] = i`) then column B (`col_b[i] = i & 1`), each `nrows` little-
//! endian u32. The query is `SUM(col_a), COUNT(*), MAX(col_a) WHERE col_b == 1`
//!   a closed form: for even `nrows`, COUNT = nrows/2, SUM = (nrows/2)^2,
//!   MAX = nrows-1.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use librheo::mem::{Grant, MemKind};
use librheo::store::Dataset;
use librheo::{println, rt};

/// The scan's failure code (0 = success), set inside the `'static` async root
/// and read after `block_on` (the executor needs a `'static` future, so the
/// result cannot be a borrow).
static SCAN_CODE: AtomicI32 = AtomicI32::new(0);

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// Dataset magic ("COL1" little-endian).
const MAGIC: u32 = 0x314C_4F43;
const HEADER: usize = 16;
/// Scan fan-out (strands / partitions).
const N: usize = 8;

/// Read a little-endian u32 from raw mapped memory at `base + off`.
#[inline(always)]
fn rd32(base: usize, off: usize) -> u32 {
    // SAFETY: `base + off` is within the live mmap (bounds enforced by callers).
    unsafe {
        let p = (base + off) as *const u8;
        u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
    }
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // ---------- 1. typed memory grants ----------
    // Reserve + commit a 2-page DDR grant; write a pattern and read it back.
    let Some(mut g) = Grant::alloc(MemKind::Ddr, 8192) else {
        return 10;
    };
    // SAFETY: the grant is fully committed and unsealed.
    let buf = unsafe { g.slice_mut(0, 8192) };
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i * 7) as u8;
    }
    for (i, &b) in buf.iter().enumerate() {
        if b != (i * 7) as u8 {
            return 11;
        }
    }
    // Decommit the second page, then re-commit it (demand paging).
    if g.decommit(4096, 4096).is_err() || g.commit(4096, 4096).is_err() {
        return 12;
    }
    // Seal it immutable: a further commit must now be refused.
    if g.seal().is_err() || g.commit(0, 4096).is_ok() {
        return 13;
    }
    // HBM is emulated on DDR (honest) - a reservation succeeds.
    if Grant::reserve(MemKind::Hbm, 4096).is_none() {
        return 14;
    }
    // Device-BAR has no backing here - the kernel refuses it.
    if Grant::reserve(MemKind::DeviceBar, 4096).is_some() {
        return 15;
    }

    // ---------- 2-4. async I/O + the zero-copy columnar scan ----------
    rt::block_on(scan());
    let code = SCAN_CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }

    println!("librheo-data: grants + async-io + zero-copy scan OK");
    OK_CODE
}

/// Record a scan failure code and stop (checked after `block_on`).
fn fail(code: i32) {
    SCAN_CODE.store(code, Ordering::Relaxed);
}

/// The async body: open the dataset, read its header, batched-read column A into
/// a grant, mmap it, fan out the scan, reduce, and assert the aggregate. On any
/// mismatch it records a non-zero code via [`fail`] and returns.
async fn scan() {
    let ds = match Dataset::open("/data.col").await {
        Ok(d) => d,
        Err(_) => {
            fail(20);
            return;
        }
    };
    let size = ds.len() as usize;
    if size < HEADER {
        fail(21);
        return;
    }

    // Async-read the 16-byte header into a committed grant (zero-copy).
    let Some(hg) = Grant::alloc(MemKind::Ddr, HEADER) else {
        fail(22);
        return;
    };
    if ds.file().read_into(&hg, 0, HEADER as u32, 0).await != Ok(HEADER as u32) {
        fail(23);
        return;
    }
    let base = hg.base();
    let magic = rd32(base, 0);
    let nrows = rd32(base, 4) as usize;
    let ncols = rd32(base, 8) as usize;
    if magic != MAGIC || ncols != 2 || nrows == 0 || !nrows.is_multiple_of(N) {
        fail(24);
        return;
    }
    let cola_off = HEADER;
    let colb_off = HEADER + nrows * 4;
    let part = nrows / N;

    // ---------- 3. batched async read of column A into a grant ----------
    // N strands async-read the N partitions of column A; one doorbell drains
    // all N completions (the batch), landing straight in the grant (zero-copy).
    let Some(ca) = Grant::alloc(MemKind::Ddr, nrows * 4) else {
        fail(25);
        return;
    };
    let ca_base = ca.base();
    let fd = ds.file().fd();
    let mut reads = Vec::new();
    for p in 0..N {
        let goff = p * part * 4;
        let foff = (cola_off + p * part * 4) as u64;
        let len = (part * 4) as u32;
        reads.push(rt::spawn(async move {
            let f = librheo::io::File::from_fd(fd);
            f.read_at((ca_base + goff) as u64, len, foff).await == Ok(len)
        }));
    }
    for h in reads {
        if !h.join().await {
            fail(26);
            return;
        }
    }

    // ---------- 4. zero-copy mmap scan ----------
    let Some(map) = ds.map_all() else {
        fail(27);
        return;
    };
    let mbase = map.base();

    // The async-read column A (in the grant) must match the mmap'd column A.
    for &i in &[0usize, 1, part, nrows - 1, nrows / 2] {
        if rd32(ca_base, i * 4) != rd32(mbase, cola_off + i * 4) {
            fail(28);
            return;
        }
    }

    // Fan the scan across N strands over mapped memory: each partition computes
    // a partial (sum, count, max) of col_a where col_b == 1.
    let mut parts = Vec::new();
    for p in 0..N {
        let lo = p * part;
        let hi = lo + part;
        parts.push(rt::spawn(async move {
            let mut sum = 0u64;
            let mut count = 0u64;
            let mut max = 0u32;
            for i in lo..hi {
                if rd32(mbase, colb_off + i * 4) == 1 {
                    let a = rd32(mbase, cola_off + i * 4);
                    sum += a as u64;
                    count += 1;
                    if a > max {
                        max = a;
                    }
                }
            }
            (sum, count, max)
        }));
    }
    // Reduce the partials.
    let mut sum = 0u64;
    let mut count = 0u64;
    let mut max = 0u32;
    for h in parts {
        let (s, c, m) = h.join().await;
        sum += s;
        count += c;
        if m > max {
            max = m;
        }
    }

    // Closed-form expected aggregate for `col_a[i]=i`, predicate `i & 1 == 1`.
    let half = (nrows / 2) as u64;
    let exp_sum = half * half;
    let exp_count = half;
    let exp_max = (nrows - 1) as u32;
    if sum != exp_sum || count != exp_count || max != exp_max {
        fail(29);
        return;
    }

    // Exercise the async write opcode (inline path: <= INLINE_MAX bytes ride in
    // the submission) - a console marker over OP_WRITE.
    if librheo::io::Stream::stdout()
        .write(b"[scan ok]\n")
        .await
        .is_err()
    {
        fail(30);
        return;
    }
    println!(
        "librheo-data: scanned {nrows} rows x2 cols, SUM={sum} COUNT={count} MAX={max} ({N} strands)"
    );
}
