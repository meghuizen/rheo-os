//! Time: a monotonic clock and async timers (docs/LIBRHEO.md Phase F,
//! docs/ARCHITECTURE.md 3 object 9). `now()` reads the kernel's monotonic tick
//! counter (`SYS_UPTIME`); `sleep`/`timeout`/`interval` are async, built on the
//! reactor's one-shot deadline (`rt::sleep_ns` over `SYS_ARM_TIMER`) so the
//! vcore runs the other strands while a strand waits - blocking exists only as a
//! park (docs/CONCURRENCY.md 1).
//!
//! Two units meet here, honestly: [`Instant`] is a raw monotonic **tick**
//! reading (per-ISA meaning, only ever compared to another reading), while a
//! [`Duration`] carries **nanoseconds** - the unit the kernel's timer takes and
//! converts against the ISA's timebase. So `now()` measures ordering/elapsed
//! ticks, and `sleep(Duration)` names a real time span.

use core::future::Future;
use core::task::Poll;

use crate::rt;
use crate::sys;

/// A monotonic instant: the kernel's tick counter at the moment of reading.
/// Ticks never go backwards on a core; the value is meaningful only relative to
/// another `Instant` (per-ISA timebase), so only differences are exposed.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl Instant {
    /// Read the monotonic clock now.
    pub fn now() -> Instant {
        Instant(sys::uptime())
    }

    /// Ticks elapsed since this instant (0 if the clock has not advanced).
    pub fn elapsed_ticks(&self) -> u64 {
        sys::uptime().wrapping_sub(self.0)
    }

    /// Raw tick reading (for a caller that wants to record it).
    pub fn ticks(&self) -> u64 {
        self.0
    }
}

/// A time span in nanoseconds - the unit the kernel's one-shot timer takes.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration {
    nanos: u64,
}

impl Duration {
    pub const fn from_nanos(nanos: u64) -> Duration {
        Duration { nanos }
    }
    pub const fn from_micros(micros: u64) -> Duration {
        Duration {
            nanos: micros.saturating_mul(1_000),
        }
    }
    pub const fn from_millis(millis: u64) -> Duration {
        Duration {
            nanos: millis.saturating_mul(1_000_000),
        }
    }
    pub const fn as_nanos(&self) -> u64 {
        self.nanos
    }
}

/// Read the monotonic clock now (shorthand for [`Instant::now`]).
pub fn now() -> Instant {
    Instant::now()
}

/// Async sleep for `d` (docs/LIBRHEO.md Phase F). Parks the strand until the
/// deadline; the vcore runs the other strands meanwhile.
pub async fn sleep(d: Duration) {
    rt::sleep_ns(d.nanos).await;
}

/// A **pacing** sleep: identical to [`sleep`], except the deadline is held in the
/// kernel timer arbiter's *pacer* slot rather than the cell-sleep slot
/// (docs/NETSTACK.md 21, rheo-net N2e). A transport that paces its sends re-arms
/// this after every segment; keeping it in its own slot is what lets an ordinary
/// `sleep`/`timeout` in the same cell stay outstanding across it.
pub async fn sleep_pacing(d: Duration) {
    rt::sleep_pacing_ns(d.nanos).await;
}

/// Run `fut` to completion, or give up after `d`. `Ok(v)` if it finished,
/// `Err(Elapsed)` if the deadline fired first. On the single-vcore cooperative
/// runtime this is a best-effort race: both are polled each turn, and the sleep
/// only fires (arming the kernel deadline) once every other strand has parked.
pub async fn timeout<F: Future>(d: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let mut fut = core::pin::pin!(fut);
    let mut timer = core::pin::pin!(sleep(d));
    core::future::poll_fn(move |cx| {
        if let Poll::Ready(v) = fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// The error [`timeout`] returns when the deadline fires first.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Elapsed;

/// A periodic ticker: `tick().await` completes once per `period` (docs/LIBRHEO.md
/// Phase F). Cooperative: the period is measured from the previous tick's
/// completion, so a slow consumer stretches the interval (no missed-tick burst).
pub struct Interval {
    period: Duration,
}

impl Interval {
    /// Wait one period.
    pub async fn tick(&mut self) {
        sleep(self.period).await;
    }
}

/// A ticker firing every `period`.
pub fn interval(period: Duration) -> Interval {
    Interval { period }
}
