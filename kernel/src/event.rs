//! Event streams (docs/ARCHITECTURE.md 3 object 10, docs/OBSERVABILITY.md):
//! typed, schema'd events carrying the 16-byte flow context the kernel
//! propagates through every queue entry and graph edge. Observability the
//! system cannot fail to produce.
//!
//! Single-host scope: a fixed-capacity per-owner ring of events. The OTel
//! export edge (OBSERVABILITY.md 6) is a later service cell; here the ring
//! is drained in-process by whoever holds the stream.

use crate::time;

/// Event kinds. The full schema is generated from the IDL (step 6); these
/// are the ones the current kernel and shell emit.
pub const EV_CELL_SPAWN: u16 = 1;
pub const EV_CELL_EXIT: u16 = 2;
pub const EV_QUEUE_SUBMIT: u16 = 3;
pub const EV_QUEUE_COMPLETE: u16 = 4;
pub const EV_GRANT: u16 = 5;
pub const EV_REVOKE: u16 = 6;
pub const EV_SHELL_CMD: u16 = 7;
pub const EV_USER: u16 = 8;

#[derive(Copy, Clone)]
pub struct Event {
    pub kind: u16,
    pub flow_id: u128,
    pub arg: u64,
    /// Monotonic tick at emit time - events are causally ordered by it.
    pub tick: u64,
}

impl Event {
    pub const ZERO: Event = Event {
        kind: 0,
        flow_id: 0,
        arg: 0,
        tick: 0,
    };
}

pub const STREAM_CAP: usize = 256;

/// A single-owner event ring. Emit is O(1); a full ring drops the oldest
/// event and counts the drop (observability must never block the hot path,
/// docs/OBSERVABILITY.md).
pub struct EventStream {
    buf: [Event; STREAM_CAP],
    head: usize,
    len: usize,
    dropped: u64,
    total: u64,
}

impl EventStream {
    pub const fn new() -> EventStream {
        EventStream {
            buf: [Event::ZERO; STREAM_CAP],
            head: 0,
            len: 0,
            dropped: 0,
            total: 0,
        }
    }

    /// Emit an event, stamping it with the current monotonic tick.
    pub fn emit(&mut self, kind: u16, flow_id: u128, arg: u64) {
        let ev = Event {
            kind,
            flow_id,
            arg,
            tick: time::monotonic(),
        };
        self.total += 1;
        let tail = (self.head + self.len) % STREAM_CAP;
        if self.len == STREAM_CAP {
            // Full: overwrite the oldest and advance head.
            self.buf[self.head] = ev;
            self.head = (self.head + 1) % STREAM_CAP;
            self.dropped += 1;
        } else {
            self.buf[tail] = ev;
            self.len += 1;
        }
    }

    /// Pop the oldest buffered event, or None.
    pub fn drain_one(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let ev = self.buf[self.head];
        self.head = (self.head + 1) % STREAM_CAP;
        self.len -= 1;
        Some(ev)
    }

    pub fn buffered(&self) -> usize {
        self.len
    }
    pub fn total(&self) -> u64 {
        self.total
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new()
    }
}
