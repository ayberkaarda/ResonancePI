# ADR-0002: Default endpoint switching through the undocumented `IPolicyConfig`

## Status

Proposed (2026-09-12)

## Context

Resonance needs to be able to change the system's default audio render
endpoint (all three roles: console, multimedia, communications) from inside
the app — this is the "switch" half of the product, the other half being
per-endpoint profile restore. Windows has never shipped a public, documented
API for this. Every shipping tool in this product class (EarTrumpet,
SoundSwitch, AudioSwitcher and others) drives the same private coclass,
`PolicyConfigClient`, through its undocumented `IPolicyConfig` interface —
there is no supported alternative; the only choice is whether Resonance
implements switching at all.

Because the interface has no published header, its vtable layout cannot be
taken from an SDK — it has to be reconstructed from independent public
reimplementations and cross-checked, because a single missing or extra slot
before `SetDefaultEndpoint` does not fail to compile; it silently dispatches
the call to a neighbouring method at runtime, with no compiler or test-suite
signal that anything is wrong. Four independent sources were compared during
implementation:

| Source | Slots before `SetDefaultEndpoint` |
|---|---|
| EarTrumpet `IPolicyConfig.cs` (github.com/File-New-Project/EarTrumpet) | 10 |
| `audioswitch` `IPolicyConfig.h` (github.com/tartakynov/audioswitch) | 10 |
| `AudioEndPointLibrary` `PolicyConfig.h` (github.com/Belphemur/AudioEndPointLibrary) | 10 |
| `coreaudio-dotnet` `IPolicyConfig.cs` | 9 (omits `ResetDeviceFormat`) |

Three of the four agree exactly, byte for byte, on both the method names and
their order. The fourth is a known outlier: it drops `ResetDeviceFormat`,
which shifts every following slot — including `SetDefaultEndpoint` — one
position early. It was not followed.

The resulting slot table, `IUnknown`'s three slots plus twelve interface
methods:

| Slot | Method |
|------|--------|
| 0–2 | `IUnknown` (`QueryInterface`, `AddRef`, `Release`) |
| 3 | `GetMixFormat` |
| 4 | `GetDeviceFormat` |
| 5 | `ResetDeviceFormat` |
| 6 | `SetDeviceFormat` |
| 7 | `GetProcessingPeriod` |
| 8 | `SetProcessingPeriod` |
| 9 | `GetShareMode` |
| 10 | `SetShareMode` |
| 11 | `GetPropertyValue` |
| 12 | `SetPropertyValue` |
| **13** | **`SetDefaultEndpoint`** |
| 14 | `SetEndpointVisibility` |

`CLSID_PolicyConfigClient` is `{870AF99C-171D-4F9E-AF0D-E63DF40C2BC9}`; the
`IPolicyConfig` IID targeted (Windows 10/11) is
`{F8679F50-850A-41CF-9C72-430F290290C8}`. The older `IPolicyConfigVista`
fallback (`{568B9108-44BF-40B4-9006-86AFE5B5A620}`) was deliberately not
implemented — it exists for pre-Windows-10 compatibility, which is out of
scope for this product, and every source above agrees the method order is
identical between the two, so adding it later is a small, low-risk addition
if it is ever needed.

A `CoCreateInstance` on the coclass succeeding only proves the coclass is
registered and answers to the targeted IID; it cannot prove the vtable slot
order is still what the three agreeing sources describe. The one thing that
can show the order is right is a real call landing on the right method — which
is exactly the shape of the risk this ADR exists to record.

## Decision

Declare `IPolicyConfig` manually in `crates/resonance-core/src/policy_config.rs`
using the slot table above, gated entirely behind a `switching` Cargo feature
(`resonance-core/Cargo.toml`) that is **off by default** — the default build
(`com-backend` only) keeps session hooking and profile recording working
without ever touching this interface. `PolicyConfig::new()` doubles as a
startup smoke check (`CoCreateInstance` against the coclass); on this
machine, with `--features switching`, the check succeeded, and a real
`switch <endpoint>` command through the `resonance-app --dump-events` CLI
produced a genuine `OnDefaultDeviceChanged` from Windows for all three roles
(console, multimedia, communications) with the expected new endpoint id, then
successfully restored the original default the same way. This is measured
confirmation, on real hardware, that the slot-13 assignment of
`SetDefaultEndpoint` is correct on this system — not merely inferred from the
three agreeing sources.

If the coclass cannot be created (interface withdrawn by a future Windows
release, or not registered on a given machine), `AudioCore` degrades to
`policy: None` and every `CoreCommand::SetDefaultEndpoint` is logged and
dropped instead of failing — profile recording and restore-by-value continue
to work, only the "switch the OS default for me" action is unavailable.
Builds without the `switching` feature take the same degrade path
unconditionally, with no `IPolicyConfig` code compiled in at all.

## Consequences

- `crates/resonance-core/Cargo.toml` gains a `switching = ["com-backend"]`
  feature. `cargo build -p resonance-core` (default features) never touches
  `IPolicyConfig`; `cargo build -p resonance-core --features switching` does.
- `AudioCore` carries an `Option<PolicyConfig>` (feature-gated) populated once
  at startup by the smoke check described above; `CoreCommand::SetDefaultEndpoint`
  routes through it and never panics on failure.
- The pinned `windows = "=0.62.2"` / `windows-core = "=0.62.2"` dependency
  versions now also cover the `#[interface]` macro's ABI contract for
  `IPolicyConfig`'s hand-declared vtable: an unpinned upgrade could silently
  change how the macro maps a `Result`-returning method to the underlying
  `HRESULT`, which is exactly the mechanism `SetDefaultEndpoint`'s declaration
  depends on. Any future `windows` crate upgrade must re-verify this module
  specifically, not just re-run the test suite.
- `PolicyConfig` holds a COM interface pointer and is therefore not `Send`,
  the same guarantee every other COM wrapper in the audio core relies on to
  stay on its own thread — the compiler, not a code review, stops it from
  reaching the UI or state threads.
- A future Windows update that changes the `IPolicyConfig` vtable would not be
  caught by `cargo build` or `cargo test` — only by the smoke check failing
  (coclass/IID gone entirely) or by a real switch silently landing on the
  wrong method (vtable reordered but interface still answers). The former
  degrades gracefully by design; the latter is a known, accepted residual risk
  of depending on unsupported API, not something this ADR claims to eliminate.

## Alternatives considered

- **Do not implement switching at all, profiles only.** Rejected: switching
  the OS default endpoint from inside the app is a core part of the product,
  not an optional extra: a user plugging in headphones and wanting Resonance
  to both switch the output and restore its remembered volume is the primary
  scenario. This ADR's feature-flag/degrade design keeps this alternative
  available as a fallback build mode rather than the whole product's default.
- **Add the `IPolicyConfigVista` fallback now.** Deferred, not rejected: all
  four sources agree its method order matches the modern interface, so the
  work is small if a pre-Windows-10 target is ever needed; adding it now would
  be speculative given the product does not target that OS range.
