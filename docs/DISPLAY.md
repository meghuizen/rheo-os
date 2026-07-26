# Display — Frame Buffers, Vsync, and Frame Pacing

**Status:** Draft v0.1. Covers the display pipeline from display-controller
engine to input-to-photon latency. Relates to GRAPHICS.md (Vulkan, compositor
cells), REALTIME.md (PeriodicTask, frame pacing), MEMORY.md (typed memory
kinds), ACCELERATORS.md (engine model), and KERNEL-RUST.md (ring buffers).

The central claim: every display problem — tearing, stutter, input lag,
HDR, VRR — has the same underlying cause: **the OS has no first-class model
for "this pixel buffer is being scanned out right now."** Linux's DRM/KMS
retrofits this onto the framebuffer model; Wayland fixes the protocol but
not the OS primitives. Lattice's sealed buffer + capability handoff + typed
DisplaySurface memory kind makes "this buffer is live on the display" a
kernel-enforced invariant rather than a convention.

---

## 1. The display controller as an engine

The display controller is an engine in the standard sense (ACCELERATORS.md 1):
- Enumerated at boot, firmware measured, trust class assigned
- Benchmarked: measured timing precision of vsync delivery
- IOMMU-mapped: its DMA reads from front buffers are grant-checked
- It has a command queue (flip commands, mode changes, cursor updates)
- It produces completion events (vsync, vblank, hotplug, underrun)

The display engine grant encodes everything a compositor needs:

```rust
pub struct DisplayEngineGrant {
    /// Which physical connector (HDMI, DP, eDP)
    pub connector:    ConnectorId,

    /// Negotiated mode — set at grant time, immutable during the grant
    pub mode:         DisplayMode,

    /// The display controller's flip command queue
    /// (buffer swap, cursor update, brightness, etc.)
    pub flip_queue:   QueuePair<FlipCommand, FlipCompletion>,

    /// Vsync/vblank events delivered here as typed completions
    pub vsync_events: CompletionQueue<VsyncEvent>,

    /// The display controller's IOMMU domain (for mapping front buffers)
    pub iommu_domain: IommuDomainGrant,
}

pub struct DisplayMode {
    pub width:          u32,
    pub height:         u32,
    pub refresh:        RefreshMode,        // see section 5
    pub pixel_format:   PixelFormat,        // ARGB8888 | XRGB2101010 | ...
    pub hdr:            HdrConfig,
    pub color_gamut:    ColorGamut,         // sRGB | P3 | BT.2020
    pub scan_tiling:    ScanTiling,         // Linear | Tiled (hw-specific)
}
```

The kernel's role is exactly what it is for every engine: set up the IOMMU
domain, route the vsync interrupt to the compositor's completion queue, and
refuse commands that violate the grant's scope. The kernel never touches
pixel data. It does not run a composition engine. It does not decide what is
on screen.

---

## 2. The DisplaySurface memory kind

Frame buffers are not ordinary memory. A new memory kind — `DisplaySurface`
— carries the constraints the display controller requires:

```rust
pub struct DisplaySurface {
    /// Physical location — display controllers often require device-local
    /// memory (GPU VRAM on discrete, shared on iGPU, CMA on embedded)
    pub location:   SurfaceLocation,    // DeviceLocal | Shared | CmaPool

    /// Pixel layout — display controllers may require linear scanout;
    /// GPUs prefer tiled. The allocation negotiates a compatible format.
    pub tiling:     ScanTiling,

    /// Dimensions aligned to hardware requirements
    pub width:      u32,
    pub height:     u32,
    pub stride:     u32,                // bytes per row (may be padded)
    pub format:     PixelFormat,

    /// IOMMU mapping for the display controller (read-only)
    /// Only present when this surface is the active front buffer
    pub scanout_map: Option<IommuMapping>,
}
```

The type system enforces constraints the conventional framebuffer model
cannot express:

```rust
// This is a compile error — the display engine requires DisplaySurface,
// not a generic DDR buffer:
fn set_front_buffer(engine: &DisplayEngineGrant, buf: Buffer<Ddr>) {
    // error: expected Buffer<DisplaySurface>, found Buffer<Ddr>
}

// Correct:
fn set_front_buffer(engine: &DisplayEngineGrant, buf: Buffer<DisplaySurface>) {
    // The buffer's IOMMU mapping is already valid for this display controller
}
```

---

## 3. The swapchain — double and triple buffering

The compositor allocates and manages the swapchain: a ring of `DisplaySurface`
buffers. The number of buffers determines the buffering model.

```rust
pub struct Swapchain {
    buffers:  Vec<SwapchainBuffer>,     // 2 = double, 3 = triple
    front:    AtomicUsize,              // index of the buffer being scanned out
    engine:   DisplayEngineGrant,
}

pub struct SwapchainBuffer {
    surface:  Buffer<DisplaySurface>,
    state:    AtomicSwapState,
    render_done: TimelineSemaphore,     // GPU signals when render completes
    flip_done:   TimelineSemaphore,     // display controller signals after scanout
}

#[derive(Debug)]
pub enum SwapState {
    Scanning,       // display controller is reading this buffer — sealed, immutable
    Pending,        // render complete, waiting for vsync to flip to front
    Rendering,      // GPU is writing into this buffer
    Available,      // free for the next render
}
```

### Double buffering — zero tearing, maximum stutter risk

```
T=0:   Front=A (scanning), Back=B (rendering)
T=10ms: GPU finishes B. Waits for vsync.
T=16.67ms: VSYNC fires.
           Flip: A → available, B → scanning.
           Front=B (scanning), Back=A (rendering)
T=16.67ms: GPU starts rendering into A immediately.
```

The risk: if the GPU finishes frame N+1 before the next vsync, it must wait.
The frame period is artificially capped at 60fps even if the GPU can render
at 120fps. Input lag = up to one full frame period.

### Triple buffering — smooth at variable frame rates

```
T=0:       Front=A (scanning), B (pending/rendered), C (rendering)
T=10ms:    GPU finishes C. C→pending. GPU starts into A (next available after flip).
           Three buffers: A is still scanning, B and C are both pending.
T=16.67ms: VSYNC.
           Flip to newest pending buffer (C, since it's fresher than B).
           A→available (display done scanning).
           GPU immediately starts rendering into A.
           C→scanning, B→available (skip B — C is newer).
T=16.67ms: GPU is already rendering A. No stall.
```

Triple buffering means the GPU never stalls waiting for a vsync: there is
always an `Available` buffer to render into. Input lag = render time + at
most one vsync period (and with VRR, less than one period).

### The swapchain buffer lifecycle as typed state transitions

```rust
impl Swapchain {
    /// Acquire a buffer for rendering. Returns immediately if one is available;
    /// parks the strand if all buffers are in Scanning or Pending state
    /// (can only happen with double buffering under heavy load).
    pub async fn acquire(&self) -> SwapchainBuffer<'_, Available> {
        loop {
            if let Some(buf) = self.find_available() {
                buf.transition(Available, Rendering);
                return buf;
            }
            // Park until a buffer becomes available (vsync will release one)
            self.buffer_available.wait().await;
        }
    }

    /// Present a completed render. Transitions the buffer to Pending and
    /// signals to the compositor that a new frame is ready.
    pub fn present(&self, buf: SwapchainBuffer<'_, Rendering>) {
        // Seal the buffer — GPU wrote it, now it's immutable
        buf.seal();
        buf.transition(Rendering, Pending);
        self.pending_frames.notify();
    }

    /// Compositor calls this on vsync to flip to the newest pending buffer.
    async fn flip_on_vsync(&self, vsync: VsyncEvent) {
        let newest_pending = self.newest_pending_buffer();
        if let Some(buf) = newest_pending {
            // Submit flip command to the display controller
            let flip = FlipCommand {
                buffer_iommu_addr: buf.surface.iommu_addr(),
                at_vblank:         vsync.vblank_start,
            };
            self.engine.flip_queue.submit(flip).await;
            self.transition_front(buf);
        }
        // Release the previous front buffer back to Available
        self.release_old_front();
    }
}
```

The sealed buffer during scanout is the key correctness guarantee: while
the display controller holds its IOMMU read grant on the front buffer, no
other entity can write to it. A GPU crash, an application exit, even the
compositor dying — the sealed buffer remains intact until the display
controller releases its grant. No tearing by construction.

---

## 4. Vsync as a typed event — not a blocking ioctl

On Linux, waiting for vsync is `ioctl(fd, DRM_IOCTL_WAIT_VBLANK, ...)` —
a blocking call. The thread is blocked in the kernel. A delay anywhere in
the kernel path adds directly to frame latency.

On Lattice, vsync is a typed completion on the display engine's event queue:

```rust
pub struct VsyncEvent {
    /// Monotonic timestamp of when this vblank interval started
    pub vblank_start:     Instant,
    /// Duration of the blanking interval (safe flip window)
    pub vblank_duration:  Duration,
    /// Sequence number — monotonically increasing, no gaps
    pub sequence:         u64,
    /// Display timing: measured actual period vs nominal (drift detection)
    pub actual_period_ns: u64,
    pub nominal_period_ns: u64,
    /// Underrun flag: did the display controller run out of pixels last frame?
    pub underrun:         bool,
}
```

The compositor's vsync loop is a strand parked on a completion queue:

```rust
async fn vsync_loop(engine: &DisplayEngineGrant, swapchain: &Swapchain) {
    loop {
        // Park until the display controller delivers a vsync event
        let vsync = engine.vsync_events.recv().await;

        // Track drift: if actual_period_ns deviates from nominal, the
        // display is running off-spec (temperature, power, cable issue)
        if vsync.actual_period_ns.abs_diff(vsync.nominal_period_ns) > 500_000 {
            log_event!(DisplayDrift {
                actual_ns:  vsync.actual_period_ns,
                nominal_ns: vsync.nominal_period_ns,
            });
        }

        // Underrun: the display showed the same frame twice (freeze)
        if vsync.underrun {
            log_event!(DisplayUnderrun { sequence: vsync.sequence });
        }

        // Flip to the newest ready frame within the vblank window
        swapchain.flip_on_vsync(vsync).await;
    }
}
```

The vsync event arrives with the exact timestamp of the vblank start — from
the display controller's own clock, mapped to the monotonic clock reference
at engine attach time (the same `engine_clock → monotonic` mapping all
engines get, REALTIME.md section 5). The compositor knows precisely when
the next scanout will begin and can schedule rendering to meet that deadline.

---

## 5. Variable refresh rate (VRR — GSYNC, FreeSync, HDMI 2.1)

With VRR, the display controller does not produce vsync at a fixed rate.
Instead, it holds the current frame until the compositor triggers a new one
(within the VRR window — typically 40-240Hz). This eliminates the frame-period
rounding that causes double-vsync stutter.

```rust
pub enum RefreshMode {
    /// Fixed: vsync fires at exactly this rate. Compositor must deliver a
    /// frame within each period or the display repeats the last frame.
    Fixed { hz: u32 },

    /// Variable: compositor triggers refresh when a new frame is ready.
    /// The display holds the current frame indefinitely (within the window).
    Variable {
        min_hz: u32,   // display refreshes at least this often (prevent flicker)
        max_hz: u32,   // fastest supported refresh
    },
}
```

In VRR mode, the swapchain's `present()` path becomes:

```rust
// VRR mode: trigger the refresh immediately when the frame is ready,
// instead of waiting for the next fixed vsync slot.
pub async fn present_vrr(&self, buf: SwapchainBuffer<'_, Rendering>) {
    buf.seal();
    buf.transition(Rendering, Pending);

    // Submit flip immediately — the display controller presents it
    // as soon as the electron beam finishes the current scanout row
    // (the "minimum scanout gap" — typically 1-2ms).
    // This is the dependency graph form: the flip node depends on
    // the render timeline semaphore being signalled.
    let flip = FlipCommand {
        buffer_iommu_addr: buf.surface.iommu_addr(),
        at_vblank:         NextOpportunity,  // not at a fixed vsync slot
    };
    self.engine.flip_queue.submit(flip).await;
}
```

The dependency graph form is cleaner: the flip command node depends on the
GPU render timeline semaphore. When the GPU signals "render complete", the
display controller engine sees its input dependency resolved and executes
the flip. One graph node, no CPU involvement in the handoff:

```
GPU render node ──timeline semaphore──> flip node (display controller engine)
```

This is the VRR ideal: the display controller presents the frame at the
exact moment the GPU finishes, with no CPU in the loop and no fixed-rate
quantisation.

---

## 6. Frame pacing — the PeriodicTask connection

Frame pacing is displaying frames at consistent intervals. At 60fps, frames
must arrive every 16.67ms ± tolerance. Inconsistent delivery (one frame at
8ms, the next at 25ms) produces visible judder even at "correct" average fps.

This is a real-time scheduling problem (REALTIME.md 4). The renderer is a
periodic task synced to vsync:

```rust
// The game's render loop — a periodic task admission-controlled against
// the display's vsync period.
let frame_task = PeriodicTask::builder()
    .period(Duration::from_micros(16_667))  // 60fps = 16.667ms period
    .budget(Duration::from_micros(13_000))  // 13ms budget (3.67ms slack)
    .deadline(Duration::from_micros(15_500))// 1.17ms before next vsync
    .priority(Priority::Hard)
    .pool(CorePool::Latency)               // dedicated core — GPU submit is fast
    .name("game-renderer")
    .build()?;

// Phase-lock the period to the actual vsync.
// When vsync events arrive, adjust the PeriodicTask's phase so
// frame completion consistently lands within the vblank window:
vsync_sync.lock_phase_to(&frame_task, vsync_stream);

loop {
    let _slot = frame_task.wait().await;

    // Acquire a back buffer from the swapchain (zero-copy; we get a
    // writable Buffer<DisplaySurface> grant)
    let back = swapchain.acquire().await;

    // Submit the render as a dependency graph:
    // [scene update] → [GPU draw] → [seal + flip signal]
    let graph = GraphBuilder::new()
        .node("update", move || update_scene(dt))
        .node("draw",   move || gpu_render(&back))
        .node("present",move || swapchain.present(back))
        .edge("update" → "draw")
        .edge("draw"   → "present")
        .build();
    graph.submit().await;

    // _slot drops: timing recorded; overrun detected if > 13ms
}
```

The phase-lock step is important: vsync events arrive at T, T+16.67ms,
T+33.33ms etc. The render loop should activate slightly before each vsync
(not after) so the frame is ready when the vblank window opens. The
`lock_phase_to` call adjusts the `PeriodicTask`'s activation phase based
on measured vsync timestamps — the same absolute-deadline arithmetic as
REALTIME.md section 4, but synced to an external clock (the display).

---

## 7. Input-to-photon latency — the full path

The latency from physical input event to photons leaving the display:

```
[Key press / mouse move]
        ↓ HID device polling (USB: 1ms, Bluetooth: 7.5ms, PS/2: <1ms)
[HID event on input queue]
        ↓ strand wakes (dedicated core: ~500ns)
[Application processes event]
        ↓ graph submission (~100ns)
[GPU renders new frame] ← longest step; depends on scene complexity
        ↓ render timeline semaphore signals (GPU → flip command)
[Display controller flips at next vblank]
        ↓ 0-16.67ms (fixed refresh) or ~0ms (VRR)
[Pixels are scanned out]
        ↓ panel response time (1ms-6ms depending on panel)
[Photons leave the screen]
```

Total on Lattice with VRR and a dedicated render core:
`HID poll + 500ns + render_time + panel_response`

No scheduler jitter on the dedicated core, no blocking kernel calls in the
path, no protocol overhead between compositor and GPU (the GPU is a graph node
in the same dependency graph as the flip command).

Compare to Linux with GNOME/Wayland:
`HID poll + kernel IRQ + compositor wakeup + app wakeup + render + wayland protocol + KMS flip + panel`

The Wayland protocol round trip (compositor ↔ app ↔ compositor) adds at
least one event-loop iteration on each side, typically 1-3ms. KMS flip is
an ioctl + kernel path. These add up to 5-15ms of overhead on top of render
time. Lattice eliminates the protocol round trips (client seals buffer, passes
capability directly to compositor — one operation) and the KMS ioctl (flip is
a queue submission from compositor → display engine).

On a 1ms polling rate mouse and a VRR 240Hz display with 8ms render time:
- Linux (GNOME): ~1 + 8 + 3 (protocol) + 2 (KMS) + 2 (panel) ≈ 16ms
- Lattice: ~1 + 0.5 + 8 + ~0 (VRR) + 2 (panel) ≈ 11.5ms

5ms improvement in input lag is perceptible. Competitive gamers use 240Hz
displays specifically to get below 10ms input lag; removing 5ms of OS
overhead matters.

---

## 8. The compositor cell — where composition lives

The compositor is an ordinary cell (GRAPHICS.md 3) holding:
- The display engine grant (including the flip queue and vsync events)
- Read capabilities to client surfaces (sealed buffers from client cells)
- A GPU partition grant for its own composition work

```rust
// Compositor cell main loop (simplified):
async fn compositor_main(ctx: CompositorContext) {
    let mut vsync_stream = ctx.display.vsync_events.stream();
    let mut client_surfaces: HashMap<ClientId, SealedBuffer<DisplaySurface>> = HashMap::new();

    loop {
        select! {
            // New surface from a client — this is a sealed buffer capability
            // handoff; zero bytes copied
            Some((client, surface)) = ctx.surface_updates.recv() => {
                client_surfaces.insert(client, surface);
                // In VRR mode: trigger an immediate flip if the frame is ready
                if ctx.display.mode.refresh.is_variable() {
                    composite_and_flip(&ctx, &client_surfaces).await;
                }
            }

            // Fixed refresh: compose and flip at vsync
            Some(vsync) = vsync_stream.next() => {
                composite_and_flip(&ctx, &client_surfaces).await;
            }
        }
    }
}

async fn composite_and_flip(
    ctx: &CompositorContext,
    surfaces: &HashMap<ClientId, SealedBuffer<DisplaySurface>>,
) {
    // Acquire a back buffer from the compositor's own swapchain
    let back = ctx.swapchain.acquire().await;

    // Composite: GPU draws all client surfaces into the back buffer.
    // This is a dependency graph: each client surface read + blend = one node.
    let graph = build_composite_graph(surfaces, &back);
    graph.submit().await;

    // Present: seal the back buffer and flip.
    ctx.swapchain.present(back);
}
```

The compositor never needs to copy pixels out of a client buffer into its
own memory and then copy them to the front buffer. Client surfaces are sealed
read-only; the compositor's GPU node reads them directly as textures. One GPU
draw call composites everything. The compositor's output goes into its own
swapchain. The flip is a capability operation. Zero intermediary copies.

---

## 9. High dynamic range (HDR)

HDR surfaces require 10bpc or 16bpc pixel formats and a colour-space metadata
header telling the display how to tone-map. In Lattice these are properties
of the `DisplayMode` and the `DisplaySurface` memory kind:

```rust
pub struct HdrConfig {
    pub enabled:       bool,
    pub metadata:      StaticHdrMetadata,     // max luminance, primaries
    pub transfer_fn:   TransferFunction,      // PQ | HLG | SDR
    pub color_space:   ColorSpace,            // BT.2020 | DCI-P3 | sRGB
}

pub struct StaticHdrMetadata {
    pub max_luminance:     f32,               // nits
    pub min_luminance:     f32,
    pub max_cll:           u16,               // max content light level
    pub max_fall:          u16,               // max frame average light level
}
```

The display engine grant negotiates HDR capability at creation time (the
display must support it, or the grant is rejected for HDR). The compositor
enables a different composition pipeline (linearise client surfaces, blend
in linear light, tone-map to the display's peak luminance, write in PQ or
HLG encoding). The GPU tile IR handles the extra precision (fp16 intermediate
buffers, not 8bpc).

---

## 10. Where each concern lives in the stack

| Concern | Kernel | Compositor cell | App / SDK |
|---|---|---|---|
| Display controller IOMMU mapping | ✓ | | |
| Vsync interrupt → typed event | ✓ | | |
| EDID / mode negotiation | ✓ (at engine attach) | | |
| Buffer format validation | ✓ (via DisplaySurface kind) | | |
| Swapchain allocation | | ✓ | |
| Buffer swap scheduling (double/triple) | | ✓ | |
| VRR trigger timing | | ✓ | |
| Composition (multi-surface blend) | | ✓ (GPU node) | |
| HDR tone mapping | | ✓ (GPU node) | |
| Frame pacing (PeriodicTask) | | | ✓ |
| Render-to-back-buffer | | | ✓ |
| Surface handoff (sealed buffer) | | ✓ receives | ✓ sends |
| Input-to-photon accounting | Vsync ts | Flip ts | HID ts |

The kernel's surface is tiny: IOMMU, vsync interrupt routing, mode
validation. The compositor owns the display pipeline. The application owns
its render. Nothing crosses the boundary via copies.

---

## 11. `sleep` and frame timing in lsh

```lsh
# Wait for the next vsync on the current display
await vsync

# Sync a script to the display refresh rate
every vsync {
    render-frame | present
}

# Target a specific frame rate (PeriodicTask, admission-controlled)
every 16.67ms budget 13ms deadline 15.5ms pool latency {
    render-frame | present
}

# Profile frame timing after a run
echo "frame p99: $(every.stats.exec_p99_ns / 1_000_000) ms"
echo "vsync miss: $(every.stats.deadline_miss)"
echo "input lag:  $(display.stats.input_to_photon_p99_us) µs"
```

---

## 12. Phase H (implemented): the virtio-gpu 2D driver + compositor scanout

Everything above is the design target. What is **built and proven on all
three ISAs** (docs/LIBRHEO.md Phase H) is the bring-up seam: a real GPU
driver that presents a client frame to a (QEMU) display surface.

### The driver (`kernel/src/hw/virtio_gpu.rs`)

A hand-written **virtio-gpu 2D driver** (virtio spec 5.7, the plain 2D /
VIRGL-off subset), structured exactly like the virtio-net / virtio-blk
drivers, over the **two transports**: virtio-mmio on arm/riscv `virt`,
virtio-pci on x86-64 q35 (through the `VIRTIO_PCI_CAP_PCI_CFG` config-space
tunnel, no BAR mapping). Reset + **minimal** feature negotiation
(`VIRTIO_F_VERSION_1` only — no VIRGL, no EDID), a single **controlq** (queue
0; the cursorq is left unconfigured). Every 2D command is a
`virtio_gpu_ctrl_hdr` (24 B) + a command body, submitted on the controlq as a
**2-descriptor chain** (`[device-readable command][device-writable response]`,
linked with `VRING_DESC_F_NEXT` — the virtio-blk request/status pattern), then
the used ring is polled for the device's response code.

The 2D bring-up runs at install time:

1. `GET_DISPLAY_INFO` → `RESP_OK_DISPLAY_INFO`, carrying `pmodes[0]` (scanout 0's
   size — informational; recorded, not used to size the resource).
2. `RESOURCE_CREATE_2D` → `RESP_OK_NODATA`: resource id 1, format
   `B8G8R8A8_UNORM`, a **fixed 128×128**.
3. `RESOURCE_ATTACH_BACKING` → `RESP_OK_NODATA`: the framebuffer backing is
   **16 frame-pool frames** (128×128×4 = 64 KiB), attached as **one
   `virtio_gpu_mem_entry` per frame** (physical address + length) — so the
   backing need not be physically contiguous, and no large kernel static is
   needed.
4. `SET_SCANOUT` → `RESP_OK_NODATA`: bind resource 1 to scanout 0.

A **present** (the `OP_GPU_PRESENT` queue opcode) then copies the cell's
framebuffer bytes into the attached frames and issues `TRANSFER_TO_HOST_2D` +
`RESOURCE_FLUSH` (both → `RESP_OK_NODATA`).

All rings, command/response buffers, and the framebuffer are allocated from
the frame pool (`crate::mm::frames::alloc()`); the only kernel static is a
small `Option<VirtioGpu>`. DMA uses **physical** addresses (`virt_to_phys`);
the CPU reaches everything through the high-half linear map (`phys_to_virt`).

**Framebuffer size — 128×128, why.** RGBA 128×128 = exactly 16 frames, a clean
constant frame count; it matches the display-info fallback; and it is large
enough to be a real transfer while small enough to keep the attach command
(32 + 16×16 = 288 B) inside one command frame. The frame allocator has no
contiguous-multi-frame API, so the multi-entry backing (one entry per frame)
is what makes a non-trivial framebuffer possible without adding one.

### The compositor wiring (librheo `display`)

librheo's `display` gains `Gpu` (the `OP_GPU_PRESENT` verb) and `Scanout` (a
client drawable backed by a framebuffer grant: draw into `pixels_mut`, then
`present().await`). The Phase E in-memory `Compositor` is **unchanged** — it
still composites a shared sealed buffer into its framebuffer and checksums it
(the zero-copy cross-cell proof). Phase H adds the real-hardware step: after
compositing, a framebuffer can be pushed to the device via a GPU present.

The GPU resource backing is a **kernel-side** framebuffer (allocated at
install, attached once), and `present(buf_va, w, h)` copies the cell's bytes
into it before transfer+flush — chosen over attaching the cell's own frames
because the cell's grant frames are not guaranteed contiguous or stable across
the resource's life; the kernel-side framebuffer has a simple, fixed lifetime.

### Honesty — the proof is the round-trip, not a visible pixel

CI runs QEMU **headless** (`-display none`), exactly as the virtio-net proof is
network-free (SLIRP's real ARP reply, not a rendered packet). There is no
monitor to assert a pixel against. The proof is therefore the **genuine driver
round-trip**: every 2D command returns its expected `RESP_OK_*` code from the
real QEMU device model. Measured on all three ISAs, **all six commands return
OK** — `GET_DISPLAY_INFO` (QEMU reports a default 1280×800 even headless),
`RESOURCE_CREATE_2D`, `RESOURCE_ATTACH_BACKING`, `SET_SCANOUT`,
`TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH` — and the `librheo-gpu` cell exits
`0x42`. `SET_SCANOUT` succeeding headless is not guaranteed on every QEMU build
(a display-less scanout can be a no-op); the pass criterion the test asserts is
the cell's `0x42` (present OK = transfer + flush OK), and the per-command report
is printed so the honest surface is visible. This does **not** claim visible
output.

### Deferred (the design above, still future work)

Real VIRGL/3D, the cursor plane (cursorq), multi-scanout, EDID/mode
negotiation, vsync-interrupt→typed-event delivery, an IOMMU-mapped
grant-checked scanout DMA, and an actual visible framebuffer on hardware. The
deliverable is the 2D scanout command round-trip + the compositor present
wiring.
