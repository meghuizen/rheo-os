# Real-Time and Time-Certain Workloads

**Status:** Draft v0.1. Covers timer precision, periodic execution guarantees,
jitter elimination, and the real-time scheduling model. Relates to
SCHEDULING.md (reservations, EDF, tickless cores), TIME-IDENTITY.md (clock
objects, monotonic vs wall), CONCURRENCY.md (PI-mandatory mutexes), and
VALIDATION.md (P40, P44, P12).

---

## 1. Why `sleep(1000ms)` is imprecise

On stock Linux, `nanosleep(1000ms)` does not sleep for exactly 1000ms.
The actual wakeup time is:

```
T_wakeup = T_now + 1000ms + scheduler_latency
                           + migration_cost (if thread moved)
                           + lock_contention_in_scheduler
                           + IRQ_handler_delay
                           + (SMI_duration, if firmware fires)
```

Each term adds jitter:

- **Periodic timer tick (HZ):** on a 250 Hz kernel, the timer interrupt fires
  every 4ms. A sleep of 1000ms might actually wait 1000-1004ms depending on
  where in the 4ms tick window it lands. Even `CONFIG_HZ_1000` gives 1ms
  granularity at best.
- **Scheduler latency:** the time from "timer fires, thread runnable" to
  "thread is actually executing." With CFS, involves a priority queue
  lookup + potential CPU migration. On a loaded system: 100µs–10ms.
- **Lock contention:** the Linux scheduler holds `rq->lock` during context
  switches. Other threads taking that lock delay your wakeup.
- **IRQ storms:** a burst of NIC interrupts, disk completions, or USB events
  runs their handlers before the scheduler runs your thread.
- **SMI (System Management Interrupt):** the worst offender. The CPU
  invisibly enters System Management Mode for firmware tasks — power
  management, thermal throttling, error correction — for 10µs to 50ms.
  The OS cannot see or prevent this. From the kernel's perspective the core
  simply stopped for N milliseconds.

PREEMPT_RT patches Linux to convert spinlocks to mutexes, make IRQ handlers
preemptible threads, and reduce the non-preemptible kernel paths. It cuts
worst-case jitter from ~10ms to ~100–200µs on typical hardware. Good, but
not zero, and the mechanism is retrofitting preemptibility onto a monolithic
kernel that was never designed for it.

---

## 2. What Lattice already has that PREEMPT_RT patches in

Lattice's design choices, taken independently, each eliminate a jitter source:

| Jitter source | PREEMPT_RT solution | Lattice design |
|---|---|---|
| Periodic tick | `CONFIG_HZ_1000` + dynamic tick | Tickless by design (SCHEDULING.md 1). No tick on dedicated cores. |
| Scheduler latency | Preemptible kernel paths | Dedicated latency-pool core: no scheduling decision needed; strand is the only runnable work |
| Lock contention | Spinlocks → sleeping mutexes | Per-core scheduler state; no shared `rq->lock`; PI is mandatory (CONCURRENCY.md 6) |
| IRQ interference | Threaded IRQs, priority-driven | IRQ affinity: no IRQs routed to latency-pool cores at all |
| Priority inversion | PI mutexes (opt-in) | PI mandatory for any mutex a reservation-holding strand touches |
| Cache/TLB cold start | `sched_setaffinity` + pinning | Strand never migrates off its dedicated core; ASID keeps TLB warm |
| Blocking syscalls | N/A (not addressed) | Structural: no blocking syscalls exist; a strand never disappears into the kernel |

The result: a cell in the latency pool on a dedicated core has a hardware
timer interrupt (TSC-deadline, sub-microsecond precision) as its only
external event. The strand runs immediately — no scheduler decision, no lock,
no migration, no IRQ competition. Worst-case wakeup latency is hardware
timer delivery (~500ns) plus the context switch within the same runtime
(~100ns). Sub-microsecond is achievable on modern hardware.

SMI remains the honest hard limit — discussed in section 6.

---

## 3. The three timer semantics

Not all time-related waits are the same. Lattice exposes three distinct
primitives with different guarantees:

### 3.1 Relative sleep — "at least this long"

```rust
// Park this strand for at least this duration.
// Best-effort: actual wakeup is T_now + duration + [scheduler_latency].
// For a dedicated-core strand: scheduler_latency ≈ 0.
// For a shared-pool strand: scheduler_latency = variable.
strand.sleep(Duration::from_millis(100)).await;
```

Appropriate for: rate-limiting, backoff loops, non-critical delays.
Not appropriate for: periodic control loops (it drifts — see section 4).

### 3.2 Absolute deadline sleep — "wake me at this instant"

```rust
// Park until the monotonic clock reaches this instant.
// Arms a one-shot hardware timer at the exact TSC value.
// For a dedicated-core strand: sub-microsecond jitter.
strand.sleep_until(Instant::now() + Duration::from_millis(10)).await;
```

The monotonic clock object (TIME-IDENTITY.md 1) provides the absolute
reference. The timer is armed in TSC-deadline mode: the LAPIC fires when the
TSC reaches the target value — not at a tick boundary, at the exact cycle.
Resolution: ~30ns on modern x86 (one TSC cycle at 3 GHz).

Appropriate for: periodic tasks (used correctly — see section 4), phased
execution, timeout enforcement.

### 3.3 Reservation slot — "guaranteed by the scheduler"

```rust
// The kernel commits to running this strand within its reserved window.
// The scheduler is responsible for meeting the deadline; the strand is
// responsible for completing within budget.
// Admission-controlled: returns Err if the task set is not schedulable.
let periodic = PeriodicTask::builder()
    .period(Duration::from_millis(10))
    .budget(Duration::from_millis(2))    // 2ms of CPU per 10ms period
    .deadline(Duration::from_millis(8))  // complete within 8ms of activation
    .priority(Priority::Hard)            // miss deadline → overrun event
    .pool(CorePool::Latency)             // dedicated tickless core
    .build()?;                           // Err(NotSchedulable) if math fails

loop {
    let _slot = periodic.wait().await;   // arms absolute timer; guaranteed slot
    do_control_loop();
    // _slot drops here; scheduler notes completion time vs deadline
}
```

The admission check at construction time runs the EDF schedulability math:
sum(Ci/Pi) ≤ 1 across all tasks on the pool. A task that cannot be admitted
gets a clear `NotSchedulable` error before any execution, not a runtime
overrun at T+3 hours.

Appropriate for: hard real-time control loops, audio sample processing,
safety-critical periodic work. This is what real-time kernels exist for,
made a first-class API primitive.

---

## 4. The drift problem — why `sleep(N)` loops are wrong

The most common mistake in periodic programming:

```rust
// WRONG — drifts over time
loop {
    do_work();           // takes some time
    sleep(10ms).await;   // 10ms from now, not from when I should have started
}
// Period is actually: 10ms + do_work_duration + wakeup_jitter
// After 1000 iterations: accumulated drift can be seconds
```

The correct pattern using absolute deadline sleep:

```rust
// RIGHT — stays phase-locked
let period  = Duration::from_millis(10);
let mut next = Instant::now();

loop {
    next += period;           // next deadline is always exactly P from origin
    do_work();
    sleep_until(next).await;  // sleep until absolute time, not relative
    // If do_work() takes longer than period:
    // next < now → sleep_until returns immediately → overrun in next iteration
}
```

And the correct pattern using the reservation API:

```rust
// BEST — admission-controlled, monitored, overrun-detected
let task = PeriodicTask::builder().period(10ms).budget(2ms)... .build()?;
loop {
    let slot = task.wait().await;
    do_work();
    let report = slot.finish();
    if report.overrun { log_event!(BudgetOverrun { by: report.overrun_by }); }
}
```

The `PeriodicTask` manages the absolute deadline arithmetic internally and
adds overrun detection: if `do_work()` consumes more than the declared budget,
the overrun is reported via a structured event, the timing stats are updated,
and the *next* period starts from the correct absolute time — no drift.

### 4.1 Feedback loops — cycles live in time, not in the graph

Control systems have feedback: iteration N+1 depends on iteration N's output.
It is tempting to model this as a *cyclic* graph. Do not. A cyclic dependency
graph breaks poison propagation (forward-only needs a DAG), cycle/deadlock
detection (a legitimate cycle looks like a bug cycle), and topological
scheduling (a cycle has no order).

Instead, **the cycle lives in time**: a control loop is a periodic
re-submission of an acyclic graph whose inputs include the previous
iteration's outputs, carried as strand-local state across activations:

```
Iteration N:    [read sensor] → [compute(state_N, sensor)] → [actuate]
                                      │ produces state_{N+1}
Iteration N+1:  [read sensor] → [compute(state_{N+1}, sensor)] → [actuate]
```

The "feedback edge" is not a graph edge — it is a value retained in the
strand between periodic activations. Each iteration is a clean DAG; the loop
is the periodic re-submission. This preserves every DAG property, matches how
control software is actually written (a function called each period with
retained state), and needs no new mechanism. See REFLECTION-NEXUS.md §5.

---

## 5. Timer hardware — what the Arch trait provides

```rust
// kernel/arch/timer.rs

pub trait ArchTimer {
    /// Arm a one-shot timer that fires when the hardware counter reaches
    /// `deadline_ticks`. The counter is the CPU's invariant TSC (x86),
    /// system counter (ARM generic timer), or mtime (RISC-V).
    ///
    /// The timer fires exactly once; re-arm for periodic behaviour.
    /// Resolution: platform-dependent; see measured values in the
    /// topology graph after boot.
    fn arm_deadline(&self, deadline_ticks: u64);

    /// Current counter value. Monotonically increasing. Fast (one rdtsc/mrs).
    fn now_ticks(&self) -> u64;

    /// Ticks per nanosecond — computed at boot, stored in topology graph.
    /// Not a compile-time constant because TSC frequency varies.
    fn ticks_per_ns(&self) -> u64;

    /// Cancel the currently armed timer (if any).
    fn disarm(&self);
}

// x86_64 implementation: TSC-deadline LAPIC mode
pub struct TscDeadlineTimer;
impl ArchTimer for TscDeadlineTimer {
    #[inline(always)]
    fn arm_deadline(&self, deadline_ticks: u64) {
        // Write directly to the TSC-deadline MSR — one WRMSR instruction.
        // When TSC reaches this value, LAPIC fires an interrupt on this core.
        // Resolution: 1 TSC cycle (~330ps at 3GHz).
        unsafe { x86_64::registers::msr::Msr::new(0x6E0).write(deadline_ticks) }
    }

    #[inline(always)]
    fn now_ticks(&self) -> u64 {
        // RDTSC: ~15 cycles (5ns at 3GHz). Serializing (RDTSCP) where needed.
        unsafe { core::arch::x86_64::_rdtsc() }
    }
}
```

Each ISA's timer:

| ISA | Mechanism | Resolution | Notes |
|---|---|---|---|
| x86-64 | TSC-deadline LAPIC (`IA32_TSC_DEADLINE` MSR) | ~1 TSC cycle (~300ps) | Requires invariant TSC; verified at boot |
| ARM64 | Generic timer (CNTP_CVAL_EL0) | ~1 counter cycle | Shared across cores; per-core virtual timer for EL0 |
| RISC-V | Sstc extension (STIMECMP) | ~1 counter cycle | Requires Sstc; falls back to SBI timer call |

The Arch trait's `arm_deadline` maps to the best available hardware. On x86,
it is one `WRMSR`; there is no shorter path from "software decision" to
"hardware timer armed."

---

## 6. SMI and NMI — the honest hard limit

**SMI (System Management Interrupt)** is the bane of real-time on x86.
The CPU transparently enters System Management Mode (SMM) — a privileged
mode invisible to the OS — to run firmware code for:
- Platform power management
- Thermal throttling responses
- Hardware error correction (MCE handling)
- BIOS compatibility shims

SMI duration: 10µs to 50ms. Occurrence: typically every few seconds for
thermal monitoring, but can be more frequent. From the OS's perspective: the
core stopped executing for N milliseconds with no warning and no notification.
No software mechanism can prevent or shorten an SMI.

**Detection:** measure TSC gaps. A TSC gap larger than the expected worst-case
interrupt latency indicates an SMI occurred. The latency pool monitoring
strand checks this continuously:

```rust
// On a dedicated latency-pool core, monitor for unexpected gaps:
loop {
    let before = timer.now_ticks();
    hint::spin_loop();  // tight loop — minimal code between samples
    let after  = timer.now_ticks();
    let gap_ns  = (after - before) / TICKS_PER_NS;

    if gap_ns > MAX_EXPECTED_INTERRUPT_LATENCY_NS {
        // An SMI (or NMI) occurred during this gap
        log_event!(SmiDetected { gap_ns, at_ticks: before });
    }
}
```

SMI occurrences are events in the OTel stream with their measured duration.
A real-time workload's timing telemetry shows SMIs explicitly — they don't
appear as "my control loop was slow" but as "firmware interrupted for 3ms."

**Mitigation (requires BIOS/firmware configuration):**
- Disable C6/C7 power states (platform enters them less frequently, reduces SMI from power transitions)
- Disable thermal throttling SMIs (configure at BIOS level)
- Use server-class hardware: IPMI-managed servers typically have fewer SMIs than desktop/workstation
- Certified RT hardware: some vendors certify platforms with documented SMI frequencies

**NMI (Non-Maskable Interrupt):** used for hardware watchdogs and machine-
check exceptions. Duration: ~µs. Frequency: rare. Less problematic than SMIs.

**The honest claim:** on well-configured server hardware with SMIs minimised,
Lattice's latency-pool model achieves worst-case jitter of **2–10µs** for
periodic tasks — competitive with PREEMPT_RT and better than stock Linux by
two orders of magnitude. The SMI is the remaining irreducible source of
deviation. For applications where even 50µs is unacceptable, dedicated FPGA
or ASIC timing hardware is required (these are real-time co-processors, not
general-purpose OSes).

---

## 7. The PeriodicTask API in full

```rust
// lsh-sdk / lattice-rt — the public API for time-certain workloads

use core::time::Duration;
use lattice_rt::{Instant, PeriodicTask, Priority, CorePool, TimingReport};

/// Builder for a periodic task reservation.
/// Admission control runs at `.build()` time.
pub struct PeriodicTaskBuilder {
    period:   Duration,
    budget:   Duration,
    deadline: Duration,
    priority: Priority,
    pool:     CorePool,
    name:     &'static str,
}

impl PeriodicTaskBuilder {
    /// The period P: activation interval.
    pub fn period(mut self, p: Duration) -> Self { self.period = p; self }

    /// The budget C: worst-case execution time per period.
    /// Over-declaration wastes reservation; under-declaration causes overruns.
    /// Measure first with `PeriodicTask::measure_budget()`.
    pub fn budget(mut self, c: Duration) -> Self { self.budget = c; self }

    /// The relative deadline D (default = period).
    /// The task must complete within D of its activation time.
    /// D ≤ P always; D < P gives slack for overrun recovery.
    pub fn deadline(mut self, d: Duration) -> Self { self.deadline = d; self }

    /// Hard: overrun = event + throttle; next period still runs.
    /// Soft: overrun = event only; best effort.
    /// Critical: overrun = event + cell-level alarm; operator decides.
    pub fn priority(mut self, p: Priority) -> Self { self.priority = p; self }

    /// CorePool::Latency: dedicated tickless core (lowest jitter, highest cost)
    /// CorePool::Shared: EDF on the shared pool (moderate jitter, shared)
    pub fn pool(mut self, pool: CorePool) -> Self { self.pool = pool; self }

    /// Build and register the periodic task.
    /// Returns Err(NotSchedulable) if sum(Ci/Pi) > 1 for this pool.
    /// Returns Err(InsufficientReservation) if the cell's reservation
    /// cannot accommodate this task's resource needs.
    pub fn build(self) -> Result<PeriodicTask, RtError>;
}

/// A running periodic task. Drop to cancel.
pub struct PeriodicTask { /* ... */ }

impl PeriodicTask {
    /// Block until the next period slot is available.
    /// Returns a guard that measures the slot's execution time.
    pub async fn wait(&self) -> PeriodicSlot;

    /// Timing statistics since the task was created.
    pub fn timing_stats(&self) -> TimingStats;

    /// Measure the actual execution time of a closure to determine budget.
    /// Runs the closure N times with high-precision timing, returns the
    /// Cworst observed. Use this before declaring the budget.
    pub async fn measure_budget<F: Fn()>(f: F, runs: u32) -> BudgetReport;
}

pub struct PeriodicSlot { /* measures wall time until drop */ }

impl Drop for PeriodicSlot {
    fn drop(&mut self) {
        // Records: actual execution time, deadline miss (if any),
        // activation jitter (actual wakeup vs scheduled wakeup)
        self.task.record_completion(self.start, Instant::now());
    }
}

pub struct TimingStats {
    pub activations:   u64,
    pub deadline_miss: u64,
    pub overruns:      u64,
    // Jitter: actual wakeup time minus scheduled wakeup time
    pub jitter_min_ns: u64,
    pub jitter_max_ns: u64,
    pub jitter_p99_ns: u64,
    pub jitter_p9999_ns: u64,
    // Execution time
    pub exec_min_ns:   u64,
    pub exec_max_ns:   u64,
    pub exec_p99_ns:   u64,
    // SMI events detected during this task's execution
    pub smi_events:    u64,
    pub smi_total_ns:  u64,
}
```

### Usage examples by workload

**Audio server (5.33ms period at 48kHz/256 frames):**

```rust
let audio_task = PeriodicTask::builder()
    .period(Duration::from_micros(5333))
    .budget(Duration::from_micros(1000))   // 1ms budget: 81% slack for jitter
    .deadline(Duration::from_micros(4000)) // complete 1.3ms before next period
    .priority(Priority::Hard)
    .pool(CorePool::Latency)
    .name("audio-refill")
    .build()?;

loop {
    let slot = audio_task.wait().await;
    let frames = audio_buffer.next_output_frames();
    synthesize_audio(&mut frames);
    hardware_dma_ring.submit(frames).await;
    // slot drops: timing recorded; overrun if > 1ms
}
```

**Industrial control loop (1ms period):**

```rust
let ctrl = PeriodicTask::builder()
    .period(Duration::from_millis(1))
    .budget(Duration::from_micros(400))    // 400µs budget, 60% utilisation
    .deadline(Duration::from_micros(800))  // complete 200µs before next period
    .priority(Priority::Critical)          // alert operator on miss
    .pool(CorePool::Latency)
    .name("motor-ctrl")
    .build()?;

let mut state = ControllerState::new();
loop {
    let slot = ctrl.wait().await;

    let sensor  = sensor_daq.read_current().await;
    let command = state.compute_output(sensor);
    actuator_dac.write(command).await;

    let stats = slot.finish_with_stats();
    // If jitter > 50µs or deadline missed: structured Critical event
    // → operator dashboard alert, not a log line the operator might miss
    if stats.activation_jitter_ns > 50_000 {
        ctx.critical(Diagnostic {
            level:   DiagLevel::Fatal,
            code:    Some(RT_JITTER_EXCEEDED),
            message: format!("jitter {}µs exceeds limit 50µs",
                             stats.activation_jitter_ns / 1000),
            ..Default::default()
        });
    }
}
```

**Financial trading — event-driven, not periodic:**

```rust
// Trading is latency-not-period: respond to market data as fast as possible.
// The reservation model still applies but for minimum latency, not period.

// Reserve: one dedicated core, high CPU budget, always-runnable
let _reservation = cell.reserve(Reservation {
    cores:  1,
    pool:   CorePool::Latency,
    budget: Budget::Always,    // this core runs whenever the strand is ready
    memory: MemoryReservation::Pinned(ddr_grant),
}).await?;

// The strategy is in the completion queue latency, not scheduling:
loop {
    // NIC queue → market data → strategy → order → NIC queue
    // Each step is a queue completion; total path: sub-10µs on good hardware
    let tick = market_feed.recv().await;     // NIC → cell: ~1µs
    let order = strategy.evaluate(tick);      // compute: ~2µs
    if let Some(order) = order {
        exchange_nic.send(order).await;       // cell → NIC: ~1µs
    }
}
```

The key for trading: the NIC queue is owned directly by the cell
(NETWORKING.md 1), so market data arrives as a completion event in userspace
without a kernel transition. The total path from packet arriving at the NIC
to a response packet leaving is sub-10µs end-to-end on RDMA-capable hardware.

---

## 8. `sleep` in the shell — what the user sees

```lsh
# Shell sleep — blocks the strand for at least N
sleep 1000ms

# Absolute sleep — useful in scripts that need phase alignment
sleep until 2025-01-01T00:00:00Z

# Periodic loop in lsh scripting — uses the PeriodicTask API
every 10ms budget 2ms {
    read-sensor /dev/temp | process-reading | emit-metric
}
# Rejected at parse time if not schedulable with current reservation

# Check the timing stats after a periodic loop completes:
echo "jitter p99: $(every.stats.jitter_p99)"
```

The shell's `every` built-in is syntactic sugar over `PeriodicTask::builder()`.
The shell's reservation (the `needs { }` block) must include enough budget
for the declared periods or the script fails to start — no surprise overruns
at 3am.

---

## 9. Jitter profiling — measuring what the OS actually delivers

Before deploying a real-time workload, measure what the hardware and OS
actually deliver on this specific machine:

```lsh
# Run the RT benchmark suite for this host:
rt-bench --pool latency --period 1ms --duration 60s

Output:
  Period:       1.000000 ms
  Activations:  60000
  Budget:       400 µs declared
  
  Wakeup jitter:
    min:    0.8 µs
    mean:   1.2 µs
    p99:    3.4 µs
    p99.9:  8.1 µs
    max:    41.2 µs   ← SMI detected (2 events in 60s)
  
  SMI events:   2
  SMI total:    39.4 µs
  
  Deadline misses:  0  (0.000%)
  
  Assessment: SUITABLE for 50µs jitter budget. NOT SUITABLE for 10µs.
  SMI detected: check BIOS power management settings.
  See: lattice rt-guide --smi-mitigation
```

The benchmark runs a calibration task and reports actual statistics. The
SMI events are surfaced explicitly, with a suggestion for mitigation.
A deployment system can gate rollout on the p99.9 jitter being within the
declared budget — automated real-time suitability certification.

---

## 10. Integration with the scheduling admission model

The EDF admission check at `PeriodicTask::build()` is the same math as the
reservation admission check in SCHEDULING.md 4, specialised for periodic tasks:

```
Utilisation bound for EDF:  U = sum(Ci / Pi)  ≤  1.0  (necessary and sufficient)
Response time bound:         Ri ≤ Di  for all tasks i
```

The kernel tracks per-pool utilisation and rejects new periodic tasks that
would push the pool over the schedulability bound. A pool at 95% utilisation
with a new task requiring 10% of a 1ms period is rejected — the 5% slack is
required for interrupt handling and context switch overhead.

The safety margin the scheduler keeps is configurable per-pool:
- Latency pool: 10% reserved for system overhead
- Shared pool: 20% (more overhead, less predictable)

These margins are conservative rather than tight — the goal is hard guarantees,
not squeezing the last 5% of utilisation.
