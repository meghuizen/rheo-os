//! The queue-pair ABI: submission/completion rings + doorbell - the entire
//! syscall surface (docs/ARCHITECTURE.md 3, docs/IO.md 1). Entry layouts
//! and the SPSC ring follow docs/KERNEL-RUST.md 3 exactly.
//!
//! At this stage the "kernel side" of a queue pair runs at the same
//! privilege level as the submitting cell (user-mode cells arrive with
//! BUILD-ORDER.md step 5); the mechanism - fixed-layout entries, grant
//! checks per entry, flow-context propagation, doorbell batching - is the
//! real one, and it is what the P2 benchmark measures.

use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::capability::{CapTable, Handle, ObjectTable, WRITE};

mod sealed {
    /// Sealed: only types explicitly marked can live in DMA-visible rings.
    /// Prevents accidental DMA of types containing pointers, which would
    /// expose host virtual addresses to devices.
    ///
    /// # Safety
    /// Implementors must be plain data: fixed layout (`repr(C)`), no
    /// pointers or references, valid for every bit pattern a device could
    /// write.
    pub unsafe trait DmaSafe: Copy + Sized + 'static {}
}
pub use sealed::DmaSafe;

/// Submission opcodes understood by the current kernel side.
pub const OP_NOP: u8 = 0;
/// Echo `payload[0..8]` back through the completion's `result` field
/// (truncated to 32 bits) - the null round trip with a data touch.
pub const OP_ECHO: u8 = 1;

/// Completion status codes.
pub const STATUS_OK: u32 = 0;
pub const STATUS_BAD_OPCODE: u32 = 1;
pub const STATUS_DENIED: u32 = 2;

/// A submission queue entry - exactly 64 bytes, one cache line, so
/// producer and consumer never false-share (docs/KERNEL-RUST.md 3).
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct SqEntry {
    pub opcode: u8,
    pub flags: u8,
    pub engine_id: u16,
    pub cap_id: u32,
    pub flow_id: u128,  // 16 bytes - the distributed trace handle
    pub user_data: u64, // returned in CqEntry unchanged
    // 24, not 32: header (8) + flow_id at its 16-alignment (16..32) +
    // user_data (8) leaves exactly 24 bytes in a 64-byte line.
    pub payload: [u8; 24], // opcode-specific
}
const _: () = assert!(core::mem::size_of::<SqEntry>() == 64);
unsafe impl DmaSafe for SqEntry {}

/// A completion queue entry - 32 bytes.
#[repr(C, align(32))]
#[derive(Copy, Clone)]
pub struct CqEntry {
    pub flow_id: u128,
    pub user_data: u64,
    pub status: u32,
    pub result: u32,
}
const _: () = assert!(core::mem::size_of::<CqEntry>() == 32);
unsafe impl DmaSafe for CqEntry {}

impl CqEntry {
    /// All-zero entry, for static ring storage.
    pub const ZERO: CqEntry = CqEntry {
        flow_id: 0,
        user_data: 0,
        status: 0,
        result: 0,
    };
}

impl SqEntry {
    /// All-zero entry, for static ring storage.
    pub const ZERO: SqEntry = SqEntry {
        opcode: 0,
        flags: 0,
        engine_id: 0,
        cap_id: 0,
        flow_id: 0,
        user_data: 0,
        payload: [0; 24],
    };

    pub fn new(opcode: u8, cap: Handle, flow_id: u128, user_data: u64) -> SqEntry {
        SqEntry {
            opcode,
            flags: 0,
            engine_id: 0,
            cap_id: cap_index(cap),
            flow_id,
            user_data,
            payload: [0; 24],
        }
    }
}

// The 64-byte ABI entry carries a 32-bit capability reference. The full
// IDL-generated ABI (BUILD-ORDER.md step 6) will define this packing; for
// now the low 16 bits are the slot and the next 16 the generation's low
// bits, reconstructed against the table at check time.
fn cap_index(handle: Handle) -> u32 {
    handle.raw_low32()
}

/// A single-producer, single-consumer ring over DMA-safe memory.
/// N must be a power of two for the index masking to work.
pub struct Ring<T: DmaSafe, const N: usize> {
    /// Ring storage. *Not* held as a Rust reference on the hot path - the
    /// other side (kernel, or later hardware) also reads/writes it, so all
    /// access is through volatile primitives.
    entries: *mut T,
    head: AtomicU32,
    tail: AtomicU32,
}

// SAFETY: the ring's memory is owned uniquely by the pair of endpoints.
unsafe impl<T: DmaSafe, const N: usize> Send for Ring<T, N> {}

impl<T: DmaSafe, const N: usize> Ring<T, N> {
    const MASK: usize = N - 1;
    const POWER_OF_TWO: () = assert!(N.is_power_of_two());

    /// # Safety
    /// `entries` must point at N valid, uniquely-owned slots of T that
    /// outlive the ring.
    pub unsafe fn new(entries: *mut T) -> Ring<T, N> {
        #[allow(clippy::let_unit_value)]
        let _ = Self::POWER_OF_TWO;
        Ring {
            entries,
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }
    }

    /// Push one entry. Returns false if the ring is full.
    /// Hot path: one volatile write + one atomic store.
    #[inline(always)]
    pub fn push(&self, entry: T) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= N {
            return false; // full
        }
        let idx = (head as usize) & Self::MASK;
        // SAFETY: idx is in-bounds by the capacity check above. Volatile
        // because the other endpoint reads this memory.
        unsafe { ptr::write_volatile(self.entries.add(idx), entry) };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pop one entry. Returns None if empty.
    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None; // empty
        }
        let idx = (tail as usize) & Self::MASK;
        // SAFETY: idx is in-bounds; volatile read matches volatile write.
        let entry = unsafe { ptr::read_volatile(self.entries.add(idx)) };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(entry)
    }
}

pub const RING_DEPTH: usize = 64;

/// A queue pair: submission ring in, completion ring out.
pub struct QueuePair {
    pub sq: Ring<SqEntry, RING_DEPTH>,
    pub cq: Ring<CqEntry, RING_DEPTH>,
}

impl QueuePair {
    /// # Safety
    /// Both backing arrays must be valid, uniquely owned, and outlive the
    /// pair.
    pub unsafe fn new(sq_storage: *mut SqEntry, cq_storage: *mut CqEntry) -> QueuePair {
        unsafe {
            QueuePair {
                sq: Ring::new(sq_storage),
                cq: Ring::new(cq_storage),
            }
        }
    }

    /// User-side submit: push one entry writing its fields individually.
    /// `#[inline(always)]` and field-wise on purpose - it inlines into
    /// U-mode code, where a whole-struct 64-byte write could lower to a
    /// `memcpy` call into unmapped kernel `.text` and fault. Mirrors
    /// `Ring::push`; the SPSC ordering is identical.
    #[inline(always)]
    pub fn submit(&self, opcode: u8, cap_id: u32, flow_id: u128, user_data: u64) -> bool {
        let head = self.sq.head.load(Ordering::Relaxed);
        let tail = self.sq.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= RING_DEPTH {
            return false;
        }
        let idx = (head as usize) & (RING_DEPTH - 1);
        // SAFETY: idx is in-bounds; the ring memory is shared and mapped.
        unsafe {
            let slot = self.sq.entries.add(idx);
            ptr::addr_of_mut!((*slot).opcode).write_volatile(opcode);
            ptr::addr_of_mut!((*slot).cap_id).write_volatile(cap_id);
            ptr::addr_of_mut!((*slot).flow_id).write_volatile(flow_id);
            ptr::addr_of_mut!((*slot).user_data).write_volatile(user_data);
        }
        self.sq.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// User-side reap: pop one completion, returning its status, or None.
    #[inline(always)]
    pub fn reap(&self) -> Option<u32> {
        let tail = self.cq.tail.load(Ordering::Relaxed);
        let head = self.cq.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail as usize) & (RING_DEPTH - 1);
        // SAFETY: idx is in-bounds; matches the volatile writer in the
        // kernel completion path.
        let status = unsafe { ptr::addr_of!((*self.cq.entries.add(idx)).status).read_volatile() };
        self.cq.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(status)
    }
}

/// The kernel side of the doorbell: drain the submission ring, grant-check
/// every entry against the submitting cell's capability table, execute,
/// and push completions. Flow context propagates unchanged - observability
/// the system cannot fail to produce (docs/ARCHITECTURE.md 3, object 10).
///
/// Returns the number of entries processed.
pub fn kernel_process(qp: &QueuePair, caps: &mut CapTable, objects: &ObjectTable) -> usize {
    let mut processed = 0;
    while let Some(entry) = qp.sq.pop() {
        let (status, result) = match caps.grant_check_low32(objects, entry.cap_id, WRITE) {
            Err(_) => (STATUS_DENIED, 0),
            Ok(_object) => match entry.opcode {
                OP_NOP => (STATUS_OK, 0),
                OP_ECHO => {
                    let mut value = [0u8; 4];
                    value.copy_from_slice(&entry.payload[..4]);
                    (STATUS_OK, u32::from_le_bytes(value))
                }
                _ => (STATUS_BAD_OPCODE, 0),
            },
        };
        qp.cq.push(CqEntry {
            flow_id: entry.flow_id,
            user_data: entry.user_data,
            status,
            result,
        });
        processed += 1;
    }
    processed
}
