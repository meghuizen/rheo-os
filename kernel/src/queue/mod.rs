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

use crate::capability::{CapError, CapTable, ObjectKind, ObjectTable, READ, WRITE};

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

// ---- the on-wire layout: one definition, re-exported ----
//
// Opcodes, status codes, flags, the entry layouts and the ring header are the
// **cross-crate** contract, so they live in `rheo-abi` and are re-exported here
// unchanged (docs/ARCHITECTURE-DEBT.md 3.1). They used to be restated in
// `librheo/src/sys.rs` by hand; a field-meaning change on one side produced
// wrong numbers with no fault. `pub use` keeps every existing path
// (`queue::SqEntry`, `queue::OP_NOP`, ...) working.
pub use rheo_abi::{
    CqEntry, FLAG_DUR_FLUSH, FLAG_DUR_FUA, FLAG_INLINE, INLINE_MAX, OP_CHAN_MSG, OP_CLOSE, OP_ECHO,
    OP_FSTAT, OP_GPU_PRESENT, OP_GRAPH_SUBMIT, OP_NET_MAC, OP_NET_RX, OP_NET_TX, OP_NOP, OP_OPEN,
    OP_READ, OP_WRITE, QUEUE_ABI_VERSION, QueueHeader, RING_DEPTH, STATUS_BAD_HANDLE,
    STATUS_BAD_OPCODE, STATUS_DENIED, STATUS_EXHAUSTED, STATUS_IO, STATUS_OK, STATUS_REVOKED,
    SqEntry,
};

// `DmaSafe` is this crate's trait, so implementing it for the ABI's entry types
// is allowed and stays where the DMA rule is enforced.
// SAFETY: both are `repr(C)` plain data - no pointers, valid for every bit
// pattern a device could write.
unsafe impl DmaSafe for SqEntry {}
// SAFETY: as above.
unsafe impl DmaSafe for CqEntry {}

/// Distinct completion status for each capability-check failure, so a cell can
/// tell revoked from exhausted from denied (docs/LIBRHEO.md Phase B) rather
/// than collapsing every `CapError` to [`STATUS_DENIED`].
fn cap_status(e: CapError) -> u32 {
    match e {
        CapError::BadHandle => STATUS_BAD_HANDLE,
        CapError::Revoked => STATUS_REVOKED,
        CapError::Exhausted => STATUS_EXHAUSTED,
        _ => STATUS_DENIED,
    }
}

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
            addr_of_mut!((*h).reserved).write([0; 8]);
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

/// The right a given opcode's capability must carry (docs/LIBRHEO.md Phase B):
/// reads need READ, mutating ops need WRITE - the hardcoded-WRITE of Phase A is
/// gone. The queue capability itself is minted READ|WRITE, so both pass; the
/// per-opcode gate is what a *narrowed* (read-only) queue cap would enforce.
fn opcode_right(opcode: u8) -> u32 {
    match opcode {
        OP_READ | OP_FSTAT | OP_OPEN | OP_CLOSE | OP_NET_RX | OP_NET_MAC => READ,
        _ => WRITE, // OP_NOP, OP_ECHO, OP_WRITE, OP_NET_TX, unknown
    }
}

/// The kernel side of the doorbell: drain the submission ring, grant-check
/// every entry against the submitting cell's capability table (with the right
/// the opcode requires), execute, and push completions. Flow context
/// propagates unchanged - observability the system cannot fail to produce
/// (docs/ARCHITECTURE.md 3, object 10).
///
/// **What the per-entry check does and does not gate** (docs/ARCHITECTURE-DEBT.md
/// 2.6, stated here because the earlier wording overclaimed it). `entry.cap_id`
/// is chosen by the *cell*, so the check establishes exactly three things: the
/// id names a **live** capability in this cell's own table (unforgeability), it
/// carries the right this opcode needs, and its epoch is current (so a revoke
/// kills it). It additionally now requires the capability to name a
/// **`QueuePair`** object - the id is a *queue* reference, and passing, say, a
/// MemoryGrant id used to satisfy the check because the resolved object was
/// discarded.
///
/// It does **not** gate the resource the opcode reaches. A cell holding a queue
/// pair can still `OP_NET_TX` an arbitrary Ethernet frame, `OP_NET_RX` any
/// received one, `OP_GPU_PRESENT`, or `OP_OPEN` any path the registered
/// `svc::FileOps` will open: those resources have no capability of their own yet.
/// Closing that needs per-resource object kinds (a socket kind + NIC steering
/// grants, a file kind for fds) - honestly deferred, docs/NETWORKING.md and
/// docs/ARCHITECTURE-DEBT.md 2.1/2.6 - and the deferral is stated rather than
/// papered over by a check that looks like it covers more than it does.
///
/// The async I/O opcodes (`OP_OPEN`/`READ`/`WRITE`/`CLOSE`/`FSTAT`,
/// docs/LIBRHEO.md Phase B) run their file work through the registered
/// `svc::FileOps`. They take user VAs from the payload, valid here because the
/// submitting cell's address space is active during its `SYS_DOORBELL` trap -
/// so a large read/write lands directly in the cell's mapped pages (zero-copy).
///
/// Returns the number of entries processed.
pub fn kernel_process(qp: &QueuePair, caps: &mut CapTable, objects: &ObjectTable) -> usize {
    let mut processed = 0;
    while let Some(entry) = qp.sq.pop() {
        let (status, result) =
            match caps.grant_check_low32(objects, entry.cap_id, opcode_right(entry.opcode)) {
                Err(e) => (cap_status(e), 0),
                // The resolved object must be the queue itself. Discarding it -
                // which this used to do - let any live capability the cell held
                // satisfy the check, so a MemoryGrant id worked as well as the
                // queue's (docs/ARCHITECTURE-DEBT.md 2.6).
                Ok(object) if objects.kind(object) != ObjectKind::QueuePair => (STATUS_DENIED, 0),
                Ok(_) => run_opcode(&entry),
            };
        // The queue window (docs/OBSERVABILITY.md 11.4): one record per completed
        // submission, here because this is the single point every one passes through.
        // `a` packs the opcode with its status, so "which operations are being refused"
        // is a group-by rather than a correlation.
        //
        // `b` is the **low 64 bits** of the flow id - a truncation, and said so rather
        // than narrowed quietly. A flow id is 16 bytes because it is W3C-traceparent
        // shaped (docs/OBSERVABILITY.md 2), and carrying it whole would take both of a
        // 32-byte record's payload fields and so cost the opcode and status. Within one
        // boot the low half identifies a flow perfectly well, and an exporter that needs
        // the full id has the submission entry itself, where it is not truncated.
        crate::obs_event!(
            crate::obs::Window::Queue,
            crate::obs::Kind::Exit,
            crate::obs::OWNER_KERNEL,
            (entry.opcode as u64) << 32 | status as u64,
            entry.flow_id as u64
        );
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

/// Read a little-endian u32 at constant offset `o` in a 24-byte payload.
/// Fixed offsets on a `[u8; 24]` let the compiler prove the reads in-bounds,
/// so no panic path is emitted (the U-mode / kernel out-of-line-call rule).
#[inline(always)]
fn rd_u32(p: &[u8; 24], o: usize) -> u32 {
    u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]])
}
#[inline(always)]
fn rd_u64(p: &[u8; 24], o: usize) -> u64 {
    u64::from_le_bytes([
        p[o],
        p[o + 1],
        p[o + 2],
        p[o + 3],
        p[o + 4],
        p[o + 5],
        p[o + 6],
        p[o + 7],
    ])
}

/// Execute one grant-checked submission and return `(status, result)`.
///
/// A submission's payload carries **cell-supplied addresses** (buffers, paths,
/// descriptors), and the cell's address space is active while the kernel drains
/// the ring - so each one is bound to that cell's user VA range before it is
/// handed to a `svc::FileOps` handler or a device driver. A rejected address
/// completes `STATUS_DENIED`: no dereference, no fault, no panic
/// (docs/ENGINEERING.md 12).
fn run_opcode(entry: &SqEntry) -> (u32, u32) {
    let p = &entry.payload;
    // Validated cell buffer, or an early `STATUS_DENIED` completion.
    macro_rules! ck {
        ($e:expr) => {
            match $e {
                Some(v) => v,
                None => return (STATUS_DENIED, 0),
            }
        };
    }
    match entry.opcode {
        OP_NOP => (STATUS_OK, 0),
        OP_ECHO => (STATUS_OK, rd_u32(p, 0)),
        OP_OPEN => {
            let path_va = rd_u64(p, 0);
            let path_len = rd_u32(p, 8) as u64;
            let flags = rd_u32(p, 12) as u64;
            ck!(crate::user::user_buf(path_va, path_len as usize));
            io_result(crate::svc::file_ops().map(|o| (o.open)(path_va, path_len, flags)))
        }
        OP_READ => {
            let buf_va = rd_u64(p, 0);
            let offset = rd_u64(p, 8);
            let len = rd_u32(p, 16) as u64;
            let fd = rd_u32(p, 20) as u64;
            ck!(crate::user::user_buf_mut(buf_va, len as usize));
            io_result(crate::svc::file_ops().map(|o| {
                (o.lseek)(fd, offset as i64, 0); // SEEK_SET (positional)
                (o.read)(fd, buf_va, len)
            }))
        }
        OP_WRITE => {
            let r = if entry.flags & FLAG_INLINE != 0 {
                // Sub-threshold: the bytes ride in the payload after
                // `[fd u32][len u32]`. Pass the payload address as `buf_va`
                // (a kernel VA readable during the drain) - no user buffer.
                let fd = rd_u32(p, 0) as u64;
                let len = (rd_u32(p, 4) as usize).min(INLINE_MAX);
                let src = p[8..].as_ptr() as u64;
                crate::svc::file_ops().map(|o| (o.write)(fd, src, len as u64))
            } else {
                let buf_va = rd_u64(p, 0);
                let offset = rd_u64(p, 8);
                let len = rd_u32(p, 16) as u64;
                let fd = rd_u32(p, 20) as u64;
                ck!(crate::user::user_buf(buf_va, len as usize));
                crate::svc::file_ops().map(|o| {
                    (o.lseek)(fd, offset as i64, 0);
                    (o.write)(fd, buf_va, len)
                })
            };
            io_result(r)
        }
        OP_CLOSE => {
            let fd = rd_u32(p, 0) as u64;
            io_result(crate::svc::file_ops().map(|o| (o.close)(fd)))
        }
        OP_FSTAT => {
            let statbuf_va = rd_u64(p, 0);
            let fd = rd_u32(p, 8) as u64;
            ck!(crate::user::user_out::<crate::abi::Stat>(statbuf_va));
            io_result(crate::svc::file_ops().map(|o| (o.fstat)(fd, statbuf_va)))
        }
        OP_GRAPH_SUBMIT => {
            let nodes_va = rd_u64(p, 0);
            let count = rd_u32(p, 8);
            let results_va = rd_u64(p, 12);
            // `graph_submit` validates both arrays (and every descriptor VA
            // inside them) against the submitting cell's user VA range itself,
            // because only it knows each extent.
            // SAFETY: the submitting cell's address space is active during the
            // drain, which is this function's contract.
            crate::svc::graph_submit(nodes_va, count, results_va)
        }
        // Raw-frame networking (docs/NETWORKING.md, LIBRHEO.md Phase G). Reached
        // through the `svc::NicOps` **bridge**, not by naming a driver: device
        // drivers live permanently outside the kernel (ARCHITECTURE.md 5), so
        // this arm must not know that the NIC is virtio-net any more than
        // `OP_OPEN` above knows the filesystem is ext4
        // (docs/ARCHITECTURE-DEBT.md 3.2). The datapath **DMA-reads** the
        // transmit buffer, so an unvalidated VA here would hand kernel memory to
        // a device - the range check stays on this side of the bridge, where the
        // submitting cell's address space is known.
        OP_NET_TX => {
            let buf_va = rd_u64(p, 0);
            let len = rd_u32(p, 8) as u64;
            ck!(crate::user::user_buf(buf_va, len as usize));
            bridged(crate::svc::nic_ops().map(|o| (o.tx)(buf_va, len)))
        }
        OP_NET_RX => {
            let buf_va = rd_u64(p, 0);
            let len = rd_u32(p, 8) as u64;
            ck!(crate::user::user_buf_mut(buf_va, len as usize));
            bridged(crate::svc::nic_ops().map(|o| (o.rx)(buf_va, len)))
        }
        OP_NET_MAC => {
            let buf_va = rd_u64(p, 0);
            ck!(crate::user::user_buf_mut(buf_va, 6));
            bridged(crate::svc::nic_ops().map(|o| (o.mac)(buf_va)))
        }
        // GPU 2D present (docs/LIBRHEO.md Phase H): through the `svc::DisplayOps`
        // bridge, for the same reason. The datapath copies `w*h*4` bytes out of
        // the cell's framebuffer, so the extent is checked here.
        OP_GPU_PRESENT => {
            let buf_va = rd_u64(p, 0);
            let w = rd_u32(p, 8);
            let h = rd_u32(p, 12);
            let fb_len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
            ck!(crate::user::user_buf(buf_va, fb_len));
            bridged(crate::svc::display_ops().map(|o| (o.present)(buf_va, w, h)))
        }
        _ => (STATUS_BAD_OPCODE, 0),
    }
}

/// A device bridge's result, or [`STATUS_IO`] when no bridge is registered. The
/// sibling of [`io_result`] for the device tables: a kernel built with no netdev
/// or no display genuinely cannot serve the opcode, and saying so is the honest
/// answer (docs/ENGINEERING.md 7) - never a silent success.
fn bridged(r: Option<(u32, u32)>) -> (u32, u32) {
    r.unwrap_or((STATUS_IO, 0))
}

/// Map a file op's `Option<i64>` (None = no personality handler) into a
/// completion `(status, result)`: a non-negative result is `STATUS_OK` with the
/// value (fd / byte count); a negative errno or a missing handler is
/// `STATUS_IO`.
fn io_result(r: Option<i64>) -> (u32, u32) {
    match r {
        Some(n) if n >= 0 => (STATUS_OK, n as u32),
        _ => (STATUS_IO, 0),
    }
}
