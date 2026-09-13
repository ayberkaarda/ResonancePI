//! On-disk state model persisted to `%APPDATA%\Resonance\profiles.json`.
//!
//! `ProfileStore` is the authoritative shape persisted to
//! `%APPDATA%\Resonance\profiles.json`. This module only defines the data
//! and its (de)serialization; the reducer that mutates a live `ProfileStore`
//! from `AudioEvent`/`UiCommand` traffic is a separate piece of work.
//!
//! Two fields in the spec shape use types that `serde` cannot derive for
//! automatically, so this module supplies small `serde(with = "...")` shim
//! modules rather than deviating from the field types themselves:
//!
//! - `SystemTime` (`last_seen`, `updated_at`): `serde` has no built-in
//!   `Serialize`/`Deserialize` for `std::time::SystemTime` (its in-memory
//!   representation is platform-specific), so [`unix_time`] encodes it as
//!   whole seconds + nanoseconds since the Unix epoch.
//! - `RoleSet` (`Settings::switch_roles`): defined in `resonance-core` and
//!   does not derive `Serialize`/`Deserialize` there (this crate must not
//!   modify `resonance-core`), so [`role_set`] encodes it as its three
//!   public `bool` fields.
//! - `HotkeyConfig` (`Settings::hotkey`): also defined in `resonance-core`
//!   without deriving `Serialize`/`Deserialize`, so [`hotkey_config`] encodes
//!   it as its five public fields, the same way [`role_set`] does for
//!   `RoleSet`.

use resonance_core::messages::{EndpointId, HotkeyConfig, ProcessKey, RoleSet};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Authoritative on-disk state, persisted as a whole on every write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileStore {
    /// Schema version; migrations are keyed off this. Current: 1.
    pub schema_version: u32,
    pub endpoints: HashMap<EndpointId, EndpointProfile>,
    pub settings: Settings,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            schema_version: 1,
            endpoints: HashMap::new(),
            settings: Settings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointProfile {
    /// Display only; the endpoint id is the map key.
    pub friendly_name: String,
    #[serde(with = "unix_time")]
    pub last_seen: SystemTime,
    pub sessions: HashMap<ProcessKey, SessionEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// 0.0..=1.0
    pub volume: f32,
    pub muted: bool,
    #[serde(with = "unix_time")]
    pub updated_at: SystemTime,
}

/// Every field of [`Settings`] carries a serde default so that a stored
/// document written by an older build — one that predates a field being added
/// — still deserializes instead of failing the *whole* document with a
/// "missing field" error. That failure mode is not local to the settings
/// object: `Settings` is nested inside [`ProfileStore`], so one unknown-to-the-
/// old-file field would reject the user's entire saved profile set and hand the
/// caller an empty store, which a subsequent save would then write over the
/// real data. Missing keys must therefore degrade to the same values
/// `Settings::default()` produces — not to the field type's zero value, which
/// for `overlay_opacity` would be a nearly invisible overlay, for
/// `prune_after_days` would prune everything immediately, and for
/// `switch_roles` would silently switch no roles at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_switch_roles", with = "role_set")]
    pub switch_roles: RoleSet,
    #[serde(default)]
    pub overlay_position: Option<(f32, f32)>,
    #[serde(default = "default_overlay_opacity")]
    pub overlay_opacity: f32,
    #[serde(default)]
    pub autostart: bool,
    /// Drop entries not seen for N days (default 90).
    #[serde(default = "default_prune_after_days")]
    pub prune_after_days: u32,
    #[serde(default, with = "hotkey_config")]
    pub hotkey: HotkeyConfig,
}

/// Matches `Settings::default()`. `RoleSet` derives `Default`, but that
/// derive yields all-`false` (switch no roles), whereas the product default is
/// `RoleSet::all()` — so a bare `#[serde(default)]` on that field would be
/// silently wrong rather than a compile error.
fn default_switch_roles() -> RoleSet {
    RoleSet::all()
}

/// Matches `Settings::default()`; `f32::default()` is `0.0`, a fully
/// transparent overlay.
fn default_overlay_opacity() -> f32 {
    0.9
}

/// Matches `Settings::default()`; `u32::default()` is `0`, which would treat
/// every stored entry as immediately prunable.
fn default_prune_after_days() -> u32 {
    90
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            switch_roles: RoleSet::all(),
            overlay_position: None,
            overlay_opacity: 0.9,
            autostart: false,
            prune_after_days: 90,
            hotkey: HotkeyConfig::default(),
        }
    }
}

/// `SystemTime` <-> Unix epoch (seconds + nanoseconds) for serde.
///
/// A `SystemTime` earlier than `UNIX_EPOCH` (not expected in practice for
/// `last_seen`/`updated_at`) clamps to the epoch on serialize rather than
/// erroring, so a corrupt/adversarial clock never turns into a save failure.
mod unix_time {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Serialize, Deserialize)]
    struct UnixTime {
        secs: u64,
        nanos: u32,
    }

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let duration = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        UnixTime {
            secs: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let UnixTime { secs, nanos } = UnixTime::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + Duration::new(secs, nanos))
    }
}

/// `RoleSet` <-> its three public `bool` fields for serde.
///
/// `resonance_core::messages::RoleSet` does not derive `Serialize`/
/// `Deserialize` (and this crate must not add that to `resonance-core`), so
/// this shim reads/writes its fields directly instead.
mod role_set {
    use resonance_core::messages::RoleSet;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    struct RoleSetShadow {
        console: bool,
        multimedia: bool,
        communications: bool,
    }

    pub fn serialize<S>(roles: &RoleSet, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RoleSetShadow {
            console: roles.console,
            multimedia: roles.multimedia,
            communications: roles.communications,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<RoleSet, D::Error>
    where
        D: Deserializer<'de>,
    {
        let shadow = RoleSetShadow::deserialize(deserializer)?;
        Ok(RoleSet {
            console: shadow.console,
            multimedia: shadow.multimedia,
            communications: shadow.communications,
        })
    }
}

/// `HotkeyConfig` <-> its five public fields for serde.
///
/// `resonance_core::messages::HotkeyConfig` does not derive `Serialize`/
/// `Deserialize` (and this crate must not add that to `resonance-core`), so
/// this shim reads/writes its fields directly instead.
mod hotkey_config {
    use resonance_core::messages::HotkeyConfig;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    struct HotkeyConfigShadow {
        ctrl: bool,
        alt: bool,
        shift: bool,
        win: bool,
        key: u32,
    }

    pub fn serialize<S>(hotkey: &HotkeyConfig, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        HotkeyConfigShadow {
            ctrl: hotkey.ctrl,
            alt: hotkey.alt,
            shift: hotkey.shift,
            win: hotkey.win,
            key: hotkey.key,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HotkeyConfig, D::Error>
    where
        D: Deserializer<'de>,
    {
        let shadow = HotkeyConfigShadow::deserialize(deserializer)?;
        Ok(HotkeyConfig {
            ctrl: shadow.ctrl,
            alt: shadow.alt,
            shift: shadow.shift,
            win: shadow.win,
            key: shadow.key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_store_default_has_schema_version_1() {
        let store = ProfileStore::default();
        assert_eq!(store.schema_version, 1);
        assert!(store.endpoints.is_empty());
        assert_eq!(store.settings, Settings::default());
    }

    #[test]
    fn settings_default_matches_spec() {
        let settings = Settings::default();
        assert_eq!(settings.switch_roles, RoleSet::all());
        assert_eq!(settings.overlay_position, None);
        assert_eq!(settings.overlay_opacity, 0.9);
        assert!(!settings.autostart);
        assert_eq!(settings.prune_after_days, 90);
        assert_eq!(
            settings.hotkey,
            resonance_core::messages::HotkeyConfig::default()
        );
    }

    #[test]
    fn session_entry_system_time_round_trips_through_json() {
        let entry = SessionEntry {
            volume: 0.42,
            muted: true,
            updated_at: SystemTime::UNIX_EPOCH
                + std::time::Duration::new(1_726_000_000, 123_000_000),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let decoded: SessionEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, decoded);
    }

    #[test]
    fn settings_role_set_round_trips_through_json() {
        let settings = Settings {
            switch_roles: RoleSet {
                console: true,
                multimedia: false,
                communications: true,
            },
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).expect("serialize");
        let decoded: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(settings, decoded);
    }

    #[test]
    fn settings_hotkey_round_trips_through_json() {
        let settings = Settings {
            hotkey: resonance_core::messages::HotkeyConfig {
                ctrl: false,
                alt: true,
                shift: true,
                win: true,
                key: 0x20,
            },
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).expect("serialize");
        let decoded: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(settings, decoded);
    }

    /// A document written before `hotkey` existed as a field must still load.
    /// Before every `Settings` field carried a serde default, the missing key
    /// failed deserialization of the *whole* `ProfileStore`, so the repository
    /// treated a perfectly good file as corrupt and fell back to an empty
    /// store — discarding every saved per-application volume.
    #[test]
    fn profile_store_from_document_without_hotkey_key_loads() {
        let json = r#"{
            "schema_version": 1,
            "endpoints": {
                "{endpoint-guid}": {
                    "friendly_name": "Speakers",
                    "last_seen": { "secs": 1726000000, "nanos": 0 },
                    "sessions": {
                        "spotify.exe": {
                            "volume": 0.1,
                            "muted": false,
                            "updated_at": { "secs": 1726000000, "nanos": 0 }
                        }
                    }
                }
            },
            "settings": {
                "switch_roles": {
                    "console": true,
                    "multimedia": false,
                    "communications": true
                },
                "overlay_position": [120.0, 240.0],
                "overlay_opacity": 0.75,
                "autostart": true,
                "prune_after_days": 30
            }
        }"#;

        let store: ProfileStore = serde_json::from_str(json).expect("deserialize");

        // The one absent field falls back to the product default.
        assert_eq!(store.settings.hotkey, HotkeyConfig::default());

        // Everything present is taken from the document, not from
        // `Settings::default()` — the fallback must be per field, not wholesale.
        assert_eq!(
            store.settings.switch_roles,
            RoleSet {
                console: true,
                multimedia: false,
                communications: true,
            }
        );
        assert_eq!(store.settings.overlay_position, Some((120.0, 240.0)));
        assert_eq!(store.settings.overlay_opacity, 0.75);
        assert!(store.settings.autostart);
        assert_eq!(store.settings.prune_after_days, 30);

        // And the real payload survives.
        let endpoint = store
            .endpoints
            .get(&EndpointId::from("{endpoint-guid}"))
            .expect("endpoint preserved");
        assert_eq!(endpoint.friendly_name, "Speakers");
        assert_eq!(
            endpoint
                .sessions
                .get(&ProcessKey::from("spotify.exe"))
                .expect("session preserved")
                .volume,
            0.1
        );
    }

    /// Each omitted key must fall back to the value `Settings::default()` uses,
    /// which for these fields is *not* the field type's zero value. Pinning the
    /// exact values here because a wrong fallback compiles cleanly and would
    /// only show up as an invisible overlay or as profile entries pruned on
    /// sight.
    #[test]
    fn settings_missing_keys_fall_back_to_product_defaults() {
        let settings: Settings = serde_json::from_str("{}").expect("deserialize empty settings");

        assert_eq!(settings.switch_roles, RoleSet::all());
        assert_ne!(settings.switch_roles, RoleSet::default());
        assert_eq!(settings.overlay_opacity, 0.9);
        assert_eq!(settings.prune_after_days, 90);
        assert_eq!(settings.overlay_position, None);
        assert!(!settings.autostart);
        assert_eq!(settings.hotkey, HotkeyConfig::default());
        assert_eq!(settings, Settings::default());
    }

    /// Omitting one key at a time must leave the other fields alone.
    #[test]
    fn settings_single_missing_key_defaults_only_that_field() {
        let without_opacity: Settings = serde_json::from_str(
            r#"{
                "switch_roles": {
                    "console": false,
                    "multimedia": true,
                    "communications": false
                },
                "overlay_position": null,
                "autostart": true,
                "prune_after_days": 7
            }"#,
        )
        .expect("deserialize");
        assert_eq!(without_opacity.overlay_opacity, 0.9);
        assert_eq!(without_opacity.prune_after_days, 7);
        assert!(without_opacity.autostart);
        assert_eq!(
            without_opacity.switch_roles,
            RoleSet {
                console: false,
                multimedia: true,
                communications: false,
            }
        );

        let without_prune: Settings = serde_json::from_str(
            r#"{
                "overlay_opacity": 0.25,
                "autostart": false
            }"#,
        )
        .expect("deserialize");
        assert_eq!(without_prune.prune_after_days, 90);
        assert_eq!(without_prune.overlay_opacity, 0.25);
        assert_eq!(without_prune.switch_roles, RoleSet::all());
    }

    /// `#[serde(default = "...")]` composes with `#[serde(with = "...")]`: the
    /// default is used only when the key is absent, and the `with` module's
    /// deserializer is used whenever it is present.
    #[test]
    fn settings_present_role_set_key_still_uses_the_shim_deserializer() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "switch_roles": {
                    "console": false,
                    "multimedia": false,
                    "communications": true
                }
            }"#,
        )
        .expect("deserialize");
        assert_eq!(
            settings.switch_roles,
            RoleSet {
                console: false,
                multimedia: false,
                communications: true,
            }
        );
        assert_eq!(settings.hotkey, HotkeyConfig::default());
    }

    #[test]
    fn profile_store_round_trips_through_json() {
        let mut sessions = HashMap::new();
        sessions.insert(
            ProcessKey::from("spotify.exe"),
            SessionEntry {
                volume: 0.75,
                muted: false,
                updated_at: SystemTime::now(),
            },
        );
        let mut endpoints = HashMap::new();
        endpoints.insert(
            EndpointId::from("{endpoint-guid}"),
            EndpointProfile {
                friendly_name: "Speakers".to_string(),
                last_seen: SystemTime::now(),
                sessions,
            },
        );
        let store = ProfileStore {
            schema_version: 1,
            endpoints,
            settings: Settings::default(),
        };

        let json = serde_json::to_string_pretty(&store).expect("serialize");
        let decoded: ProfileStore = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(store, decoded);
    }
}
