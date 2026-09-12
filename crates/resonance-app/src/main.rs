//! Resonance binary.
//!
//! At this stage the only thing it can do is `--dump-events`: start the audio
//! core, print the active render endpoints, and then print every device event
//! the core reports until the process is interrupted with Ctrl+C.

use std::process::ExitCode;

use crossbeam_channel::unbounded;
use resonance_core::core::{self, CoreStartup};
use resonance_core::messages::{AudioEvent, CoreCommand, CoreError};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
resonance-app

USAGE:
    resonance-app --dump-events

OPTIONS:
    --dump-events    Print the active render endpoints, then stream audio device
                     events to stdout until interrupted (Ctrl+C).
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

fn dump_events_mode() -> ExitCode {
    // The command sender is kept alive for the whole run: dropping it would
    // close the channel and stop the core thread.
    let (_cmd_tx, cmd_rx) = unbounded::<CoreCommand>();
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
    println!("listening for device events (Ctrl+C to exit)");
    println!("try: plug/unplug a device, or change the default output in Windows sound settings");
    println!();

    for event in ev_rx.iter() {
        println!("{}", describe_event(&event));
    }

    // Reached only if the core thread dropped its event sender.
    core.join();
    ExitCode::SUCCESS
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
