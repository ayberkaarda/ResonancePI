//! COM-backed audio core.
//!
//! Everything in this module runs on one dedicated thread named
//! `resonance-audio-core`, which initialises COM as a multi-threaded apartment
//! (MTA). Every COM interface pointer created here is created, used and
//! released on that thread; none of them ever crosses a channel or a thread
//! boundary. The only things that leave the thread are owned values
//! (`EndpointView`, `EndpointId`, `AudioEvent`).
//!
//! Device and session notifications are delivered by the audio service on its
//! own MTA worker threads. Every notification sink therefore holds nothing but
//! owned data and a cloned channel sender: it never calls a COM method, never
//! takes a lock, and returns immediately after enqueueing.
//!
//! `IAudioSessionNotification::OnSessionCreated` is the one callback that is
//! handed a live COM object rather than plain data. Because the callback is not
//! allowed to call into COM, it only takes a reference on the object
//! (`Ref::cloned`, an AddRef) and forwards it over an internal channel to the
//! core thread, which does all the real work. That channel is deliberately
//! private to this module: it carries a Windows type and must not appear in
//! `messages`, which stays platform-independent.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use tracing::{debug, info, trace, warn};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, AudioSessionDisconnectReason,
    AudioSessionState, AudioSessionStateActive, AudioSessionStateExpired,
    DisconnectReasonDeviceRemoval, DisconnectReasonExclusiveModeOverride,
    DisconnectReasonFormatChanged, DisconnectReasonServerShutdown,
    DisconnectReasonSessionDisconnected, DisconnectReasonSessionLogoff, EDataFlow, ERole,
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl,
    IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDevice,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, ISimpleAudioVolume,
    MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateGuid, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows_core::{
    implement, Error as ComError, Interface, Ref, Result as ComResult, BOOL, GUID, PCWSTR, PWSTR,
};

use crate::messages::{
    AudioEvent, CoreCommand, CoreError, DataFlow, DisconnectReason, EndpointId, EndpointState,
    EndpointView, ProcessKey, Role, RoleSet, SessionInfo, SessionInstanceId, SessionState,
};
#[cfg(feature = "switching")]
use crate::policy_config::PolicyConfig;

/// The role reported as "the" default render endpoint at startup. Windows'
/// "Default Device" in the sound control panel maps to the console role.
const DEFAULT_ROLE: ERole = eConsole;

/// Upper bound for a process image path, in UTF-16 code units.
///
/// `QueryFullProcessImageNameW` can return an extended-length path, which is
/// capped at 32767 characters; this buffer is generous enough for every real
/// executable path while staying small enough to live on the stack.
const IMAGE_PATH_CAPACITY: usize = 1024;

/// Process-wide GUID that tags every volume/mute write Resonance makes itself.
///
/// `ISimpleAudioVolume::SetMasterVolume`/`SetMute` echo back through
/// `IAudioSessionEvents::OnSimpleVolumeChanged`; comparing the event context
/// against this GUID is what separates "the user moved a slider" from "we just
/// restored a profile".
static EVENT_CONTEXT: OnceLock<GUID> = OnceLock::new();

/// Generate the process-wide event context once, on the core thread.
///
/// Idempotent: a second call returns the GUID produced by the first.
fn init_event_context() -> Result<GUID, CoreError> {
    if let Some(existing) = EVENT_CONTEXT.get() {
        return Ok(*existing);
    }
    // SAFETY: `CoCreateGuid` takes no arguments and writes into a stack slot
    // owned by the wrapper. It has no apartment requirement, so it is safe to
    // call here on the audio core thread right after CoInitializeEx.
    let guid = unsafe { CoCreateGuid() }
        .map_err(|e| CoreError::ComInitFailed(format!("CoCreateGuid failed: {e}")))?;
    Ok(*EVENT_CONTEXT.get_or_init(|| guid))
}

/// Result of the one-shot bootstrap performed by the core thread: the world as
/// it looked the moment notifications were armed. Owned data only.
#[derive(Debug, Clone)]
pub struct CoreStartup {
    /// Active render endpoints, in enumeration order.
    pub endpoints: Vec<EndpointView>,
    /// Default render endpoint for the console role, if one exists.
    pub default_render: Option<EndpointId>,
}

/// Handle to the running audio core thread.
pub struct CoreThread {
    startup: CoreStartup,
    join: Option<JoinHandle<()>>,
}

impl CoreThread {
    /// The endpoint list captured during bootstrap.
    pub fn startup(&self) -> &CoreStartup {
        &self.startup
    }

    /// Wait for the core thread to finish. It stops on `CoreCommand::Shutdown`
    /// or when the command channel is closed.
    pub fn join(mut self) {
        if let Some(handle) = self.join.take() {
            if handle.join().is_err() {
                warn!("audio core thread panicked");
            }
        }
    }
}

/// Start the audio core thread and block until it has initialised COM,
/// enumerated the active render endpoints and registered for device
/// notifications.
///
/// Returns the initial endpoint snapshot, or the error that stopped the core
/// from starting.
pub fn spawn(
    cmd_rx: Receiver<CoreCommand>,
    ev_tx: Sender<AudioEvent>,
) -> Result<CoreThread, CoreError> {
    let (init_tx, init_rx) = bounded::<Result<CoreStartup, CoreError>>(1);

    let join = thread::Builder::new()
        .name("resonance-audio-core".to_owned())
        .spawn(move || core_thread_main(cmd_rx, ev_tx, init_tx))
        .map_err(|e| CoreError::Other(format!("failed to spawn audio core thread: {e}")))?;

    match init_rx.recv() {
        Ok(Ok(startup)) => Ok(CoreThread {
            startup,
            join: Some(join),
        }),
        Ok(Err(err)) => {
            let _ = join.join();
            Err(err)
        }
        Err(_) => Err(CoreError::Other(
            "audio core thread exited before reporting initialisation".to_owned(),
        )),
    }
}

fn core_thread_main(
    cmd_rx: Receiver<CoreCommand>,
    ev_tx: Sender<AudioEvent>,
    init_tx: Sender<Result<CoreStartup, CoreError>>,
) {
    // Declared before `core` so that it is dropped *after* it: every COM
    // pointer must be released before CoUninitialize runs.
    let _com = match ComGuard::initialize_mta() {
        Ok(guard) => guard,
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    if let Err(err) = init_event_context() {
        let _ = init_tx.send(Err(err));
        return;
    }

    // Internal, Windows-typed handoff channel: `OnSessionCreated` is the only
    // callback that receives a live COM object, and it may not touch it. It
    // AddRefs the object and posts it here; everything below runs on this
    // thread.
    let (raw_tx, raw_rx) = unbounded::<RawSessionEvent>();

    let mut core = match AudioCore::new(ev_tx, raw_tx) {
        Ok(core) => core,
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    let startup = match core.bootstrap_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    info!(
        endpoints = startup.endpoints.len(),
        sessions = core.session_count(),
        "audio core ready (MTA, endpoint and session notifications registered)"
    );
    let _ = init_tx.send(Ok(startup));

    loop {
        select! {
            recv(cmd_rx) -> message => match message {
                Ok(CoreCommand::Shutdown) => {
                    debug!("core received Shutdown");
                    break;
                }
                Ok(other) => core.handle(other),
                Err(_) => {
                    debug!("command channel closed, stopping audio core");
                    break;
                }
            },
            recv(raw_rx) -> message => match message {
                Ok(RawSessionEvent::Created { endpoint, control }) => {
                    core.on_session_created(&endpoint, control.0);
                }
                Err(_) => {
                    // Unreachable in practice: `core` owns a sender for this
                    // channel, so it cannot disconnect while the loop runs.
                    warn!("internal session channel closed, stopping audio core");
                    break;
                }
            },
        }
    }

    // `core` drops here, on the thread that created its COM pointers: the
    // notification callback is unregistered and the enumerator released before
    // `_com` runs CoUninitialize.
    drop(core);
}

/// RAII guard around `CoInitializeEx`/`CoUninitialize`.
///
/// Deliberately neither `Send` nor `Sync` (it holds a raw marker and COM
/// apartment state is per-thread): it must be dropped on the very thread that
/// initialised the apartment.
struct ComGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ComGuard {
    fn initialize_mta() -> Result<Self, CoreError> {
        // SAFETY: called exactly once, at the top of the dedicated audio core
        // thread, before any other COM call on it. `CoInitializeEx` takes no
        // pointer we own (`None` for the reserved parameter). It returns an
        // HRESULT rather than a Result; S_FALSE (already initialised on this
        // thread with the same apartment model) is a success code and is
        // accepted by `.ok()`. RPC_E_CHANGED_MODE would mean this thread is
        // already an STA, which is a hard error for the audio core.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.ok().map_err(|e: ComError| {
            CoreError::ComInitFailed(format!("CoInitializeEx(COINIT_MULTITHREADED) failed: {e}"))
        })?;
        debug!("COM initialised as MTA on the audio core thread");
        Ok(Self {
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the `CoInitializeEx` performed by
        // `initialize_mta()` on this same thread. The guard is not `Send`, so
        // it cannot be dropped anywhere else, and every COM pointer owned by
        // the core has already been released by the time this runs (drop
        // order: `AudioCore` first, guard last).
        unsafe { CoUninitialize() };
        debug!("COM uninitialised on the audio core thread");
    }
}

/// Owns the COM state of the core thread.
struct AudioCore {
    enumerator: IMMDeviceEnumerator,
    /// Keeps the notification sink alive for as long as it is registered.
    notify: IMMNotificationClient,
    /// One hook per active render endpoint. Profiles are per endpoint and
    /// sessions live on non-default devices too, so every active render
    /// endpoint gets its own session manager.
    endpoints: HashMap<EndpointId, EndpointHook>,
    /// Outbound, platform-independent event channel.
    ev_tx: Sender<AudioEvent>,
    /// Internal handoff channel handed to every `SessionNotifier`.
    raw_tx: Sender<RawSessionEvent>,
    /// Default endpoint switching, when it is both compiled in and available on
    /// this machine.
    ///
    /// `None` means the startup check could not create the undocumented
    /// `PolicyConfigClient`; the core then runs in "profiles only" mode, where
    /// switching requests are logged and dropped instead of failing.
    #[cfg(feature = "switching")]
    policy: Option<PolicyConfig>,
}

impl AudioCore {
    fn new(ev_tx: Sender<AudioEvent>, raw_tx: Sender<RawSessionEvent>) -> Result<Self, CoreError> {
        // SAFETY: runs on the audio core thread, after CoInitializeEx has put
        // it into the MTA. `MMDeviceEnumerator` is a static CLSID constant, so
        // the pointer passed in is valid for the duration of the call; no
        // aggregation (`None`). The returned interface is owned by this struct
        // and never leaves this thread.
        let enumerator: IMMDeviceEnumerator = unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }
        .map_err(|e| {
            CoreError::ComInitFailed(format!("CoCreateInstance(MMDeviceEnumerator) failed: {e}"))
        })?;

        let notify: IMMNotificationClient = DeviceNotifySink { tx: ev_tx.clone() }.into();

        // SAFETY: runs on the audio core thread. `notify` is kept alive in the
        // returned struct for at least as long as the registration (it is
        // unregistered in `Drop` before the field is released), which is what
        // the audio service requires of a notification client.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&notify) }.map_err(|e| {
            CoreError::ComInitFailed(format!("RegisterEndpointNotificationCallback failed: {e}"))
        })?;

        Ok(Self {
            enumerator,
            notify,
            endpoints: HashMap::new(),
            ev_tx,
            raw_tx,
            #[cfg(feature = "switching")]
            policy: probe_policy_config(),
        })
    }

    fn bootstrap_snapshot(&mut self) -> Result<CoreStartup, CoreError> {
        let endpoints = self.active_render_endpoints()?;
        let default_render = self.default_render_endpoint();
        Ok(CoreStartup {
            endpoints,
            default_render,
        })
    }

    /// Number of sessions currently hooked across all endpoints.
    fn session_count(&self) -> usize {
        self.endpoints
            .values()
            .map(|hook| hook.sessions.len())
            .sum()
    }

    #[inline]
    fn emit(&self, event: AudioEvent) {
        let _ = self.ev_tx.send(event);
    }

    fn active_render_endpoints(&mut self) -> Result<Vec<EndpointView>, CoreError> {
        // SAFETY: runs on the audio core thread that owns `self.enumerator`.
        // The returned collection is a COM object owned by this scope.
        let collection = unsafe {
            self.enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        }
        .map_err(|e| {
            CoreError::DeviceEnumerationFailed(format!("EnumAudioEndpoints(eRender) failed: {e}"))
        })?;

        // SAFETY: `collection` is a live interface pointer owned by this scope
        // and used only on this thread.
        let count = unsafe { collection.GetCount() }.map_err(|e| {
            CoreError::DeviceEnumerationFailed(format!("IMMDeviceCollection::GetCount failed: {e}"))
        })?;

        let mut endpoints = Vec::with_capacity(count as usize);
        for index in 0..count {
            // SAFETY: `index` is below the count just read from the same
            // collection, which is alive for this whole loop.
            let device = match unsafe { collection.Item(index) } {
                Ok(device) => device,
                Err(e) => {
                    warn!(index, error = %e, "IMMDeviceCollection::Item failed, skipping endpoint");
                    continue;
                }
            };
            match endpoint_view(&device) {
                Ok(view) => {
                    self.hook_endpoint(&device, &view.id);
                    endpoints.push(view);
                }
                Err(err) => {
                    warn!(index, error = ?err, "could not describe endpoint, skipping it");
                }
            }
        }

        Ok(endpoints)
    }

    /// Attach an `IAudioSessionManager2` to one render endpoint, arm session
    /// notifications on it and hook every session that already exists.
    ///
    /// Failures are logged and skipped: one endpoint that refuses to activate a
    /// session manager must not take the whole core down.
    fn hook_endpoint(&mut self, device: &IMMDevice, endpoint: &EndpointId) {
        if self.endpoints.contains_key(endpoint) {
            return;
        }

        // SAFETY: runs on the audio core thread that owns `device`. The CLSCTX
        // is a plain flag and `None` means "no activation parameters"; the
        // returned manager is owned by the `EndpointHook` built below and never
        // leaves this thread.
        let activated = unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) };
        let manager = match activated {
            Ok(manager) => manager,
            Err(e) => {
                warn!(%endpoint, error = %e, "IMMDevice::Activate(IAudioSessionManager2) failed, endpoint has no session support");
                return;
            }
        };

        let notifier: IAudioSessionNotification = SessionNotifier {
            endpoint: endpoint.clone(),
            tx: self.raw_tx.clone(),
        }
        .into();

        // SAFETY: runs on the audio core thread. `notifier` is kept alive by
        // the `EndpointHook` for as long as the registration lasts; the hook's
        // `Drop` unregisters before releasing it.
        if let Err(e) = unsafe { manager.RegisterSessionNotification(&notifier) } {
            warn!(%endpoint, error = %e, "RegisterSessionNotification failed, no session events for this endpoint");
            return;
        }

        let hook = EndpointHook {
            endpoint: endpoint.clone(),
            manager,
            notifier,
            sessions: HashMap::new(),
        };

        // The session enumeration API discards new-session notifications until
        // the existing list has been retrieved once, and retrieving the list
        // means calling `GetCount` on the enumerator, not merely obtaining it.
        // Skipping this leaves `OnSessionCreated` permanently silent.
        let existing = hook.enumerate_sessions();

        self.endpoints.insert(endpoint.clone(), hook);
        debug!(%endpoint, existing = existing.len(), "session notifications armed");

        for control in existing {
            self.on_session_created(endpoint, control);
        }
    }

    /// Do the real work for a session that either already existed or was just
    /// announced by `OnSessionCreated`. Always runs on the core thread.
    fn on_session_created(&mut self, endpoint: &EndpointId, control: IAudioSessionControl) {
        if !self.endpoints.contains_key(endpoint) {
            trace!(%endpoint, "session announced for an endpoint that is not hooked, ignoring");
            return;
        }
        match hook_session(endpoint, control, &self.ev_tx) {
            Ok(Some((info, handle))) => {
                let Some(hook) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                if hook.sessions.contains_key(&info.instance) {
                    // Already hooked (duplicate notification, or a resync that
                    // raced a live notification). `handle` drops here and
                    // unregisters its own sink.
                    trace!(%endpoint, instance = %info.instance, "session already hooked, dropping duplicate");
                    return;
                }
                debug!(%endpoint, process = %info.process, instance = %info.instance, volume = info.volume, muted = info.muted, "session hooked");
                hook.sessions.insert(info.instance.clone(), handle);
                self.emit(AudioEvent::SessionCreated {
                    endpoint: endpoint.clone(),
                    session: info,
                });
            }
            Ok(None) => {}
            Err(err) => {
                warn!(%endpoint, error = ?err, "could not hook a session");
            }
        }
    }

    /// Re-enumerate one endpoint: hook sessions that appeared while we were not
    /// listening and re-announce the ones already known with their current
    /// volume, so a consumer that missed events can rebuild its picture.
    fn resync_endpoint(&mut self, endpoint: &EndpointId) {
        let Some(hook) = self.endpoints.get(endpoint) else {
            warn!(%endpoint, "ResyncEndpoint for an endpoint that is not hooked");
            return;
        };

        // Both COM reads happen while the immutable borrow of the registry is
        // alive; the results are owned, so the borrow ends before anything is
        // inserted back into it below.
        let readings: Vec<(SessionInstanceId, ComResult<(f32, bool)>)> = hook
            .sessions
            .iter()
            .map(|(instance, handle)| (instance.clone(), handle.read_volume()))
            .collect();
        let controls = hook.enumerate_sessions();

        for (instance, reading) in readings {
            match reading {
                Ok((volume, muted)) => self.emit(AudioEvent::SessionVolumeChanged {
                    instance,
                    volume,
                    muted,
                    own_change: false,
                }),
                Err(e) => {
                    warn!(%endpoint, %instance, error = %e, "could not read session volume during resync");
                }
            }
        }

        for control in controls {
            self.on_session_created(endpoint, control);
        }
    }

    /// Apply a volume/mute pair to one hooked session, tagged with
    /// `EVENT_CONTEXT` so the echo comes back marked as our own change.
    fn apply_session_volume(&self, instance: &SessionInstanceId, volume: f32, muted: bool) {
        let Some(handle) = self
            .endpoints
            .values()
            .find_map(|hook| hook.sessions.get(instance))
        else {
            debug!(%instance, "ApplySessionVolume for an unknown session, ignoring");
            return;
        };
        if let Err(e) = handle.apply(volume, muted) {
            warn!(%instance, error = %e, "ApplySessionVolume failed");
        }
    }

    fn default_render_endpoint(&self) -> Option<EndpointId> {
        // SAFETY: runs on the audio core thread that owns `self.enumerator`.
        // A missing default device is reported as an error HRESULT rather than
        // a null pointer, and is handled below.
        let device = match unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(eRender, DEFAULT_ROLE)
        } {
            Ok(device) => device,
            Err(e) => {
                debug!(error = %e, "no default render endpoint for the console role");
                return None;
            }
        };
        match device_id(&device) {
            Ok(id) => Some(id),
            Err(err) => {
                warn!(error = ?err, "could not read the id of the default render endpoint");
                None
            }
        }
    }

    /// Make `id` the default render endpoint for the selected roles.
    ///
    /// Switching is unsupported API, so every failure short of a panic is a log
    /// line: a machine where `IPolicyConfig` is missing still records and
    /// restores profiles, it just cannot change the default device itself.
    #[cfg(feature = "switching")]
    fn set_default_endpoint(&self, id: &EndpointId, roles: RoleSet) {
        let Some(policy) = self.policy.as_ref() else {
            warn!(%id, "SetDefaultEndpoint ignored: switching unavailable (IPolicyConfig unsupported on this system)");
            return;
        };
        match policy.set_default(id, roles) {
            Ok(()) => debug!(%id, ?roles, "default endpoint switch requested"),
            Err(e) => warn!(%id, ?roles, error = %e, "SetDefaultEndpoint failed"),
        }
    }

    /// Without the `switching` feature the core has no way to change the default
    /// endpoint, so the request is logged and dropped.
    #[cfg(not(feature = "switching"))]
    fn set_default_endpoint(&self, id: &EndpointId, _roles: RoleSet) {
        warn!(%id, "SetDefaultEndpoint ignored: built without the `switching` feature");
    }

    fn handle(&mut self, command: CoreCommand) {
        match command {
            CoreCommand::Shutdown => {}
            CoreCommand::ApplySessionVolume {
                instance,
                volume,
                muted,
            } => self.apply_session_volume(&instance, volume, muted),
            CoreCommand::SetDefaultEndpoint { id, roles } => self.set_default_endpoint(&id, roles),
            CoreCommand::ResyncEndpoint(id) => self.resync_endpoint(&id),
        }
    }
}

/// Startup smoke check for default endpoint switching.
///
/// Creating the undocumented `PolicyConfigClient` once at startup is what tells
/// us whether switching can work at all on this machine: the call fails if the
/// coclass is not registered or no longer answers to the interface id, which is
/// how a Windows release that withdrew the interface would present itself. A
/// success does not prove the vtable slots are still in the expected order —
/// only a real switch can show that — so it is a liveness check, not a
/// correctness proof.
///
/// Runs on the audio core thread; the returned object never leaves it.
#[cfg(feature = "switching")]
fn probe_policy_config() -> Option<PolicyConfig> {
    match PolicyConfig::new() {
        Ok(policy) => {
            debug!("IPolicyConfig available, default endpoint switching is enabled");
            Some(policy)
        }
        Err(e) => {
            warn!(error = %e, "IPolicyConfig unavailable, continuing without default endpoint switching");
            None
        }
    }
}

impl Drop for AudioCore {
    fn drop(&mut self) {
        // SAFETY: runs on the audio core thread (the only thread that ever
        // holds an `AudioCore`), while both the enumerator and the sink are
        // still alive. Unregistering before the sink is released is what keeps
        // the audio service from calling into a freed object.
        if let Err(e) = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.notify)
        } {
            warn!(error = %e, "UnregisterEndpointNotificationCallback failed");
        } else {
            debug!("endpoint notification callback unregistered");
        }
    }
}

/// Session state of one render endpoint: its session manager, the registered
/// new-session notifier, and every session currently hooked on it.
///
/// Lives only inside `AudioCore`, which lives only on the core thread, so its
/// `Drop` — and every COM release it performs — runs there.
struct EndpointHook {
    endpoint: EndpointId,
    manager: IAudioSessionManager2,
    /// Keeps the `#[implement]` object alive while it is registered.
    notifier: IAudioSessionNotification,
    sessions: HashMap<SessionInstanceId, SessionHandle>,
}

impl EndpointHook {
    /// Retrieve the endpoint's current session list.
    ///
    /// Calling `GetCount` on the enumerator is mandatory and not merely
    /// informational: until the list has been retrieved once, the audio service
    /// discards new-session notifications for this manager.
    fn enumerate_sessions(&self) -> Vec<IAudioSessionControl> {
        // SAFETY: runs on the audio core thread that owns `self.manager`; the
        // enumerator returned is a COM object owned by this scope.
        let enumerator = match unsafe { self.manager.GetSessionEnumerator() } {
            Ok(enumerator) => enumerator,
            Err(e) => {
                warn!(endpoint = %self.endpoint, error = %e, "GetSessionEnumerator failed");
                return Vec::new();
            }
        };

        // SAFETY: `enumerator` is live and used only on this thread. Unlike
        // `IMMDeviceCollection::GetCount` this one yields a signed count.
        let count = match unsafe { enumerator.GetCount() } {
            Ok(count) => count,
            Err(e) => {
                warn!(endpoint = %self.endpoint, error = %e, "IAudioSessionEnumerator::GetCount failed");
                return Vec::new();
            }
        };

        let mut sessions = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            // SAFETY: `index` is below the count just read from the same
            // enumerator, which stays alive for this whole loop.
            match unsafe { enumerator.GetSession(index) } {
                Ok(control) => sessions.push(control),
                Err(e) => {
                    warn!(endpoint = %self.endpoint, index, error = %e, "IAudioSessionEnumerator::GetSession failed");
                }
            }
        }
        sessions
    }
}

impl Drop for EndpointHook {
    fn drop(&mut self) {
        // The sessions are released first, each unregistering its own event
        // sink, before this endpoint's new-session notification goes away.
        self.sessions.clear();

        // SAFETY: runs on the audio core thread while both the manager and the
        // notifier are still alive. Unregistering before the notifier is
        // released is what keeps the audio service from calling into a freed
        // object.
        if let Err(e) = unsafe { self.manager.UnregisterSessionNotification(&self.notifier) } {
            warn!(endpoint = %self.endpoint, error = %e, "UnregisterSessionNotification failed");
        } else {
            debug!(endpoint = %self.endpoint, "session notifications disarmed");
        }
    }
}

/// Internal, Windows-typed handoff message.
///
/// Deliberately private to this module: it carries a COM interface pointer and
/// therefore must never appear in `messages`, which stays free of `windows`
/// types so `resonance-state` can be built and tested on any OS.
enum RawSessionEvent {
    Created {
        endpoint: EndpointId,
        control: MtaSessionControl,
    },
}

/// An owned `IAudioSessionControl` reference in transit between two threads of
/// the same multi-threaded apartment.
///
/// The `windows` wrapper types are not `Send` — correctly so, since a COM
/// pointer generally may not cross an apartment boundary without marshalling.
/// This wrapper narrows that rule to the one case Resonance relies on and makes
/// the reasoning explicit at the type level rather than leaving it implicit.
struct MtaSessionControl(IAudioSessionControl);

// SAFETY: the only producer of this value is `SessionNotifier::OnSessionCreated`
// and the only consumer is the audio core thread. Both are in the same
// multi-threaded apartment — the audio service calls MTA sinks from its own MTA
// worker threads, and the core thread initialises COM with
// COINIT_MULTITHREADED — and COM allows an interface pointer to be used from
// any thread of the apartment it belongs to without marshalling. The reference
// taken by `Ref::cloned` is released by whichever of those two threads drops
// the value, so the release also happens inside that same apartment.
unsafe impl Send for MtaSessionControl {}

/// New-session notification sink, one per endpoint.
///
/// Holds an endpoint id and a channel sender; no COM pointer, no lock. The one
/// thing it does with the object it is handed is take a reference on it
/// (`Ref::cloned` is an AddRef, not a call into the object), because the
/// pointer is only valid for the duration of the callback. Everything else —
/// the QueryInterface, the process lookup, the registration — happens on the
/// core thread.
#[implement(IAudioSessionNotification)]
struct SessionNotifier {
    endpoint: EndpointId,
    tx: Sender<RawSessionEvent>,
}

/// Invoked on audio-service worker threads, so it must be `Send + Sync`.
/// Asserted at compile time so a future field that is not thread-safe fails the
/// build here.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SessionNotifier>();
};

#[allow(non_snake_case)]
impl IAudioSessionNotification_Impl for SessionNotifier_Impl {
    fn OnSessionCreated(&self, newsession: Ref<'_, IAudioSessionControl>) -> ComResult<()> {
        let Some(control) = newsession.cloned() else {
            trace!(endpoint = %self.endpoint, "OnSessionCreated with a null session, ignoring");
            return Ok(());
        };
        trace!(endpoint = %self.endpoint, "OnSessionCreated");
        let _ = self.tx.send(RawSessionEvent::Created {
            endpoint: self.endpoint.clone(),
            control: MtaSessionControl(control),
        });
        Ok(())
    }
}

/// One hooked session. Owns the COM pointers for that session and keeps its
/// event sink alive; created, used and dropped on the core thread only.
struct SessionHandle {
    control: IAudioSessionControl2,
    volume: ISimpleAudioVolume,
    /// Keeps the `#[implement]` object alive while it is registered.
    sink: IAudioSessionEvents,
}

impl SessionHandle {
    fn read_volume(&self) -> ComResult<(f32, bool)> {
        // SAFETY: runs on the audio core thread that owns `self.volume`; both
        // calls only write into stack slots owned by the wrapper.
        let level = unsafe { self.volume.GetMasterVolume() }?;
        // SAFETY: as above. `GetMute` yields a `BOOL`, not a Rust `bool`.
        let muted = unsafe { self.volume.GetMute() }?;
        Ok((level, muted.as_bool()))
    }

    fn apply(&self, volume: f32, muted: bool) -> ComResult<()> {
        let context = EVENT_CONTEXT
            .get()
            .ok_or_else(|| ComError::from_hresult(windows::Win32::Foundation::E_UNEXPECTED))?;
        // SAFETY: runs on the audio core thread that owns `self.volume`.
        // `context` points at a `'static` GUID that outlives the call; passing
        // it is what makes the resulting `OnSimpleVolumeChanged` recognisable
        // as our own write rather than a user action.
        unsafe { self.volume.SetMasterVolume(volume.clamp(0.0, 1.0), context) }?;
        // SAFETY: as above; `SetMute` takes a Rust `bool` here, unlike
        // `GetMute`, which returns a `BOOL`.
        unsafe { self.volume.SetMute(muted, context) }?;
        Ok(())
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // SAFETY: runs on the audio core thread (the registry that owns every
        // `SessionHandle` lives there), while both the control and the sink are
        // still alive. Unregistering before the sink is released keeps the
        // audio service from calling into a freed object.
        if let Err(e) = unsafe { self.control.UnregisterAudioSessionNotification(&self.sink) } {
            warn!(error = %e, "UnregisterAudioSessionNotification failed");
        }
    }
}

/// Per-session event sink.
///
/// Holds an instance id and a channel sender: no COM pointer, no lock. Its
/// methods run on audio-service worker threads, so they only translate the
/// callback into an owned `AudioEvent`, enqueue it and return.
#[implement(IAudioSessionEvents)]
struct SessionEventSink {
    instance: SessionInstanceId,
    tx: Sender<AudioEvent>,
}

/// Invoked on audio-service worker threads, so it must be `Send + Sync`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SessionEventSink>();
};

impl SessionEventSink {
    #[inline]
    fn emit(&self, event: AudioEvent) {
        // The channel is unbounded, so this never blocks the calling worker
        // thread. If the consumer is gone the event is dropped on purpose.
        let _ = self.tx.send(event);
    }
}

#[allow(non_snake_case)]
impl IAudioSessionEvents_Impl for SessionEventSink_Impl {
    fn OnDisplayNameChanged(&self, _name: &PCWSTR, _context: *const GUID) -> ComResult<()> {
        Ok(())
    }

    fn OnIconPathChanged(&self, _path: &PCWSTR, _context: *const GUID) -> ComResult<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        newvolume: f32,
        newmute: BOOL,
        eventcontext: *const GUID,
    ) -> ComResult<()> {
        let own_change = is_own_event_context(eventcontext);
        trace!(instance = %self.instance, newvolume, own_change, "OnSimpleVolumeChanged");
        self.emit(AudioEvent::SessionVolumeChanged {
            instance: self.instance.clone(),
            volume: newvolume,
            muted: newmute.as_bool(),
            own_change,
        });
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _channelcount: u32,
        _newchannelvolumearray: *const f32,
        _changedchannel: u32,
        _eventcontext: *const GUID,
    ) -> ComResult<()> {
        // Resonance manages the master volume of a session, not its individual
        // channels; there is no message type for per-channel changes.
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _newgroupingparam: *const GUID,
        _eventcontext: *const GUID,
    ) -> ComResult<()> {
        Ok(())
    }

    fn OnStateChanged(&self, newstate: AudioSessionState) -> ComResult<()> {
        let state = SessionState::from(newstate);
        trace!(instance = %self.instance, ?state, "OnStateChanged");
        self.emit(AudioEvent::SessionStateChanged {
            instance: self.instance.clone(),
            state,
        });
        Ok(())
    }

    fn OnSessionDisconnected(
        &self,
        disconnectreason: AudioSessionDisconnectReason,
    ) -> ComResult<()> {
        let reason = DisconnectReason::from(disconnectreason);
        trace!(instance = %self.instance, ?reason, "OnSessionDisconnected");
        self.emit(AudioEvent::SessionDisconnected {
            instance: self.instance.clone(),
            reason,
        });
        Ok(())
    }
}

/// Decide whether a callback's event context is the GUID Resonance stamps on
/// its own volume writes.
fn is_own_event_context(context: *const GUID) -> bool {
    // SAFETY: the audio service passes either null or a pointer to a GUID that
    // is valid for the duration of the callback. The value is only read here
    // and the pointer is not retained.
    let observed = unsafe { context.as_ref() };
    observed
        .zip(EVENT_CONTEXT.get())
        .is_some_and(|(observed, ours)| observed == ours)
}

/// Windows defines three session states; an unrecognised value could only come
/// from a future revision and is treated as inactive, the state with no
/// side effects.
impl From<AudioSessionState> for SessionState {
    fn from(value: AudioSessionState) -> Self {
        if value == AudioSessionStateActive {
            Self::Active
        } else if value == AudioSessionStateExpired {
            Self::Expired
        } else {
            Self::Inactive
        }
    }
}

/// Windows defines six disconnect reasons; an unrecognised value could only
/// come from a future revision and is reported as a server shutdown, whose
/// handling — let the session go — is correct for any unknown cause.
impl From<AudioSessionDisconnectReason> for DisconnectReason {
    fn from(value: AudioSessionDisconnectReason) -> Self {
        if value == DisconnectReasonDeviceRemoval {
            Self::DeviceRemoval
        } else if value == DisconnectReasonFormatChanged {
            Self::FormatChanged
        } else if value == DisconnectReasonSessionLogoff {
            Self::SessionLogoff
        } else if value == DisconnectReasonSessionDisconnected {
            Self::SessionDisconnected
        } else if value == DisconnectReasonExclusiveModeOverride {
            Self::ExclusiveModeOverride
        } else if value == DisconnectReasonServerShutdown {
            Self::ServerShutdown
        } else {
            trace!(raw = value.0, "unknown AudioSessionDisconnectReason");
            Self::ServerShutdown
        }
    }
}

/// Turn a freshly announced `IAudioSessionControl` into a hooked session.
///
/// Returns `Ok(None)` for sessions Resonance deliberately ignores (the system
/// sounds session). Every COM call below happens on the core thread.
fn hook_session(
    endpoint: &EndpointId,
    control: IAudioSessionControl,
    ev_tx: &Sender<AudioEvent>,
) -> Result<Option<(SessionInfo, SessionHandle)>, CoreError> {
    let control: IAudioSessionControl2 = control
        .cast()
        .map_err(|e| session_error(format!("cast to IAudioSessionControl2 failed: {e}")))?;

    // SAFETY: `control` is a live interface pointer used on the core thread.
    // This method returns a raw HRESULT rather than a Result: S_OK means "yes,
    // system sounds", S_FALSE means "no". Treating both as success — which
    // `.ok()` would do — would silently answer the question wrongly.
    let is_system_sounds = unsafe { control.IsSystemSoundsSession() } == S_OK;
    if is_system_sounds {
        trace!(%endpoint, "skipping the system sounds session");
        return Ok(None);
    }

    // SAFETY: `control` is live and used on the core thread. The returned
    // string is allocated with CoTaskMemAlloc and ownership passes to us;
    // `take_pwstr` reads it once and frees it with the matching allocator.
    let raw_instance = unsafe { control.GetSessionInstanceIdentifier() }
        .map_err(|e| session_error(format!("GetSessionInstanceIdentifier failed: {e}")))?;
    let instance = take_pwstr(raw_instance).ok_or_else(|| {
        session_error("GetSessionInstanceIdentifier returned no usable string".to_owned())
    })?;

    // SAFETY: as above; this identifier is the per-application one and is used
    // only as a fallback source for the process name.
    let raw_identifier = unsafe { control.GetSessionIdentifier() }.ok();
    let identifier = raw_identifier.and_then(take_pwstr);

    // SAFETY: `control` is live and used on the core thread; `GetProcessId`
    // writes into a stack slot owned by the wrapper.
    let pid = unsafe { control.GetProcessId() }
        .map_err(|e| session_error(format!("GetProcessId failed: {e}")))?;

    let process = resolve_process_key(pid, identifier.as_deref());

    let volume: ISimpleAudioVolume = control
        .cast()
        .map_err(|e| session_error(format!("cast to ISimpleAudioVolume failed: {e}")))?;

    let instance: SessionInstanceId = Arc::from(instance.as_str());

    let sink: IAudioSessionEvents = SessionEventSink {
        instance: instance.clone(),
        tx: ev_tx.clone(),
    }
    .into();

    // SAFETY: runs on the core thread. `sink` is moved into the returned
    // `SessionHandle`, which keeps it alive for as long as the registration
    // lasts and unregisters it in `Drop`.
    unsafe { control.RegisterAudioSessionNotification(&sink) }
        .map_err(|e| session_error(format!("RegisterAudioSessionNotification failed: {e}")))?;

    let handle = SessionHandle {
        control,
        volume,
        sink,
    };

    let (level, muted) = handle
        .read_volume()
        .map_err(|e| session_error(format!("could not read the session volume: {e}")))?;

    let info = SessionInfo {
        instance,
        process,
        endpoint: endpoint.clone(),
        volume: level,
        muted,
    };
    Ok(Some((info, handle)))
}

fn session_error(message: String) -> CoreError {
    CoreError::Other(message)
}

/// Resolve the identity a profile entry is keyed by.
///
/// A protected or elevated process refuses `OpenProcess`, in which case the
/// executable name is recovered from the session identifier, which embeds the
/// image path. The last resort is the pid, which is not stable across restarts
/// and must therefore never be persisted.
fn resolve_process_key(pid: u32, session_identifier: Option<&str>) -> ProcessKey {
    if let Some(name) = process_image_name(pid) {
        return Arc::from(name.as_str());
    }
    if let Some(name) = session_identifier.and_then(exe_name_from_session_identifier) {
        trace!(pid, %name, "process name recovered from the session identifier");
        return Arc::from(name.as_str());
    }
    debug!(
        pid,
        "could not resolve a process name, falling back to the pid"
    );
    Arc::from(format!("pid:{pid}").as_str())
}

/// Lower-cased file name of the image backing `pid`, if it can be read.
fn process_image_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }

    // SAFETY: `OpenProcess` takes only scalars. The returned handle is owned by
    // this function and is closed on every path below before it returns.
    let process: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .inspect_err(|e| trace!(pid, error = %e, "OpenProcess failed"))
        .ok()?;

    let mut buffer = [0u16; IMAGE_PATH_CAPACITY];
    let mut length = buffer.len() as u32;

    // SAFETY: `process` is a live handle opened just above. `buffer` is a stack
    // array of `length` UTF-16 units that outlives the call, and `length` is an
    // in/out parameter: it carries the capacity in and the written length out.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };

    // SAFETY: `process` was opened by this function, has not been closed yet
    // and is not used after this point.
    if let Err(e) = unsafe { CloseHandle(process) } {
        trace!(pid, error = %e, "CloseHandle failed for a process query handle");
    }

    queried
        .inspect_err(|e| trace!(pid, error = %e, "QueryFullProcessImageNameW failed"))
        .ok()?;

    let length = (length as usize).min(buffer.len());
    let path = String::from_utf16(&buffer[..length]).ok()?;
    file_name_lowercase(&path)
}

/// Recover an executable name from a session identifier.
///
/// The identifier looks like
/// `{0.0.0.0000}.{guid}|\Device\HarddiskVolume4\...\app.exe%b{guid}`: the image
/// path sits in the last `|`-separated field, followed by a `%b` suffix.
fn exe_name_from_session_identifier(identifier: &str) -> Option<String> {
    let tail = identifier.rsplit('|').next()?;
    let path = tail.split("%b").next().unwrap_or(tail);
    let name = file_name_lowercase(path)?;
    name.ends_with(".exe").then_some(name)
}

/// Last path component, lower-cased. `None` for a path with no file name.
fn file_name_lowercase(path: &str) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_lowercase())
    }
}

/// Read a COM-allocated wide string and release it with the allocator that
/// produced it.
fn take_pwstr(raw: PWSTR) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is a non-null, NUL-terminated wide string allocated by COM
    // on our behalf; it is read once here and freed immediately afterwards. No
    // copy of the pointer outlives this function.
    let text = unsafe { raw.to_string() };
    // SAFETY: `raw` has not been freed yet, and freeing it with
    // `CoTaskMemFree` is the documented ownership contract of the getters that
    // return it.
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
    text.ok()
}

/// Device notification sink.
///
/// Holds nothing but a channel sender: no COM pointer, no lock. Its methods are
/// invoked on audio-service worker threads, so they only translate the callback
/// into an owned `AudioEvent`, enqueue it and return.
#[implement(IMMNotificationClient)]
struct DeviceNotifySink {
    tx: Sender<AudioEvent>,
}

/// The sink is handed to the audio service, which calls it from arbitrary MTA
/// worker threads; it must therefore be `Send + Sync`. Asserted at compile time
/// so a future field that is not thread-safe fails the build here.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DeviceNotifySink>();
};

impl DeviceNotifySink {
    #[inline]
    fn emit(&self, event: AudioEvent) {
        // The channel is unbounded, so this never blocks the calling worker
        // thread. If the consumer is gone the event is dropped on purpose.
        let _ = self.tx.send(event);
    }
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceNotifySink_Impl {
    fn OnDeviceStateChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        dwnewstate: DEVICE_STATE,
    ) -> ComResult<()> {
        let Some(id) = endpoint_id_from_pcwstr(pwstrdeviceid) else {
            return Ok(());
        };
        let Some(state) = endpoint_state(dwnewstate) else {
            trace!(device = %id, raw_state = dwnewstate.0, "OnDeviceStateChanged with unknown state");
            return Ok(());
        };
        trace!(device = %id, ?state, "OnDeviceStateChanged");
        self.emit(AudioEvent::EndpointStateChanged { id, state });
        Ok(())
    }

    fn OnDeviceAdded(&self, pwstrdeviceid: &PCWSTR) -> ComResult<()> {
        let Some(id) = endpoint_id_from_pcwstr(pwstrdeviceid) else {
            return Ok(());
        };
        trace!(device = %id, "OnDeviceAdded");
        self.emit(AudioEvent::EndpointAdded(id));
        Ok(())
    }

    fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> ComResult<()> {
        let Some(id) = endpoint_id_from_pcwstr(pwstrdeviceid) else {
            return Ok(());
        };
        trace!(device = %id, "OnDeviceRemoved");
        self.emit(AudioEvent::EndpointRemoved(id));
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> ComResult<()> {
        let Some(flow) = data_flow(flow) else {
            return Ok(());
        };
        let Some(role) = endpoint_role(role) else {
            return Ok(());
        };
        // A null id means "there is no default device for this role any more".
        let id = endpoint_id_from_pcwstr(pwstrdefaultdeviceid);
        trace!(?flow, ?role, device = ?id, "OnDefaultDeviceChanged");
        self.emit(AudioEvent::DefaultEndpointChanged { flow, role, id });
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &windows::Win32::Foundation::PROPERTYKEY,
    ) -> ComResult<()> {
        // Property changes (friendly-name edits, format changes, ...) are not
        // consumed yet; there is no message type for them.
        Ok(())
    }
}

/// Copy a callback string parameter into an owned id.
///
/// Returns `None` for a null pointer or for a string that is not valid UTF-16.
fn endpoint_id_from_pcwstr(raw: &PCWSTR) -> Option<EndpointId> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: the pointer belongs to the audio service and is documented to be
    // a NUL-terminated wide string that stays valid for the duration of the
    // callback. The value is copied into an owned `Arc<str>` here and the
    // pointer is not retained beyond this call.
    let owned = unsafe { raw.to_string() };
    match owned {
        Ok(text) => Some(Arc::from(text.as_str())),
        Err(e) => {
            trace!(error = %e, "device id was not valid UTF-16, dropping the notification");
            None
        }
    }
}

fn endpoint_view(device: &IMMDevice) -> Result<EndpointView, CoreError> {
    let id = device_id(device)?;

    // SAFETY: `device` is a live interface pointer owned by the caller on the
    // audio core thread; `GetState` writes into a stack slot owned by the
    // wrapper.
    let raw_state = unsafe { device.GetState() }.map_err(|e| {
        CoreError::DeviceEnumerationFailed(format!("IMMDevice::GetState failed for {id}: {e}"))
    })?;
    let state = endpoint_state(raw_state).ok_or_else(|| {
        CoreError::DeviceEnumerationFailed(format!(
            "IMMDevice::GetState returned unknown state {} for {id}",
            raw_state.0
        ))
    })?;

    let friendly_name = friendly_name(device)?;

    Ok(EndpointView {
        id,
        friendly_name,
        state,
    })
}

/// Read `IMMDevice::GetId` and copy it into an owned `EndpointId`.
fn device_id(device: &IMMDevice) -> Result<EndpointId, CoreError> {
    // SAFETY: `device` is a live interface pointer used on the audio core
    // thread. `GetId` allocates the returned string with CoTaskMemAlloc and
    // transfers ownership to us.
    let raw = unsafe { device.GetId() }
        .map_err(|e| CoreError::DeviceEnumerationFailed(format!("IMMDevice::GetId failed: {e}")))?;

    if raw.is_null() {
        return Err(CoreError::DeviceEnumerationFailed(
            "IMMDevice::GetId returned a null string".to_owned(),
        ));
    }

    // SAFETY: `raw` is the non-null, NUL-terminated wide string just returned
    // by `GetId`; it is read once here and freed immediately afterwards with
    // the allocator that produced it. No copy of the pointer outlives this
    // block.
    let text = unsafe { raw.to_string() };
    // SAFETY: `raw` was allocated by COM on our behalf and has not been freed
    // yet; freeing it here is the documented ownership contract of `GetId`.
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };

    let text = text.map_err(|e| {
        CoreError::DeviceEnumerationFailed(format!("IMMDevice::GetId returned invalid UTF-16: {e}"))
    })?;
    Ok(Arc::from(text.as_str()))
}

/// Read `PKEY_Device_FriendlyName` from the device's property store.
fn friendly_name(device: &IMMDevice) -> Result<Arc<str>, CoreError> {
    // SAFETY: `device` is a live interface pointer used on the audio core
    // thread; the property store is opened read-only and owned by this scope.
    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ) }.map_err(|e| {
        CoreError::DeviceEnumerationFailed(format!("IMMDevice::OpenPropertyStore failed: {e}"))
    })?;

    // SAFETY: `PKEY_Device_FriendlyName` is a static constant, so the pointer
    // passed in is valid for the whole call. The returned PROPVARIANT is owned
    // by us and cleared below before it goes out of scope.
    let mut value: PROPVARIANT =
        unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.map_err(|e| {
            CoreError::DeviceEnumerationFailed(format!(
                "IPropertyStore::GetValue(PKEY_Device_FriendlyName) failed: {e}"
            ))
        })?;

    // SAFETY: `value` was just initialised by `GetValue` and is still owned by
    // this scope; the string it points at is copied before the variant is
    // cleared.
    let name = unsafe { propvariant_string(&value) };

    // SAFETY: `value` is a live, owned PROPVARIANT that has not been cleared
    // yet; clearing it releases the string allocation made by `GetValue`.
    if let Err(e) = unsafe { PropVariantClear(&mut value) } {
        warn!(error = %e, "PropVariantClear failed for a friendly-name property");
    }

    name.map(|text| Arc::from(text.as_str())).ok_or_else(|| {
        CoreError::DeviceEnumerationFailed(
            "PKEY_Device_FriendlyName was not a string value".to_owned(),
        )
    })
}

/// Extract a `VT_LPWSTR` payload from a PROPVARIANT as an owned `String`.
///
/// # Safety
///
/// `value` must be a fully initialised PROPVARIANT that has not been cleared.
unsafe fn propvariant_string(value: &PROPVARIANT) -> Option<String> {
    // SAFETY: the caller guarantees `value` is initialised. Reading `vt` first
    // and only then the matching union arm is the documented way to inspect a
    // PROPVARIANT; the pointer read from `pwszVal` is valid until the variant
    // is cleared, which happens only after this function returns.
    unsafe {
        let inner = &value.Anonymous.Anonymous;
        if inner.vt != VT_LPWSTR {
            return None;
        }
        let text = inner.Anonymous.pwszVal;
        if text.is_null() {
            return None;
        }
        text.to_string().ok()
    }
}

fn endpoint_state(state: DEVICE_STATE) -> Option<EndpointState> {
    if state == DEVICE_STATE_ACTIVE {
        Some(EndpointState::Active)
    } else if state == DEVICE_STATE_DISABLED {
        Some(EndpointState::Disabled)
    } else if state == DEVICE_STATE_NOTPRESENT {
        Some(EndpointState::NotPresent)
    } else if state == DEVICE_STATE_UNPLUGGED {
        Some(EndpointState::Unplugged)
    } else {
        None
    }
}

fn data_flow(flow: EDataFlow) -> Option<DataFlow> {
    if flow == eRender {
        Some(DataFlow::Render)
    } else if flow == eCapture {
        Some(DataFlow::Capture)
    } else {
        None
    }
}

fn endpoint_role(role: ERole) -> Option<Role> {
    if role == eConsole {
        Some(Role::Console)
    } else if role == eMultimedia {
        Some(Role::Multimedia)
    } else if role == eCommunications {
        Some(Role::Communications)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use windows::Win32::Media::Audio::AudioSessionStateInactive;

    const TEST_ID: &str = "{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Drives the sink through the real COM vtable, the same way the audio
    /// service does, and checks that each callback turns into the expected
    /// owned event. This exercises the translation layer only; that Windows
    /// actually delivers these callbacks is verified by running the binary
    /// against real hardware.
    #[test]
    fn notification_callbacks_enqueue_owned_events() {
        let (tx, rx) = unbounded();
        let client: IMMNotificationClient = DeviceNotifySink { tx }.into();
        let id = wide(TEST_ID);

        // SAFETY: `client` is our own object, not a foreign one, and `id` is a
        // NUL-terminated wide string that outlives every call below — exactly
        // the contract the audio service follows when it calls these methods.
        unsafe {
            client.OnDeviceAdded(PCWSTR(id.as_ptr())).unwrap();
            client
                .OnDeviceStateChanged(PCWSTR(id.as_ptr()), DEVICE_STATE_UNPLUGGED)
                .unwrap();
            client
                .OnDefaultDeviceChanged(eRender, eMultimedia, PCWSTR(id.as_ptr()))
                .unwrap();
            client
                .OnDefaultDeviceChanged(eCapture, eConsole, PCWSTR::null())
                .unwrap();
            client.OnDeviceRemoved(PCWSTR(id.as_ptr())).unwrap();
        }

        match rx.try_recv().expect("EndpointAdded") {
            AudioEvent::EndpointAdded(id) => assert_eq!(&*id, TEST_ID),
            _ => panic!("expected EndpointAdded"),
        }
        match rx.try_recv().expect("EndpointStateChanged") {
            AudioEvent::EndpointStateChanged { id, state } => {
                assert_eq!(&*id, TEST_ID);
                assert_eq!(state, EndpointState::Unplugged);
            }
            _ => panic!("expected EndpointStateChanged"),
        }
        match rx.try_recv().expect("DefaultEndpointChanged") {
            AudioEvent::DefaultEndpointChanged { flow, role, id } => {
                assert_eq!(flow, DataFlow::Render);
                assert_eq!(role, Role::Multimedia);
                assert_eq!(id.as_deref(), Some(TEST_ID));
            }
            _ => panic!("expected DefaultEndpointChanged"),
        }
        match rx
            .try_recv()
            .expect("DefaultEndpointChanged with no device")
        {
            AudioEvent::DefaultEndpointChanged { flow, role, id } => {
                assert_eq!(flow, DataFlow::Capture);
                assert_eq!(role, Role::Console);
                assert!(id.is_none(), "a null device id must become None");
            }
            _ => panic!("expected DefaultEndpointChanged"),
        }
        match rx.try_recv().expect("EndpointRemoved") {
            AudioEvent::EndpointRemoved(id) => assert_eq!(&*id, TEST_ID),
            _ => panic!("expected EndpointRemoved"),
        }
        assert!(rx.try_recv().is_err(), "no extra events expected");
    }

    #[test]
    fn property_changes_are_not_forwarded_yet() {
        let (tx, rx) = unbounded();
        let client: IMMNotificationClient = DeviceNotifySink { tx }.into();
        let id = wide(TEST_ID);

        // SAFETY: as above — our own object, and `id` outlives the call.
        unsafe {
            client
                .OnPropertyValueChanged(PCWSTR(id.as_ptr()), PKEY_Device_FriendlyName)
                .unwrap();
        }

        assert!(rx.try_recv().is_err(), "property changes emit no event yet");
    }

    const TEST_INSTANCE: &str = "{0.0.0.00000000}.{1111}|\\Device\\HarddiskVolume4\\a.exe%b{2222}";

    fn test_sink() -> (IAudioSessionEvents, Receiver<AudioEvent>) {
        let (tx, rx) = unbounded();
        let instance: SessionInstanceId = Arc::from(TEST_INSTANCE);
        let sink: IAudioSessionEvents = SessionEventSink { instance, tx }.into();
        (sink, rx)
    }

    /// Echo filtering (R3): a volume change carrying our process-wide event
    /// context is our own write and must be reported as such; anything else —
    /// another application's context, or no context at all — is a user action.
    #[test]
    fn session_volume_callback_flags_only_our_own_writes() {
        let ours = init_event_context().expect("event context");
        let foreign = GUID::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_ne!(ours, foreign);

        let (sink, rx) = test_sink();

        // SAFETY: `sink` is our own object, not a foreign one, and both GUIDs
        // are stack values that outlive the calls — exactly the contract the
        // audio service follows when it invokes this method.
        unsafe {
            sink.OnSimpleVolumeChanged(0.42, false, &ours).unwrap();
            sink.OnSimpleVolumeChanged(0.13, true, &foreign).unwrap();
            sink.OnSimpleVolumeChanged(0.99, false, std::ptr::null())
                .unwrap();
        }

        let expected = [
            (0.42_f32, false, true),
            (0.13, true, false),
            (0.99, false, false),
        ];
        for (volume, muted, own_change) in expected {
            match rx.try_recv().expect("SessionVolumeChanged") {
                AudioEvent::SessionVolumeChanged {
                    instance,
                    volume: got_volume,
                    muted: got_muted,
                    own_change: got_own,
                } => {
                    assert_eq!(&*instance, TEST_INSTANCE);
                    assert_eq!(got_volume, volume);
                    assert_eq!(got_muted, muted);
                    assert_eq!(got_own, own_change, "own_change for volume {volume}");
                }
                _ => panic!("expected SessionVolumeChanged"),
            }
        }
        assert!(rx.try_recv().is_err(), "no extra events expected");
    }

    #[test]
    fn session_state_and_disconnect_callbacks_map_to_events() {
        let (sink, rx) = test_sink();
        let text = wide("whatever");

        // SAFETY: our own object; `text` is a NUL-terminated wide string that
        // outlives every call, and the channel slice is a live stack array.
        unsafe {
            sink.OnStateChanged(AudioSessionStateActive).unwrap();
            sink.OnSessionDisconnected(DisconnectReasonFormatChanged)
                .unwrap();
            sink.OnDisplayNameChanged(PCWSTR(text.as_ptr()), std::ptr::null())
                .unwrap();
            sink.OnIconPathChanged(PCWSTR(text.as_ptr()), std::ptr::null())
                .unwrap();
            sink.OnChannelVolumeChanged(&[0.5_f32, 0.5], 0, std::ptr::null())
                .unwrap();
            sink.OnGroupingParamChanged(std::ptr::null(), std::ptr::null())
                .unwrap();
        }

        match rx.try_recv().expect("SessionStateChanged") {
            AudioEvent::SessionStateChanged { instance, state } => {
                assert_eq!(&*instance, TEST_INSTANCE);
                assert_eq!(state, SessionState::Active);
            }
            _ => panic!("expected SessionStateChanged"),
        }
        match rx.try_recv().expect("SessionDisconnected") {
            AudioEvent::SessionDisconnected { instance, reason } => {
                assert_eq!(&*instance, TEST_INSTANCE);
                assert_eq!(reason, DisconnectReason::FormatChanged);
            }
            _ => panic!("expected SessionDisconnected"),
        }
        assert!(
            rx.try_recv().is_err(),
            "display name, icon, per-channel and grouping changes emit no event"
        );
    }

    #[test]
    fn session_notification_callback_ignores_a_null_session() {
        let (tx, rx) = unbounded::<RawSessionEvent>();
        let endpoint: EndpointId = Arc::from(TEST_ID);
        let notifier: IAudioSessionNotification = SessionNotifier { endpoint, tx }.into();

        // SAFETY: our own object; a null session is what the audio service
        // would pass if it had nothing to announce, and must not be
        // dereferenced.
        unsafe {
            notifier
                .OnSessionCreated(None::<&IAudioSessionControl>)
                .unwrap();
        }

        assert!(rx.try_recv().is_err(), "a null session enqueues nothing");
    }

    #[test]
    fn session_state_conversion_covers_every_documented_value() {
        assert_eq!(
            SessionState::from(AudioSessionStateActive),
            SessionState::Active
        );
        assert_eq!(
            SessionState::from(AudioSessionStateInactive),
            SessionState::Inactive
        );
        assert_eq!(
            SessionState::from(AudioSessionStateExpired),
            SessionState::Expired
        );
        assert_eq!(
            SessionState::from(AudioSessionState(99)),
            SessionState::Inactive,
            "an unknown state must not be reported as active"
        );
    }

    #[test]
    fn disconnect_reason_conversion_covers_every_documented_value() {
        let pairs = [
            (
                DisconnectReasonDeviceRemoval,
                DisconnectReason::DeviceRemoval,
            ),
            (
                DisconnectReasonServerShutdown,
                DisconnectReason::ServerShutdown,
            ),
            (
                DisconnectReasonFormatChanged,
                DisconnectReason::FormatChanged,
            ),
            (
                DisconnectReasonSessionLogoff,
                DisconnectReason::SessionLogoff,
            ),
            (
                DisconnectReasonSessionDisconnected,
                DisconnectReason::SessionDisconnected,
            ),
            (
                DisconnectReasonExclusiveModeOverride,
                DisconnectReason::ExclusiveModeOverride,
            ),
        ];
        for (raw, expected) in pairs {
            assert_eq!(DisconnectReason::from(raw), expected);
        }
        assert_eq!(
            DisconnectReason::from(AudioSessionDisconnectReason(99)),
            DisconnectReason::ServerShutdown
        );
    }

    #[test]
    fn process_key_falls_back_to_the_session_identifier_then_to_the_pid() {
        // pid 0 is never a real application session, so `OpenProcess` is not
        // even attempted and the identifier is used instead.
        let identifier =
            "{0.0.0.00000000}.{abcd}|\\Device\\HarddiskVolume4\\Program Files\\Spotify\\Spotify.EXE%b{ef01}";
        assert_eq!(&*resolve_process_key(0, Some(identifier)), "spotify.exe");
        assert_eq!(&*resolve_process_key(0, None), "pid:0");
        assert_eq!(&*resolve_process_key(0, Some("no path here")), "pid:0");
    }

    #[test]
    fn executable_name_is_recovered_from_a_session_identifier() {
        assert_eq!(
            exe_name_from_session_identifier("{0.0.0}.{1}|\\Device\\Harddisk\\Chrome.exe%b{2}")
                .as_deref(),
            Some("chrome.exe")
        );
        assert_eq!(
            exe_name_from_session_identifier("{0.0.0}.{1}|\\Device\\Harddisk\\Chrome.exe")
                .as_deref(),
            Some("chrome.exe")
        );
        assert_eq!(
            exe_name_from_session_identifier("{0.0.0}.{1}|\\Device\\Harddisk\\something%b{2}"),
            None,
            "only an .exe path is a usable process key"
        );
        assert_eq!(exe_name_from_session_identifier(""), None);
    }

    #[test]
    fn file_names_are_lowercased_and_stripped_of_their_directory() {
        assert_eq!(
            file_name_lowercase("C:\\Program Files\\App\\App.EXE").as_deref(),
            Some("app.exe")
        );
        assert_eq!(file_name_lowercase("App.exe").as_deref(), Some("app.exe"));
        assert_eq!(file_name_lowercase("C:\\dir\\"), None);
        assert_eq!(file_name_lowercase(""), None);
    }
}
