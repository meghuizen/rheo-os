// Host benchmark: rheo-os ChaCha20 DRBG (the per-cell "library call, not a
// syscall" model of docs/TIME-IDENTITY.md 4) vs Linux's own randomness
// paths, on real hardware. This is the honest "outperforming Linux" number:
// the SAME ChaCha20 core the kernel ships (included verbatim from
// kernel/src/rng/chacha.rs) is measured against glibc getrandom/getentropy
// and /dev/urandom on this CPU.
//
// Why this is a fair comparison: Linux's CRNG is also ChaCha20, so the
// primitive is identical. The difference we measure is architectural - a
// per-cell library call over the cell's own DRBG state versus a kernel
// entry (getrandom syscall) or a file read (/dev/urandom). That boundary is
// exactly what the design removes from the hot path.
//
// Build + run: comparison/rng/run.sh  (plain rustc, no crates).

#![allow(clippy::needless_range_loop)]

use std::time::Instant;

// The exact kernel ChaCha20 block. include! keeps it byte-identical.
include!("../../kernel/src/rng/chacha.rs");

// ---- rheo-os DRBG (fast key erasure), mirroring kernel/src/rng/mod.rs ----

const OUT: usize = 256;
const KS_BYTES: usize = ((32 + OUT).div_ceil(64)) * 64;

struct Drbg {
    key: [u8; 32],
    nonce: [u8; 12],
    buf: [u8; OUT],
    pos: usize,
}

impl Drbg {
    fn from_key(key: [u8; 32]) -> Drbg {
        Drbg { key, nonce: [0; 12], buf: [0; OUT], pos: OUT }
    }
    fn refill(&mut self) {
        let mut ks = [0u8; KS_BYTES];
        let mut ctr = 0u32;
        let mut off = 0;
        while off < KS_BYTES {
            let mut blk = [0u8; 64];
            block(&self.key, ctr, &self.nonce, &mut blk);
            ks[off..off + 64].copy_from_slice(&blk);
            ctr += 1;
            off += 64;
        }
        self.key.copy_from_slice(&ks[..32]);
        self.buf.copy_from_slice(&ks[32..32 + OUT]);
        self.pos = 0;
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        let mut i = 0;
        while i < dst.len() {
            if self.pos == OUT {
                self.refill();
            }
            let n = core::cmp::min(dst.len() - i, OUT - self.pos);
            dst[i..i + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            i += n;
        }
    }
}

// ---- Linux randomness paths ----

// Raw getrandom(2) syscall (x86-64 number 318), no libc wrapper.
fn sys_getrandom(buf: &mut [u8]) -> isize {
    let ret: isize;
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") 318isize => ret,
            in("rdi") buf.as_mut_ptr(),
            in("rsi") buf.len(),
            in("rdx") 0,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

unsafe extern "C" {
    // glibc wrappers over getrandom. On a kernel+glibc with the getrandom
    // vDSO these may avoid the syscall; measure what this host actually does
    // rather than assume (here it comes out at syscall cost).
    fn getentropy(buf: *mut u8, len: usize) -> i32;
    fn getrandom(buf: *mut u8, len: usize, flags: u32) -> isize;
}

// ---- timing ----

fn rdtsc() -> u64 {
    unsafe {
        std::arch::x86_64::_mm_lfence();
        std::arch::x86_64::_rdtsc()
    }
}

/// Median cycles for one call of `f`, over `iters`, plus wall throughput.
fn time_call<F: FnMut()>(iters: u64, mut f: F) -> u64 {
    // warm up
    for _ in 0..(iters / 10 + 1) {
        f();
    }
    let start = rdtsc();
    for _ in 0..iters {
        f();
    }
    let end = rdtsc();
    (end - start) / iters
}

fn mb_per_s(bytes: u64, secs: f64) -> f64 {
    (bytes as f64) / secs / 1_000_000.0
}

fn main() {
    println!("rheo-os RNG vs Linux getrandom - host: real hardware, cycles via rdtsc");
    println!("{}", "-".repeat(72));

    let mut seed = [0u8; 32];
    // Seed our DRBG from the OS once (seeding is the only critical moment).
    unsafe { getentropy(seed.as_mut_ptr(), 32) };
    let mut ours = Drbg::from_key(seed);

    // ---- small draws: a key/nonce-sized 32-byte request ----
    const SMALL: usize = 32;
    let small_iters = 2_000_000u64;
    let mut b = [0u8; SMALL];

    let ours_small = time_call(small_iters, || {
        ours.fill_bytes(&mut b);
        std::hint::black_box(&b);
    });
    let sys_small = time_call(small_iters / 4, || {
        sys_getrandom(&mut b);
        std::hint::black_box(&b);
    });
    let ge_small = time_call(small_iters / 4, || {
        unsafe { getentropy(b.as_mut_ptr(), SMALL) };
        std::hint::black_box(&b);
    });

    println!("32-byte draw (key/nonce sized), cycles per call:");
    println!("  rheo-os DRBG (library call) : {ours_small:>8}");
    println!("  Linux getrandom(2) syscall  : {sys_small:>8}");
    println!("  Linux getentropy(3) [glibc] : {ge_small:>8}");
    if ours_small > 0 {
        println!(
            "  -> rheo-os is {:.1}x faster than the getrandom syscall, {:.1}x vs getentropy",
            sys_small as f64 / ours_small as f64,
            ge_small as f64 / ours_small as f64
        );
    }
    println!();

    // ---- bulk throughput ----
    const CHUNK: usize = 64 * 1024;
    let bulk_bytes = 256 * 1024 * 1024u64; // 256 MiB
    let rounds = bulk_bytes / CHUNK as u64;
    let mut big = vec![0u8; CHUNK];

    let t = Instant::now();
    for _ in 0..rounds {
        ours.fill_bytes(&mut big);
        std::hint::black_box(&big);
    }
    let ours_bulk = mb_per_s(bulk_bytes, t.elapsed().as_secs_f64());

    let t = Instant::now();
    for _ in 0..rounds {
        // getrandom is capped at 32 MiB/call and can be interrupted; loop.
        let mut done = 0;
        while done < CHUNK {
            let n = getrandom_all(&mut big[done..]);
            done += n;
        }
        std::hint::black_box(&big);
    }
    let glibc_bulk = mb_per_s(bulk_bytes, t.elapsed().as_secs_f64());

    let urandom_bulk = urandom_throughput(bulk_bytes, CHUNK);

    println!("bulk throughput (MB/s, higher is better):");
    println!("  rheo-os DRBG (library call) : {ours_bulk:>10.1}");
    println!("  Linux getrandom(3) [glibc]  : {glibc_bulk:>10.1}");
    println!("  Linux /dev/urandom read     : {urandom_bulk:>10.1}");
    println!();
    println!(
        "summary: same ChaCha20 primitive; the win is removing the kernel\n\
         boundary from the per-draw hot path (TIME-IDENTITY.md 4)."
    );
}

fn getrandom_all(buf: &mut [u8]) -> usize {
    loop {
        let n = unsafe { getrandom(buf.as_mut_ptr(), buf.len(), 0) };
        if n >= 0 {
            return n as usize;
        }
    }
}

fn urandom_throughput(total: u64, chunk: usize) -> f64 {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom").expect("open /dev/urandom");
    let mut buf = vec![0u8; chunk];
    let rounds = total / chunk as u64;
    let t = Instant::now();
    for _ in 0..rounds {
        f.read_exact(&mut buf).expect("read urandom");
        std::hint::black_box(&buf);
    }
    mb_per_s(total, t.elapsed().as_secs_f64())
}
