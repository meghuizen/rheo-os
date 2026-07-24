//! Locking, adapted to the strand model (docs/CONCURRENCY.md 6).
//!
//! The primary lock is an **async `Mutex`**: a contended `lock().await` parks
//! the strand and yields the vcore to another strand, instead of blocking the
//! vcore in the kernel (a futex) or spinning. Unlocking hands off to the next
//! waiter. This is the "blocking is structural" rule applied to mutual
//! exclusion - the vcore is never lost to a held lock.
//!
//! `TicketLock` is a fair spin lock for the *future* multi-vcore case (true
//! parallelism, SMP). It must never guard against another strand on the *same*
//! vcore: single-vcore, spinning cannot make progress because the holder
//! cannot run to release it - use the async `Mutex` there. It is included as
//! the SMP-ready primitive and tested only uncontended.

use crate::strand::{complete, next_token, park_on};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use core::cell::{RefCell, UnsafeCell};
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

// ------------------------------------------------------------- async Mutex

struct MutexState {
    locked: bool,
    waiters: VecDeque<u64>,
}

struct MutexInner<T> {
    data: UnsafeCell<T>,
    state: RefCell<MutexState>,
}

/// An async mutex. Cheap to share across strands (clone the handle). A
/// contended `lock().await` parks the strand; `unlock` (guard drop) wakes the
/// next waiter.
pub struct Mutex<T> {
    inner: Rc<MutexInner<T>>,
}

impl<T> Clone for Mutex<T> {
    fn clone(&self) -> Mutex<T> {
        Mutex {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Mutex<T> {
        Mutex {
            inner: Rc::new(MutexInner {
                data: UnsafeCell::new(value),
                state: RefCell::new(MutexState {
                    locked: false,
                    waiters: VecDeque::new(),
                }),
            }),
        }
    }

    /// Acquire the lock, parking the strand while it is held elsewhere.
    pub async fn lock(&self) -> MutexGuard<T> {
        loop {
            let token = {
                let mut s = self.inner.state.borrow_mut();
                if !s.locked {
                    s.locked = true;
                    return MutexGuard {
                        inner: self.inner.clone(),
                    };
                }
                let token = next_token();
                s.waiters.push_back(token);
                token
            };
            park_on(token).await;
        }
    }
}

/// RAII guard. Unlocks and wakes the next waiter on drop.
pub struct MutexGuard<T> {
    inner: Rc<MutexInner<T>>,
}

impl<T> Deref for MutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: `locked` guarantees this is the only live guard, and the
        // executor is single-vcore cooperative, so no other strand touches the
        // data until this guard is dropped (even across await points, because
        // the lock stays held).
        unsafe { &*self.inner.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref` - unique access while locked.
        unsafe { &mut *self.inner.data.get() }
    }
}

impl<T> Drop for MutexGuard<T> {
    fn drop(&mut self) {
        let waiter = {
            let mut s = self.inner.state.borrow_mut();
            s.locked = false;
            s.waiters.pop_front()
        };
        if let Some(token) = waiter {
            complete(token);
        }
    }
}

// ------------------------------------------------------------- ticket lock

/// A fair (FIFO) ticket spin lock for the future multi-vcore case. Uncontended
/// acquire is two atomics; contended spins until its ticket is served. See the
/// module note: do NOT use this to guard against a strand on the same vcore.
pub struct TicketLock<T> {
    next: AtomicU32,
    owner: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: the ticket protocol serialises access; intended for cross-vcore use.
unsafe impl<T: Send> Sync for TicketLock<T> {}

impl<T> TicketLock<T> {
    pub const fn new(value: T) -> TicketLock<T> {
        TicketLock {
            next: AtomicU32::new(0),
            owner: AtomicU32::new(0),
            data: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> TicketGuard<'_, T> {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.owner.load(Ordering::Acquire) != ticket {
            spin_loop();
        }
        TicketGuard { lock: self }
    }
}

pub struct TicketGuard<'a, T> {
    lock: &'a TicketLock<T>,
}

impl<T> Deref for TicketGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: this ticket owns the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for TicketGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: this ticket owns the lock.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for TicketGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.fetch_add(1, Ordering::Release);
    }
}
