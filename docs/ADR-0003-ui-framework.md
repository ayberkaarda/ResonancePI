# ADR-0003: UI framework — egui + eframe (glow backend)

## Status

Accepted (2026-09-12)

## Context

The overlay is a borderless, transparent, always-on-top, draggable widget
showing per-endpoint tabs and per-session volume/mute controls. The core
memory budget is tight: core-only process RSS must stay at or below 10 MB,
and RSS with the overlay open at or below 30 MB. Idle CPU must stay near
0% — the overlay only repaints in response to events from the audio core,
never on a timer.

Three candidates were evaluated:

- **Tauri.** Ships a WebView2-backed window. Even an empty WebView2 window
  typically costs on the order of 80-150 MB of RSS, which alone blows past
  the entire memory budget for this app by roughly an order of magnitude.
  Transparent, click-through overlays on WebView2 are also fragile in
  practice (compositor interaction varies across GPU/driver
  combinations).
- **Slint.** Small footprint, a real declarative UI language. Viable as a
  fallback, but transparent/borderless/always-on-top window support on
  its winit backend is less mature than egui's, and its retained-mode
  model would introduce a second piece of UI state (Slint's own models)
  layered on top of the app's `Snapshot`-based state, duplicating the
  single-source-of-truth the state manager already provides.
- **egui + eframe (glow backend).** Pure Rust, immediate-mode. No FFI
  surface beyond winit and OpenGL. `ViewportBuilder` exposes
  `with_transparent`, `with_decorations(false)`, `with_always_on_top`,
  and `ViewportCommand::StartDrag` for borderless window dragging.
  Immediate-mode rendering means the UI redraws from a cheap clone of the
  latest `Snapshot` each frame it repaints — there is no UI-side mutable
  state to keep in sync with the state manager. Repaints are driven only
  by `request_repaint()` calls triggered from new snapshots, so idle CPU
  stays at 0% between events. The glow (OpenGL) rendering backend is
  roughly 10 MB lighter than the alternative wgpu backend.

## Decision

egui + eframe, glow rendering backend.

## Consequences

- winit initializes COM on the main thread as STA (`OleInitialize`, needed
  for drag-and-drop support). This makes the UI thread STA. The audio core
  therefore runs on its own dedicated thread, initialized as MTA
  (`CoInitializeEx(COINIT_MULTITHREADED)`). No COM interface pointer may
  ever cross this STA/MTA boundary; the UI thread only ever sees a
  `Snapshot` (an owned, cheaply-cloneable value), never a COM object.
- The overlay window is created lazily on first toggle (not at process
  startup) and destroyed — not merely hidden — when closed, to actually
  reclaim the memory it used rather than keep it resident.
- Because rendering is immediate-mode and driven from `Snapshot`, the UI
  crate has no independent state machine to keep consistent with the
  state manager; a stale or dropped snapshot is simply not rendered
  instead of requiring reconciliation logic.
- Slint remains a documented fallback: if egui's transparency proves
  unreliable on a specific target GPU/driver combination during testing,
  switching is a UI-crate-only change (the `Snapshot`/`UiCommand`
  interface between the UI crate and the rest of the app does not change).

## Alternatives considered

Tauri was rejected primarily on memory grounds (see Context above) — its
WebView2 dependency alone exceeds this app's entire memory budget before
any application code runs. Slint was rejected as the default choice but
kept as a documented fallback for the transparency-support risk noted
above.
