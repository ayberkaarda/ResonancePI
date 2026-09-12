use std::sync::Arc;

pub type EndpointId = Arc<str>;
pub type SessionInstanceId = Arc<str>;
pub type ProcessKey = Arc<str>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFlow {
    Render,
    Capture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoleSet {
    pub console: bool,
    pub multimedia: bool,
    pub communications: bool,
}

impl RoleSet {
    pub fn all() -> Self {
        Self {
            console: true,
            multimedia: true,
            communications: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Inactive,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    DeviceRemoval,
    ServerShutdown,
    FormatChanged,
    SessionLogoff,
    SessionDisconnected,
    ExclusiveModeOverride,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub instance: SessionInstanceId,
    pub process: ProcessKey,
    pub endpoint: EndpointId,
    pub volume: f32,
    pub muted: bool,
}

#[derive(Debug, Clone)]
pub struct EndpointView {
    pub id: EndpointId,
    pub friendly_name: Arc<str>,
    pub state: EndpointState,
}

#[derive(Debug, Clone)]
pub struct SessionView {
    pub instance: SessionInstanceId,
    pub process: ProcessKey,
    pub endpoint: EndpointId,
    pub volume: f32,
    pub muted: bool,
    pub state: SessionState,
}

#[derive(Debug, Clone)]
pub enum CoreError {
    ComInitFailed(String),
    DeviceEnumerationFailed(String),
    Other(String),
}

pub enum AudioEvent {
    DefaultEndpointChanged {
        flow: DataFlow,
        role: Role,
        id: Option<EndpointId>,
    },
    EndpointStateChanged {
        id: EndpointId,
        state: EndpointState,
    },
    EndpointAdded(EndpointId),
    EndpointRemoved(EndpointId),
    SessionCreated {
        endpoint: EndpointId,
        session: SessionInfo,
    },
    SessionVolumeChanged {
        instance: SessionInstanceId,
        volume: f32,
        muted: bool,
        own_change: bool,
    },
    SessionStateChanged {
        instance: SessionInstanceId,
        state: SessionState,
    },
    SessionDisconnected {
        instance: SessionInstanceId,
        reason: DisconnectReason,
    },
    CoreError(CoreError),
}

pub enum CoreCommand {
    ApplySessionVolume {
        instance: SessionInstanceId,
        volume: f32,
        muted: bool,
    },
    SetDefaultEndpoint {
        id: EndpointId,
        roles: RoleSet,
    },
    ResyncEndpoint(EndpointId),
    Shutdown,
}

pub enum UiCommand {
    SwitchEndpoint(EndpointId),
    SetSessionVolume {
        endpoint: EndpointId,
        process: ProcessKey,
        volume: f32,
    },
    SetSessionMute {
        endpoint: EndpointId,
        process: ProcessKey,
        muted: bool,
    },
    ForgetProfileEntry {
        endpoint: EndpointId,
        process: ProcessKey,
    },
    ToggleOverlay,
    Quit,
}

#[derive(Clone)]
pub struct Snapshot {
    pub endpoints: Arc<[EndpointView]>,
    pub default_endpoint: Option<EndpointId>,
    pub live_sessions: Arc<[SessionView]>,
    pub revision: u64,
}
