//! Persistence trait for loading and saving the profile store.
//!
//! `ProfileRepository` is the seam between the state manager and disk. This
//! module defines the trait, the error type, and an [`InMemoryRepository`]
//! for tests. The real `%APPDATA%\Resonance\profiles.json` implementation
//! (atomic replace via `.tmp` + `MoveFileExW`, `.bak` fallback, hash-skip)
//! is separate, later work and intentionally not started here.

use crate::store::ProfileStore;
use std::sync::Mutex;

/// Errors a [`ProfileRepository`] can report.
///
/// Underlying errors (`std::io::Error`, `serde_json::Error`) are captured as
/// their `Display` text rather than wrapped directly, so this type stays
/// `Send + Sync` and cheap to construct/compare without pulling platform- or
/// library-specific error types into this crate's public API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoError {
    Io(String),
    Serialize(String),
    Deserialize(String),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoError::Io(msg) => write!(f, "repository I/O error: {msg}"),
            RepoError::Serialize(msg) => write!(f, "repository serialize error: {msg}"),
            RepoError::Deserialize(msg) => write!(f, "repository deserialize error: {msg}"),
        }
    }
}

impl std::error::Error for RepoError {}

/// Loads and saves the authoritative [`ProfileStore`].
///
/// `load` on a missing store (e.g. first run, no file yet) returns
/// `Ok(ProfileStore::default())` — a missing store is not an error.
pub trait ProfileRepository: Send {
    fn load(&self) -> Result<ProfileStore, RepoError>;
    fn save(&self, store: &ProfileStore) -> Result<(), RepoError>;
}

/// In-memory [`ProfileRepository`] for tests — no file I/O.
#[derive(Debug, Default)]
pub struct InMemoryRepository {
    state: Mutex<Option<ProfileStore>>,
}

impl InMemoryRepository {
    /// Empty repository; `load()` returns `ProfileStore::default()` until a
    /// `save()` happens, exactly like a missing on-disk file would.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Repository pre-seeded with `store`, as if it had already been saved.
    pub fn seeded(store: ProfileStore) -> Self {
        Self {
            state: Mutex::new(Some(store)),
        }
    }
}

impl ProfileRepository for InMemoryRepository {
    fn load(&self) -> Result<ProfileStore, RepoError> {
        let guard = self
            .state
            .lock()
            .map_err(|e| RepoError::Io(e.to_string()))?;
        Ok(guard.clone().unwrap_or_default())
    }

    fn save(&self, store: &ProfileStore) -> Result<(), RepoError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| RepoError::Io(e.to_string()))?;
        *guard = Some(store.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resonance_core::messages::EndpointId;
    use std::collections::HashMap;

    #[test]
    fn load_on_empty_repository_returns_default_store() {
        let repo = InMemoryRepository::new();
        let loaded = repo.load().expect("load should not fail");
        assert_eq!(loaded, ProfileStore::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let repo = InMemoryRepository::new();
        let mut store = ProfileStore::default();
        store.endpoints.insert(
            EndpointId::from("{endpoint-guid}"),
            crate::store::EndpointProfile {
                friendly_name: "Headphones".to_string(),
                last_seen: std::time::SystemTime::now(),
                sessions: HashMap::new(),
            },
        );

        repo.save(&store).expect("save should not fail");
        let loaded = repo.load().expect("load should not fail");
        assert_eq!(loaded, store);
    }

    #[test]
    fn seeded_repository_loads_seed() {
        let store = ProfileStore::default();
        let repo = InMemoryRepository::seeded(store.clone());
        let loaded = repo.load().expect("load should not fail");
        assert_eq!(loaded, store);
    }
}
