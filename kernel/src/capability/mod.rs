//! The capability core: mint, delegate, derive-subset, revoke-by-epoch,
//! grant-check (docs/ARCHITECTURE.md 8.2, docs/SECURITY-IDENTITY.md 2,
//! BUILD-ORDER.md step 4).
//!
//! Kept deliberately small and allocation-free: fixed-capacity tables,
//! plain arrays, no unsafe. This is the code the Verus proofs will cover;
//! until they exist, the four proof properties are exercised at runtime by
//! the cap-invariants test kernel:
//!
//! 1. Unforgeability - a handle not produced by mint/derive/delegate fails
//!    the grant check (generation counters catch stale and guessed handles).
//! 2. Monotonic attenuation - derive and delegate can never widen rights.
//! 3. Revocation soundness - after an object's epoch is bumped, every
//!    capability minted or derived under an older epoch fails.
//! 4. Isolation - capability tables are per-cell; no operation reaches an
//!    object without a capability in the calling cell's own table.
//!
//! The compile-time typed-rights layer from docs/KERNEL-RUST.md 2 lives in
//! `typed`; it wraps these runtime checks, it does not replace them.

pub mod typed;

// Rights bits and the budget sentinel (docs/KERNEL-RUST.md 2). Defined once in
// `rheo-abi` and re-exported under the kernel's short names, because a **cell**
// names them too now that `SYS_CAP_DERIVE` exists - restating them here would
// be the divergence class that crate deletes (docs/ARCHITECTURE-DEBT.md 3.1).
pub use rheo_abi::{
    BUDGET_UNLIMITED, RIGHT_ALL as ALL, RIGHT_DELEGATE as DELEGATE, RIGHT_EXECUTE as EXECUTE,
    RIGHT_MAP as MAP, RIGHT_READ as READ, RIGHT_REVOKE as REVOKE, RIGHT_WRITE as WRITE,
};

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CapError {
    /// Handle was never minted, was already consumed, or is stale.
    BadHandle,
    /// The object's epoch moved past the capability's epoch.
    Revoked,
    /// The capability lacks a required right.
    InsufficientRights,
    /// Attempt to derive or delegate with rights the parent lacks.
    WidenAttempt,
    /// Delegation without the DELEGATE right.
    NotDelegatable,
    /// The capability's metered budget ran out.
    Exhausted,
    /// No free slot in the destination table.
    TableFull,
    /// Object table is full.
    TooManyObjects,
}

/// Index into the kernel object table.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ObjectId(pub u32);

/// An unforgeable per-cell capability handle: slot index in the low bits,
/// slot generation in the high bits. Constructible only by this module.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Handle(u64);

impl Handle {
    fn new(slot: usize, generation: u64) -> Handle {
        Handle((generation << 16) | slot as u64)
    }

    fn slot(self) -> usize {
        (self.0 & 0xFFFF) as usize
    }

    fn generation(self) -> u64 {
        self.0 >> 16
    }

    /// For the unforgeability test only: a handle value picked by an
    /// attacker rather than returned by the kernel.
    ///
    /// Also the constructor the capability syscalls use on a cell-supplied
    /// handle, and that is exactly right: "forge" is what a cell does with
    /// every handle it passes, and the table's slot/generation/epoch checks are
    /// what make a forged one useless.
    pub fn forge(raw: u64) -> Handle {
        Handle(raw)
    }

    /// The full 64-bit handle a cell holds and passes back. Unlike
    /// [`Handle::raw_low32`] the generation is not truncated, so this is the
    /// form `SYS_CAP_DERIVE` returns.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// The 32-bit ABI form carried in a queue entry's `cap_id` field:
    /// slot in the low 16 bits, the generation's low 16 bits above it.
    /// The final IDL-generated ABI (BUILD-ORDER.md step 6) owns this
    /// packing; the truncated generation still catches slot reuse.
    pub fn raw_low32(self) -> u32 {
        ((self.generation() as u32 & 0xFFFF) << 16) | self.slot() as u32
    }
}

/// The conversion `rheo_abi::SqEntry::new` uses to accept a `Handle` directly:
/// a submission's `cap_id` field *is* the handle's 32-bit ABI form (the low 16
/// bits the slot, the next 16 the generation's low bits, reconstructed against
/// the table at check time; the IDL-generated ABI of BUILD-ORDER.md step 6 will
/// own this packing). Defined here rather than in `rheo-abi` because that crate
/// must not know about the capability table - it is the wire format, nothing
/// more.
impl From<Handle> for u32 {
    fn from(h: Handle) -> u32 {
        h.raw_low32()
    }
}

/// What a capability points at. The full kernel object model is
/// docs/ARCHITECTURE.md 3; only the kinds needed by current tests exist.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    MemoryGrant,
    QueuePair,
    /// An open file (docs/LIBRHEO.md Phase B). The async I/O layer will promote
    /// an fd to a first-class capability of this kind; today fds remain
    /// `svc::FileOps` handles carried in the queue-entry payload, so this kind
    /// is reserved for that promotion (a documented next step).
    File,
    /// A byte stream (console/pipe/socket) - reserved alongside `File`.
    Stream,
    /// An admission-checked CPU/memory reservation (docs/ARCHITECTURE.md 3
    /// object 7, docs/LIBRHEO.md Phase C). A cell holds this capability for a
    /// reservation admitted by the per-cell EDF controller; commit/query/release
    /// grant-check it.
    Reservation,
    /// A cell - a spawn/scheduling domain (docs/ARCHITECTURE.md 3 object 1). A
    /// capability of this kind carrying WRITE is the **cell-spawn authority**
    /// (`SYS_SPAWN`, docs/LIBRHEO.md Phase F): a cell without it cannot create
    /// cells (no ambient authority). Held by an orchestrator/shell; **not** in a
    /// spawned child's table - which is now true rather than aspirational,
    /// because a spawned child has its own table (docs/ARCHITECTURE-DEBT.md 2.3)
    /// and this capability is not among the few things minted into it.
    Cell,
}

impl ObjectKind {
    /// The stable ABI number a cell sees in [`rheo_abi::CapInfo::kind`]. Not
    /// the Rust discriminant: this crosses the ABI, so reordering the enum must
    /// not change what a cell reads.
    pub fn abi_code(self) -> u32 {
        match self {
            ObjectKind::MemoryGrant => rheo_abi::CAP_KIND_MEMORY_GRANT,
            ObjectKind::QueuePair => rheo_abi::CAP_KIND_QUEUE_PAIR,
            ObjectKind::File => rheo_abi::CAP_KIND_FILE,
            ObjectKind::Stream => rheo_abi::CAP_KIND_STREAM,
            ObjectKind::Reservation => rheo_abi::CAP_KIND_RESERVATION,
            ObjectKind::Cell => rheo_abi::CAP_KIND_CELL,
        }
    }
}

#[derive(Copy, Clone)]
struct Object {
    kind: ObjectKind,
    /// Bumped by revoke: every capability carrying an older epoch dies.
    epoch: u32,
}

// Total kernel objects (cells, grants, queues, ...) the system tracks. A
// fixed table indexed by a monotonic counter that does not yet reclaim a
// destroyed object's id (docs/TILES.md 12): a cell that creates and drops
// many grants over its life consumes ids until this cap. Raised 128 -> 512
// for headroom (an Object is 8 B, so 4 KiB) while the reclamation design -
// which must bump the object epoch to keep revocation sound (section 8.2) -
// remains future work. Real-workload sizing is flagged in docs/TILES.md 12.
const MAX_OBJECTS: usize = 512;

/// The kernel object table. One per system.
pub struct ObjectTable {
    objects: [Object; MAX_OBJECTS],
    next: usize,
}

impl ObjectTable {
    pub const fn new() -> ObjectTable {
        ObjectTable {
            objects: [Object {
                kind: ObjectKind::MemoryGrant,
                epoch: 0,
            }; MAX_OBJECTS],
            next: 0,
        }
    }

    /// Create a kernel object. The creating cell gets the root capability
    /// via `CapTable::mint`.
    pub fn create(&mut self, kind: ObjectKind) -> Result<ObjectId, CapError> {
        if self.next >= MAX_OBJECTS {
            return Err(CapError::TooManyObjects);
        }
        let id = self.next;
        self.objects[id] = Object { kind, epoch: 0 };
        self.next += 1;
        Ok(ObjectId(id as u32))
    }

    /// Revoke by epoch (docs/SECURITY-IDENTITY.md 3): one increment
    /// invalidates every outstanding capability to this object, O(1).
    pub fn revoke_epoch(&mut self, object: ObjectId) {
        self.objects[object.0 as usize].epoch += 1;
    }

    pub fn kind(&self, object: ObjectId) -> ObjectKind {
        self.objects[object.0 as usize].kind
    }

    /// The object's current epoch, for a caller that wants to observe a revoke
    /// having happened rather than infer it from a failing check
    /// (docs/ENGINEERING.md 1).
    pub fn epoch_of(&self, object: ObjectId) -> u32 {
        self.objects[object.0 as usize].epoch
    }

    #[inline(always)]
    fn epoch(&self, object: ObjectId) -> u32 {
        self.objects[object.0 as usize].epoch
    }
}

impl Default for ObjectTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Copy, Clone)]
struct CapSlot {
    object: u32,
    rights: u32,
    /// Object epoch at mint/derive time; a mismatch at check time = revoked.
    epoch: u32,
    /// Bumped on every free; makes stale handles to reused slots fail.
    generation: u64,
    budget: u64,
    in_use: bool,
}

const EMPTY_SLOT: CapSlot = CapSlot {
    object: 0,
    rights: 0,
    epoch: 0,
    generation: 0,
    budget: 0,
    in_use: false,
};

/// Capability slots a cell gets **inline**, before its table funds a frame
/// (docs/EXECUTION-MODEL.md 9.7).
///
/// This was `MAX_CAPS_PER_CELL = 256` and a fixed `[CapSlot; 256]` per cell, which at 16
/// cells is **131,072 bytes of `.bss`** - by a wide margin the largest fixed table left,
/// and a hard ceiling on how many objects one cell could ever reach. 32 covers the
/// ordinary cell at zero frames; past that the table grows into frames charged to the
/// cell itself, so the answer to "how many capabilities may a cell hold" is its budget.
pub const CAPS_INLINE: usize = 32;

/// A cell's capability table. The *only* path from a cell to a kernel
/// object - which is what makes the isolation lemma checkable: disjoint
/// tables mean disjoint reachable objects.
pub struct CapTable {
    slots: crate::mm::kmeta::Elastic<CapSlot, CAPS_INLINE>,
}

impl CapTable {
    pub const fn new() -> CapTable {
        CapTable {
            slots: crate::mm::kmeta::Elastic::with_inline([EMPTY_SLOT; CAPS_INLINE]),
        }
    }

    /// Charge this table's growth to `owner`, transferring anything it already holds.
    ///
    /// A capability table is built by whatever launches a cell and **adopted** by the
    /// cell at `install`, so the frames genuinely change hands - which is why
    /// `Funded::set_owner` transfers rather than relabels.
    pub fn set_owner(&mut self, owner: crate::mm::kmeta::Owner) {
        self.slots.set_owner(owner);
    }

    /// Frames this table holds beyond its inline half.
    pub fn frames_held(&self) -> usize {
        self.slots.frames_held()
    }

    /// Release the funded tail. **Every path that hands a cell slot back must call
    /// this**, or the frames leak until the next boot - the S1' scar, which this session
    /// hit a third time by teaching one release path and not its siblings.
    pub fn release(&mut self) {
        self.slots.reset(EMPTY_SLOT);
    }

    /// Slots addressable right now (the inline half plus any funded tail).
    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    fn slot(&self, i: usize) -> Option<&CapSlot> {
        self.slots.get(i)
    }

    fn slot_mut(&mut self, i: usize) -> Option<&mut CapSlot> {
        self.slots.get_mut(i)
    }

    fn alloc_slot(&mut self) -> Result<usize, CapError> {
        // Linear scan of what exists, then grow. `TableFull` now means "this cell's
        // budget refused another frame", which is attributable, rather than "the fixed
        // array is full", which was not (docs/MEMORY.md 7, no OOM killer).
        self.slots
            .alloc(EMPTY_SLOT, |s| !s.in_use)
            .ok_or(CapError::TableFull)
    }

    /// Mint the root capability for an object into this table. In the full
    /// system only object creation mints; there is no other way to obtain
    /// rights not derived from an existing capability (unforgeability).
    pub fn mint(
        &mut self,
        objects: &ObjectTable,
        object: ObjectId,
        rights: u32,
        budget: u64,
    ) -> Result<Handle, CapError> {
        let slot = self.alloc_slot()?;
        let generation = self.slot(slot).map_or(0, |s| s.generation) + 1;
        let write = CapSlot {
            object: object.0,
            rights,
            epoch: objects.epoch(object),
            generation,
            budget,
            in_use: true,
        };
        if !self.slots.set(slot, write) {
            return Err(CapError::TableFull);
        }
        Ok(Handle::new(slot, generation))
    }

    /// The hot path (P1 in docs/ARCHITECTURE.md 8.4): validate a handle
    /// against required rights. A handful of loads and compares; every
    /// check below is load-bearing for one of the four properties.
    #[inline(always)]
    pub fn grant_check(
        &mut self,
        objects: &ObjectTable,
        handle: Handle,
        required: u32,
    ) -> Result<ObjectId, CapError> {
        let index = handle.slot();
        let Some(slot) = self.slot_mut(index) else {
            return Err(CapError::BadHandle);
        };
        if !slot.in_use || slot.generation != handle.generation() {
            return Err(CapError::BadHandle);
        }
        if slot.rights & required != required {
            return Err(CapError::InsufficientRights);
        }
        let object = ObjectId(slot.object);
        if slot.epoch != objects.epoch(object) {
            return Err(CapError::Revoked);
        }
        if slot.budget != BUDGET_UNLIMITED {
            if slot.budget == 0 {
                return Err(CapError::Exhausted);
            }
            slot.budget -= 1;
        }
        Ok(object)
    }

    /// Grant check on the 32-bit ABI form of a handle (queue entries).
    /// Same checks as `grant_check`, with the generation compared on its
    /// low 16 bits as packed by `Handle::raw_low32`.
    #[inline(always)]
    pub fn grant_check_low32(
        &mut self,
        objects: &ObjectTable,
        cap_id: u32,
        required: u32,
    ) -> Result<ObjectId, CapError> {
        let index = (cap_id & 0xFFFF) as usize;
        let Some(slot) = self.slot_mut(index) else {
            return Err(CapError::BadHandle);
        };
        if !slot.in_use || slot.generation as u32 & 0xFFFF != cap_id >> 16 {
            return Err(CapError::BadHandle);
        }
        if slot.rights & required != required {
            return Err(CapError::InsufficientRights);
        }
        let object = ObjectId(slot.object);
        if slot.epoch != objects.epoch(object) {
            return Err(CapError::Revoked);
        }
        if slot.budget != BUDGET_UNLIMITED {
            if slot.budget == 0 {
                return Err(CapError::Exhausted);
            }
            slot.budget -= 1;
        }
        Ok(object)
    }

    /// Derive a child capability with a subset of the parent's rights.
    /// Widening is structurally impossible: the subset check runs against
    /// the parent's stored rights, never against caller-supplied state.
    pub fn derive_subset(
        &mut self,
        objects: &ObjectTable,
        parent: Handle,
        rights: u32,
        budget: u64,
    ) -> Result<Handle, CapError> {
        let object = self.grant_check(objects, parent, 0)?;
        let parent_rights = self.slot(parent.slot()).ok_or(CapError::BadHandle)?.rights;
        if rights & parent_rights != rights {
            return Err(CapError::WidenAttempt);
        }
        self.mint_derived(objects, object, rights, budget)
    }

    /// Move a capability to another cell's table (consumes it here).
    /// Requires DELEGATE; rights transfer unchanged - narrow first with
    /// `derive_subset` when the receiver should get less.
    pub fn delegate(
        &mut self,
        objects: &ObjectTable,
        handle: Handle,
        target: &mut CapTable,
    ) -> Result<Handle, CapError> {
        let object = self.grant_check(objects, handle, 0)?;
        let slot = self.slot(handle.slot()).ok_or(CapError::BadHandle)?;
        if slot.rights & DELEGATE == 0 {
            return Err(CapError::NotDelegatable);
        }
        let (rights, budget) = (slot.rights, slot.budget);
        let new = target.mint_derived(objects, object, rights, budget)?;
        self.free(handle);
        Ok(new)
    }

    /// Drop a capability (RAII release in the typed layer).
    pub fn free(&mut self, handle: Handle) {
        let index = handle.slot();
        if let Some(slot) = self.slot_mut(index)
            && slot.in_use
            && slot.generation == handle.generation()
        {
            slot.in_use = false;
        }
    }

    // -------------------------------------------------------------------
    // The 32-bit ABI form (docs/ARCHITECTURE-DEBT.md 2.1)
    // -------------------------------------------------------------------
    //
    // A `Handle` is the kernel's 64-bit form; `Handle::raw_low32` is the form
    // that crosses the ABI, and it is the *only* form a cell ever receives -
    // `SYS_QUEUE_INFO`, `SYS_GRANT`, `SYS_CONNECT` and every queue entry use
    // it. So the capability verbs take it too. Making them take the 64-bit
    // form instead would have been a quiet trap: the two are numerically equal
    // while a slot's generation stays below 2^16, so it would work in every
    // test and start failing after 65536 reuses of one slot.

    /// Resolve the 32-bit ABI form to a live slot, spending no budget.
    ///
    /// Checks the same three things `grant_check_low32` does - the slot is in
    /// use, the truncated generation matches, the object's epoch has not
    /// moved. Nothing else: those three are exactly what establishes that this
    /// handle still names that object.
    fn slot_of_low32(&self, objects: &ObjectTable, cap_id: u32) -> Result<usize, CapError> {
        let index = (cap_id & 0xFFFF) as usize;

        let slot = self.slot(index).ok_or(CapError::BadHandle)?;
        if !slot.in_use || slot.generation as u32 & 0xFFFF != cap_id >> 16 {
            return Err(CapError::BadHandle);
        }
        if slot.epoch != objects.epoch(ObjectId(slot.object)) {
            return Err(CapError::Revoked);
        }
        Ok(index)
    }

    /// What a capability carries, **without spending budget**.
    ///
    /// Deliberately not `grant_check(handle, 0)`: that decrements a finite
    /// budget, so introspecting a metered capability would consume it - looking
    /// at a capability would change it. Returns `(object, rights, budget)`; a
    /// revoked or stale handle is an error, so a cell can use this to *observe*
    /// a revoke rather than infer it from a later failure
    /// (docs/ENGINEERING.md 1).
    pub fn inspect_low32(
        &self,
        objects: &ObjectTable,
        cap_id: u32,
    ) -> Result<(ObjectId, u32, u64), CapError> {
        let i = self.slot_of_low32(objects, cap_id)?;
        let Some(s) = self.slot(i) else {
            return Err(CapError::BadHandle);
        };
        Ok((ObjectId(s.object), s.rights, s.budget))
    }

    /// [`CapTable::derive_subset`] on the 32-bit ABI form, returning the same
    /// form. Widening is refused exactly as it is there - the subset test runs
    /// against the parent's *stored* rights, never against anything the caller
    /// supplied.
    ///
    /// Unlike `derive_subset` this does not spend a budget unit of the parent:
    /// a metered capability meters *uses of the object*, and deriving reaches
    /// the object no more than inspecting does.
    pub fn derive_subset_low32(
        &mut self,
        objects: &ObjectTable,
        parent: u32,
        rights: u32,
        budget: u64,
    ) -> Result<u32, CapError> {
        let i = self.slot_of_low32(objects, parent)?;
        let (parent_rights, object) = {
            let p = self.slot(i).ok_or(CapError::BadHandle)?;
            (p.rights, ObjectId(p.object))
        };
        if rights & parent_rights != rights {
            return Err(CapError::WidenAttempt);
        }
        // A derivation may narrow the budget but never exceed the parent's
        // remaining one - otherwise metering would be escapable by deriving.
        let parent_budget = self.slot(i).ok_or(CapError::BadHandle)?.budget;
        if parent_budget != BUDGET_UNLIMITED
            && (budget == BUDGET_UNLIMITED || budget > parent_budget)
        {
            return Err(CapError::WidenAttempt);
        }
        self.mint_derived(objects, object, rights, budget)
            .map(|h| h.raw_low32())
    }

    /// Release the 32-bit ABI form from this table. Reports whether it named a
    /// live capability, so a double drop is visible rather than a silent
    /// success (docs/ENGINEERING.md 7).
    pub fn free_low32(&mut self, objects: &ObjectTable, cap_id: u32) -> Result<(), CapError> {
        let i = self.slot_of_low32(objects, cap_id)?;
        if let Some(x) = self.slot_mut(i) {
            x.in_use = false;
        }
        Ok(())
    }

    /// Replace this table's contents with a copy of `other`'s - the `fork`
    /// inheritance step (docs/POSIX-PERSONALITY.md 2, docs/ARCHITECTURE-DEBT.md
    /// 2.3).
    ///
    /// A **copy**, not a shared pointer, which is what makes the child's table
    /// its own: the parent dropping or deriving afterwards does not change what
    /// the child holds. Epoch revocation still reaches both, because that lives
    /// on the *object*, not on the table - which is exactly the fork semantics
    /// POSIX describes for descriptors (the table is copied, the thing behind it
    /// is shared).
    /// Returns false when the child's table could not be grown to hold the parent's -
    /// a `fork` the caller must **refuse**, not complete with a truncated table. A
    /// silently short capability table is a cell missing authority it believes it has,
    /// which is worse than a refused fork.
    ///
    /// A deep copy, slot by slot. It used to be `self.slots = other.slots`, a raw copy of
    /// the whole array - which becomes a copy of a `Funded` *descriptor* the moment the
    /// table is funded, giving two owners of one directory frame. That is the S1' scar
    /// exactly (`fork`'s `copy_nonoverlapping` of `LinuxState`), and it is why `Elastic`
    /// is not `Copy`: the compiler refuses the old line rather than letting it compile
    /// into a double free.
    #[must_use]
    pub fn copy_from(&mut self, other: &CapTable) -> bool {
        self.release();
        for i in 0..other.capacity() {
            let Some(&src) = other.slot(i) else { continue };
            if !src.in_use {
                continue;
            }
            if self.slots.grow_to(i, EMPTY_SLOT).is_none() || !self.slots.set(i, src) {
                return false;
            }
        }
        true
    }

    /// Empty this table. Used when a cell slot is reused, so a new cell can
    /// never inherit a dead one's capabilities by accident.
    pub fn clear(&mut self) {
        self.slots.reset(EMPTY_SLOT);
    }

    /// Count of live capabilities (used by tests).
    pub fn live_count(&self) -> usize {
        (0..self.capacity())
            .filter(|&i| self.slot(i).is_some_and(|s| s.in_use))
            .count()
    }

    /// True if this table holds a live capability of `kind` carrying all of
    /// `required` rights, its object un-revoked. The cell-spawn authority check
    /// (docs/LIBRHEO.md Phase F): `SYS_SPAWN` requires an `ObjectKind::Cell`
    /// capability with WRITE, so a cell without one cannot create cells (no
    /// ambient authority). A read-only scan; no budget is decremented.
    pub fn holds(&self, objects: &ObjectTable, kind: ObjectKind, required: u32) -> bool {
        (0..self.capacity()).any(|i| {
            let Some(s) = self.slot(i) else { return false };
            if !s.in_use || s.rights & required != required {
                return false;
            }
            let obj = ObjectId(s.object);
            objects.kind(obj) == kind && s.epoch == objects.epoch(obj)
        })
    }

    fn mint_derived(
        &mut self,
        objects: &ObjectTable,
        object: ObjectId,
        rights: u32,
        budget: u64,
    ) -> Result<Handle, CapError> {
        let slot = self.alloc_slot()?;
        let generation = self.slot(slot).map_or(0, |s| s.generation) + 1;
        let write = CapSlot {
            object: object.0,
            rights,
            epoch: objects.epoch(object),
            generation,
            budget,
            in_use: true,
        };
        if !self.slots.set(slot, write) {
            return Err(CapError::TableFull);
        }
        Ok(Handle::new(slot, generation))
    }
}

impl Default for CapTable {
    fn default() -> Self {
        Self::new()
    }
}
