//! Backend wiring for `resonance-app`.
//!
//! Connects three independently-owned pieces into one running backend:
//!
//! - the audio core thread, started and owned by `resonance_core::core`;
//! - a reducer thread that owns the `StateManager` and is the only place
//!   `AudioEvent`/`UiCommand` traffic is folded into state;
//! - a persistence thread that is the only thread that ever touches the
//!   on-disk profile store.
//!
//! None of the three ever share a lock: they talk exclusively through
//! `crossbeam_channel`. There is no UI wired up yet — `resonance-ui` is being
//! built separately — so this module only exposes the channel ends a future
//! UI will need (`BackendHandles::ui_cmd_tx`, `BackendHandles::snapshot_rx`).

use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, tick, unbounded, Receiver, Sender};
use tracing::{debug, info, warn};

use resonance_core::core::{self, CoreStartup, CoreThread};
use resonance_core::messages::{
    AudioEvent, CoreCommand, CoreError, HotkeyConfig, Snapshot, UiCommand,
};
use resonance_state::{
    Dispatch, JsonFileRepository, PersistRequest, PersistResult, ProfileRepository, RepoError,
    StateManager,
};

/// How often the reducer checks whether the write-behind debounce has
/// elapsed. Small relative to `StateManager::PERSIST_DEBOUNCE` (300 ms) so a
/// pending write is not delayed noticeably past its deadline; this is the one
/// timer the reducer runs, and it only ever does work when something is
/// actually dirty.
const PERSIST_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Everything that can stop the backend from starting. Every variant is
/// something a caller can report to the user; nothing here panics.
#[derive(Debug)]
pub enum BackendError {
    /// Could not determine or open the on-disk profile store location.
    Repository(RepoError),
    /// The audio core thread failed to initialise.
    Core(CoreError),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Repository(err) => write!(f, "profile repository unavailable: {err}"),
            BackendError::Core(err) => write!(
                f,
                "audio core failed to start: {}",
                describe_core_error(err)
            ),
        }
    }
}

fn describe_core_error(err: &CoreError) -> String {
    match err {
        CoreError::ComInitFailed(message) => format!("COM init failed: {message}"),
        CoreError::DeviceEnumerationFailed(message) => {
            format!("device enumeration failed: {message}")
        }
        CoreError::Other(message) => message.clone(),
    }
}

/// Handles to a running backend.
///
/// `ui_cmd_tx` and `snapshot_rx` are the seam a future UI thread will use.
/// `snapshot_rx` is a plain unbounded channel, not a single-slot "latest
/// value" channel: the reducer publishes one `Snapshot` per state-affecting
/// step, and a burst of events can queue up several before a consumer looks.
/// A consumer that only cares about the current state must drain it with
/// `try_recv()` in a loop and keep only the last value read — that
/// "take-latest" policy is the consumer's responsibility, this module does
/// not implement it.
pub struct BackendHandles {
    /// Endpoint snapshot captured when the audio core finished bootstrapping.
    pub startup: CoreStartup,
    /// Commands a UI issues; consumed by the reducer thread.
    pub ui_cmd_tx: Sender<UiCommand>,
    /// Read model published after every state-affecting step. See the type
    /// docs above for the consumption policy.
    pub snapshot_rx: Receiver<Snapshot>,
    /// The hotkey as loaded from the profile store, for a UI to register
    /// before it has received a single `Snapshot`. Later changes flow back
    /// through `UiCommand::SetHotkey` rather than through a live channel: the
    /// UI is the only thing that ever changes it, so it applies a change to
    /// itself immediately and reports it here only for persistence.
    pub initial_hotkey: HotkeyConfig,

    core: CoreThread,
    reducer: JoinHandle<()>,
    persistence: JoinHandle<()>,
    shutdown_tx: Sender<()>,
}

impl BackendHandles {
    /// Orderly shutdown: flush any pending profile write synchronously, stop
    /// the audio core, and join every thread this module started.
    ///
    /// This only prepares the flush mechanism itself; it does not install a
    /// Ctrl+C / console control handler (that belongs to a later hardening
    /// pass). Call it from whatever shutdown trigger the binary already has
    /// (here: the `quit` console command, same as `--dump-events`).
    pub fn shutdown(self) {
        // Tells the reducer to flush and stop. The reducer in turn stops the
        // persistence thread (by dropping its `PersistRequest` sender once it
        // returns) and the audio core (by sending `CoreCommand::Shutdown`).
        let _ = self.shutdown_tx.send(());
        if self.reducer.join().is_err() {
            warn!("reducer thread panicked during shutdown");
        }
        self.core.join();
        if self.persistence.join().is_err() {
            warn!("persistence thread panicked during shutdown");
        }
    }
}

/// Start the backend: load the profile store, start the audio core thread,
/// and start the reducer and persistence threads that connect everything.
///
/// Blocks until the audio core has finished its own bootstrap (endpoint
/// enumeration plus notification registration), exactly like `core::spawn`
/// does on its own.
pub fn spawn_backend() -> Result<BackendHandles, BackendError> {
    let store_path = JsonFileRepository::app_data_path().map_err(BackendError::Repository)?;

    // Two repository instances over the same file: one used once, here, to
    // load the initial store, and one owned by the persistence thread for
    // every save. `JsonFileRepository` keeps its hash-skip optimisation in a
    // `Cell`, so an instance must stay on one thread; using two separate
    // instances avoids needing a `Mutex` around it for no real benefit (the
    // file on disk is the only thing actually shared, and it is only ever
    // written from the persistence thread).
    let load_repo = JsonFileRepository::new(store_path.clone());
    let state_manager = StateManager::new(&load_repo);
    if let Some(err) = state_manager.load_error() {
        warn!(error = %err, "profile store failed to load, starting from an empty store");
    }
    // Read before `state_manager` moves into the reducer thread below: this
    // is the one piece of persisted settings a caller needs before the
    // reducer thread exists to publish anything.
    let initial_hotkey = state_manager.settings().hotkey;
    let save_repo = JsonFileRepository::new(store_path);

    let (cmd_tx, cmd_rx) = unbounded::<CoreCommand>();
    let (ev_tx, ev_rx) = unbounded::<AudioEvent>();
    let (ui_cmd_tx, ui_cmd_rx) = unbounded::<UiCommand>();
    let (snapshot_tx, snapshot_rx) = unbounded::<Snapshot>();
    let (persist_tx, persist_rx) = unbounded::<PersistRequest>();
    let (persist_result_tx, persist_result_rx) = unbounded::<PersistResult>();
    let (shutdown_tx, shutdown_rx) = unbounded::<()>();

    let core = core::spawn(cmd_rx, ev_tx).map_err(BackendError::Core)?;
    let startup = core.startup().clone();

    let persistence = thread::Builder::new()
        .name("resonance-persistence".to_owned())
        .spawn(move || persistence_thread_main(save_repo, persist_rx, persist_result_tx))
        .expect("failed to spawn persistence thread");

    let reducer = thread::Builder::new()
        .name("resonance-reducer".to_owned())
        .spawn(move || {
            reducer_thread_main(ReducerChannels {
                state: state_manager,
                ev_rx,
                ui_cmd_rx,
                persist_result_rx,
                shutdown_rx,
                cmd_tx,
                persist_tx,
                snapshot_tx,
            })
        })
        .expect("failed to spawn reducer thread");

    Ok(BackendHandles {
        startup,
        ui_cmd_tx,
        snapshot_rx,
        initial_hotkey,
        core,
        reducer,
        persistence,
        shutdown_tx,
    })
}

/// Persistence thread body: the only thread that ever calls
/// `ProfileRepository::save`.
///
/// Ends when `persist_rx` runs dry after its sender is dropped, which is the
/// reducer thread's doing as it returns from `reducer_thread_main`. Any
/// request already queued at that point is still delivered by `iter()`
/// before the loop ends, so a request sent just before shutdown is not lost.
fn persistence_thread_main(
    repo: JsonFileRepository,
    persist_rx: Receiver<PersistRequest>,
    result_tx: Sender<PersistResult>,
) {
    for request in persist_rx.iter() {
        let result = match repo.save(&request.0) {
            Ok(()) => {
                debug!("profile store saved");
                PersistResult::Saved
            }
            Err(err) => {
                warn!(error = %err, "profile store save failed");
                PersistResult::Failed(err)
            }
        };
        if result_tx.send(result).is_err() {
            // Reducer thread is gone; nothing left to report to.
            break;
        }
    }
}

/// Everything the reducer thread owns or reads from. Grouped into one struct
/// so `reducer_thread_main` takes one argument instead of eight.
struct ReducerChannels {
    state: StateManager,
    ev_rx: Receiver<AudioEvent>,
    ui_cmd_rx: Receiver<UiCommand>,
    persist_result_rx: Receiver<PersistResult>,
    shutdown_rx: Receiver<()>,
    cmd_tx: Sender<CoreCommand>,
    persist_tx: Sender<PersistRequest>,
    snapshot_tx: Sender<Snapshot>,
}

/// Reducer thread body: owns the `StateManager`, and is the sole authority on
/// what the audio core is told to do and on what gets persisted.
///
/// Every branch below either changes `state` or reacts to something that
/// already happened; none of them call into COM or touch disk directly. The
/// only I/O this thread performs is sending on channels.
fn reducer_thread_main(mut ch: ReducerChannels) {
    let persist_poll = tick(PERSIST_POLL_INTERVAL);
    publish_snapshot(&ch.state, &ch.snapshot_tx);

    loop {
        select! {
            recv(ch.ev_rx) -> message => match message {
                Ok(event) => {
                    log_audio_event(&event);
                    let dispatches = ch.state.handle_audio_event(event);
                    apply_dispatches(&ch.state, dispatches, &ch.cmd_tx);
                    publish_snapshot(&ch.state, &ch.snapshot_tx);
                }
                Err(_) => {
                    debug!("audio event channel closed, stopping reducer");
                    break;
                }
            },
            recv(ch.ui_cmd_rx) -> message => match message {
                Ok(command) => {
                    let dispatches = ch.state.handle_ui_command(command);
                    apply_dispatches(&ch.state, dispatches, &ch.cmd_tx);
                    publish_snapshot(&ch.state, &ch.snapshot_tx);
                }
                Err(_) => {
                    // No UI thread exists yet, so `ui_cmd_tx` may have no
                    // live sender at all; that is expected and not a reason
                    // to stop the reducer (unlike `ev_rx`, whose only sender
                    // is the audio core, which we do care about losing).
                }
            },
            recv(ch.persist_result_rx) -> message => {
                if let Ok(result) = message {
                    ch.state.on_persist_result(result);
                }
            },
            recv(persist_poll) -> _ => {
                if let Some(request) = ch.state.take_persist_request(Instant::now()) {
                    let _ = ch.persist_tx.send(request);
                }
            },
            recv(ch.shutdown_rx) -> _ => {
                info!("reducer received shutdown, flushing pending profile writes");
                flush_synchronously(&mut ch.state, &ch.persist_tx, &ch.persist_result_rx);
                break;
            },
        }
    }

    let _ = ch.cmd_tx.send(CoreCommand::Shutdown);
    // `ch.persist_tx` is dropped here, at the end of this function, which is
    // what lets the persistence thread's `persist_rx.iter()` end.
}

/// Sends every non-stale dispatch to the audio core, in order, and drops the
/// rest. Staleness is judged against the generation current *right now*, on
/// this thread, which is always at least as new as the generation the
/// dispatches were produced under.
fn apply_dispatches(state: &StateManager, dispatches: Vec<Dispatch>, cmd_tx: &Sender<CoreCommand>) {
    let current = state.current_switch_generation();
    for dispatch in dispatches {
        if dispatch.is_stale(current) {
            debug!(
                generation = dispatch.generation,
                current, "dropping dispatch superseded by a newer endpoint switch"
            );
            continue;
        }
        let _ = cmd_tx.send(dispatch.command);
    }
}

fn publish_snapshot(state: &StateManager, snapshot_tx: &Sender<Snapshot>) {
    let _ = snapshot_tx.send(state.snapshot());
}

/// Drains and applies every persist result already sitting in the channel,
/// so `flush_synchronously`'s own `recv()` cannot be answered by a stale
/// result left over from a periodic write that raced the shutdown signal.
fn drain_persist_results(state: &mut StateManager, persist_result_rx: &Receiver<PersistResult>) {
    while let Ok(result) = persist_result_rx.try_recv() {
        state.on_persist_result(result);
    }
}

/// Unconditional, blocking flush used only on the shutdown path.
///
/// `StateManager::flush_persist_request` ignores the debounce window, so
/// whatever is dirty right now is sent immediately; this function then waits
/// for the persistence thread to confirm the write before returning, which
/// is what makes the flush synchronous from the reducer's point of view.
fn flush_synchronously(
    state: &mut StateManager,
    persist_tx: &Sender<PersistRequest>,
    persist_result_rx: &Receiver<PersistResult>,
) {
    drain_persist_results(state, persist_result_rx);

    let Some(request) = state.flush_persist_request() else {
        return;
    };
    if persist_tx.send(request).is_err() {
        warn!("persistence thread gone, could not flush pending profile write");
        return;
    }
    match persist_result_rx.recv() {
        Ok(result) => state.on_persist_result(result),
        Err(_) => warn!("persistence thread gone before confirming the flush"),
    }
}

/// Logs one audio event at `debug` for `--run` observability. This runs on
/// the reducer thread, not inside a COM callback, so it is not subject to the
/// "callbacks log at trace only" rule.
fn log_audio_event(event: &AudioEvent) {
    match event {
        AudioEvent::DefaultEndpointChanged { flow, role, id } => {
            debug!(?flow, ?role, id = id.as_deref(), "DefaultEndpointChanged");
        }
        AudioEvent::EndpointStateChanged { id, state } => {
            debug!(%id, ?state, "EndpointStateChanged");
        }
        AudioEvent::EndpointAdded(id) => debug!(%id, "EndpointAdded"),
        AudioEvent::EndpointRemoved(id) => debug!(%id, "EndpointRemoved"),
        AudioEvent::SessionCreated { endpoint, session } => {
            debug!(%endpoint, process = %session.process, instance = %session.instance, "SessionCreated");
        }
        AudioEvent::SessionVolumeChanged {
            instance,
            volume,
            muted,
            own_change,
        } => debug!(%instance, volume, muted, own_change, "SessionVolumeChanged"),
        AudioEvent::SessionStateChanged { instance, state } => {
            debug!(%instance, ?state, "SessionStateChanged");
        }
        AudioEvent::SessionDisconnected { instance, reason } => {
            debug!(%instance, ?reason, "SessionDisconnected");
        }
        AudioEvent::CoreError(err) => warn!(error = %describe_core_error(err), "CoreError"),
    }
}
