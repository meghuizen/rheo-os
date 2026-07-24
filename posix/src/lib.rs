//! The POSIX personality + filesystem stack (docs/POSIX-PERSONALITY.md,
//! docs/FILESYSTEMS.md), built on the OS's native primitives rather than a
//! Linux kernel:
//!
//! - `vfs`: the translation-layer core - a `FileSystem` trait every backend
//!   plugs into.
//! - `ramfs`: a read-write in-memory filesystem (the working store).
//! - `ext4`: a read-only driver for a real ext4 image (Tier-1 legacy disk
//!   interchange, FILESYSTEMS.md 1). No block driver yet, so the image is a
//!   RAM buffer (`include_bytes!` in the OS); the parsing is the real work.
//! - `mount`: the per-session `/` - a mount table + path resolution.
//! - `sys`: the POSIX file syscall surface (fd table, open/read/write/...).
//! - `fs`: a `std::fs`-shaped facade, so standard-library file code runs here.
//!
//! Filesystems present the `&self` `FileSystem` interface, so a mount table
//! holds them as trait objects; the eventual design serves each from its own
//! userspace filesystem *cell* over the queue-pair ABI (the OS's FUSE), which
//! reuses the queue reactor the runtime already demonstrates.

#![no_std]

extern crate alloc;

pub mod ext4;
pub mod fs;
pub mod mount;
pub mod ramfs;
pub mod sys;
pub mod vfs;

pub use ext4::Ext4;
pub use ramfs::RamFs;
pub use vfs::{DirEntry, Errno, FileSystem, FileType, Metadata, NodeId};

/// Drop all mounts and open descriptors (tests start from a clean state).
pub fn reset() {
    mount::reset();
    sys::reset();
}
