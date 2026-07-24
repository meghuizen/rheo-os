//! Compile-time typed rights (docs/KERNEL-RUST.md 2): wrong-capability
//! accesses become compile errors before they ever reach the runtime
//! grant check. The runtime check still runs - this layer adds ergonomics
//! and an extra static guarantee, it replaces nothing.

use core::marker::PhantomData;

use super::{BUDGET_UNLIMITED, CapError, CapTable, Handle, ObjectId, ObjectTable};

/// Rights as a const-generic bitmask; a zero-size type with no runtime cost.
#[derive(Copy, Clone, Debug)]
pub struct Rights<const MASK: u32>;

pub trait RightSet: Copy {
    const MASK: u32;
}

impl<const M: u32> RightSet for Rights<M> {
    const MASK: u32 = M;
}

/// Compile-time subset check: A is a subset of B iff (A & B) == A.
pub struct Assert<const COND: bool>;
pub trait IsTrue {}
impl IsTrue for Assert<true> {}

pub trait SubsetOf<R: RightSet>: RightSet {}
impl<const A: u32, const B: u32> SubsetOf<Rights<B>> for Rights<A> where
    Assert<{ A & B == A }>: IsTrue
{
}

/// Convenience aliases (docs/KERNEL-RUST.md 2).
pub type ReadOnly<T> = Capability<T, Rights<{ super::READ }>>;
pub type ReadWrite<T> = Capability<T, Rights<{ super::READ | super::WRITE }>>;
pub type Delegatable<T> = Capability<T, Rights<{ super::READ | super::DELEGATE }>>;
pub type Full<T> = Capability<
    T,
    Rights<{ super::READ | super::WRITE | super::EXECUTE | super::DELEGATE | super::MAP }>,
>;

/// An unforgeable, typed handle to a kernel resource.
/// Non-Clone by design: the move is the transfer.
pub struct Capability<T, R: RightSet> {
    handle: Handle,
    _phantom: PhantomData<fn() -> (T, R)>,
}

impl<T, R: RightSet> Capability<T, R> {
    /// Mint with exactly this type's rights mask.
    pub fn mint(
        table: &mut CapTable,
        objects: &ObjectTable,
        object: ObjectId,
    ) -> Result<Self, CapError> {
        let handle = table.mint(objects, object, R::MASK, BUDGET_UNLIMITED)?;
        Ok(Capability {
            handle,
            _phantom: PhantomData,
        })
    }

    /// Attenuation: narrow the rights. Widening is a *compile* error -
    /// `R2: SubsetOf<R>` cannot be satisfied if R2 has bits R lacks.
    /// No kernel call needed: the handle is unchanged, the narrower type
    /// prevents misuse statically, and the kernel still validates at use.
    #[inline]
    pub fn attenuate<R2>(self) -> Capability<T, R2>
    where
        R2: RightSet + SubsetOf<R>,
    {
        Capability {
            handle: self.handle,
            _phantom: PhantomData,
        }
    }

    /// Runtime grant check for exactly this type's rights mask.
    #[inline(always)]
    pub fn grant_check(
        &self,
        table: &mut CapTable,
        objects: &ObjectTable,
    ) -> Result<ObjectId, CapError> {
        table.grant_check(objects, self.handle, R::MASK)
    }

    /// Escape hatch to the raw handle (kernel-internal plumbing).
    pub fn raw(&self) -> Handle {
        self.handle
    }
}
