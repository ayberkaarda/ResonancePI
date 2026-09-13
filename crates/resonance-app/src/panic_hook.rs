//! Panic hook: log the panic, then make a bounded, best-effort attempt to get
//! the pending profile write onto disk before the process dies.
//!
//! The release profile aborts on panic, which means a panic on *any* thread —
//! the audio core, the reducer, the UI event loop — ends the whole process as
//! soon as the hook returns. There is no unwinding, so no `Drop`, no
//! `JoinHandle`, and no ordinary shutdown path runs afterwards. This hook is
//! the only code that gets to do anything at all, and whatever it does has to
//! be finished by the time it returns.
//!
//! # What this actually guarantees
//!
//! Best-effort, and no more than that:
//!
//! - It cannot work before the backend exists. A panic during startup is
//!   logged and nothing is flushed, because there is nothing to flush yet.
//! - It cannot work when the panicking thread *is* the reducer thread. The
//!   flush is performed by the reducer, so asking it to flush while it is
//!   unwinding into an abort is a request nobody will ever answer; the wait
//!   below then just expires.
//! - Even on the good path it waits a bounded time for an acknowledgement
//!   rather than joining the persistence thread, so a write that is slower
//!   than the deadline is abandoned.
//!
//! That is acceptable rather than alarming: the profile store is written by
//! an atomic replace that leaves the previous good file in place until the new
//! one is complete, and a backup copy is kept, so a write that never starts or
//! never finishes costs at most the most recent unsaved change — it cannot
//! leave a half-written store behind.

use std::panic::PanicHookInfo;
use std::sync::OnceLock;
use std::time::Duration;

use tracing::error;

use crate::backend::EmergencyFlush;

/// How long the hook waits for the reducer to confirm the flush.
///
/// Long enough for a small JSON file to be written and confirmed, short
/// enough that it is not noticeable as a hang when the acknowledgement is
/// never coming (which is the expected case for a panic on the reducer thread
/// itself).
const FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// Set once, after the backend starts. `OnceLock` rather than a mutex on
/// purpose: the hook may run on a thread that is already panicking, and a
/// lock could be held by the very thread that panicked while holding it.
/// Reading a `OnceLock` cannot deadlock.
static EMERGENCY_FLUSH: OnceLock<EmergencyFlush> = OnceLock::new();

/// Installs the panic hook. Call once, after logging is initialised (so the
/// hook's own log line has somewhere to go) and before anything that can
/// panic is started.
///
/// The previously installed hook is kept and called last, so the standard
/// panic message and any backtrace still reach stderr as usual.
pub fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_panic(info);
        flush_state();
        previous(info);
    }));
}

/// Publishes the flush handle to the hook. Called by `main` right after the
/// backend starts; a panic before this point is logged but flushes nothing,
/// there being no state to flush yet.
pub fn set_emergency_flush(flush: EmergencyFlush) {
    // A second call would mean two backends in one process, which does not
    // happen; ignoring it is still better than panicking inside the code whose
    // job is to handle panics.
    let _ = EMERGENCY_FLUSH.set(flush);
}

fn log_panic(info: &PanicHookInfo<'_>) {
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown location>".to_owned());

    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>").to_owned();

    error!(
        thread = %thread,
        location = %location,
        message = %panic_message(info),
        "panic, the process will terminate"
    );
}

/// The panic payload as text.
///
/// `panic!` with a formatted message produces a `String` payload and a
/// `panic!` with a literal produces a `&str`; anything else (a payload from
/// `panic_any`) is not text at all and is reported as such rather than
/// guessed at.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

fn flush_state() {
    let Some(flush) = EMERGENCY_FLUSH.get() else {
        error!("no backend running, nothing to flush");
        return;
    };

    if flush.request(FLUSH_TIMEOUT) {
        error!("pending profile write flushed before exit");
    } else {
        error!(
            timeout_ms = FLUSH_TIMEOUT.as_millis() as u64,
            "flush not confirmed before exit, the most recent change may be lost"
        );
    }
}
