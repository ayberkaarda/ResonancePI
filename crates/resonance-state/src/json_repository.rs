//! File-backed [`ProfileRepository`].
//!
//! [`JsonFileRepository`] reads and writes the authoritative
//! `%APPDATA%\Resonance\profiles.json`. Three properties matter here:
//!
//! - **Durable replace.** The new document is written to a sibling `.tmp`
//!   file, flushed to the device, and then renamed over the live file.
//!   A reader therefore never observes a half-written store.
//! - **Backup rotation.** The previous good document is kept as a sibling
//!   `.bak` file, refreshed on every write, and used when the main file no
//!   longer parses.
//! - **Hash skip.** The serialized bytes are hashed and compared against the
//!   last written hash; an unchanged store performs no file operation at all,
//!   so a burst of debounced writes collapses to one actual disk write.
//!
//! This module deliberately uses nothing but `std::fs` / `std::io` for its
//! I/O. This crate is platform-independent by construction and must compile
//! and run its tests on any OS, so no platform API bindings may be linked
//! into it. `File::sync_all` and `fs::rename` already map onto the durable
//! flush and the replacing move on Windows through the standard library's own
//! platform layer, which keeps the crate portable without giving up the
//! atomic-replace behaviour the persistence design depends on.

use crate::repository::{ProfileRepository, RepoError};
use crate::store::ProfileStore;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Suffix appended to the store file name for the staging file.
const TMP_SUFFIX: &str = ".tmp";
/// Suffix appended to the store file name for the rotated previous document.
const BAK_SUFFIX: &str = ".bak";

/// [`ProfileRepository`] backed by a JSON document on disk.
///
/// `last_hash` is `None` until a document is known to be on disk in a state
/// this repository produced; `Some(h)` means "the file currently holds the
/// serialization whose hash is `h`", which is what lets [`Self::save`] skip
/// redundant writes. `None` always forces the next write, so every path that
/// leaves the main file untrustworthy (never written, or repaired from the
/// backup) leaves the field unset on purpose.
#[derive(Debug)]
pub struct JsonFileRepository {
    path: PathBuf,
    last_hash: Cell<Option<u64>>,
}

impl JsonFileRepository {
    /// Repository over `path`. No I/O happens here; the parent directory is
    /// created lazily on the first successful [`Self::save`].
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_hash: Cell::new(None),
        }
    }

    /// Repository over the standard `%APPDATA%\Resonance\profiles.json`.
    pub fn app_data() -> Result<Self, RepoError> {
        Ok(Self::new(Self::app_data_path()?))
    }

    /// `%APPDATA%\Resonance\profiles.json`.
    ///
    /// Reads the `APPDATA` environment variable rather than calling a shell
    /// path API, which keeps this crate free of platform bindings. On a
    /// machine where `APPDATA` is unset (any non-Windows host, or a stripped
    /// service environment) this reports an error instead of inventing a
    /// location — the caller decides what to do with that.
    pub fn app_data_path() -> Result<PathBuf, RepoError> {
        let app_data = std::env::var("APPDATA")
            .map_err(|e| RepoError::Io(format!("APPDATA environment variable unusable: {e}")))?;
        if app_data.is_empty() {
            return Err(RepoError::Io(
                "APPDATA environment variable is empty".to_string(),
            ));
        }
        Ok(PathBuf::from(app_data)
            .join("Resonance")
            .join("profiles.json"))
    }

    /// Path of the main store document.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path of the rotated previous document (`<store file>.bak`).
    pub fn backup_path(&self) -> Result<PathBuf, RepoError> {
        sibling_with_suffix(&self.path, BAK_SUFFIX)
    }

    /// Path of the staging file (`<store file>.tmp`).
    fn temp_path(&self) -> Result<PathBuf, RepoError> {
        sibling_with_suffix(&self.path, TMP_SUFFIX)
    }

    /// Records the hash of `store` as the state now on disk.
    ///
    /// Hashing the re-serialized form (not the bytes just read) keeps the
    /// field directly comparable with what [`Self::save`] computes. A file
    /// written by hand in a different layout but parsing to the same store is
    /// therefore treated as already current.
    fn remember(&self, store: &ProfileStore) {
        match serde_json::to_string_pretty(store) {
            Ok(json) => self.last_hash.set(Some(hash_bytes(json.as_bytes()))),
            // Unserializable store: leave the field unset so the next save
            // writes rather than silently skipping.
            Err(_) => self.last_hash.set(None),
        }
    }

    /// Main document failed to parse: try the rotated backup.
    ///
    /// `last_hash` is intentionally left unset here. The main file is known to
    /// be corrupt, so the next `save` must write even if the state manager
    /// hands back exactly the store that was recovered.
    fn load_from_backup(&self, main_error: &serde_json::Error) -> Result<ProfileStore, RepoError> {
        let backup = self.backup_path()?;
        let text = fs::read_to_string(&backup).map_err(|e| {
            RepoError::Deserialize(format!(
                "store file {} is corrupt ({main_error}) and its backup {} could not be read ({e})",
                self.path.display(),
                backup.display()
            ))
        })?;
        serde_json::from_str::<ProfileStore>(&text).map_err(|backup_error| {
            RepoError::Deserialize(format!(
                "store file {} is corrupt ({main_error}) and its backup {} is corrupt too ({backup_error})",
                self.path.display(),
                backup.display()
            ))
        })
    }
}

impl ProfileRepository for JsonFileRepository {
    fn load(&self) -> Result<ProfileStore, RepoError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            // No document yet (first run): a missing store is not an error.
            // `last_hash` stays unset so the first save actually writes.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfileStore::default());
            }
            Err(e) => return Err(io_error("read store file", &self.path, &e)),
        };

        match serde_json::from_str::<ProfileStore>(&text) {
            Ok(store) => {
                self.remember(&store);
                Ok(store)
            }
            Err(main_error) => self.load_from_backup(&main_error),
        }
    }

    fn save(&self, store: &ProfileStore) -> Result<(), RepoError> {
        let json =
            serde_json::to_string_pretty(store).map_err(|e| RepoError::Serialize(e.to_string()))?;
        let hash = hash_bytes(json.as_bytes());
        if self.last_hash.get() == Some(hash) {
            return Ok(());
        }

        let temp = self.temp_path()?;
        let backup = self.backup_path()?;

        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|e| io_error("create store directory", parent, &e))?;
        }

        write_and_sync(&temp, json.as_bytes())?;

        // Rotation order: refresh the backup by *copying* the live document,
        // then rename the staging file over it. Copying (rather than moving)
        // the old document means the main file is never absent, not even for
        // an instant: an interruption mid-copy leaves the old main file intact
        // and only a truncated backup, and an interruption around the rename
        // leaves either the old or the new complete document. Moving the old
        // file into place instead would open a window where the main file does
        // not exist, and `load` treats a missing file as "first run, default
        // store" — a crash in that window would read as silent data loss.
        if self.path.exists() {
            if let Err(e) = fs::copy(&self.path, &backup) {
                let _ = fs::remove_file(&temp);
                return Err(io_error("rotate store file into backup", &backup, &e));
            }
        }

        if let Err(e) = fs::rename(&temp, &self.path) {
            let _ = fs::remove_file(&temp);
            return Err(io_error("replace store file", &self.path, &e));
        }

        self.last_hash.set(Some(hash));
        Ok(())
    }
}

/// `<path><suffix>`, e.g. `profiles.json` -> `profiles.json.tmp`.
fn sibling_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf, RepoError> {
    let file_name = path.file_name().ok_or_else(|| {
        RepoError::Io(format!(
            "store path has no file name component: {}",
            path.display()
        ))
    })?;
    let mut name = file_name.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Non-cryptographic content fingerprint; only ever compared for equality.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Writes `bytes` to `path` and flushes them to the device before returning.
///
/// The file is closed (and, on failure, removed) before returning, so no
/// handle or stray staging file outlives this call.
fn write_and_sync(path: &Path, bytes: &[u8]) -> Result<(), RepoError> {
    let mut file =
        fs::File::create(path).map_err(|e| io_error("create temporary store file", path, &e))?;

    if let Err(e) = file.write_all(bytes) {
        let error = io_error("write temporary store file", path, &e);
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }

    // Full flush including metadata: the staging file must be on the device
    // before it is renamed over the live document.
    if let Err(e) = file.sync_all() {
        let error = io_error("flush temporary store file", path, &e);
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }

    Ok(())
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> RepoError {
    RepoError::Io(format!("failed to {action} ({}): {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EndpointProfile, SessionEntry};
    use resonance_core::messages::{EndpointId, ProcessKey};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Unique scratch directory, removed when the test ends (also on panic).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let n = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "resonance-state-{}-{label}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create scratch directory");
            Self(dir)
        }

        fn store_path(&self) -> PathBuf {
            self.0.join("profiles.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixed_time(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::new(secs, 0)
    }

    fn sample_store(volume: f32) -> ProfileStore {
        let mut sessions = HashMap::new();
        sessions.insert(
            ProcessKey::from("spotify.exe"),
            SessionEntry {
                volume,
                muted: false,
                updated_at: fixed_time(1_726_000_000),
            },
        );
        let mut endpoints = HashMap::new();
        endpoints.insert(
            EndpointId::from("{endpoint-guid}"),
            EndpointProfile {
                friendly_name: "Headphones".to_string(),
                last_seen: fixed_time(1_726_000_001),
                sessions,
            },
        );
        ProfileStore {
            schema_version: 1,
            endpoints,
            settings: crate::store::Settings::default(),
        }
    }

    #[test]
    fn repository_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<JsonFileRepository>();
    }

    #[test]
    fn load_without_file_returns_default_store() {
        let dir = TempDir::new("missing");
        let repo = JsonFileRepository::new(dir.store_path());

        let loaded = repo.load().expect("missing file must not be an error");

        assert_eq!(loaded, ProfileStore::default());
        assert!(!dir.store_path().exists(), "load must not create the file");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new("roundtrip");
        let repo = JsonFileRepository::new(dir.store_path());
        let store = sample_store(0.75);

        repo.save(&store).expect("save");
        let loaded = repo.load().expect("load");

        assert_eq!(loaded, store);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = TempDir::new("mkdir");
        let nested = dir.0.join("Resonance").join("profiles.json");
        let repo = JsonFileRepository::new(nested.clone());

        repo.save(&sample_store(0.5)).expect("save");

        assert!(nested.exists(), "save must create the parent directory");
    }

    #[test]
    fn save_leaves_no_temporary_file() {
        let dir = TempDir::new("notmp");
        let repo = JsonFileRepository::new(dir.store_path());
        let temp = repo.temp_path().expect("temp path");

        repo.save(&sample_store(0.25)).expect("first save");
        assert!(
            !temp.exists(),
            "temporary file left behind after first save"
        );

        repo.save(&sample_store(0.30)).expect("second save");
        assert!(
            !temp.exists(),
            "temporary file left behind after second save"
        );
    }

    #[test]
    fn saving_an_unchanged_store_performs_no_write() {
        let dir = TempDir::new("hashskip");
        let repo = JsonFileRepository::new(dir.store_path());
        let store = sample_store(0.6);

        repo.save(&store).expect("first save");

        // Remove the document behind the repository's back. If the second
        // save touched the disk at all it would recreate it; the hash skip is
        // proven by the file staying absent. (Stronger than comparing mtimes,
        // whose resolution is coarse enough to hide a real second write.)
        fs::remove_file(dir.store_path()).expect("remove store file");

        repo.save(&store).expect("second save");

        assert!(
            !dir.store_path().exists(),
            "unchanged store must not be written again"
        );
    }

    #[test]
    fn saving_a_changed_store_writes_again() {
        let dir = TempDir::new("changed");
        let repo = JsonFileRepository::new(dir.store_path());

        repo.save(&sample_store(0.1)).expect("first save");
        let changed = sample_store(0.9);
        repo.save(&changed).expect("second save");

        let loaded = repo.load().expect("load");
        assert_eq!(loaded, changed);
    }

    #[test]
    fn second_save_rotates_previous_document_into_backup() {
        let dir = TempDir::new("rotate");
        let repo = JsonFileRepository::new(dir.store_path());
        let backup = repo.backup_path().expect("backup path");
        let first = sample_store(0.2);
        let second = sample_store(0.8);

        repo.save(&first).expect("first save");
        assert!(!backup.exists(), "no backup before a document is replaced");

        repo.save(&second).expect("second save");

        let rotated: ProfileStore =
            serde_json::from_str(&fs::read_to_string(&backup).expect("read backup"))
                .expect("backup parses");
        assert_eq!(rotated, first, "backup must hold the previous document");
        assert_eq!(repo.load().expect("load"), second);
    }

    #[test]
    fn corrupt_main_file_falls_back_to_backup() {
        let dir = TempDir::new("fallback");
        let repo = JsonFileRepository::new(dir.store_path());
        let backup = repo.backup_path().expect("backup path");
        let store = sample_store(0.44);

        fs::write(
            &backup,
            serde_json::to_string_pretty(&store).expect("serialize"),
        )
        .expect("write backup");
        fs::write(dir.store_path(), "{ this is not json").expect("write corrupt main");

        let loaded = repo.load().expect("must recover from the backup");
        assert_eq!(loaded, store);
    }

    #[test]
    fn save_after_backup_fallback_repairs_the_main_file() {
        let dir = TempDir::new("repair");
        let repo = JsonFileRepository::new(dir.store_path());
        let backup = repo.backup_path().expect("backup path");
        let store = sample_store(0.33);

        fs::write(
            &backup,
            serde_json::to_string_pretty(&store).expect("serialize"),
        )
        .expect("write backup");
        fs::write(dir.store_path(), "not json at all").expect("write corrupt main");

        let recovered = repo.load().expect("recover from backup");
        // The recovered store is byte-identical to the backup, so this only
        // rewrites the corrupt main file if the hash skip was left disarmed.
        repo.save(&recovered).expect("save");

        let reloaded: ProfileStore =
            serde_json::from_str(&fs::read_to_string(dir.store_path()).expect("read main"))
                .expect("main file must parse again");
        assert_eq!(reloaded, store);
    }

    #[test]
    fn corrupt_main_and_corrupt_backup_report_an_error() {
        let dir = TempDir::new("bothcorrupt");
        let repo = JsonFileRepository::new(dir.store_path());
        let backup = repo.backup_path().expect("backup path");

        fs::write(dir.store_path(), "{ broken").expect("write corrupt main");
        fs::write(&backup, "also broken }").expect("write corrupt backup");

        match repo.load() {
            Err(RepoError::Deserialize(_)) => {}
            other => panic!("expected a deserialize error, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_main_without_backup_reports_an_error() {
        let dir = TempDir::new("nobackup");
        let repo = JsonFileRepository::new(dir.store_path());

        fs::write(dir.store_path(), "{ broken").expect("write corrupt main");

        match repo.load() {
            Err(RepoError::Deserialize(_)) => {}
            other => panic!("expected a deserialize error, got {other:?}"),
        }
    }

    #[test]
    fn sibling_paths_keep_the_full_file_name() {
        let repo = JsonFileRepository::new(PathBuf::from("C:\\data\\Resonance\\profiles.json"));

        assert_eq!(
            repo.temp_path().expect("temp path").file_name(),
            Some("profiles.json.tmp".as_ref())
        );
        assert_eq!(
            repo.backup_path().expect("backup path").file_name(),
            Some("profiles.json.bak".as_ref())
        );
    }

    #[test]
    fn app_data_path_ends_with_the_store_location() {
        match JsonFileRepository::app_data_path() {
            Ok(path) => {
                assert!(
                    path.ends_with("Resonance/profiles.json")
                        || path.ends_with("Resonance\\profiles.json"),
                    "unexpected store path: {}",
                    path.display()
                );
            }
            // APPDATA is not set on non-Windows hosts; reporting that is the
            // documented behaviour, so there is nothing to assert.
            Err(RepoError::Io(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }
}
