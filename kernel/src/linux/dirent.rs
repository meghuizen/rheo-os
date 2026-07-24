//! Linux directory-entry packing and `st_mode` synthesis (docs/LINUX-COMPAT.md
//! L2). Portable: the byte layouts here are the same on all three ISAs (only
//! `struct stat` differs per ABI, and that lives in `arch::linux_abi`). The
//! VFS behind `svc::FileOps` speaks a small internal record format
//! (`[u32 kind][u32 name_len][name]`, see tests/src/vfs_personality.rs); this
//! module converts a file kind + name into the Linux `struct linux_dirent64`
//! and the `st_mode` bits glibc expects.
//!
//! Sources: `struct linux_dirent64` from `fs/readdir.c`; `S_IF*`/`DT_*` from
//! `include/uapi/linux/stat.h` and `dirent.h`.

/// File-type bits in `st_mode`.
pub const S_IFREG: u32 = 0o100000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;

/// `d_type` values for `linux_dirent64`.
const DT_UNKNOWN: u8 = 0;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

/// VFS kind codes (kept in sync with tests/src/vfs_personality.rs `kind_code`):
/// 0 = regular, 1 = directory, 2 = symlink, 3 = other.
pub const KIND_REGULAR: u64 = 0;
pub const KIND_DIR: u64 = 1;
pub const KIND_SYMLINK: u64 = 2;

/// Full `st_mode` (type bits + a fixed permission set) for a VFS kind code.
/// Regular/link 0644, directory 0755 - the personality does not model real
/// Unix permissions, so it reports conventional defaults (docs/LINUX-COMPAT.md
/// 3: synthesized identity).
pub fn mode_for_kind(kind_code: u64) -> u32 {
    match kind_code {
        KIND_DIR => S_IFDIR | 0o755,
        KIND_SYMLINK => S_IFLNK | 0o777,
        _ => S_IFREG | 0o644,
    }
}

/// `d_type` for a VFS kind code.
fn dtype_for_kind(kind_code: u64) -> u8 {
    match kind_code {
        KIND_DIR => DT_DIR,
        KIND_SYMLINK => DT_LNK,
        KIND_REGULAR => DT_REG,
        _ => DT_UNKNOWN,
    }
}

/// Fixed part of a `linux_dirent64` before the name: d_ino(8) + d_off(8) +
/// d_reclen(2) + d_type(1) = 19 bytes; the name (with a NUL) follows.
const HDR: usize = 19;

/// Append one `linux_dirent64` record for `(ino, kind, name)` into `out` at
/// `off`. Returns the new offset, or None if the record does not fit (the
/// caller stops - honest truncation). `d_reclen` is rounded up to 8 bytes.
pub fn pack(out: &mut [u8], off: usize, ino: u64, kind_code: u64, name: &[u8]) -> Option<usize> {
    let reclen = (HDR + name.len() + 1 + 7) & !7;
    if off + reclen > out.len() {
        return None;
    }
    let d_off = (off + reclen) as u64; // next record's offset (opaque cookie)
    out[off..off + 8].copy_from_slice(&ino.to_ne_bytes());
    out[off + 8..off + 16].copy_from_slice(&d_off.to_ne_bytes());
    out[off + 16..off + 18].copy_from_slice(&(reclen as u16).to_ne_bytes());
    out[off + 18] = dtype_for_kind(kind_code);
    out[off + HDR..off + HDR + name.len()].copy_from_slice(name);
    // Remaining bytes (NUL + alignment padding) are already zero if `out` was
    // zeroed; zero them explicitly so a reused buffer cannot leak stale bytes.
    for b in out
        .iter_mut()
        .take(off + reclen)
        .skip(off + HDR + name.len())
    {
        *b = 0;
    }
    Some(off + reclen)
}
