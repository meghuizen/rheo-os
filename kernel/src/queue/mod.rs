//! The queue-pair ABI: submission/completion rings + doorbell - the entire
//! syscall surface (docs/ARCHITECTURE.md 3, docs/IO.md 1). Entry layouts
//! and the SPSC ring follow docs/KERNEL-RUST.md 3 exactly.
//!
//! At this stage the "kernel side" of a queue pair runs at the same
//! privilege level as the submitting cell (user-mode cells arrive with
//! BUILD-ORDER.md step 5); the mechanism - fixed-layout entries, grant
//! checks per entry, flow-context propagation, doorbell batching - is the
//! real one, and it is what the P2 benchmark measures.
//!
//! **On-wire layout (docs/IO.md 1, docs/LIBRHEO.md).** A queue pair is a
//! single contiguous shared region a separately-compiled library (librheo)
//! can bind to: a `repr(C)` [`QueueHeader`] with the ring indices at fixed
//! offsets, followed by the SQ entry array and the CQ entry array. Both the
//! kernel and a loaded cell overlay a [`QueuePair`] on the region (from the
//! same physical frames, at their own VAs); the head/tail atomics live *in*
//! the region, not in the Rust struct, so the two overlays share them.

use core::ptr::{self, addr_of, addr_of_mut};
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
/// Echo `payload[0..4]` back through the completion's `result` field - the
/// null round trip with a data touch, used by the librheo async proof.
pub const OP_ECHO: u8 = 1;

/// Completion status codes.
pub const STATUS_OK: u32 = 0;
pub const STATUS_BAD_OPCODE: u32 = 1;
pub const STATUS_DENIED: u32 = 2;

/// On-wire ABI version carried in the ring header (docs/IO.md 1). A cell
/// binding the region checks this before trusting the layout.
pub const QUEUE_ABI_VERSION: u32 = 1;

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

/// The shared ring header (docs/IO.md 1): version + geometry + the four ring
/// indices, at fixed `repr(C)` offsets so an independently-compiled cell
/// binds to the same words the kernel does. Exactly one cache line (64 B).
#[repr(C)]
pub struct QueueHeader {
    /// ABI version ([`QUEUE_ABI_VERSION`]).
    pub version: u32,
    /// Ring depth (entries per ring); [`RING_DEPTH`].
    pub depth: u32,
    /// Byte offset of the SQ entry array from the region base.
    pub sq_off: u32,
    /// Byte offset of the CQ entry array from the region base.
    pub cq_off: u32,
    pub sq_head: AtomicU32,
    pub sq_tail: AtomicU32,
    pub cq_head: AtomicU32,
    pub cq_tail: AtomicU32,
    _reserved: [u32; 8],
}
const _: () = assert!(core::mem::size_of::<QueueHeader>() == 64);

pub const RING_DEPTH: usize = 64;

const HEADER_SIZE: usize = 64;
const SQ_OFF: usize = HEADER_SIZE;
const SQ_BYTES: usize = RING_DEPTH * core::mem::size_of::<SqEntry>();
const CQ_OFF: usize = SQ_OFF + SQ_BYTES;
const CQ_BYTES: usize = RING_DEPTH * core::mem::size_of::<CqEntry>();

/// A single-producer, single-consumer ring overlaid on the shared region.
/// N must be a power of two for the index masking to work. The head/tail
/// live in the region's [`QueueHeader`] (raw pointers here), so both endpoint
/// overlays - kernel and cell - drive the *same* indices.
pub struct Ring<T: DmaSafe, const N: usize> {
    /// Ring storage. *Not* held as a Rust reference on the hot path - the
    /// other side (kernel, cell, or later hardware) also reads/writes it, so
    /// all access is through volatile primitives.
    entries: *mut T,
    /// The producer index, in the shared header.
    head: *const AtomicU32,
    /// The consumer index, in the shared header.
    tail: *const AtomicU32,
}

// SAFETY: the ring's memory is owned uniquely by the pair of endpoints.
unsafe impl<T: DmaSafe, const N: usize> Send for Ring<T, N> {}

impl<T: DmaSafe, const N: usize> Ring<T, N> {
    const MASK: usize = N - 1;
    const POWER_OF_TWO: () = assert!(N.is_power_of_two());

    #[inline(always)]
    fn head(&self) -> &AtomicU32 {
        // SAFETY: `head` points into the live shared header for the ring's life.
        unsafe { &*self.head }
    }
    #[inline(always)]
    fn tail(&self) -> &AtomicU32 {
        // SAFETY: as above.
        unsafe { &*self.tail }
    }

    /// Push one entry. Returns false if the ring is full.
    /// Hot path: one volatile write + one atomic store.
    #[inline(always)]
    pub fn push(&self, entry: T) -> bool {
        #[allow(clippy::let_unit_value)]
        let _ = Self::POWER_OF_TWO;
        let head = self.head().load(Ordering::Relaxed);
        let tail = self.tail().load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= N {
            return false; // full
        }
        let idx = (head as usize) & Self::MASK;
        // SAFETY: idx is in-bounds by the capacity check above. Volatile
        // because the other endpoint reads this memory.
        unsafe { ptr::write_volatile(self.entries.add(idx), entry) };
        self.head().store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pop one entry. Returns None if empty.
    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail().load(Ordering::Relaxed);
        let head = self.head().load(Ordering::Acquire);
        if tail == head {
            return None; // empty
        }
        let idx = (tail as usize) & Self::MASK;
        // SAFETY: idx is in-bounds; volatile read matches volatile write.
        let entry = unsafe { ptr::read_volatile(self.entries.add(idx)) };
        self.tail().store(tail.wrapping_add(1), Ordering::Release);
        Some(entry)
    }
}

/// A queue pair: submission ring in, completion ring out. An overlay over a
/// single shared region (see the module header); construct it with
/// [`QueuePair::init`]/[`QueuePair::attach`] rather than by field.
pub struct QueuePair {
    pub sq: Ring<SqEntry, RING_DEPTH>,
    pub cq: Ring<CqEntry, RING_DEPTH>,
    base: *mut u8,
}

impl QueuePair {
    /// Total bytes a queue-pair region occupies, rounded up to a page so the
    /// kernel can map it into a loaded cell. header + SQ array + CQ array.
    pub const REGION_SIZE: usize = (CQ_OFF + CQ_BYTES + 0xFFF) & !0xFFF;

    /// Bytes of the region actually used (before page rounding). For asserts.
    pub const USED_SIZE: usize = CQ_OFF + CQ_BYTES;

    /// Write a fresh header at `base` (version, geometry, zeroed indices).
    ///
    /// # Safety
    /// `base` must point at [`REGION_SIZE`](Self::REGION_SIZE) writable bytes,
    /// 64-byte aligned, uniquely owned for the pair's life.
    pub unsafe fn init_header(base: *mut u8) {
        let h = base as *mut QueueHeader;
        // Field-wise so nothing lowers to a struct memcpy (the U-mode rule);
        // and the atomics start at 0.
        unsafe {
            addr_of_mut!((*h).version).write(QUEUE_ABI_VERSION);
            addr_of_mut!((*h).depth).write(RING_DEPTH as u32);
            addr_of_mut!((*h).sq_off).write(SQ_OFF as u32);
            addr_of_mut!((*h).cq_off).write(CQ_OFF as u32);
            addr_of_mut!((*h).sq_head).write(AtomicU32::new(0));
            addr_of_mut!((*h).sq_tail).write(AtomicU32::new(0));
            addr_of_mut!((*h).cq_head).write(AtomicU32::new(0));
            addr_of_mut!((*h).cq_tail).write(AtomicU32::new(0));
            addr_of_mut!((*h)._reserved).write([0; 8]);
        }
    }

    /// Overlay a queue pair on an already-initialised region at `base`. Reads
    /// nothing; ring pointers are computed from the fixed layout (this crate
    /// owns the geometry). librheo's independent overlay reads the header's
    /// `sq_off`/`cq_off` instead - both agree because the layout is the ABI.
    ///
    /// # Safety
    /// `base` must be a region previously prepared by [`init_header`](Self::init_header)
    /// (here or on the shared frames), valid for the pair's life.
    pub unsafe fn attach(base: *mut u8) -> QueuePair {
        let h = base as *const QueueHeader;
        // SAFETY: base is a valid region; the header lies at its start and the
        // arrays at the fixed offsets.
        unsafe {
            QueuePair {
                sq: Ring {
                    entries: base.add(SQ_OFF) as *mut SqEntry,
                    head: addr_of!((*h).sq_head),
                    tail: addr_of!((*h).sq_tail),
                },
                cq: Ring {
                    entries: base.add(CQ_OFF) as *mut CqEntry,
                    head: addr_of!((*h).cq_head),
                    tail: addr_of!((*h).cq_tail),
                },
                base,
            }
        }
    }

    /// Initialise a header at `base` and overlay a pair on it - for the case
    /// where the region is reachable at one VA (identity-mapped `.user`
    /// cells, host tests). For a loaded cell the kernel calls `init_header`
    /// through its linear map, then `attach` at the cell's VA.
    ///
    /// # Safety
    /// See [`init_header`](Self::init_header).
    pub unsafe fn init(base: *mut u8) -> QueuePair {
        unsafe {
            Self::init_header(base);
            Self::attach(base)
        }
    }

    /// The region base this overlay binds to.
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// User-side submit: push one entry writing its fields individually.
    /// `#[inline(always)]` and field-wise on purpose - it inlines into
    /// U-mode code, where a whole-struct 64-byte write could lower to a
    /// `memcpy` call into unmapped kernel `.text` and fault. Mirrors
    /// `Ring::push`; the SPSC ordering is identical. Leaves `payload` as
    /// whatever the slot last held (OP_NOP ignores it); use
    /// [`submit_args`](Self::submit_args) to carry data.
    #[inline(always)]
    pub fn submit(&self, opcode: u8, cap_id: u32, flow_id: u128, user_data: u64) -> bool {
        let head = self.sq.head().load(Ordering::Relaxed);
        let tail = self.sq.tail().load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= RING_DEPTH {
            return false;
        }
        let idx = (head as usize) & (RING_DEPTH - 1);
        // SAFETY: idx is in-bounds; the ring memory is shared and mapped.
        unsafe {
            let slot = self.sq.entries.add(idx);
            addr_of_mut!((*slot).opcode).write_volatile(opcode);
            addr_of_mut!((*slot).cap_id).write_volatile(cap_id);
            addr_of_mut!((*slot).flow_id).write_volatile(flow_id);
            addr_of_mut!((*slot).user_data).write_volatile(user_data);
        }
        self.sq
            .head()
            .store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Submit carrying up to 24 bytes of opcode arguments in `payload`. For
    /// loaded (self-contained) cells, which have their own `mem*`; the
    /// hand-written `.user` cells use [`submit`](Self::submit) instead.
    #[inline]
    pub fn submit_args(
        &self,
        opcode: u8,
        cap_id: u32,
        flow_id: u128,
        user_data: u64,
        args: &[u8],
    ) -> bool {
        let head = self.sq.head().load(Ordering::Relaxed);
        let tail = self.sq.tail().load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= RING_DEPTH {
            return false;
        }
        let idx = (head as usize) & (RING_DEPTH - 1);
        let n = args.len().min(24);
        // SAFETY: idx is in-bounds; the ring memory is shared and mapped.
        unsafe {
            let slot = self.sq.entries.add(idx);
            addr_of_mut!((*slot).opcode).write_volatile(opcode);
            addr_of_mut!((*slot).flags).write_volatile(0);
            addr_of_mut!((*slot).engine_id).write_volatile(0);
            addr_of_mut!((*slot).cap_id).write_volatile(cap_id);
            addr_of_mut!((*slot).flow_id).write_volatile(flow_id);
            addr_of_mut!((*slot).user_data).write_volatile(user_data);
            let pl = addr_of_mut!((*slot).payload) as *mut u8;
            for (i, &b) in args[..n].iter().enumerate() {
                pl.add(i).write_volatile(b);
            }
            for i in n..24 {
                pl.add(i).write_volatile(0);
            }
        }
        self.sq
            .head()
            .store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// User-side reap: pop one completion, returning its status, or None.
    #[inline(always)]
    pub fn reap(&self) -> Option<u32> {
        let tail = self.cq.tail().load(Ordering::Relaxed);
        let head = self.cq.head().load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail as usize) & (RING_DEPTH - 1);
        // SAFETY: idx is in-bounds; matches the volatile writer in the
        // kernel completion path.
        let status = unsafe { addr_of!((*self.cq.entries.add(idx)).status).read_volatile() };
        self.cq
            .tail()
            .store(tail.wrapping_add(1), Ordering::Release);
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
