# Power and Energy Management

**Status:** Draft v0.1. New subsystem, forced into existence by the embedded /
IoT / low-power / remote profiles (PROFILES.md 4) and useful for server
efficiency and desktop/laptop. Deprioritized for the wall-powered fleet
server; central for everything battery-, solar-, or thermally-constrained.

Position: **energy is a first-class, typed, metered, reservable resource -
managed the same way CPU, memory, and bandwidth already are.** Power
management is not a bolt-on daemon (Linux's historical approach); it is an
extension of the reservation, metering, and pressure-event machinery the OS
already has, plus per-engine control exposed through the Arch trait. This
reuse is itself evidence the minimal core spans the range (PROFILES.md 1).

## 1. Why this fits the existing model cleanly

The design already treats CPU cycles, memory, queue depth, and I/O bandwidth
as typed, budgeted, admission-controlled resources (SCHEDULING.md 4,
CLUSTER.md 4). Energy is one more of the same shape:

- **Energy budget = joules over a window**, alongside "budget of cycles." A
  cell or a whole node can hold an energy reservation.
- **Metering already exists** - every capability carries usage counters
  (CONTAINERS-KUBERNETES.md 6); adding an energy counter per engine and per
  cell is the same mechanism.
- **Pressure events already exist** - memory reclaim uses cooperative pressure
  events with deadlines (MEMORY.md 7). Brownout (battery low, solar drop, a
  thermal cap) is the identical pattern: an **energy-pressure event** asks
  cells to shed load within a deadline before the system throttles by force.

So power management is largely *composition*, not new kernel mechanism - which
is what the governance rule wants (ARCHITECTURE.md 6). Only the hardware
control primitives (DVFS, idle states, power gating) are genuinely new, and
they live behind the Arch trait like every other hardware difference.

## 2. Hardware control primitives (Arch trait)

Per-ISA implementations behind one interface (TARGET-ARCHITECTURES.md 4):

- **DVFS** - dynamic voltage/frequency scaling per core or per domain
  (P-states, ARM OPPs). The scheduler chooses frequency as an output, not a
  background governor guessing.
- **Idle / sleep states** - core C-states, cluster idle, and deep
  suspend-to-RAM / suspend-to-disk for desktop and embedded. The tickless
  design (SCHEDULING.md 1) already means an idle core has no periodic wakeups,
  so entering and staying in deep idle is natural rather than fought.
- **Per-engine power gating** - an idle GPU/NPU partition, an unused NIC
  queue, or a quiesced accelerator is powered down. Engines already declare
  state and are attested/metered (ACCELERATORS.md 1); power state is one more
  attribute the kernel controls and the topology graph exposes.
- **Thermal input** - temperature and thermal caps are events; a thermal limit
  is an energy-pressure source (section 4).

## 3. Energy-aware scheduling

- **Race-to-idle vs pace-to-deadline.** For deadline work (real-time
  reservations), run fast enough to hit the deadline then idle deep; for
  throughput work under an energy budget, pace at the most efficient
  frequency. The scheduler picks per reservation class, using the measured
  energy/frequency curve in the topology graph (not a datasheet).
- **Consolidation.** Under light load, pack work onto fewer cores/nodes and
  power-gate the rest - the inverse of the interference-spreading the server
  profile does. A profile flag chooses the policy (performance-spread vs
  energy-consolidate).
- **Deferral.** Non-urgent graph jobs (batch training, compaction, background
  zeroing) carry an energy/urgency class and are deferred when energy is
  scarce, run when it is abundant (e.g. midday solar) - expressed as
  scheduling policy over the existing dependency-graph and reservation model.
- **Energy as an admission input.** On a constrained node, admitting a
  workload checks the energy budget as well as CPU/memory - you cannot reserve
  power you do not have, the same honesty as the RT admission math.

## 3a. Idle is a measured property, not an assumption

A system can be tickless in principle and still bleed wakeups in practice.
Linux's `nohz_full` "never fully wins" (SCHEDULING.md 1) because RCU callbacks
and accounting leak in; and userspace itself is usually the bigger offender -
clients that poll, repaint on a timer, or run housekeeping loops keep the CPU
out of deep sleep even when the screen is static. The lesson from lean-stack
experiments (e.g. the Frame X11 server, which measured roughly a third of
X.Org's idle CPU and a desktop that "sits completely still until touched", and
window-manager/terminal components using zero CPU time over minutes of idle):
**an idle stack that genuinely does nothing lets the CPU stay in its deepest
sleep states, and the difference from a stack with quiet busywork is large -
but you only know you have it if you measure it.**

Lattice's architecture makes true idle the default rather than a hand-crafted
achievement: a strand parks on a completion-queue doorbell and generates *zero*
wakeups until a real event arrives (there is no polling path to leak CPU); the
compositor parks on the vsync event queue rather than spinning (DISPLAY.md 4);
structured channels mean an idle client is genuinely blocked, not looping
(SHELL.md 9). But "by construction" is a claim that has to be defended with
numbers, so idle behaviour is a first-class, gated property:

- **Idle-wakeup budget.** Each profile declares a maximum wakeups-per-second
  at idle (for the whole system and per cell). A dedicated latency-pool core
  at idle targets **zero** timer wakeups; a shared-pool core targets a small
  bounded number (housekeeping only). This is measured continuously and
  exported on the event stream, the same way throughput and latency are.
- **Wakeup attribution.** Because every wakeup is an event with a flow ID, a
  cell that wakes the CPU without cause is *identifiable* - "which cell is
  keeping this core out of C6, and why" is a query, not a guessing game with
  `powertop`. A cell that exceeds its idle-wakeup budget is visible and
  attributable, exactly as a memory or CPU overrun is.
- **Regression gate.** Idle wakeups-per-second and the resulting residency in
  deep C-states are a validation target (VALIDATION.md P43 for the low-power
  profile), gated against a baseline. A change that adds idle busywork - a new
  poll loop, a stray periodic timer - fails the gate the same way a latency
  regression does. Idle cleanliness cannot silently rot.

The honest caveat the same experiments show: at the platform level the panel,
radio, and other always-on components can dominate total idle watts, so a large
CPU-idle win may not translate proportionally to battery life on every device
(Frame and X.Org drew similar total battery watts at idle despite the 3x CPU
gap). The CPU-idle win still matters - it is thermal headroom, it is fan-off
operation, it is energy for the constrained/solar profile where the CPU is a
larger fraction of the budget, and it is the difference between a core that
can deep-sleep and one that cannot - but the claim is scoped to CPU/SoC energy
and deep-sleep residency, not asserted as whole-device battery life
independent of the panel and radio.

### 3a.1 Four techniques the runtime and SDK should enforce

Inspecting Frame's actual serve loop (a single `poll` over listen + client +
input + DRM + uevent + VT fds) shows the concrete disciplines that produce a
"completely still until touched" idle. Lattice gets these structurally rather
than by hand, but they must be enforced by the runtime and the SDK, not left
to application authors:

1. **One blocking wait over all sources; infinite timeout when truly idle.**
   Frame's loop blocks in `poll` with timeout `-1` whenever nothing is pending
   - no spin, no periodic re-check. Lattice's equivalent is a strand parked on
   its completion-queue doorbell with no armed timer (SCHEDULING.md 1). The
   rule the runtime enforces: an idle strand has *no* timer armed unless a real
   deadline exists. A periodic re-check "just in case" is the anti-pattern that
   defeats deep sleep, and the runtime should make it hard to write by default.

2. **A timeout is armed only when there is a concrete future deadline, and it
   is the exact time until that deadline - not a poll interval.** Frame arms a
   finite timeout in exactly three cases: a page-flip is in flight (cap 100 ms,
   a safety net for a lost DRM completion), a one-cycle background repaint was
   deferred (16 ms), or the screen-blank deadline is approaching (wake *once*,
   exactly at the deadline). Otherwise the timeout is infinite. This is the
   absolute-deadline discipline from REALTIME.md 4 applied to idle: never a
   recurring tick, always a one-shot armed at a specific instant, and removed
   the moment the reason is gone.

3. **Disabled event sources cost nothing.** Frame represents an empty client
   slot or an unavailable device as a negative fd, which `poll` ignores at zero
   cost - it never has to scan or reap dead sources. Lattice's analogue: a
   completion queue with no registered producer contributes nothing to a
   strand's wait set; there is no "check if this went away" polling. Capacity
   that is provisioned but unused imposes no idle cost.

4. **Damage-driven work, never full recompute.** Frame repaints only the dirty
   rectangles a change recorded (`damage_add` → repaint just those rects +
   `clflush` just those lines), and holds a single `comp_dirty` flag so a burst
   of draws collapses to one repaint per wake rather than one per request. The
   OS-level principle: work is triggered by a specific event carrying a
   specific delta, and coalesced. This is exactly the completion-window
   batching (OPEN-QUESTIONS.md 1) and the structured-channel model (SHELL.md 9)
   - an idle client does zero work because nothing produced a delta, and a
   flurry of deltas collapses to one unit of work at the next wake, not N.

The general statement: **idle cleanliness is the absence of self-generated
work.** Every wakeup must trace to an external event (input, completion,
deadline, message); a system with no pending external events and no armed
deadline does literally nothing and the CPU reaches its deepest sleep state.
Frame achieves this by hand in assembly; Lattice's doorbell-parked strands,
one-shot deadline timers, capability-gated event sources, and delta-coalesced
work make it the default - and §3a's measured wakeup budget is what keeps it
true over time.

The remote/low-power profile's core need (PROFILES.md 4):

- **Energy source as a metered input.** Battery state of charge, solar/charge
  input, and grid availability are telemetry on the event stream; the node
  knows its current and projected energy budget.
- **Brownout as an energy-pressure event.** When the budget tightens
  (battery low, solar drops, thermal cap hit), the kernel emits energy-pressure
  events with deadlines: cells shed elastic load, defer deferrable jobs, drop
  to lower DVFS points, and power-gate idle engines - cooperatively first,
  then enforced (identical shape to memory reclaim, MEMORY.md 7). No abrupt
  crash; graceful, bounded degradation.
- **Safe shutdown and durability.** If energy cannot sustain operation, the
  durability classes (IO.md 2) plus A/B content-addressed images (BOOT.md 2)
  mean a power loss is survivable - `durable` writes are persistent, state is
  reconstructable, and the node boots clean and reconciles when power returns.
- **Local autonomy meanwhile.** Combined with the offline-first cluster design
  (leases, partition tolerance, store-and-forward sync - PROFILES.md 4,
  ARCHITECTURE.md 4.8), a solar-powered node with a flaky uplink runs its
  local workload within its energy budget and syncs opportunistically.

## 5. Per-profile posture

Power management is a profile-selected subsystem (PROFILES.md 6):

- **Server / fleet:** efficiency-oriented - DVFS and idle for power/thermal
  savings and cost, but wall power, so no battery/brownout logic. Even here it
  pays: race-to-idle plus tickless plus engine power-gating reduce datacenter
  energy, a real operating cost.
- **Embedded / IoT:** full DVFS + deep idle + tight footprint; energy budgets
  routine; often battery-backed.
- **Remote / low-power:** the full battery/solar/brownout stack (section 4) is
  central, not optional.
- **Desktop / laptop:** suspend/resume, lid/idle behavior, and
  battery-vs-plugged policies - a large but well-understood surface, part of
  the desktop profile's later ecosystem effort (PROFILES.md 3.2).

## 6. What is deliberately not attempted (yet)

- **Sub-MMU microcontroller power modes** (the deepest sleep tricks of
  MCU-class parts) fall under the out-of-scope deep-embedded boundary
  (PROFILES.md 6) - a different runtime's concern.
- **Mobile-handset power stacks** (aggressive radio/baseband power management,
  vendor-specific SoC sleep) are not a target (phone/mobile is out).
- **Vendor-proprietary platform power firmware** is contained, never trusted
  in-kernel (the QCE doctrine, ACCELERATORS.md) - the OS drives standard
  interfaces (ACPI/OPP/PSCI-class) and treats board-specific blobs as
  contained driver cells.

## 7. Honest costs

- Energy-aware scheduling adds real complexity to the scheduler and needs
  measured per-platform energy/frequency curves (another attach-time
  benchmark, like offload benchmarking in BOOT.md 5) - datasheet power numbers
  are as untrustworthy as datasheet throughput.
- The battery/solar/brownout stack is genuine new work that did not exist for
  the server target; it is the price of the low-power/remote profile, and it
  is the reason that profile is sequenced after the server and networking
  profiles (PROFILES.md 5).
- Deep suspend/resume correctness (saving and restoring device/engine state)
  is notoriously bug-prone; it gets the same fault-injection and soak testing
  as the rest (ARCHITECTURE.md 8.3, 8.5), and DRBG reseed-on-resume is
  mandatory (TIME-IDENTITY.md 4) exactly as for VM restore.
