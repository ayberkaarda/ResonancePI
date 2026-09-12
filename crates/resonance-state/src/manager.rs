//! State Manager — reducer, restore algorithm and write-behind debounce
//! for the audio session profile manager.
//!
//! [`StateManager`] is a pure state machine: it owns the authoritative
//! in-memory [`ProfileStore`], a non-persisted live view of the endpoints and
//! sessions the core currently sees, and it turns `AudioEvent` / `UiCommand`
//! traffic into [`Dispatch`]es (commands for the audio core) plus
//! [`PersistRequest`]s (work for the persistence thread).
//!
//! It owns no threads, no timers and no I/O:
//!
//! - the debounce deadline is evaluated against an `Instant` passed in by the
//!   caller ([`StateManager::take_persist_request`]), so the 300 ms window is
//!   testable without sleeping and without a background timer thread;
//! - the repository is only touched once, in [`StateManager::new`], to load the
//!   initial store. Saving is the persistence thread's job — this type merely
//!   hands out a cheap `Arc<ProfileStore>` snapshot when the window elapses.
//!
//! Two shapes deliberately differ from a literal reading of the spec:
//!
//! - the restore algorithm reads and writes a "current default endpoint", but
//!   the persisted `ProfileStore` has no such field: the current default is a
//!   property of the running machine, not of the saved profile. It therefore
//!   lives on the manager and is deliberately not persisted.
//! - `switch_generation` is kept entirely inside this type. `CoreCommand` has
//!   no generation field, so every emitted command is wrapped in a
//!   [`Dispatch`] carrying the generation it was produced under. Dropping
//!   stale work is the core's decision; this module only produces the
//!   information needed to make it.

use crate::repository::{ProfileRepository, RepoError};
use crate::store::{EndpointProfile, ProfileStore, SessionEntry, Settings};
use resonance_core::messages::{
    AudioEvent, CoreCommand, DataFlow, EndpointId, EndpointState, EndpointView, ProcessKey, Role,
    SessionInstanceId, SessionState, SessionView, Snapshot, UiCommand,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Write-behind debounce window: bursts of mutations collapse into one save.
pub const PERSIST_DEBOUNCE: Duration = Duration::from_millis(300);

/// Seconds in a day, used by [`StateManager::prune`].
const SECONDS_PER_DAY: u64 = 86_400;

/// Volume assumed for a profile entry whose volume is not known from any
/// source (no stored entry, no live session): full scale, i.e. "do not
/// attenuate".
const DEFAULT_VOLUME: f32 = 1.0;

/// A `CoreCommand` together with the switch generation it was produced under.
///
/// The core compares `generation` against the newest generation it has seen
/// and may drop anything older, which is what makes a rapid double switch
/// safe: the second switch supersedes the first, and the commands belonging
/// to the first are recognisably obsolete. This crate never drops anything
/// itself — it has no view of what the core has already executed.
pub struct Dispatch {
    pub generation: u64,
    pub command: CoreCommand,
}

impl Dispatch {
    fn new(generation: u64, command: CoreCommand) -> Self {
        Self {
            generation,
            command,
        }
    }

    /// True when a newer switch has superseded this command.
    pub fn is_stale(&self, current_generation: u64) -> bool {
        self.generation < current_generation
    }

    pub fn command(&self) -> &CoreCommand {
        &self.command
    }
}

/// Work handed to the persistence thread.
///
/// `ProfileStore` is cloned once into an `Arc` so the persistence thread can
/// serialise it without holding any lock on the manager.
pub struct PersistRequest(pub Arc<ProfileStore>);

/// Outcome reported back by the persistence thread.
pub enum PersistResult {
    Saved,
    Failed(RepoError),
}

/// Authoritative state machine of the State Manager thread.
pub struct StateManager {
    /// Authoritative, in-memory; disk is write-behind.
    store: ProfileStore,
    /// Current default render endpoint. Not persisted (see module docs).
    default_endpoint: Option<EndpointId>,
    /// Live, non-persisted view feeding [`Snapshot`].
    live_endpoints: HashMap<EndpointId, EndpointView>,
    live_sessions: HashMap<SessionInstanceId, SessionView>,
    revision: u64,
    /// Instant of the *first* mutation since the last persist request. Later
    /// mutations do not push it forward, so a burst of slider drags collapses
    /// into one write instead of starving the writer.
    dirty_since: Option<Instant>,
    switch_generation: u64,
    /// Failure of the initial `repository.load()`, if any. Kept instead of
    /// logged so this crate needs no logging dependency; the wiring layer
    /// reports it.
    load_error: Option<RepoError>,
    /// Most recent persistence failure reported through
    /// [`StateManager::on_persist_result`].
    persist_error: Option<RepoError>,
}

impl StateManager {
    /// Loads the initial store from `repository`.
    ///
    /// A failing or corrupt repository is not fatal: the manager starts from
    /// `ProfileStore::default()` and the error is retained (see
    /// [`StateManager::load_error`]). Falling back to `.bak` is the
    /// repository's responsibility, not this type's.
    pub fn new(repository: &dyn ProfileRepository) -> Self {
        let (store, load_error) = match repository.load() {
            Ok(store) => (store, None),
            Err(err) => (ProfileStore::default(), Some(err)),
        };
        let mut manager = Self::from_store(store);
        manager.load_error = load_error;
        manager
    }

    /// Builds a manager directly around `store`, without touching a
    /// repository. Useful for tests and for a caller that has already loaded.
    pub fn from_store(store: ProfileStore) -> Self {
        Self {
            store,
            default_endpoint: None,
            live_endpoints: HashMap::new(),
            live_sessions: HashMap::new(),
            revision: 0,
            dirty_since: None,
            switch_generation: 0,
            load_error: None,
            persist_error: None,
        }
    }

    // ---------------------------------------------------------------- reads

    pub fn store(&self) -> &ProfileStore {
        &self.store
    }

    pub fn settings(&self) -> &Settings {
        &self.store.settings
    }

    pub fn default_endpoint(&self) -> Option<&EndpointId> {
        self.default_endpoint.as_ref()
    }

    /// Newest switch generation. Anything a [`Dispatch`] carries below this
    /// value belongs to a superseded switch.
    pub fn current_switch_generation(&self) -> u64 {
        self.switch_generation
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn live_session(&self, instance: &SessionInstanceId) -> Option<&SessionView> {
        self.live_sessions.get(instance)
    }

    pub fn live_session_count(&self) -> usize {
        self.live_sessions.len()
    }

    pub fn load_error(&self) -> Option<&RepoError> {
        self.load_error.as_ref()
    }

    pub fn persist_error(&self) -> Option<&RepoError> {
        self.persist_error.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// Current UI read model.
    ///
    /// Both lists are sorted so that repeated snapshots of unchanged state are
    /// identical: `HashMap` iteration order is not stable, and an unstable
    /// order would make the overlay's rows jump on every repaint.
    pub fn snapshot(&self) -> Snapshot {
        let mut endpoints: Vec<EndpointView> = self.live_endpoints.values().cloned().collect();
        endpoints.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name).then(a.id.cmp(&b.id)));

        let mut live_sessions: Vec<SessionView> = self.live_sessions.values().cloned().collect();
        live_sessions.sort_by(|a, b| a.process.cmp(&b.process).then(a.instance.cmp(&b.instance)));

        Snapshot {
            endpoints: endpoints.into(),
            default_endpoint: self.default_endpoint.clone(),
            live_sessions: live_sessions.into(),
            revision: self.revision,
        }
    }

    // --------------------------------------------------------- audio events

    /// Folds one `AudioEvent` into the state and returns the commands the core
    /// must execute, in order.
    pub fn handle_audio_event(&mut self, event: AudioEvent) -> Vec<Dispatch> {
        match event {
            AudioEvent::DefaultEndpointChanged { flow, role, id } => {
                self.on_default_endpoint_changed(flow, role, id)
            }
            AudioEvent::EndpointStateChanged { id, state } => {
                self.on_endpoint_state_changed(id, state);
                Vec::new()
            }
            AudioEvent::EndpointAdded(id) => {
                self.on_endpoint_added(id);
                Vec::new()
            }
            AudioEvent::EndpointRemoved(id) => {
                self.on_endpoint_removed(&id);
                Vec::new()
            }
            AudioEvent::SessionCreated { endpoint, session } => {
                let view = SessionView {
                    instance: session.instance.clone(),
                    process: session.process.clone(),
                    endpoint: endpoint.clone(),
                    volume: clamp_volume(session.volume),
                    muted: session.muted,
                    state: SessionState::Active,
                };
                self.live_sessions
                    .insert(session.instance.clone(), view.clone());
                self.revision += 1;
                let generation = self.switch_generation;
                self.restore_or_seed(&view, generation)
                    .into_iter()
                    .collect()
            }
            AudioEvent::SessionVolumeChanged {
                instance,
                volume,
                muted,
                own_change,
            } => {
                self.on_session_volume_changed(&instance, volume, muted, own_change);
                Vec::new()
            }
            AudioEvent::SessionStateChanged { instance, state } => {
                if let Some(session) = self.live_sessions.get_mut(&instance) {
                    session.state = state;
                    self.revision += 1;
                }
                Vec::new()
            }
            AudioEvent::SessionDisconnected { instance, .. } => {
                if self.live_sessions.remove(&instance).is_some() {
                    self.revision += 1;
                }
                Vec::new()
            }
            // Not state-affecting: reported by the wiring layer, which owns
            // the logging subscriber.
            AudioEvent::CoreError(_) => Vec::new(),
        }
    }

    /// The restore path.
    ///
    /// Windows raises `OnDefaultDeviceChanged` once per role, and in practice
    /// more than once for the same role on some hardware. The roles are
    /// coalesced here by using `eMultimedia` as the single trigger and by
    /// ignoring a change to the endpoint that is already the default, so one
    /// physical switch performs exactly one restore. Capture endpoints are out
    /// of scope for this product and are dropped entirely.
    fn on_default_endpoint_changed(
        &mut self,
        flow: DataFlow,
        role: Role,
        id: Option<EndpointId>,
    ) -> Vec<Dispatch> {
        if flow != DataFlow::Render || role != Role::Multimedia {
            return Vec::new();
        }
        if self.default_endpoint.as_deref() == id.as_deref() {
            return Vec::new();
        }

        self.default_endpoint = id.clone();
        self.switch_generation += 1;
        self.revision += 1;
        let generation = self.switch_generation;

        // No default endpoint at all (last render device removed): nothing to
        // resync or restore.
        let Some(new_id) = id else {
            return Vec::new();
        };

        // The endpoint is being used right now, so its profile is "seen" —
        // this also creates the profile on first switch, which is what makes
        // the unconditional `mark_dirty()` below honest.
        let now = SystemTime::now();
        self.touch_endpoint(&new_id, now);

        let mut dispatches = vec![Dispatch::new(
            generation,
            CoreCommand::ResyncEndpoint(new_id.clone()),
        )];

        let sessions: Vec<SessionView> = self
            .live_sessions
            .values()
            .filter(|s| s.endpoint == new_id)
            .cloned()
            .collect();
        for session in sessions {
            dispatches.extend(self.restore_or_seed(&session, generation));
        }

        self.mark_dirty();
        dispatches
    }

    /// Lookup-or-seed for a single session.
    ///
    /// A known process key on that endpoint is restored from the profile; a
    /// first sighting seeds the profile with whatever the session currently
    /// has. The live view is intentionally *not* updated to the restored
    /// value here: the core will echo the write back as
    /// `SessionVolumeChanged { own_change: true }`, and until that arrives the
    /// live view should keep reporting what the session actually is.
    fn restore_or_seed(&mut self, session: &SessionView, generation: u64) -> Option<Dispatch> {
        let stored = self
            .store
            .endpoints
            .get(&session.endpoint)
            .and_then(|profile| profile.sessions.get(&session.process))
            .copied();

        match stored {
            Some(entry) => Some(Dispatch::new(
                generation,
                CoreCommand::ApplySessionVolume {
                    instance: session.instance.clone(),
                    volume: clamp_volume(entry.volume),
                    muted: entry.muted,
                },
            )),
            None => {
                self.record_entry(
                    &session.endpoint,
                    &session.process,
                    session.volume,
                    session.muted,
                );
                None
            }
        }
    }

    fn on_session_volume_changed(
        &mut self,
        instance: &SessionInstanceId,
        volume: f32,
        muted: bool,
        own_change: bool,
    ) {
        let Some(session) = self.live_sessions.get_mut(instance) else {
            return;
        };
        session.volume = clamp_volume(volume);
        session.muted = muted;
        let endpoint = session.endpoint.clone();
        let process = session.process.clone();
        self.revision += 1;

        // Echo of our own write: the live view is up to date either way, but
        // recording it would overwrite the profile with the value we just
        // restored from it.
        if !own_change {
            self.record_entry(&endpoint, &process, volume, muted);
        }
    }

    fn on_endpoint_added(&mut self, id: EndpointId) {
        let friendly_name = self.known_friendly_name(&id);
        self.live_endpoints.insert(
            id.clone(),
            EndpointView {
                id,
                friendly_name,
                state: EndpointState::Active,
            },
        );
        self.revision += 1;
    }

    fn on_endpoint_removed(&mut self, id: &EndpointId) {
        let mut changed = self.live_endpoints.remove(id).is_some();
        // A removed device cannot host live sessions. The core also emits
        // `SessionDisconnected` for them, but dropping them here keeps the
        // snapshot from briefly showing sessions on a device that is gone.
        let before = self.live_sessions.len();
        self.live_sessions.retain(|_, s| &s.endpoint != id);
        changed |= self.live_sessions.len() != before;
        if changed {
            self.revision += 1;
        }
    }

    fn on_endpoint_state_changed(&mut self, id: EndpointId, state: EndpointState) {
        match self.live_endpoints.get_mut(&id) {
            Some(view) => view.state = state,
            None => {
                let friendly_name = self.known_friendly_name(&id);
                self.live_endpoints.insert(
                    id.clone(),
                    EndpointView {
                        id,
                        friendly_name,
                        state,
                    },
                );
            }
        }
        self.revision += 1;
    }

    // ----------------------------------------------------------- UI commands

    /// Folds one `UiCommand` into the state and returns the resulting core
    /// commands.
    pub fn handle_ui_command(&mut self, command: UiCommand) -> Vec<Dispatch> {
        let generation = self.switch_generation;
        match command {
            // The generation is *not* bumped here: the switch becomes real
            // when the resulting `DefaultEndpointChanged` comes back from the
            // core, and that is where the restore (and the bump) happens.
            UiCommand::SwitchEndpoint(id) => vec![Dispatch::new(
                generation,
                CoreCommand::SetDefaultEndpoint {
                    id,
                    roles: self.store.settings.switch_roles,
                },
            )],
            UiCommand::SetSessionVolume {
                endpoint,
                process,
                volume,
            } => self.set_profile_and_apply(&endpoint, &process, Some(volume), None, generation),
            UiCommand::SetSessionMute {
                endpoint,
                process,
                muted,
            } => self.set_profile_and_apply(&endpoint, &process, None, Some(muted), generation),
            UiCommand::ForgetProfileEntry { endpoint, process } => {
                let removed = self
                    .store
                    .endpoints
                    .get_mut(&endpoint)
                    .and_then(|profile| profile.sessions.remove(&process))
                    .is_some();
                if removed {
                    self.mark_dirty();
                }
                Vec::new()
            }
            // Overlay lifetime and process exit belong to the UI/app layer.
            UiCommand::ToggleOverlay | UiCommand::Quit => Vec::new(),
        }
    }

    /// Applies a user-initiated volume and/or mute change.
    ///
    /// This is a deliberate user action, so it is recorded in the profile
    /// unconditionally (the `own_change` echo rule only exists to stop our own
    /// *restores* from being mistaken for user intent). The live view is
    /// updated immediately so the overlay slider does not snap back while the
    /// core round-trip is in flight.
    ///
    /// A process key may own several live sessions
    /// (browser tabs, Discord); the change is applied to every live session
    /// with that key on that endpoint — "last write wins", applied to all.
    fn set_profile_and_apply(
        &mut self,
        endpoint: &EndpointId,
        process: &ProcessKey,
        volume: Option<f32>,
        muted: Option<bool>,
        generation: u64,
    ) -> Vec<Dispatch> {
        let (current_volume, current_muted) = self.resolve_entry(endpoint, process);
        let volume = clamp_volume(volume.unwrap_or(current_volume));
        let muted = muted.unwrap_or(current_muted);

        self.record_entry(endpoint, process, volume, muted);

        let mut dispatches = Vec::new();
        for session in self.live_sessions.values_mut() {
            if &session.endpoint != endpoint || &session.process != process {
                continue;
            }
            session.volume = volume;
            session.muted = muted;
            dispatches.push(Dispatch::new(
                generation,
                CoreCommand::ApplySessionVolume {
                    instance: session.instance.clone(),
                    volume,
                    muted,
                },
            ));
        }
        if !dispatches.is_empty() {
            self.revision += 1;
        }
        dispatches
    }

    /// Best known (volume, muted) for a profile key: the stored entry first,
    /// then any live session with that key, then "unattenuated, unmuted".
    fn resolve_entry(&self, endpoint: &EndpointId, process: &ProcessKey) -> (f32, bool) {
        if let Some(entry) = self
            .store
            .endpoints
            .get(endpoint)
            .and_then(|profile| profile.sessions.get(process))
        {
            return (entry.volume, entry.muted);
        }
        if let Some(session) = self
            .live_sessions
            .values()
            .find(|s| &s.endpoint == endpoint && &s.process == process)
        {
            return (session.volume, session.muted);
        }
        (DEFAULT_VOLUME, false)
    }

    // ---------------------------------------------------------- persistence

    /// Arms the write-behind debounce. Already-dirty state keeps its original
    /// deadline so a continuous burst of mutations cannot postpone the write
    /// indefinitely.
    fn mark_dirty(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
    }

    /// Returns a persist request once the 300 ms window has elapsed.
    ///
    /// `now` is supplied by the caller (the manager's event loop, or a test)
    /// rather than read here, so this stays a pure function of its inputs.
    pub fn take_persist_request(&mut self, now: Instant) -> Option<PersistRequest> {
        let dirty_since = self.dirty_since?;
        if now.saturating_duration_since(dirty_since) < PERSIST_DEBOUNCE {
            return None;
        }
        self.dirty_since = None;
        Some(PersistRequest(Arc::new(self.store.clone())))
    }

    /// Unconditional flush, ignoring the debounce — the shutdown path
    /// `None` when there is nothing pending.
    pub fn flush_persist_request(&mut self) -> Option<PersistRequest> {
        self.dirty_since.take()?;
        Some(PersistRequest(Arc::new(self.store.clone())))
    }

    /// Feedback from the persistence thread. A failed save re-arms the
    /// debounce so the next window retries instead of silently losing the
    /// change.
    pub fn on_persist_result(&mut self, result: PersistResult) {
        match result {
            PersistResult::Saved => self.persist_error = None,
            PersistResult::Failed(err) => {
                self.persist_error = Some(err);
                self.mark_dirty();
            }
        }
    }

    // --------------------------------------------------------------- pruning

    /// Drops endpoint profiles not seen for `settings.prune_after_days`.
    /// Endpoints that are currently present, and the current
    /// default, are never pruned regardless of `last_seen`. `prune_after_days
    /// == 0` disables pruning. Returns the number of profiles removed.
    pub fn prune(&mut self, now: SystemTime) -> usize {
        let days = self.store.settings.prune_after_days;
        if days == 0 {
            return 0;
        }
        let max_age = Duration::from_secs(u64::from(days) * SECONDS_PER_DAY);
        let live: HashSet<EndpointId> = self.live_endpoints.keys().cloned().collect();
        let default = self.default_endpoint.clone();

        let before = self.store.endpoints.len();
        self.store.endpoints.retain(|id, profile| {
            if live.contains(id) || default.as_ref() == Some(id) {
                return true;
            }
            // A `last_seen` in the future (clock moved backwards) is treated as
            // fresh rather than as an error.
            now.duration_since(profile.last_seen)
                .map(|age| age < max_age)
                .unwrap_or(true)
        });

        let removed = before - self.store.endpoints.len();
        if removed > 0 {
            self.mark_dirty();
        }
        removed
    }

    // --------------------------------------------------------------- helpers

    /// Writes one profile entry and arms the debounce.
    fn record_entry(
        &mut self,
        endpoint: &EndpointId,
        process: &ProcessKey,
        volume: f32,
        muted: bool,
    ) {
        let now = SystemTime::now();
        let entry = SessionEntry {
            volume: clamp_volume(volume),
            muted,
            updated_at: now,
        };
        self.touch_endpoint(endpoint, now)
            .sessions
            .insert(process.clone(), entry);
        self.mark_dirty();
    }

    /// Returns the profile for `endpoint`, creating it if needed, refreshing
    /// `last_seen` and filling in a friendly name once one is known.
    fn touch_endpoint(&mut self, endpoint: &EndpointId, now: SystemTime) -> &mut EndpointProfile {
        let friendly_name = self
            .live_endpoints
            .get(endpoint)
            .map(|view| view.friendly_name.to_string())
            .unwrap_or_default();

        let profile = self
            .store
            .endpoints
            .entry(endpoint.clone())
            .or_insert_with(|| EndpointProfile {
                friendly_name: String::new(),
                last_seen: now,
                sessions: HashMap::new(),
            });
        if !friendly_name.is_empty() {
            profile.friendly_name = friendly_name;
        }
        profile.last_seen = now;
        profile
    }

    /// Display name for an endpoint we only know by id: the stored profile
    /// name if we have one, otherwise the id itself (better than a blank row).
    fn known_friendly_name(&self, id: &EndpointId) -> Arc<str> {
        self.store
            .endpoints
            .get(id)
            .map(|profile| profile.friendly_name.as_str())
            .filter(|name| !name.is_empty())
            .map_or_else(|| id.clone(), Arc::from)
    }
}

/// Keeps every recorded and applied volume inside `0.0..=1.0`. `NaN` collapses
/// to `0.0` because `clamp` panics on a `NaN` bound and `max` propagates it.
fn clamp_volume(volume: f32) -> f32 {
    if volume.is_nan() {
        return 0.0;
    }
    volume.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::InMemoryRepository;
    use resonance_core::messages::{DisconnectReason, SessionInfo};

    const HEADPHONES: &str = "{headphones}";
    const SPEAKERS: &str = "{speakers}";

    fn id(s: &str) -> EndpointId {
        EndpointId::from(s)
    }

    fn manager() -> StateManager {
        StateManager::from_store(ProfileStore::default())
    }

    fn default_changed(endpoint: Option<&str>) -> AudioEvent {
        AudioEvent::DefaultEndpointChanged {
            flow: DataFlow::Render,
            role: Role::Multimedia,
            id: endpoint.map(id),
        }
    }

    fn session_created(endpoint: &str, process: &str, instance: &str, volume: f32) -> AudioEvent {
        AudioEvent::SessionCreated {
            endpoint: id(endpoint),
            session: SessionInfo {
                instance: SessionInstanceId::from(instance),
                process: ProcessKey::from(process),
                endpoint: id(endpoint),
                volume,
                muted: false,
            },
        }
    }

    /// (instance, volume, muted) of every `ApplySessionVolume` in `dispatches`.
    fn applies(dispatches: &[Dispatch]) -> Vec<(String, f32, bool)> {
        dispatches
            .iter()
            .filter_map(|d| match &d.command {
                CoreCommand::ApplySessionVolume {
                    instance,
                    volume,
                    muted,
                } => Some((instance.to_string(), *volume, *muted)),
                _ => None,
            })
            .collect()
    }

    fn resyncs(dispatches: &[Dispatch]) -> Vec<String> {
        dispatches
            .iter()
            .filter_map(|d| match &d.command {
                CoreCommand::ResyncEndpoint(endpoint) => Some(endpoint.to_string()),
                _ => None,
            })
            .collect()
    }

    fn stored(manager: &StateManager, endpoint: &str, process: &str) -> Option<SessionEntry> {
        manager
            .store()
            .endpoints
            .get(&id(endpoint))
            .and_then(|p| p.sessions.get(&ProcessKey::from(process)))
            .copied()
    }

    /// Repository whose `load` always fails, for the corrupt-store path.
    struct FailingRepository;

    impl ProfileRepository for FailingRepository {
        fn load(&self) -> Result<ProfileStore, RepoError> {
            Err(RepoError::Deserialize("bad json".into()))
        }
        fn save(&self, _store: &ProfileStore) -> Result<(), RepoError> {
            Ok(())
        }
    }

    #[test]
    fn new_loads_the_store_from_the_repository() {
        let mut seed = ProfileStore::default();
        seed.settings.prune_after_days = 7;
        let repo = InMemoryRepository::seeded(seed.clone());

        let manager = StateManager::new(&repo);

        assert_eq!(manager.store(), &seed);
        assert!(manager.load_error().is_none());
    }

    #[test]
    fn new_falls_back_to_default_store_when_load_fails() {
        let manager = StateManager::new(&FailingRepository);

        assert_eq!(manager.store(), &ProfileStore::default());
        assert!(matches!(
            manager.load_error(),
            Some(RepoError::Deserialize(_))
        ));
    }

    #[test]
    fn capture_flow_default_change_is_ignored() {
        let mut m = manager();

        let out = m.handle_audio_event(AudioEvent::DefaultEndpointChanged {
            flow: DataFlow::Capture,
            role: Role::Multimedia,
            id: Some(id(HEADPHONES)),
        });

        assert!(out.is_empty());
        assert!(m.default_endpoint().is_none());
        assert_eq!(m.current_switch_generation(), 0);
    }

    #[test]
    fn console_and_communications_roles_do_not_trigger_a_restore() {
        let mut m = manager();

        for role in [Role::Console, Role::Communications] {
            let out = m.handle_audio_event(AudioEvent::DefaultEndpointChanged {
                flow: DataFlow::Render,
                role,
                id: Some(id(HEADPHONES)),
            });
            assert!(out.is_empty());
        }

        assert!(m.default_endpoint().is_none());
        assert_eq!(m.current_switch_generation(), 0);
    }

    #[test]
    fn repeated_default_change_to_the_same_endpoint_restores_once() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.5));

        let first = m.handle_audio_event(default_changed(Some(HEADPHONES)));
        let second = m.handle_audio_event(default_changed(Some(HEADPHONES)));

        assert_eq!(resyncs(&first), vec![HEADPHONES.to_string()]);
        assert!(second.is_empty(), "duplicate notification must be a no-op");
        assert_eq!(m.current_switch_generation(), 1);
    }

    #[test]
    fn first_sighting_seeds_the_profile_instead_of_applying() {
        let mut m = manager();

        let out = m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.42));

        assert!(applies(&out).is_empty(), "nothing to restore yet");
        let entry = stored(&m, HEADPHONES, "spotify.exe").expect("profile seeded");
        assert_eq!(entry.volume, 0.42);
        assert!(!entry.muted);
        assert!(m.is_dirty());
    }

    #[test]
    fn known_session_is_restored_on_session_created() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));
        m.handle_audio_event(AudioEvent::SessionDisconnected {
            instance: SessionInstanceId::from("s1"),
            reason: DisconnectReason::SessionDisconnected,
        });

        let out = m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s2", 0.90));

        assert_eq!(applies(&out), vec![("s2".to_string(), 0.30, false)]);
        // The seeded value stays authoritative; the fresh session's 0.90 is
        // what we are correcting, not what we record.
        assert_eq!(stored(&m, HEADPHONES, "spotify.exe").unwrap().volume, 0.30);
    }

    #[test]
    fn switch_restores_stored_entries_and_seeds_unknown_ones() {
        let mut m = manager();
        // Learn spotify on speakers at 0.80, then move the session there.
        m.handle_audio_event(session_created(SPEAKERS, "spotify.exe", "s1", 0.80));
        m.handle_audio_event(AudioEvent::SessionDisconnected {
            instance: SessionInstanceId::from("s1"),
            reason: DisconnectReason::DeviceRemoval,
        });
        m.handle_audio_event(session_created(SPEAKERS, "spotify.exe", "s2", 0.10));
        // discord is live on the endpoint but has no profile entry at switch
        // time, which is the seed branch of the restore loop.
        m.handle_audio_event(session_created(SPEAKERS, "discord.exe", "s3", 0.55));
        m.handle_ui_command(UiCommand::ForgetProfileEntry {
            endpoint: id(SPEAKERS),
            process: ProcessKey::from("discord.exe"),
        });

        let out = m.handle_audio_event(default_changed(Some(SPEAKERS)));

        assert_eq!(resyncs(&out), vec![SPEAKERS.to_string()]);
        assert!(
            matches!(
                out.first().map(|d| &d.command),
                Some(CoreCommand::ResyncEndpoint(_))
            ),
            "resync must be dispatched before any apply"
        );
        // spotify has a stored entry: restored. discord has none: seeded from
        // whatever it currently is, with no apply.
        assert_eq!(applies(&out), vec![("s2".to_string(), 0.80, false)]);
        assert_eq!(stored(&m, SPEAKERS, "discord.exe").unwrap().volume, 0.55);
        assert_eq!(m.default_endpoint(), Some(&id(SPEAKERS)));
    }

    #[test]
    fn sessions_on_other_endpoints_are_not_touched_by_a_switch() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.20));
        m.handle_audio_event(default_changed(Some(SPEAKERS)));

        let out = m.handle_audio_event(default_changed(Some(HEADPHONES)));
        let _ = out;

        let out = m.handle_audio_event(default_changed(Some(SPEAKERS)));
        assert!(applies(&out).is_empty());
    }

    #[test]
    fn own_change_updates_live_view_but_not_the_profile() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));
        let seeded = stored(&m, HEADPHONES, "spotify.exe").unwrap();

        m.handle_audio_event(AudioEvent::SessionVolumeChanged {
            instance: SessionInstanceId::from("s1"),
            volume: 0.77,
            muted: true,
            own_change: true,
        });

        let live = m.live_session(&SessionInstanceId::from("s1")).unwrap();
        assert_eq!(live.volume, 0.77);
        assert!(live.muted);
        assert_eq!(
            stored(&m, HEADPHONES, "spotify.exe").unwrap(),
            seeded,
            "our own write must not be recorded as user intent"
        );
    }

    #[test]
    fn user_change_is_recorded_in_the_profile() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));

        m.handle_audio_event(AudioEvent::SessionVolumeChanged {
            instance: SessionInstanceId::from("s1"),
            volume: 0.65,
            muted: false,
            own_change: false,
        });

        let entry = stored(&m, HEADPHONES, "spotify.exe").unwrap();
        assert_eq!(entry.volume, 0.65);
        assert_eq!(
            m.live_session(&SessionInstanceId::from("s1"))
                .unwrap()
                .volume,
            0.65
        );
    }

    #[test]
    fn volume_changed_for_an_unknown_session_is_ignored() {
        let mut m = manager();

        m.handle_audio_event(AudioEvent::SessionVolumeChanged {
            instance: SessionInstanceId::from("ghost"),
            volume: 0.5,
            muted: false,
            own_change: false,
        });

        assert!(m.store().endpoints.is_empty());
        assert!(!m.is_dirty());
    }

    #[test]
    fn rapid_double_switch_bumps_the_generation_and_marks_the_first_batch_stale() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.20));
        m.handle_audio_event(session_created(SPEAKERS, "spotify.exe", "s2", 0.80));

        let first = m.handle_audio_event(default_changed(Some(HEADPHONES)));
        let second = m.handle_audio_event(default_changed(Some(SPEAKERS)));

        assert_eq!(m.current_switch_generation(), 2);
        assert!(first.iter().all(|d| d.generation == 1));
        assert!(second.iter().all(|d| d.generation == 2));
        let current = m.current_switch_generation();
        assert!(first.iter().all(|d| d.is_stale(current)));
        assert!(second.iter().all(|d| !d.is_stale(current)));
    }

    #[test]
    fn debounce_holds_for_300ms_and_is_not_extended_by_later_mutations() {
        let mut m = manager();
        let t0 = Instant::now();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.20));

        assert!(m
            .take_persist_request(t0 + Duration::from_millis(100))
            .is_none());

        // A second mutation inside the window must not push the deadline out.
        m.handle_audio_event(AudioEvent::SessionVolumeChanged {
            instance: SessionInstanceId::from("s1"),
            volume: 0.25,
            muted: false,
            own_change: false,
        });

        assert!(m
            .take_persist_request(t0 + Duration::from_millis(250))
            .is_none());
        let request = m
            .take_persist_request(t0 + Duration::from_millis(500))
            .expect("window elapsed");
        assert_eq!(
            request.0.endpoints[&id(HEADPHONES)].sessions[&ProcessKey::from("spotify.exe")].volume,
            0.25
        );
        assert!(!m.is_dirty());
        assert!(m
            .take_persist_request(t0 + Duration::from_secs(10))
            .is_none());
    }

    #[test]
    fn flush_returns_pending_state_regardless_of_the_window() {
        let mut m = manager();
        assert!(m.flush_persist_request().is_none());

        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.20));
        assert!(m.flush_persist_request().is_some());
        assert!(!m.is_dirty());
    }

    #[test]
    fn failed_persist_rearms_the_debounce() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.20));
        let t0 = Instant::now();
        m.take_persist_request(t0 + Duration::from_millis(500))
            .unwrap();
        assert!(!m.is_dirty());

        m.on_persist_result(PersistResult::Failed(RepoError::Io("disk full".into())));

        assert!(m.is_dirty(), "a failed save must be retried");
        assert!(m.persist_error().is_some());

        m.take_persist_request(Instant::now() + Duration::from_millis(500))
            .expect("retry");
        m.on_persist_result(PersistResult::Saved);
        assert!(m.persist_error().is_none());
    }

    #[test]
    fn ui_switch_endpoint_uses_the_configured_roles() {
        let mut store = ProfileStore::default();
        store.settings.switch_roles = resonance_core::messages::RoleSet {
            console: true,
            multimedia: true,
            communications: false,
        };
        let mut m = StateManager::from_store(store);

        let out = m.handle_ui_command(UiCommand::SwitchEndpoint(id(SPEAKERS)));

        match out.as_slice() {
            [Dispatch {
                command: CoreCommand::SetDefaultEndpoint { id: target, roles },
                ..
            }] => {
                assert_eq!(target, &id(SPEAKERS));
                assert!(!roles.communications);
            }
            _ => panic!("expected a single SetDefaultEndpoint"),
        }
        // The switch is not "done" until the core reports it back.
        assert!(m.default_endpoint().is_none());
        assert_eq!(m.current_switch_generation(), 0);
    }

    #[test]
    fn ui_set_session_volume_records_updates_live_view_and_applies_to_all_matching_sessions() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "chrome.exe", "tab1", 0.50));
        m.handle_audio_event(session_created(HEADPHONES, "chrome.exe", "tab2", 0.50));
        m.handle_audio_event(session_created(SPEAKERS, "chrome.exe", "other", 0.50));

        let out = m.handle_ui_command(UiCommand::SetSessionVolume {
            endpoint: id(HEADPHONES),
            process: ProcessKey::from("chrome.exe"),
            volume: 0.25,
        });

        let mut applied = applies(&out);
        applied.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            applied,
            vec![
                ("tab1".to_string(), 0.25, false),
                ("tab2".to_string(), 0.25, false)
            ]
        );
        assert_eq!(stored(&m, HEADPHONES, "chrome.exe").unwrap().volume, 0.25);
        assert_eq!(stored(&m, SPEAKERS, "chrome.exe").unwrap().volume, 0.50);
        assert_eq!(
            m.live_session(&SessionInstanceId::from("tab1"))
                .unwrap()
                .volume,
            0.25
        );
        assert_eq!(
            m.live_session(&SessionInstanceId::from("other"))
                .unwrap()
                .volume,
            0.50
        );
    }

    #[test]
    fn ui_set_session_mute_keeps_the_recorded_volume() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));

        let out = m.handle_ui_command(UiCommand::SetSessionMute {
            endpoint: id(HEADPHONES),
            process: ProcessKey::from("spotify.exe"),
            muted: true,
        });

        assert_eq!(applies(&out), vec![("s1".to_string(), 0.30, true)]);
        let entry = stored(&m, HEADPHONES, "spotify.exe").unwrap();
        assert_eq!(entry.volume, 0.30);
        assert!(entry.muted);
    }

    #[test]
    fn ui_change_for_an_endpoint_with_no_live_session_still_records() {
        let mut m = manager();

        let out = m.handle_ui_command(UiCommand::SetSessionVolume {
            endpoint: id(SPEAKERS),
            process: ProcessKey::from("spotify.exe"),
            volume: 0.10,
        });

        assert!(out.is_empty(), "nothing live to apply to");
        assert_eq!(stored(&m, SPEAKERS, "spotify.exe").unwrap().volume, 0.10);
    }

    #[test]
    fn ui_forget_profile_entry_removes_the_record() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));
        m.take_persist_request(Instant::now() + Duration::from_millis(500));

        let out = m.handle_ui_command(UiCommand::ForgetProfileEntry {
            endpoint: id(HEADPHONES),
            process: ProcessKey::from("spotify.exe"),
        });

        assert!(out.is_empty());
        assert!(stored(&m, HEADPHONES, "spotify.exe").is_none());
        assert!(m.is_dirty());
    }

    #[test]
    fn ui_forget_unknown_entry_does_not_dirty_the_store() {
        let mut m = manager();

        m.handle_ui_command(UiCommand::ForgetProfileEntry {
            endpoint: id(HEADPHONES),
            process: ProcessKey::from("nothing.exe"),
        });

        assert!(!m.is_dirty());
    }

    #[test]
    fn overlay_and_quit_commands_are_no_ops_in_this_layer() {
        let mut m = manager();
        let revision = m.revision();

        assert!(m.handle_ui_command(UiCommand::ToggleOverlay).is_empty());
        assert!(m.handle_ui_command(UiCommand::Quit).is_empty());
        assert_eq!(m.revision(), revision);
        assert!(!m.is_dirty());
    }

    #[test]
    fn endpoint_lifecycle_updates_the_live_view() {
        let mut m = manager();
        m.handle_audio_event(AudioEvent::EndpointAdded(id(HEADPHONES)));
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));

        m.handle_audio_event(AudioEvent::EndpointStateChanged {
            id: id(HEADPHONES),
            state: EndpointState::Unplugged,
        });
        assert_eq!(m.snapshot().endpoints[0].state, EndpointState::Unplugged);

        m.handle_audio_event(AudioEvent::EndpointRemoved(id(HEADPHONES)));
        let snapshot = m.snapshot();
        assert!(snapshot.endpoints.is_empty());
        assert!(snapshot.live_sessions.is_empty());
        // The profile survives the device going away — that is the point.
        assert!(stored(&m, HEADPHONES, "spotify.exe").is_some());
    }

    #[test]
    fn session_state_change_and_disconnect_are_reflected_live() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "spotify.exe", "s1", 0.30));

        m.handle_audio_event(AudioEvent::SessionStateChanged {
            instance: SessionInstanceId::from("s1"),
            state: SessionState::Inactive,
        });
        assert_eq!(
            m.live_session(&SessionInstanceId::from("s1"))
                .unwrap()
                .state,
            SessionState::Inactive
        );

        m.handle_audio_event(AudioEvent::SessionDisconnected {
            instance: SessionInstanceId::from("s1"),
            reason: DisconnectReason::SessionLogoff,
        });
        assert_eq!(m.live_session_count(), 0);
    }

    #[test]
    fn snapshot_is_sorted_and_carries_revision_and_default() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "zed.exe", "s2", 0.30));
        m.handle_audio_event(session_created(HEADPHONES, "arc.exe", "s1", 0.30));
        m.handle_audio_event(default_changed(Some(HEADPHONES)));

        let snapshot = m.snapshot();
        let processes: Vec<&str> = snapshot.live_sessions.iter().map(|s| &*s.process).collect();
        assert_eq!(processes, vec!["arc.exe", "zed.exe"]);
        assert_eq!(snapshot.default_endpoint, Some(id(HEADPHONES)));
        assert_eq!(snapshot.revision, m.revision());
        assert!(snapshot.revision > 0);
    }

    #[test]
    fn core_error_does_not_touch_the_state() {
        use resonance_core::messages::CoreError;
        let mut m = manager();
        let revision = m.revision();

        let out = m.handle_audio_event(AudioEvent::CoreError(CoreError::Other("boom".into())));

        assert!(out.is_empty());
        assert_eq!(m.revision(), revision);
        assert!(!m.is_dirty());
    }

    #[test]
    fn prune_drops_stale_profiles_but_keeps_present_ones() {
        let now = SystemTime::now();
        let ancient = now - Duration::from_secs(200 * SECONDS_PER_DAY);
        let mut store = ProfileStore::default();
        for endpoint in [HEADPHONES, SPEAKERS, "{old}"] {
            store.endpoints.insert(
                id(endpoint),
                EndpointProfile {
                    friendly_name: endpoint.to_string(),
                    last_seen: ancient,
                    sessions: HashMap::new(),
                },
            );
        }
        let mut m = StateManager::from_store(store);
        m.handle_audio_event(AudioEvent::EndpointAdded(id(HEADPHONES)));
        m.handle_audio_event(default_changed(Some(SPEAKERS)));

        let removed = m.prune(now);

        assert_eq!(removed, 1);
        assert!(m.store().endpoints.contains_key(&id(HEADPHONES)));
        assert!(m.store().endpoints.contains_key(&id(SPEAKERS)));
        assert!(!m.store().endpoints.contains_key(&id("{old}")));
    }

    #[test]
    fn prune_is_disabled_when_prune_after_days_is_zero() {
        let mut store = ProfileStore::default();
        store.settings.prune_after_days = 0;
        store.endpoints.insert(
            id("{old}"),
            EndpointProfile {
                friendly_name: String::new(),
                last_seen: SystemTime::UNIX_EPOCH,
                sessions: HashMap::new(),
            },
        );
        let mut m = StateManager::from_store(store);

        assert_eq!(m.prune(SystemTime::now()), 0);
        assert_eq!(m.store().endpoints.len(), 1);
    }

    #[test]
    fn volumes_are_clamped_into_the_unit_range() {
        let mut m = manager();
        m.handle_audio_event(session_created(HEADPHONES, "loud.exe", "s1", 4.2));
        assert_eq!(stored(&m, HEADPHONES, "loud.exe").unwrap().volume, 1.0);

        m.handle_audio_event(session_created(HEADPHONES, "nan.exe", "s2", f32::NAN));
        assert_eq!(stored(&m, HEADPHONES, "nan.exe").unwrap().volume, 0.0);

        let out = m.handle_ui_command(UiCommand::SetSessionVolume {
            endpoint: id(HEADPHONES),
            process: ProcessKey::from("loud.exe"),
            volume: -3.0,
        });
        assert_eq!(applies(&out), vec![("s1".to_string(), 0.0, false)]);
        assert_eq!(stored(&m, HEADPHONES, "loud.exe").unwrap().volume, 0.0);
    }

    #[test]
    fn switching_records_the_endpoint_as_seen() {
        let mut m = manager();
        m.handle_audio_event(AudioEvent::EndpointAdded(id(SPEAKERS)));

        m.handle_audio_event(default_changed(Some(SPEAKERS)));

        let profile = m.store().endpoints.get(&id(SPEAKERS)).expect("touched");
        assert_eq!(profile.friendly_name, SPEAKERS);
        assert!(m.is_dirty());
    }

    #[test]
    fn losing_the_default_endpoint_is_handled() {
        let mut m = manager();
        m.handle_audio_event(default_changed(Some(HEADPHONES)));

        let out = m.handle_audio_event(default_changed(None));

        assert!(out.is_empty());
        assert!(m.default_endpoint().is_none());
        assert_eq!(m.current_switch_generation(), 2);
    }
}
