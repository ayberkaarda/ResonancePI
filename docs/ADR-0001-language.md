# ADR-0001: Implementation language — Rust

## Status

Accepted (2026-09-12)

## Context

Resonance hooks live WASAPI audio sessions via COM (`IMMDeviceEnumerator`,
`IAudioSessionManager2`, `IAudioSessionEvents`, the undocumented
`IPolicyConfig`) and restores per-endpoint volume/mute profiles in real
time. COM notifications arrive on arbitrary MTA worker threads owned by
`AudioSrv`; the failure mode that defines this product class (EarTrumpet,
SoundSwitch, and similar tools have a long history of these bugs) is a
callback touching state or COM objects that belong to another thread.

Two candidates were considered: Rust (`windows` crate) and C++20 (WIL/WRL +
`winrt::com_ptr`). Both require the same undocumented-API risk for
`IPolicyConfig`, and both offer comparable COM interop ergonomics for
declaring it manually. The deciding factors were thread-safety of COM
callbacks (Rust rejects sharing non-`Send`/`Sync` types across threads at
compile time, though see the correction below), long-running daemon
robustness (no dangling COM references or use-after-free), and toolchain
simplicity (`cargo` vs CMake/MSVC/vcpkg). C++ retains an edge on raw
memory/binary footprint (roughly 3-5 MB core vs 5-8 MB), which is not
decisive for a tray utility on a modern desktop.

## Decision

Rust, with the `windows` crate pinned to an exact version. The version is
not upgraded mid-project: COM method signature shapes (`*const GUID` vs
`&GUID`, `PCWSTR`, `BOOL` vs `bool`) have changed between crate versions
in the past, and an unreviewed upgrade risks silently miscompiling a COM
call site.

## Consequences

- `resonance-core` owns every COM interface pointer; `Drop` runs there.
  `resonance-state` stays free of `windows`/`windows-sys` dependencies so
  it compiles and tests on any OS.
- **Measured, not assumed:** the `windows` crate's COM wrapper types are
  *not* `Send` by default in the pinned version. Moving a COM interface
  pointer to another thread through an ordinary channel is a genuine
  compile error (`E0277`, confirmed while implementing session hooking).
  This is closer to the original framing above than an earlier draft of
  this document assumed. The protection is not unconditional, though: a
  handful of places legitimately need to move an owned COM pointer between
  two threads of the *same* multi-threaded apartment (for example, handing
  a freshly created audio session from the thread that received the
  creation notification to the dedicated audio-core thread). Those cases
  use a one-line wrapper type with an explicit `unsafe impl Send`, whose
  safety comment states which two threads are involved, that both belong
  to the same apartment, and which of them runs the eventual `Drop`. That
  wrapper makes each such crossing visible and individually justified,
  rather than relying on it being safe in general.
- Every `unsafe` block requires a comment stating the thread, apartment,
  and pointer-lifetime invariant it relies on.
- Every COM method signature is verified against the pinned `windows`
  crate version's published documentation before being treated as
  correct; a signature mismatch is fixed by finding the right type, never
  by casting.
- Overlay: egui + eframe, glow backend — see ADR-0003 for the UI framework
  decision and its own trade-offs (Tauri rejected on memory grounds).
- `IPolicyConfig` is undocumented on either language choice; the vtable
  risk is identical and is tracked separately in ADR-0002.

## Alternatives considered

C++20 (WIL/WRL + `winrt::com_ptr`) — rejected as the default path. Viable
if a hard sub-10 MB total footprint becomes a product requirement, or to
reuse an existing ImGui/DX11 overlay codebase. Not pursued because the
~10 MB footprint delta is not decisive for a tray utility on a modern
desktop, and Rust's ownership model still reduces (if not eliminates) a
class of COM lifetime bugs that this product's predecessors have
repeatedly hit.
