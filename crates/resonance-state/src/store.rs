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

use resonance_core::messages::{EndpointId, ProcessKey, RoleSet};
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(with = "role_set")]
    pub switch_roles: RoleSet,
    pub overlay_position: Option<(f32, f32)>,
    pub overlay_opacity: f32,
    pub autostart: bool,
    /// Drop entries not seen for N days (default 90).
    pub prune_after_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            switch_roles: RoleSet::all(),
            overlay_position: None,
            overlay_opacity: 0.9,
            autostart: false,
            prune_after_days: 90,
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
