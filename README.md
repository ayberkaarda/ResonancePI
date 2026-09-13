# Resonance

A lightweight Windows background tool that remembers each application's
volume and mute state per audio output device, and restores it
automatically whenever the default device changes.

WASAPI keeps a running application's session volume attached to whichever
physical output it was created on; Windows itself does nothing to restore
it when you switch, say, from headphones to speakers. Resonance watches
live audio sessions, records volume/mute changes you make per device, and
re-applies the right values the moment a session appears or the default
device changes — surfaced through a small always-on-top overlay, a tray
icon, and a global keyboard shortcut (`Ctrl+Alt+V` by default,
configurable from the overlay's settings panel).

## Requirements

- Windows 10 or later, 64-bit.
- No installer; a single executable (`resonance-app.exe`).

## Usage

Run `resonance-app.exe --overlay`. By default a small icon docks in the
bottom-right corner of the screen; hover over it to see the connected
devices and click one to switch to it directly, or click the icon itself
(or press the global shortcut, or use the tray icon) to open the full
overlay, pick a device tab, and adjust or forget per-application entries.
Press and hold the icon to drag it anywhere on screen; it stays there
across restarts. Right-click it to hide it; bring it back from the tray
icon's "Show icon" menu item. Toggling "Start with Windows" in the
settings panel launches it automatically at sign-in.

Two additional command-line modes exist for diagnosing the backend without
any UI — `resonance-app --run` and `resonance-app --dump-events` — see
`resonance-app --help` for details; they are development tools, not part
of the normal user workflow.

## Known limitations

- **Default-device switching depends on an undocumented Windows
  interface.** Windows has never shipped a public API for changing the
  system default audio output; every tool in this space (including this
  one) relies on the same private `IPolicyConfig` interface Windows
  Settings itself uses internally. Resonance checks whether it works at
  startup and, if it doesn't (a future Windows update changes or removes
  it), continues running in profile-only mode: it still restores
  volumes automatically when *you* switch the default device through
  Windows' own UI, it just can't switch the device *for* you from its own
  overlay.
- **One profile entry per executable, not per session.** An application
  that opens more than one simultaneous audio session (rare, but it
  happens) shares a single saved volume/mute entry across all of them —
  Resonance cannot tell two sessions from the same program apart for the
  purposes of a saved profile.
- **Sessions belonging to protected, elevated, or UWP-sandboxed
  processes** may not be identifiable by name (Windows refuses the
  identity query). When that happens, Resonance still shows and controls
  the session for the current run, but does not save a profile entry for
  it — a process id is not a stable identity across restarts, and saving
  by pid would eventually apply the wrong saved profile to an unrelated
  process that later reuses the same id.
- **The overlay's on-screen footprint does not scale with Windows display
  scaling** (100%/125%/150%/…) — it stays a fixed physical size rather
  than growing at higher scale factors. It renders correctly at every
  scale tested; it's simply a fixed-size panel by design, not a scaling
  defect.
- **The corner widget starts in the bottom-right corner** unless you've
  already dragged it somewhere else, in which case it remembers that
  position across restarts.
- **While the overlay panel is open, "Show icon" (tray menu) and the
  widget's own "Hide" have no effect.** Both work normally again as soon
  as the panel is closed.
- **Right-clicking the widget while its device list is showing collapses
  it back to the small icon** while the "Hide" menu is open — the menu
  still works correctly, it's only a visual quirk.
- **The widget's device rows all use the same icon** — there's no
  reliable way to tell a headphone from a speaker from a generic Windows
  audio endpoint, so each row shows the device's real name instead of
  guessing an icon for it.

## License

See individual dependency licenses (`cargo deny check licenses`, or
`Cargo.lock`, list the full third-party dependency tree).
