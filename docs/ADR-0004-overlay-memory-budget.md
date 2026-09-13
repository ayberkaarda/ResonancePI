# ADR-0004: Revised memory budget for the overlay (OpenGL driver resident cost)

## Status

Proposed (2026-09-13)

## Context

`docs/ADR-0003-ui-framework.md` set a memory budget of core-only RSS ≤10 MB
and overlay-open RSS ≤30 MB, based on comparing Tauri's WebView2 footprint
(80–150 MB) against egui/eframe's expected footprint. That comparison did not
account for one specific cost: keeping an OpenGL rendering context alive at
all, independent of anything the application draws.

Measured on real hardware (AMD Radeon RX 9060 XT, release build,
`Get-Process`):

| State | Working Set | Private Bytes |
|---|---|---|
| Backend only, no UI code path exercised | 12.0 MB | 2.4 MB |
| Tray + hotkey running, overlay never opened | 14.3 MB | 2.7 MB |
| Overlay open | 92.4–92.7 MB | 199.6–200.3 MB |
| Overlay closed again (same process) | 64.5–64.6 MB | 148.8–148.9 MB |

The overlay only ever creates its OpenGL context lazily, when the user first
opens it (a design already in place: the tray icon and the global keyboard
shortcut run on their own threads with no rendering context at all, and the
window that shows the overlay is destroyed, not merely hidden, when closed).
Despite that, opening the overlay once raises resident memory by roughly 78
MB, and closing it again only gives back about 28 MB of that.

The direct cause was confirmed by inspecting the process's loaded modules after
closing the overlay:

```
ModuleName      MB
----------      --
OPENGL32.dll   1.05
atig6pxx.dll   0.17
atio6axx.dll  62.21
amdihk64.dll   0.24
```

`atio6axx.dll` — AMD's OpenGL installable client driver — stays loaded for
the rest of the process's life once the first OpenGL context is created, and
it does not release the majority of its own private heap allocations when
that context is destroyed. This is a property of how Windows loads OpenGL
ICDs (the loader has no general mechanism to unload one once a context has
used it) and of this driver's own internal allocator, not a resource leak in
the application: `eframe` 0.36.2's shutdown path was read end to end
(`painter.destroy()` frees every GL program, texture, vertex and element
buffer it created; the surrounding `glutin` context is then dropped, which
calls `wglDeleteContext`) and does not retain anything. Two open/close cycles
measured identically (92.4 → 64.5, then 92.7 → 64.6) rather than growing,
which is consistent with a one-time driver load rather than a per-cycle
leak.

The same class of cost is not specific to this rendering backend: published
measurements of the same empty transparent window rendered through several
different Windows renderers (Slint's own comparison across its femtovg,
skia-opengl, skia-d3d and software backends) put every OpenGL-based renderer
in the same 50+ MB range, with only the Direct3D and pure-software paths
substantially lower. Switching UI frameworks would not avoid this cost while
staying on an OpenGL renderer.

Two paths were evaluated to actually meet the original ≤30 MB figure:

- **Stop using OpenGL, render with the CPU and present through
  `UpdateLayeredWindow`.** This is the only approach that would plausibly
  meet the original number, since it never loads a GPU driver ICD at all. It
  was rejected for now: the two pieces of the Rust ecosystem this would need
  are not ready — the one existing software backend for egui is pinned to an
  egui version well behind the one this project uses, and the general-purpose
  software framebuffer crate available in the ecosystem does not support a
  transparent, alpha-blended surface, which this overlay depends on. Building
  that path from scratch means writing the presentation layer directly
  against `UpdateLayeredWindow` — a real, but open-ended, piece of work with
  no existing reference implementation in this ecosystem to build on, and a
  history of edge cases around remote desktop and lock-screen transitions
  that would need their own testing.
- **Accept the driver's resident cost and revise the budget to match what is
  actually achievable**, while still making the two changes that reduce the
  application's own contribution to it (below).

## Decision

Revise the memory budget for the overlay to reflect the measured OpenGL
driver cost, and apply two small, low-risk reductions to the application's
own share of it:

- Overlay open: **≤90 MB Working Set**, Private Bytes reported alongside it
  rather than budgeted separately (it tracks the same driver cost and moves
  with it).
- Overlay closed again, after having been opened at least once in this
  process: **≤70 MB Working Set**.
- Idle, overlay never opened: **≤15 MB Working Set** — unchanged from what
  was already measured and is unaffected by this decision.
- Core-only (no UI code path at all): **≤10 MB**, unchanged.

Every number above was measured on one machine (AMD Radeon RX 9060 XT) and
is expected to vary on other GPUs and drivers; it is not a portable constant.
Re-measuring on an NVIDIA and an Intel integrated GPU is listed as an open
item below rather than assumed.

Applied alongside this budget revision, two changes reduce what the
application itself adds on top of the driver's fixed cost:

- The egui font atlas is capped to a maximum texture width of 2048 pixels
  (`InputState::max_texture_side`) instead of the driver's own maximum
  (16384 pixels on this GPU), which the atlas would otherwise grow into as
  glyphs are rasterized. A width this small is already generous for one
  compact overlay's text.
- The bundled default font set (which includes a full emoji font and a
  wide-coverage Unicode fallback font) is replaced with one small, embedded
  Latin font sized for exactly what this overlay's UI text needs.

These two changes are expected to reduce, not eliminate, the application's
own contribution on top of the driver floor; they do not change the
conclusion above that ≤30 MB is not reachable while an OpenGL context is
alive.

## Consequences

- The three budget figures in the toolchain decisions become renderer- and
  GPU-dependent facts, not fixed constants: any future change to the
  rendering backend, or a materially different GPU/driver combination in
  later testing, requires re-measuring and, if needed, revising this ADR
  rather than assuming the numbers here still hold.
- Every future measurement against this budget reports Working Set and
  Private Bytes together, and states which GPU/driver it was measured on —
  a bare "RSS" number without that context does not meaningfully compare
  against these figures.
- Switching to CPU rasterization remains available as a future option if a
  product requirement makes the driver's resident cost unacceptable (for
  example, a mode that keeps the overlay open continuously rather than
  toggled). It is not pursued now because the two crates it would depend on
  are not ready for this codebase's egui version and this window's
  transparency requirement respectively; revisiting it means checking
  whether that ecosystem gap has closed, not just re-running the same spike.

## Alternatives considered

- **Do nothing, leave the ≤30 MB figure in place as aspirational.** Rejected:
  an unenforceable budget that every real measurement fails is worse than a
  revised one that gate checks can actually hold the project to.
- **Switch to Direct3D/DXGI instead of OpenGL.** Not pursued in this
  decision: `eframe`'s only non-OpenGL backend renders through `wgpu`, and
  `wgpu`'s DirectX 12 and Vulkan backends on Windows currently have open,
  unresolved issues with per-pixel window transparency — the exact property
  this overlay depends on — so it was not a safe substitution here. Revisiting
  this is reasonable once that upstream transparency support matures.
- **CPU rasterization now instead of deferred.** Rejected for this decision
  for the reasons in Context: the two library dependencies it needs are not
  ready, and building the presentation layer from scratch is an open-ended
  effort better scoped as its own piece of work with its own review, not
  folded into a budget correction.
