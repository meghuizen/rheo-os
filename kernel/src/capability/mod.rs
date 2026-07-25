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

/// Rights bits (docs/KERNEL-RUST.md 2).
pub const READ: u32 = 1 << 0;
pub const WRITE: u32 = 1 << 1;
pub const EXECUTE: u32 = 1 << 2;
pub const DELEGATE: u32 = 1 << 3;
pub const MAP: u32 = 1 << 4;

/// "No budget metering" sentinel. A finite budget is decremented by every
/// successful grant check and exhausts to `Exhausted`.
pub const BUDGET_UNLIMITED: u64 = u64::MAX;

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
    pub fn forge(raw: u64) -> Handle {
        Handle(raw)
    }

    /// The 32-bit ABI form carried in a queue entry's `cap_id` field:
    /// slot in the low 16 bits, the generation's low 16 bits above it.
    /// The final IDL-generated ABI (BUILD-ORDER.md step 6) owns this
    /// packing; the truncated generation still catches slot reuse.
    pub fn raw_low32(self) -> u32 {
        ((self.generation() as u32 & 0xFFFF) << 16) | self.slot() as u32
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
}

#[derive(Copy, Clone)]
struct Object {
    kind: ObjectKind,
    /// Bumped by revoke: every capability carrying an older epoch dies.
    epoch: u32,
}

const MAX_OBJECTS: usize = 128;

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

pub const MAX_CAPS_PER_CELL: usize = 256;

/// A cell's capability table. The *only* path from a cell to a kernel
/// object - which is what makes the isolation lemma checkable: disjoint
/// tables mean disjoint reachable objects.
pub struct CapTable {
    slots: [CapSlot; MAX_CAPS_PER_CELL],
}

impl CapTable {
    pub const fn new() -> CapTable {
        CapTable {
            slots: [EMPTY_SLOT; MAX_CAPS_PER_CELL],
        }
    }

    fn alloc_slot(&mut self) -> Result<usize, CapError> {
        // Linear scan is fine at this size; a free list comes with scale.
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.in_use {
                return Ok(i);
            }
        }
        Err(CapError::TableFull)
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
        let generation = self.slots[slot].generation + 1;
        self.slots[slot] = CapSlot {
            object: object.0,
            rights,
            epoch: objects.epoch(object),
            generation,
            budget,
            in_use: true,
        };
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
        if index >= MAX_CAPS_PER_CELL {
            return Err(CapError::BadHandle);
        }
        let slot = &mut self.slots[index];
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
        if index >= MAX_CAPS_PER_CELL {
            return Err(CapError::BadHandle);
        }
        let slot = &mut self.slots[index];
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
        let parent_rights = self.slots[parent.slot()].rights;
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
        let slot = &self.slots[handle.slot()];
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
        if index < MAX_CAPS_PER_CELL
            && self.slots[index].in_use
            && self.slots[index].generation == handle.generation()
        {
            self.slots[index].in_use = false;
        }
    }

    /// Count of live capabilities (used by tests).
    pub fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.in_use).count()
    }

    fn mint_derived(
        &mut self,
        objects: &ObjectTable,
        object: ObjectId,
        rights: u32,
        budget: u64,
    ) -> Result<Handle, CapError> {
        let slot = self.alloc_slot()?;
        let generation = self.slots[slot].generation + 1;
        self.slots[slot] = CapSlot {
            object: object.0,
            rights,
            epoch: objects.epoch(object),
            generation,
            budget,
            in_use: true,
        };
        Ok(Handle::new(slot, generation))
    }
}

impl Default for CapTable {
    fn default() -> Self {
        Self::new()
    }
}
