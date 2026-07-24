//! Capability rights encoded at the type level (docs/KERNEL-RUST.md 2). The
//! kernel's runtime grant check stays the unforgeable guarantee; this is the
//! ergonomics layer that turns a wrong-rights access into a *compile* error
//! before it ever reaches the check.
//!
//! `Cap<T, R>` carries its rights in the type `R = Rights<MASK>`. Attenuation
//! (narrowing) is allowed and checked at compile time via `SubsetOf`;
//! widening does not type-check.

use core::marker::PhantomData;

pub const READ: u32 = 1 << 0;
pub const WRITE: u32 = 1 << 1;
pub const EXECUTE: u32 = 1 << 2;
pub const DELEGATE: u32 = 1 << 3;
pub const MAP: u32 = 1 << 4;

/// A zero-size witness of a rights bitmask.
#[derive(Copy, Clone, Debug)]
pub struct Rights<const MASK: u32>;

pub trait RightSet: Copy {
    const MASK: u32;
}
impl<const M: u32> RightSet for Rights<M> {
    const MASK: u32 = M;
}

/// Compile-time boolean assertion: `Assert<{cond}>: IsTrue` only holds when
/// `cond` is true, so a false condition fails to satisfy the bound.
pub struct Assert<const COND: bool>;
pub trait IsTrue {}
impl IsTrue for Assert<true> {}

/// `A` is a subset of `B` iff `A & B == A`. Used to gate attenuation.
pub trait SubsetOf<R: RightSet>: RightSet {}
impl<const A: u32, const B: u32> SubsetOf<Rights<B>> for Rights<A> where
    Assert<{ A & B == A }>: IsTrue
{
}

pub type ReadOnly = Rights<{ READ }>;
pub type ReadWrite = Rights<{ READ | WRITE }>;
pub type Executable = Rights<{ READ | EXECUTE }>;
pub type Full = Rights<{ READ | WRITE | EXECUTE | DELEGATE | MAP }>;

/// A typed handle to a kernel object. The rights live in `R`; the runtime
/// still validates the handle at use time (this only prevents the mistake
/// earlier, in the type checker).
pub struct Cap<T, R: RightSet> {
    handle: u64,
    _phantom: PhantomData<fn() -> (T, R)>,
}

impl<T, R: RightSet> Cap<T, R> {
    pub const fn from_handle(handle: u64) -> Cap<T, R> {
        Cap {
            handle,
            _phantom: PhantomData,
        }
    }

    pub const fn handle(&self) -> u64 {
        self.handle
    }

    /// The rights mask this capability's type carries.
    pub const fn mask(&self) -> u32 {
        R::MASK
    }

    /// Runtime check mirroring the kernel grant check (bit subset).
    pub fn allows(&self, right: u32) -> bool {
        R::MASK & right == right
    }

    /// Narrow the rights. A compile error if `R2` is not a subset of `R`.
    /// The handle is unchanged - narrowing is purely a type operation.
    pub fn attenuate<R2>(self) -> Cap<T, R2>
    where
        R2: RightSet + SubsetOf<R>,
    {
        Cap {
            handle: self.handle,
            _phantom: PhantomData,
        }
    }
}
