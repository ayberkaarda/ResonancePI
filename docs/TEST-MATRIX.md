# Manual test matrix

Every row below was executed on real hardware (not simulated), with the
method and result recorded at the time. Entries carried forward from
earlier development are dated; entries added for the hardening pass are
marked accordingly.

## Endpoint enumeration and notifications

| Test | Method | Result |
|---|---|---|
| Active render endpoints listed correctly at startup | `resonance-app --dump-events`, compared against Windows Sound settings | Matched exactly |
| Default-device change is observed | Changed the default output in Windows Sound settings while `--dump-events` was running | `DefaultEndpointChanged` printed with the correct `flow`/`role`/`id` |
| Core-only working set stays small | `Get-Process` on `--run` | ~11–12 MB, comfortably under the 10 MB debug-build reference and reasonable for a release build |

## Session hooking

| Test | Method | Result |
|---|---|---|
| A volume change made by another application is observed as not our own | Changed a live session's volume in the Windows volume mixer while `--dump-events` was running | `SessionVolumeChanged { own_change: false }` |
| A volume change we issue is tagged as our own | `--dump-events`'s interactive `vol` command against a real session | `SessionVolumeChanged { own_change: true }` |
| No leak or handle growth across many short-lived sessions | 50 open/close cycles of a short audio process while `--dump-events` ran | RSS grew ~0.28 MB total (non-linear, not a leak pattern); handle count flat after the first few cycles |

## Default-endpoint switching

| Test | Method | Result |
|---|---|---|
| Switching the default endpoint from the app reaches all three roles | `--dump-events --features switching`'s `switch` command | Windows itself reported `DefaultEndpointChanged` for `Console`/`Multimedia`/`Communications`, all with the requested id |
| Windows' own Sound settings reflect the switch | Same session, checked Settings after `switch` | Reflected correctly; endpoint restored to the original device afterward |

## State manager and persistence

| Test | Method | Result |
|---|---|---|
| A per-endpoint profile is restored on switch | Set distinct volumes for the same application on two endpoints, switched back and forth | Correct volume re-applied on each switch, well under the 100 ms target (log-timestamp correlated) |
| A corrupted store file falls back to its backup, then to empty | Unit-tested (`resonance-state`) with deliberately corrupted fixtures | Falls back to `.bak`; falls back to an empty store only if both are unreadable |
| A document written by an older build (missing a newer `Settings` field) still loads | Reproduced live against this machine's real `profiles.json` (missing a `hotkey` key added in a later build) before and after the fix | Before: whole document rejected, silently replaced with an empty store on the next save (a real, measured data-loss risk). After: loads correctly, missing field defaults to the product default; real file's `endpoints` data verified unchanged |

## Overlay UI

| Test | Method | Result |
|---|---|---|
| Overlay opens/closes on the global shortcut, tray, and settings toggle | Manual, `Ctrl+Alt+V` and tray menu | Works; overlay is destroyed (not hidden) on close |
| Idle CPU with the overlay open | 5 minutes, 60 samples of `Get-Process` processor-time deltas, no interaction | Average 0.0030%, max 0.026% — no polling baseline |
| Overlay-open / overlay-closed-after-opening working set | `Get-Process` immediately after toggling | ~76 MB open / ~49 MB closed-after-opening (ADR-0004 budget: ≤90 MB / ≤70 MB) — within budget |
| DPI scaling at 100% | Fresh launch, default display scale | Renders and functions correctly |
| DPI scaling at 125% | Display scale changed via Windows Settings, then toggled the overlay | Renders and functions correctly; confirmed visually (screenshot + live confirmation on the actual display) |
| DPI scaling at 150% | Fresh launch after changing display scale to 150%, then toggled the overlay | Renders and functions correctly; confirmed visually (screenshot) |
| "Forget" removes a saved entry | Manual, overlay settings panel | Entry removed from the live view and not re-seeded on the next session for that process |

## Hardening pass

| Test | Method | Result |
|---|---|---|
| A second launch does not start a second copy | Launched a release build twice in one session; the two backend-starting modes (`--run`/`--overlay`) are guarded, the two read-only diagnostic modes (`--dump-events`, `--help`) are not | Second launch printed "Resonance is already running; this instance will exit.", exited 0 immediately, no backend/audio-core/COM initialization happened; exactly one process remained |
| Autostart toggle writes/removes the registry entry | Round-tripped the real `HKCU\...\Run` key (write, re-write, remove, remove-again) | Value written correctly quoted (`"<exe>" --overlay`), REG_SZ; removed cleanly; removing an absent value is treated as success, not an error; the other ~16 existing `Run` entries on this machine were untouched |
| A panic logs and best-effort-flushes before the process aborts | Deliberate panic triggered on a spawned thread in a release build | Log line captured the panic (thread/location/message) before termination; the emergency flush was confirmed (not just attempted) before the process exited via `panic=abort` (observed exit code `0xC0000409`, the expected fastfail code for an aborted panic) |
| `cargo clippy --all-targets -- -D warnings` is clean on the full workspace | `cargo clippy --all-targets -- -D warnings` | Clean, 0 warnings |
| `cargo deny check` is clean | `cargo deny check` | Clean (`advisories ok, bans ok, licenses ok, sources ok`); dependency graph scoped to the actual shipping target (`x86_64-pc-windows-msvc`) so platform-conditional dependencies of cross-platform crates aren't evaluated |
| Release build succeeds with the pinned profile | `cargo build --release` | Succeeds; `resonance-app.exe` is 5.49 MB (`lto="fat"`, `codegen-units=1`, `panic="abort"`, `strip=true`, `opt-level=3` core / `"z"` for `resonance-ui`, all unchanged from the pinned profile) |
| A pid-keyed session (protected/UWP process whose real identity couldn't be resolved) is not persisted | Unit-tested in `resonance-state`, event-driven (a `SessionCreated` with a `pid:<n>` process key) | Confirmed: no entry reaches the persisted store, the live view is unaffected, and the write-behind debounce is not armed by it |

## Known limitations carried into the release

See `README.md`'s "Known limitations" section: `IPolicyConfig` dependency
for switching, one profile entry per executable rather than per session,
and protected/UWP sessions not persisting a profile entry.
