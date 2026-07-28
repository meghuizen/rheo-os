//! `libtileso.so` - the tile framework's GEMM kernel behind a C ABI.
//!
//! Same `#[path]` include as `kernel/engine.rs`, `bench-core` and `tilelinux`: one
//! source, compiled here into a shared object so a **dynamically linked** caller can
//! reach it. That is the only route by which a JS runtime's FFI can call a tile kernel,
//! which is why it exists (docs/TILES.md 13.4c).

#[path = "../../../../librheo/src/tile/kernels.rs"]
#[allow(dead_code)]
mod kernels;

/// Compute the tiled int8 -> i32 GEMM and return an FNV-1a hash of the whole output.
///
/// A hash rather than a buffer: a caller reached through FFI then needs no marshalling
/// at all, and the value is exact - one wrong element changes it. The operands are
/// derived here from the same index formula `tilelinux` and `librheo-fa` use, so all
/// three must produce the same number.
/// The same value narrowed to 31 bits, for callers that cannot hold a `u64` exactly.
///
/// A JS number is exact only to 2^53, so handing JavaScript the full hash would make the
/// comparison meaningless in a way that presents as a mismatch. Narrowing here rather
/// than in the caller keeps the check exact on both sides (docs/TILES.md 13.4d).
#[no_mangle]
pub extern "C" fn tile_gemm_check(m: u32, n: u32, k: u32) -> i32 {
    (tile_gemm_hash(m, n, k) & 0x7FFF_FFFF) as i32
}

#[no_mangle]
pub extern "C" fn tile_gemm_hash(m: u32, n: u32, k: u32) -> u64 {
    let (m, n, k) = (m as usize, n as usize, k as usize);
    if m == 0 || n == 0 || k == 0 || m > 256 || n > 256 || k > 256 {
        return 0;
    }
    let fill = |buf: &mut [i8], salt: u32| {
        for (i, x) in buf.iter_mut().enumerate() {
            let h = (i as u32)
                .wrapping_mul(2_246_822_519)
                .wrapping_add(salt.wrapping_mul(31));
            *x = ((h >> 17) & 0x7F) as i8 - 64;
        }
    };
    let mut a = vec![0i8; m * k];
    let mut b = vec![0i8; k * n];
    fill(&mut a, 5);
    fill(&mut b, 7);
    let mut c = vec![0i32; m * n];
    // SAFETY: the operands cover m*k and k*n, and `c` covers exactly m*n.
    unsafe {
        kernels::gemm_i8_i32(
            a.as_ptr(),
            k,
            b.as_ptr(),
            n,
            c.as_mut_ptr(),
            n,
            m,
            n,
            k,
        );
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in &c {
        for byte in v.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}
