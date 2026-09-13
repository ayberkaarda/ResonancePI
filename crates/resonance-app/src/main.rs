//! Resonance binary.
//!
//! Three modes exist today:
//!
//! - `--dump-events`: start the audio core only, print the active render
//!   endpoints, and then print every device and session event the core
//!   reports until the process is interrupted (Ctrl+C, or the `quit`
//!   console command).
//! - `--run`: start the full backend (audio core + state manager +
//!   persistence, see [`backend`]) and keep it running until `quit`, printing
//!   audio events and snapshot revisions to stdout instead of showing any UI.
//!   Useful for verifying the backend on its own.
//! - `--overlay`: start the full backend and hand its command sender and
//!   snapshot receiver to `resonance_ui::run`, which owns the tray icon,
//!   the global keyboard shortcut, and the overlay window itself. This is
//!   the real application; the other two modes exist for debugging.

mod autostart;
mod backend;
mod panic_hook;
mod single_instance;

use std::io::{self, BufRead};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};
use resonance_core::core::{self, CoreStartup};
use resonance_core::messages::{
    AudioEvent, CoreCommand, CoreError, EndpointView, ProcessKey, RoleSet, SessionInstanceId,
    Snapshot,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
resonance-app

USAGE:
    resonance-app --dump-events
    resonance-app --run
    resonance-app --overlay

OPTIONS:
    --dump-events    Start the audio core only, then stream audio device and
                     session events to stdout until interrupted (Ctrl+C) or
                     the `quit` console command.
    --run            Start the full backend (audio core, state manager,
                     persistence) and keep it running, printing audio events
                     and snapshot revisions, until `quit`. No overlay/tray UI:
                     this is for backend verification only.
    --overlay        Start the full backend and the overlay/tray UI. This is
                     the real application.
    -h, --help       Show this message.

ENVIRONMENT:
    RESONANCE_LOG    Log filter (default: info). Example: RESONANCE_LOG=trace
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    DumpEvents,
    Run,
    Overlay,
}

impl Mode {
    /// Whether this mode starts the full backend, and therefore the
    /// persistence thread that owns the profile store on disk.
    ///
    /// This is what decides whether the single-instance guard applies. The
    /// point of the guard is to stop two copies of the application from
    /// fighting over two process-wide resources — the global keyboard
    /// shortcut and the profile store — and it is the backend that claims
    /// both. `--dump-events` claims neither: it starts the audio core alone,
    /// only listens, and writes nothing, so it is deliberately left
    /// unguarded and can be run alongside a live instance to observe it,
    /// which is the entire reason that mode exists.
    fn starts_backend(self) -> bool {
        match self {
            Mode::Run | Mode::Overlay => true,
            Mode::DumpEvents | Mode::None => false,
        }
    }
}

fn main() -> ExitCode {
    let mut mode = Mode::None;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--dump-events" => mode = Mode::DumpEvents,
            "--run" => mode = Mode::Run,
            "--overlay" => mode = Mode::Overlay,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument `{other}`\n");
                eprint!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    init_logging();

    // Installed before anything that can panic is started, so a panic during
    // backend startup is still logged. It can only flush once the backend
    // exists to be flushed; see the module docs.
    panic_hook::install();

    // Held for the rest of `main` so the mutex name stays claimed for as long
    // as this process runs. `--help` and the argument-error paths have already
    // returned above without reaching this point: asking for usage text while
    // the application is running is not a second instance of anything, and
    // refusing to print it would be a pointless obstruction.
    let _instance_guard = if mode.starts_backend() {
        match single_instance::acquire() {
            single_instance::Instance::Only(guard) => Some(guard),
            single_instance::Instance::AlreadyRunning => {
                println!("Resonance is already running; this instance will exit.");
                info!("another instance holds the single-instance mutex, exiting quietly");
                return ExitCode::SUCCESS;
            }
        }
    } else {
        None
    };

    match mode {
        Mode::DumpEvents => dump_events_mode(),
        Mode::Run => run_mode(),
        Mode::Overlay => overlay_mode(),
        Mode::None => {
            eprintln!("error: no mode selected\n");
            eprint!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_env("RESONANCE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[derive(Debug, Clone)]
struct SessionSnapshot {
    instance: SessionInstanceId,
    process: ProcessKey,
    volume: f32,
    muted: bool,
}

fn dump_events_mode() -> ExitCode {
    let (cmd_tx, cmd_rx) = unbounded::<CoreCommand>();
    let (ev_tx, ev_rx) = unbounded::<AudioEvent>();

    let core = match core::spawn(cmd_rx, ev_tx) {
        Ok(core) => core,
        Err(err) => {
            eprintln!(
                "error: audio core failed to start: {}",
                describe_error(&err)
            );
            return ExitCode::FAILURE;
        }
    };

    print_startup(core.startup());

    println!();
    println!("listening for device and session events (Ctrl+C to exit)");
    println!("try: plug/unplug a device, change the default output in Windows sound settings,");
    println!("     or move an application's slider in the Windows volume mixer");
    println!();
    println!("commands (type at the prompt below, then Enter):");
    println!("  list                                             list known sessions");
    println!("  vol <instance-prefix> <0.0-1.0> mute|unmute       send ApplySessionVolume");
    println!("  endpoints                                         list known render endpoints");
    println!(
        "  switch <index-or-id-prefix>                       send SetDefaultEndpoint (all roles)"
    );
    println!("  help                                              show this again");
    println!();

    let sessions: Arc<Mutex<Vec<SessionSnapshot>>> = Arc::new(Mutex::new(Vec::new()));
    let endpoints: Arc<[EndpointView]> = startup_endpoints_snapshot(core.startup());

    let stdin_sessions = Arc::clone(&sessions);
    let stdin_endpoints = Arc::clone(&endpoints);
    let stdin_cmd_tx = cmd_tx.clone();
    thread::spawn(move || read_commands(stdin_cmd_tx, stdin_sessions, stdin_endpoints));

    for event in ev_rx.iter() {
        println!("{}", describe_event(&event));
        update_sessions(&sessions, &event);
    }

    // Reached only if the core thread dropped its event sender.
    core.join();
    ExitCode::SUCCESS
}

/// `--run`: start the full backend and keep it alive until `quit`.
///
/// This does not yet drive an overlay/tray UI — `resonance-ui` is separate,
/// unfinished work. What this mode proves is that the backend itself works
/// end to end: the audio core reports events, the reducer restores/records
/// profile entries and republishes a `Snapshot` after every step, and the
/// persistence thread writes `%APPDATA%\Resonance\profiles.json`.
fn run_mode() -> ExitCode {
    let handles = match backend::spawn_backend() {
        Ok(handles) => handles,
        Err(err) => {
            eprintln!("error: backend failed to start: {err}");
            return ExitCode::FAILURE;
        }
    };

    panic_hook::set_emergency_flush(handles.emergency_flush());

    print_startup(&handles.startup);

    println!();
    println!(
        "backend running: audio core + state manager + persistence (Ctrl+C or 'quit' to stop)"
    );
    println!("no overlay/tray UI yet -- this mode only makes the backend observable");
    println!("try: plug/unplug a device, change the default output in Windows sound settings,");
    println!("     or move an application's slider in the Windows volume mixer");
    println!();
    println!("commands (type at the prompt below, then Enter):");
    println!("  quit    flush any pending profile write, stop the backend and exit");
    println!("  help    show this again");
    println!();

    let snapshot_printer = spawn_snapshot_printer(handles.snapshot_rx.clone());

    read_run_commands();

    handles.shutdown();
    // The printer thread's loop ends on its own once the reducer thread (the
    // only sender for `snapshot_tx`) has returned and dropped its sender.
    let _ = snapshot_printer.join();

    ExitCode::SUCCESS
}

/// `--overlay`: start the full backend and hand it to the overlay/tray UI.
///
/// `resonance_ui::run` owns the event loop from here on and blocks until the
/// user quits (from the tray menu or the overlay itself); this function's job
/// is only to start the backend first and shut it down afterwards.
fn overlay_mode() -> ExitCode {
    let handles = match backend::spawn_backend() {
        Ok(handles) => handles,
        Err(err) => {
            eprintln!("error: backend failed to start: {err}");
            return ExitCode::FAILURE;
        }
    };

    panic_hook::set_emergency_flush(handles.emergency_flush());

    // Reconcile the registry with the stored setting once per launch. Windows
    // gives the user ways to remove the entry that this application never sees
    // (Task Manager's Startup tab, or another tool editing the key), which
    // would otherwise leave the overlay's toggle showing "on" forever while
    // nothing actually started with Windows. Re-asserting it here is also what
    // makes a failed `SetAutostart` write self-heal on the next launch. It is
    // only ever a convenience, so a failure is logged and nothing more.
    if let Err(err) = autostart::apply(handles.initial_autostart) {
        warn!(
            error = %err,
            enabled = handles.initial_autostart,
            "could not reconcile the autostart entry at startup"
        );
    }

    let result = resonance_ui::run(
        handles.ui_cmd_tx.clone(),
        handles.snapshot_rx.clone(),
        handles.initial_hotkey,
        handles.initial_autostart,
        handles.initial_widget_visible,
        handles.initial_widget_position,
    );
    handles.shutdown();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: overlay UI failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Prints every published `Snapshot`'s revision and headline counts.
///
/// This deliberately prints *every* value it receives rather than draining to
/// the latest one: it exists to observe the backend during manual
/// verification, not to demonstrate the "take only the latest" consumption
/// policy a real UI would use (documented on `BackendHandles::snapshot_rx`).
fn spawn_snapshot_printer(snapshot_rx: Receiver<Snapshot>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for snapshot in snapshot_rx.iter() {
            println!(
                "snapshot                revision={} endpoints={} live_sessions={} default={}",
                snapshot.revision,
                snapshot.endpoints.len(),
                snapshot.live_sessions.len(),
                snapshot.default_endpoint.as_deref().unwrap_or("<none>")
            );
        }
    })
}

/// Blocks reading console commands for `--run` until `quit` (or EOF/Ctrl+C,
/// which closes stdin and ends the loop the same way `--dump-events` does).
fn read_run_commands() {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        match line.trim() {
            "quit" => break,
            "help" => {
                println!("commands:");
                println!("  quit    flush any pending profile write, stop the backend and exit");
                println!("  help    show this message");
            }
            "" => continue,
            _ => println!("unknown command, type 'help' for the command list"),
        }
    }
}

fn update_sessions(sessions: &Arc<Mutex<Vec<SessionSnapshot>>>, event: &AudioEvent) {
    let mut sessions = sessions.lock().unwrap();
    match event {
        AudioEvent::SessionCreated { session, .. } => {
            sessions.retain(|s| s.instance != session.instance);
            sessions.push(SessionSnapshot {
                instance: session.instance.clone(),
                process: session.process.clone(),
                volume: session.volume,
                muted: session.muted,
            });
        }
        AudioEvent::SessionVolumeChanged {
            instance,
            volume,
            muted,
            ..
        } => {
            if let Some(snapshot) = sessions.iter_mut().find(|s| &s.instance == instance) {
                snapshot.volume = *volume;
                snapshot.muted = *muted;
            }
        }
        AudioEvent::SessionDisconnected { instance, .. } => {
            sessions.retain(|s| &s.instance != instance);
        }
        _ => {}
    }
}

fn startup_endpoints_snapshot(startup: &CoreStartup) -> Arc<[EndpointView]> {
    startup.endpoints.clone().into()
}

fn read_commands(
    cmd_tx: Sender<CoreCommand>,
    sessions: Arc<Mutex<Vec<SessionSnapshot>>>,
    endpoints: Arc<[EndpointView]>,
) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("help") => print_command_help(),
            Some("list") => print_session_list(&sessions),
            Some("endpoints") => print_endpoint_list(&endpoints),
            Some("quit") => {
                let _ = cmd_tx.send(CoreCommand::Shutdown);
                break;
            }
            Some("vol") => {
                let rest: Vec<&str> = parts.collect();
                handle_vol_command(&cmd_tx, &sessions, &rest);
            }
            Some("switch") => {
                let rest: Vec<&str> = parts.collect();
                handle_switch_command(&cmd_tx, &endpoints, &rest);
            }
            _ => println!("unknown command, type 'help' for the command list"),
        }
    }
}

fn handle_vol_command(
    cmd_tx: &Sender<CoreCommand>,
    sessions: &Arc<Mutex<Vec<SessionSnapshot>>>,
    args: &[&str],
) {
    let [prefix, volume, mute_state] = args else {
        println!("usage: vol <instance-prefix> <0.0-1.0> mute|unmute");
        return;
    };

    let volume: f32 = match volume.parse() {
        Ok(volume) if (0.0..=1.0).contains(&volume) => volume,
        _ => {
            println!("error: volume must be a number between 0.0 and 1.0");
            return;
        }
    };

    let muted = match *mute_state {
        "mute" => true,
        "unmute" => false,
        _ => {
            println!("error: last argument must be 'mute' or 'unmute'");
            return;
        }
    };

    let instance = match resolve_instance(sessions, prefix) {
        Ok(instance) => instance,
        Err(message) => {
            println!("{message}");
            return;
        }
    };

    let _ = cmd_tx.send(CoreCommand::ApplySessionVolume {
        instance,
        volume,
        muted,
    });
    println!("sent ApplySessionVolume instance={prefix} volume={volume:.3} muted={muted}");
}

fn resolve_instance(
    sessions: &Arc<Mutex<Vec<SessionSnapshot>>>,
    prefix: &str,
) -> Result<SessionInstanceId, String> {
    let sessions = sessions.lock().unwrap();

    if let Some(exact) = sessions.iter().find(|s| s.instance.as_ref() == prefix) {
        return Ok(exact.instance.clone());
    }

    let matches: Vec<&SessionSnapshot> = sessions
        .iter()
        .filter(|s| s.instance.starts_with(prefix))
        .collect();

    match matches.as_slice() {
        [] => Err("error: unknown instance, try 'list'".to_string()),
        [only] => Ok(only.instance.clone()),
        _ => Err("error: ambiguous instance prefix, be more specific".to_string()),
    }
}

fn handle_switch_command(cmd_tx: &Sender<CoreCommand>, endpoints: &[EndpointView], args: &[&str]) {
    let [target] = args else {
        println!("usage: switch <index-or-id-prefix>");
        return;
    };

    let endpoint = match resolve_endpoint(endpoints, target) {
        Ok(endpoint) => endpoint,
        Err(message) => {
            println!("{message}");
            return;
        }
    };

    let _ = cmd_tx.send(CoreCommand::SetDefaultEndpoint {
        id: endpoint.id.clone(),
        roles: RoleSet::all(),
    });
    println!(
        "sent SetDefaultEndpoint id={} ({})",
        endpoint.id, endpoint.friendly_name
    );
}

fn resolve_endpoint<'a>(
    endpoints: &'a [EndpointView],
    target: &str,
) -> Result<&'a EndpointView, String> {
    if let Ok(index) = target.parse::<usize>() {
        if let Some(endpoint) = endpoints.get(index) {
            return Ok(endpoint);
        }
    }

    if let Some(exact) = endpoints.iter().find(|e| e.id.as_ref() == target) {
        return Ok(exact);
    }

    let matches: Vec<&EndpointView> = endpoints
        .iter()
        .filter(|e| e.id.starts_with(target))
        .collect();

    match matches.as_slice() {
        [] => Err("error: unknown endpoint index or id prefix, try 'endpoints'".to_string()),
        [only] => Ok(only),
        _ => Err("error: ambiguous endpoint id prefix, be more specific".to_string()),
    }
}

fn print_command_help() {
    println!("commands:");
    println!("  list                                             list known sessions");
    println!("  vol <instance-prefix> <0.0-1.0> mute|unmute       send ApplySessionVolume");
    println!("  endpoints                                         list known render endpoints");
    println!(
        "  switch <index-or-id-prefix>                       send SetDefaultEndpoint (all roles)"
    );
    println!(
        "  quit                                              send Shutdown and stop reading input"
    );
    println!("  help                                              show this message");
}

fn print_endpoint_list(endpoints: &[EndpointView]) {
    if endpoints.is_empty() {
        println!("no known endpoints");
        return;
    }
    println!("known render endpoints ({}):", endpoints.len());
    for (index, endpoint) in endpoints.iter().enumerate() {
        println!(
            "  [{index}] {} id={} state={:?}",
            endpoint.friendly_name, endpoint.id, endpoint.state
        );
    }
}

fn print_session_list(sessions: &Arc<Mutex<Vec<SessionSnapshot>>>) {
    let sessions = sessions.lock().unwrap();
    if sessions.is_empty() {
        println!("no known sessions yet");
        return;
    }
    println!("known sessions ({}):", sessions.len());
    for session in sessions.iter() {
        println!(
            "  instance={} process={} volume={:.3} muted={}",
            session.instance, session.process, session.volume, session.muted
        );
    }
}

fn print_startup(startup: &CoreStartup) {
    println!("active render endpoints ({}):", startup.endpoints.len());
    for (index, endpoint) in startup.endpoints.iter().enumerate() {
        println!("  [{index}] {}", endpoint.friendly_name);
        println!("       id    = {}", endpoint.id);
        println!("       state = {:?}", endpoint.state);
    }
    match &startup.default_render {
        Some(id) => println!("default render endpoint (console role): {id}"),
        None => println!("default render endpoint (console role): <none>"),
    }
}

fn describe_event(event: &AudioEvent) -> String {
    match event {
        AudioEvent::DefaultEndpointChanged { flow, role, id } => {
            let id = id.as_deref().unwrap_or("<none>");
            format!("DefaultEndpointChanged flow={flow:?} role={role:?} id={id}")
        }
        AudioEvent::EndpointStateChanged { id, state } => {
            format!("EndpointStateChanged   state={state:?} id={id}")
        }
        AudioEvent::EndpointAdded { id, friendly_name } => {
            format!("EndpointAdded          id={id} name={friendly_name}")
        }
        AudioEvent::EndpointRemoved(id) => format!("EndpointRemoved        id={id}"),
        AudioEvent::SessionCreated { endpoint, session } => format!(
            "SessionCreated         endpoint={endpoint} process={} instance={}",
            session.process, session.instance
        ),
        AudioEvent::SessionVolumeChanged {
            instance,
            volume,
            muted,
            own_change,
        } => format!(
            "SessionVolumeChanged   instance={instance} volume={volume:.3} muted={muted} own_change={own_change}"
        ),
        AudioEvent::SessionStateChanged { instance, state } => {
            format!("SessionStateChanged    instance={instance} state={state:?}")
        }
        AudioEvent::SessionDisconnected { instance, reason } => {
            format!("SessionDisconnected    instance={instance} reason={reason:?}")
        }
        AudioEvent::CoreError(err) => format!("CoreError              {}", describe_error(err)),
    }
}

fn describe_error(err: &CoreError) -> String {
    match err {
        CoreError::ComInitFailed(message) => format!("COM init failed: {message}"),
        CoreError::DeviceEnumerationFailed(message) => {
            format!("device enumeration failed: {message}")
        }
        CoreError::Other(message) => message.clone(),
    }
}
