//! Resonance binary.
//!
//! At this stage the only thing it can do is `--dump-events`: start the audio
//! core, print the active render endpoints, and then print every device and
//! session event the core reports until the process is interrupted with Ctrl+C.

use std::io::{self, BufRead};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{unbounded, Sender};
use resonance_core::core::{self, CoreStartup};
use resonance_core::messages::{
    AudioEvent, CoreCommand, CoreError, EndpointView, ProcessKey, RoleSet, SessionInstanceId,
};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
resonance-app

USAGE:
    resonance-app --dump-events

OPTIONS:
    --dump-events    Print the active render endpoints, then stream audio device
                     and session events to stdout until interrupted (Ctrl+C).
    -h, --help       Show this message.

ENVIRONMENT:
    RESONANCE_LOG    Log filter (default: info). Example: RESONANCE_LOG=trace
";

fn main() -> ExitCode {
    let mut dump_events = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--dump-events" => dump_events = true,
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

    if !dump_events {
        eprintln!("error: no mode selected\n");
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    }

    dump_events_mode()
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
        AudioEvent::EndpointAdded(id) => format!("EndpointAdded          id={id}"),
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
