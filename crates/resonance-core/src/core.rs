//! COM-backed audio core.
//!
//! Everything in this module runs on one dedicated thread named
//! `resonance-audio-core`, which initialises COM as a multi-threaded apartment
//! (MTA). Every COM interface pointer created here is created, used and
//! released on that thread; none of them ever crosses a channel or a thread
//! boundary. The only things that leave the thread are owned values
//! (`EndpointView`, `EndpointId`, `AudioEvent`).
//!
//! Device notifications are delivered by the audio service on its own MTA
//! worker threads. The notification sink therefore holds nothing but a cloned
//! `Sender<AudioEvent>`: it never calls a COM method, never takes a lock, and
//! returns immediately after enqueueing an event.

use std::ffi::c_void;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};
use tracing::{debug, info, trace, warn};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT,
    DEVICE_STATE_UNPLUGGED,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows_core::{implement, Error as ComError, Result as ComResult, PCWSTR};

use crate::messages::{
    AudioEvent, CoreCommand, CoreError, DataFlow, EndpointId, EndpointState, EndpointView, Role,
};

/// The role reported as "the" default render endpoint at startup. Windows'
/// "Default Device" in the sound control panel maps to the console role.
const DEFAULT_ROLE: ERole = eConsole;

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

    let core = match AudioCore::new(ev_tx) {
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
        "audio core ready (MTA, endpoint notifications registered)"
    );
    let _ = init_tx.send(Ok(startup));

    loop {
        match cmd_rx.recv() {
            Ok(CoreCommand::Shutdown) => {
                debug!("core received Shutdown");
                break;
            }
            Err(_) => {
                debug!("command channel closed, stopping audio core");
                break;
            }
            Ok(other) => core.handle(other),
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
}

impl AudioCore {
    fn new(ev_tx: Sender<AudioEvent>) -> Result<Self, CoreError> {
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

        let notify: IMMNotificationClient = DeviceNotifySink { tx: ev_tx }.into();

        // SAFETY: runs on the audio core thread. `notify` is kept alive in the
        // returned struct for at least as long as the registration (it is
        // unregistered in `Drop` before the field is released), which is what
        // the audio service requires of a notification client.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&notify) }.map_err(|e| {
            CoreError::ComInitFailed(format!("RegisterEndpointNotificationCallback failed: {e}"))
        })?;

        Ok(Self { enumerator, notify })
    }

    fn bootstrap_snapshot(&self) -> Result<CoreStartup, CoreError> {
        let endpoints = self.active_render_endpoints()?;
        let default_render = self.default_render_endpoint();
        Ok(CoreStartup {
            endpoints,
            default_render,
        })
    }

    fn active_render_endpoints(&self) -> Result<Vec<EndpointView>, CoreError> {
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
                Ok(view) => endpoints.push(view),
                Err(err) => {
                    warn!(index, error = ?err, "could not describe endpoint, skipping it");
                }
            }
        }

        Ok(endpoints)
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

    fn handle(&self, command: CoreCommand) {
        match command {
            CoreCommand::Shutdown => {}
            CoreCommand::ApplySessionVolume { .. } => {
                warn!("ApplySessionVolume ignored: session support is not implemented yet");
            }
            CoreCommand::SetDefaultEndpoint { .. } => {
                warn!("SetDefaultEndpoint ignored: endpoint switching is not implemented yet");
            }
            CoreCommand::ResyncEndpoint(_) => {
                warn!("ResyncEndpoint ignored: session support is not implemented yet");
            }
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
}
