# Graphics and Display

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.11 (Vulkan/HID note).
Scope is deliberately narrow - this is a fleet OS, not a desktop.

Position: graphics is not a special subsystem. A GPU is an engine
(ACCELERATORS.md), Vulkan's explicit model *is* the native model, and
display is a compositor cell doing queue handoff. There is no in-kernel
display server, no DRM/KMS-equivalent monolith.

## 1. Why Vulkan fits almost suspiciously well

The engine model was, in effect, generalized GPU scheduling from the start,
so Vulkan maps nearly 1:1:

| Vulkan concept | Native form |
|---|---|
| Queues (graphics/compute/transfer) | Engine queue pairs |
| Command buffers | Submission entries / dependency-graph nodes |
| **Timeline semaphores** | The kernel's native cross-engine sync object |
| Device memory allocations | Typed memory grants (`Buffer<Hbm>` etc.) |
| Descriptor sets | Capability references to buffers/images |
| Swapchain images | Sealed buffers handed to the compositor |

A Vulkan driver cell is therefore a thin encoder over native primitives, not
a translation layer fighting an alien model. Timeline semaphores being the
native sync object is the key alignment - the thing Vulkan added in its
maturity is the thing this OS is built on.

## 2. Two uses of Vulkan

1. **Compute lowering floor.** For GPUs without a blessed vendor stack,
   Vulkan compute is the portable tile-IR target (AI-ARCHITECTURE.md 4).
   llama.cpp's Vulkan backend is the existence proof that serious compute
   runs this way.
2. **Graphics proper.** Rendering pipelines run as engine work; presentation
   is queue handoff of a sealed swapchain image to a compositor cell.

## 3. The compositor is a cell

- A compositor is an ordinary cell holding: display-controller engine grants
  (scanout), input event-queue grants (HID, below), and read grants to
  client swapchain images (sealed buffers).
- Clients render into their own buffers, seal, and hand a read capability to
  the compositor - zero-copy presentation, and a client cannot scribble a
  frame mid-scanout (the seal enforces it).
- Multiple compositors can exist (per session, per seat, headless virtual
  display for remoting); none is privileged by anything but its grants.

## 4. HID and input

- Input devices are **event-queue sources** granted to a session/compositor
  cell. A keyboard or pointer is a device engine emitting typed events on a
  queue - the same event stream machinery as everything else.
- Input routing is capability-scoped: a cell receives input only for
  surfaces it owns, so the classic X11 "any client can snoop all input"
  problem has no equivalent - there is no global input bus.
- Remote input (VNC/RDP-style, or a browser-based console) is a cell
  synthesizing events onto a queue under an explicit grant, indistinguishable
  downstream from local input.

## 5. Display scope - what this OS does and does not do

Does:

- Headless rendering and compute (the overwhelming fleet case).
- A basic compositor + Vulkan for local console, remote console, and
  operator/kiosk surfaces.
- Video decode/encode as engine work (relevant for transcode fleets - the
  whisper/transcode composition in AI-ARCHITECTURE.md 3).

Does not (initially, and stated as such in TARGET-ARCHITECTURES.md 8):

- Ship a desktop environment, window-manager ecosystem, or consumer GPU
  feature breadth.
- Chase the desktop Linux graphics stack (Wayland protocol compatibility,
  Mesa driver matrix, X11). A Wayland *personality* is conceivable later as
  an edge translation (compositor protocol -> native compositor), the same
  pattern as POSIX and Kubernetes - but it is not a goal now.
- Deep power management / display-idle logic tuned for laptops.

## 6. Honest note

Graphics is the least-developed area of this design on purpose: the target is
servers, and every hour spent on a desktop stack is an hour not spent on the
capability core or the compilation service. The claim here is only that the
*foundations* (engine model, Vulkan mapping, sealed-buffer presentation,
capability-scoped input) leave a clean, in-doctrine place for graphics to
grow if a form factor ever demands it - not that a rich display stack exists.
