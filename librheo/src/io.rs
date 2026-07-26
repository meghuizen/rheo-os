//! One-model async I/O (docs/IO.md 1, docs/LIBRHEO.md Phase B). The only way a
//! cell asks the kernel to move bytes is a submission on its queue pair; a
//! "blocking" read is a strand parking on the completion token and the vcore
//! running other strands meanwhile. Files/streams are `Op`s that complete
//! through the reactor:
//!
//! - **submit -> completion future**: [`File::open`]/[`read_at`](File::read_at)/
//!   [`write_at`](File::write_at)/[`close`](File::close) each submit one OP_*
//!   and `.await` its completion.
//! - **batched**: N `read_at` futures spawned as strands and joined submit N
//!   entries, then `block_on` rings the doorbell **once** and drains all N
//!   completions - "one wakeup, N strands resumed" (CONCURRENCY.md 1).
//! - **contract-based** (IO.md): [`Contract`] carries a durability class and a
//!   latency window on a write. QEMU has no durable / real-time backend, so the
//!   contract is advisory today (recorded in the op flags, honored best-effort);
//!   documented in docs/LIBRHEO.md Phase B.
//! - **inline vs by-reference** (IO.md threshold): a write at or below
//!   [`sys::INLINE_MAX`] bytes rides inline in the submission; a larger read or
//!   write is **by reference** at a buffer/grant VA - and above the threshold it
//!   is **zero-copy**, landing directly in the cell's mapped grant pages with no
//!   kernel bounce (see [`File::read_into`]).

use crate::mem::Grant;
use crate::rt;
use crate::sys;

/// Durability class of a write (docs/IO.md). Advisory in QEMU (no durable
/// backend); mapped to op flags and honored best-effort.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Durability {
    /// No durability guarantee (write-back).
    None,
    /// Flush to the device on completion.
    Flush,
    /// Force-unit-access: durable before completion.
    Fua,
}

/// An I/O contract (docs/IO.md): the durability class and a soft latency window
/// (microseconds; 0 = best-effort). Advisory today.
#[derive(Copy, Clone, Debug)]
pub struct Contract {
    pub durability: Durability,
    pub latency_us: u32,
}

impl Default for Contract {
    fn default() -> Contract {
        Contract {
            durability: Durability::None,
            latency_us: 0,
        }
    }
}

impl Contract {
    fn flags(&self) -> u8 {
        match self.durability {
            Durability::None => 0,
            Durability::Flush => sys::FLAG_DUR_FLUSH,
            Durability::Fua => sys::FLAG_DUR_FUA,
        }
    }
}

fn put_u64(a: &mut [u8; 24], off: usize, v: u64) {
    a[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(a: &mut [u8; 24], off: usize, v: u32) {
    a[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// An async file over the OP_* queue opcodes + the reactor. `fd` is an opaque
/// handle the `svc::FileOps`/VFS layer owns (a first-class file capability is a
/// documented next step; docs/LIBRHEO.md Phase B).
pub struct File {
    fd: u32,
}

impl File {
    /// Open `path`; returns the file or the completion status on error.
    pub async fn open(path: &str) -> Result<File, u32> {
        let mut a = [0u8; 24];
        put_u64(&mut a, 0, path.as_ptr() as u64);
        put_u32(&mut a, 8, path.len() as u32);
        // flags @12 = 0 (read)
        let cqe = rt::submit_and_await(sys::OP_OPEN, a).await;
        if cqe.status == sys::STATUS_OK {
            Ok(File { fd: cqe.result })
        } else {
            Err(cqe.status)
        }
    }

    /// Wrap an already-open fd (e.g. the console: 0 stdin, 1 stdout, 2 stderr).
    pub fn from_fd(fd: u32) -> File {
        File { fd }
    }

    pub fn fd(&self) -> u32 {
        self.fd
    }

    /// Read `len` bytes at `offset` into the cell buffer at `buf_va`. Above the
    /// inline threshold this is zero-copy (lands directly at `buf_va`). Returns
    /// the byte count.
    pub async fn read_at(&self, buf_va: u64, len: u32, offset: u64) -> Result<u32, u32> {
        let mut a = [0u8; 24];
        put_u64(&mut a, 0, buf_va);
        put_u64(&mut a, 8, offset);
        put_u32(&mut a, 16, len);
        put_u32(&mut a, 20, self.fd);
        let cqe = rt::submit_and_await(sys::OP_READ, a).await;
        if cqe.status == sys::STATUS_OK {
            Ok(cqe.result)
        } else {
            Err(cqe.status)
        }
    }

    /// Zero-copy read of `[offset, offset+len)` straight into a committed
    /// [`Grant`] at grant offset `goff` - no bounce buffer.
    pub async fn read_into(
        &self,
        g: &Grant,
        goff: usize,
        len: u32,
        offset: u64,
    ) -> Result<u32, u32> {
        self.read_at((g.base() + goff) as u64, len, offset).await
    }

    /// Write `buf` at `offset` under `contract`. At or below the inline
    /// threshold the bytes ride in the submission; larger writes go by
    /// reference (zero-copy) from `buf`. Returns the byte count.
    pub async fn write_contract(
        &self,
        buf: &[u8],
        offset: u64,
        contract: Contract,
    ) -> Result<u32, u32> {
        let cqe = if buf.len() <= sys::INLINE_MAX {
            let mut a = [0u8; 24];
            put_u32(&mut a, 0, self.fd);
            put_u32(&mut a, 4, buf.len() as u32);
            a[8..8 + buf.len()].copy_from_slice(buf);
            rt::submit_and_await_flags(sys::OP_WRITE, sys::FLAG_INLINE | contract.flags(), a).await
        } else {
            let mut a = [0u8; 24];
            put_u64(&mut a, 0, buf.as_ptr() as u64);
            put_u64(&mut a, 8, offset);
            put_u32(&mut a, 16, buf.len() as u32);
            put_u32(&mut a, 20, self.fd);
            rt::submit_and_await_flags(sys::OP_WRITE, contract.flags(), a).await
        };
        if cqe.status == sys::STATUS_OK {
            Ok(cqe.result)
        } else {
            Err(cqe.status)
        }
    }

    /// Write `buf` at `offset` with the default (best-effort) contract.
    pub async fn write_at(&self, buf: &[u8], offset: u64) -> Result<u32, u32> {
        self.write_contract(buf, offset, Contract::default()).await
    }

    /// `fstat`: returns the file size in bytes.
    pub async fn size(&self) -> Result<u64, u32> {
        // The kernel writes a `Stat { size u64, kind u64 }` at the buffer VA.
        let mut st = [0u64; 2];
        let mut a = [0u8; 24];
        put_u64(&mut a, 0, st.as_mut_ptr() as u64);
        put_u32(&mut a, 8, self.fd);
        let cqe = rt::submit_and_await(sys::OP_FSTAT, a).await;
        if cqe.status == sys::STATUS_OK {
            Ok(st[0])
        } else {
            Err(cqe.status)
        }
    }

    /// Close the file.
    pub async fn close(self) -> Result<(), u32> {
        let mut a = [0u8; 24];
        put_u32(&mut a, 0, self.fd);
        let cqe = rt::submit_and_await(sys::OP_CLOSE, a).await;
        if cqe.status == sys::STATUS_OK {
            Ok(())
        } else {
            Err(cqe.status)
        }
    }
}

/// A byte stream (console / pipe): an [`File`] over a fixed fd with no
/// positional offset. The `term` phase builds the full terminal on this.
pub struct Stream {
    file: File,
}

impl Stream {
    /// The cell's stdout (fd 1).
    pub fn stdout() -> Stream {
        Stream {
            file: File::from_fd(1),
        }
    }

    /// Write `buf` to the stream.
    pub async fn write(&self, buf: &[u8]) -> Result<u32, u32> {
        self.file.write_at(buf, 0).await
    }
}
