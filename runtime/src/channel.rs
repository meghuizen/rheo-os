//! An mpsc channel over strands. `recv().await` on an empty channel parks the
//! receiving strand; `send` pushes and wakes it. This is the OS-native
//! replacement for a mutex+condvar work queue: the "block" is a strand park,
//! not a kernel wait, so it never loses the vcore (docs/CONCURRENCY.md 1, 3).
//!
//! Single-vcore cooperative, so `Rc<RefCell<..>>` is sound; senders and
//! receivers are cheap clones of that shared cell.

use crate::strand::{complete, next_token, park_on};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use core::cell::RefCell;

struct Chan<T> {
    buf: VecDeque<T>,
    token: u64,
    senders: usize,
}

pub struct Sender<T> {
    inner: Rc<RefCell<Chan<T>>>,
}

pub struct Receiver<T> {
    inner: Rc<RefCell<Chan<T>>>,
}

/// Create a channel. Every send wakes a parked receiver via the shared token.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(RefCell::new(Chan {
        buf: VecDeque::new(),
        token: next_token(),
        senders: 1,
    }));
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        self.inner.borrow_mut().senders += 1;
        Sender {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let token = {
            let mut c = self.inner.borrow_mut();
            c.senders -= 1;
            c.token
        };
        // Wake a parked receiver so it can observe the closed channel.
        complete(token);
    }
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) {
        let token = {
            let mut c = self.inner.borrow_mut();
            c.buf.push_back(value);
            c.token
        };
        complete(token);
    }
}

impl<T> Receiver<T> {
    /// Receive the next value, parking the strand while the channel is empty.
    /// Returns `None` once the channel is empty and all senders are gone.
    pub async fn recv(&self) -> Option<T> {
        loop {
            let token = {
                let mut c = self.inner.borrow_mut();
                if let Some(v) = c.buf.pop_front() {
                    return Some(v);
                }
                if c.senders == 0 {
                    return None;
                }
                c.token
            };
            park_on(token).await;
        }
    }
}
