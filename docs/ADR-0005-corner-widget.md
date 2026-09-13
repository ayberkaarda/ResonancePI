# ADR-0005: A persistent corner widget rendered without egui

## Status

Proposed (2026-09-13)

## Context

The overlay's lifecycle was deliberately built so that no window, and no
OpenGL context, exists until the user asks for the full panel: `eframe`
is only started on `ToggleOverlay` and torn back down the moment the
panel closes, because the graphics driver behind even an empty window
costs tens of megabytes of resident memory the application has no
business holding at rest.

A new requirement asks for an optional, persistent presence on screen
regardless of that: a small icon docked in a corner of the desktop that
stays visible while the application runs, can be dismissed with a
right-click ("Hide"), and can be brought back from the tray icon's own
menu ("Show icon"). Clicking it opens the same full overlay panel the
global shortcut and the tray icon already open.

Rendering that corner icon through `eframe`/`egui`, the same way the full
panel is rendered, would mean an `eframe::run_native` call living for as
long as the widget is visible — which, being the new default persistent
state, is most of the time the application runs. That reintroduces
exactly the idle-memory cost the current lifecycle design exists to
avoid, for a widget whose content needs turned out to still be modest
(see the revision below).

**Revision (still within the initial proposal, before acceptance):** the
requirement grew in three ways before this ADR was accepted: the widget
should be draggable to any position (remembered across restarts, the same
way the full panel's position already is), and hovering over it — no
click needed — should grow it into a small list of the currently
connected audio endpoints, each clickable to switch the default endpoint
directly, without opening the full panel. This is real, live,
interactive content, which the first draft of this ADR flagged as the
specific trigger for revisiting the native-rendering decision. It is kept
native anyway (see Decision) because the actual content is still small
and low-frequency: a handful of rows that only need to repaint when the
hover state changes or the endpoint list itself changes (rare), not a
continuously-animating or densely-interactive surface. A dense settings
UI, live-updating volume bars, or anything requiring per-frame redraws
would tip this back toward `egui`; a short, mostly-static list does not.

## Decision

The corner widget is rendered as a plain Win32 layered window
(`WS_EX_LAYERED`, alpha-blended via `UpdateLayeredWindow`), on its own
dedicated thread with its own message pump — the same pattern already
used for the tray icon and the global hotkey, neither of which pull in
`eframe` either. No OpenGL context and no `egui` context exist for it.
Redraws happen only on a state change (hover enter/leave, an animation
step, a snapshot update while grown, a drag move) rather than on a timer
or a per-frame loop; the thread otherwise sleeps in `GetMessageW`, woken
only by an actual interaction.

At rest it shows a single static icon. Hovering (no click) grows it, over
a short animation, into a list of the currently connected audio
endpoints, read from the same shared snapshot the overlay panel already
reads — the widget thread only ever reads this, it does not own or
compute it. Clicking a listed endpoint switches the default endpoint
immediately (the same effect as picking its tab in the full panel) without
opening that panel. Moving the mouse away shrinks it back. A left-button
press-and-hold on the icon while still at rest — not while grown, to keep
"drag it" and "pick an endpoint from the list" from ever being the same
gesture — repositions the widget anywhere on screen; the position is
persisted the same way the full panel's is.

Endpoints have no headphone-vs-speaker (or any device-class) flag
available anywhere in this codebase's data model (`EndpointView` carries
only an id, a friendly name, and a state) — inventing one via string
matching on the friendly name would be locale-fragile and often wrong.
Each row therefore uses one consistent generic icon plus the endpoint's
actual friendly name as text, rather than guessing a per-device-type
glyph.

## Consequences

- `resonance-ui` gains a third windowing/rendering path (native
  layered-window GDI, alongside the existing `eframe`/glow panel and the
  tray icon's own icon abstraction), rather than reusing the panel's
  renderer for a second window. This is more code than a shared renderer
  would be, in exchange for the widget costing no meaningful resident
  memory beyond its bitmaps and one thread.
- The widget's visibility and position are persisted settings
  (`Settings.widget_visible`, `Settings.widget_position`), reduced by
  `resonance-state` exactly like every other setting; only the native
  window itself — creating it, showing it, hiding it, moving it, redrawing
  it — is owned by `resonance-ui`.
- The widget thread reads the same shared latest-snapshot state the
  overlay panel's bridge thread already maintains; it does not open its
  own subscription or duplicate that plumbing.
- Text layout and per-row hit-testing are hand-rolled against GDI
  (`DrawTextW`/`TextOut` and simple rectangle containment), which is
  markedly more code than `egui`'s layout would need for the same list.
  That cost is accepted here because the list is short and its content
  changes rarely. If the widget's content grows past a short list — live
  volume levels, per-row controls, anything needing frequent redraws —
  this decision should be revisited in a superseding ADR; this one no
  longer covers a "static icon only" widget, but it still assumes a small,
  low-frequency-repaint surface.

## Alternatives considered

- **Render it with `eframe`, as a second always-open viewport.** Rejected
  for the reason in Context: it would keep an OpenGL context resident for
  as long as the widget is visible, which is most of the application's
  running time by design — the opposite of what the existing overlay
  lifecycle was built to achieve.
- **Show only a taskbar/tray icon, no on-screen widget at all.** This is
  the application's existing behavior; it does not meet the request for a
  persistent, glanceable, click-to-open presence somewhere on the desktop
  itself rather than tucked into the notification area.
