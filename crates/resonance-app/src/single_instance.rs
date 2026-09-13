//! Single-instance guard built on a named Win32 mutex.
//!
//! Two copies of the running application would fight over the same two
//! process-wide resources: the global keyboard shortcut (only one process can
//! hold a given hotkey registration) and the on-disk profile store (two
//! independent write-behind persistence threads would overwrite each other's
//! last write). A named mutex is the cheapest way to detect that: the kernel
//! object namespace is the shared state, so no file, socket or window handle
//! has to be invented for it.
//!
//! The name lives in the `Local\` namespace rather than `Global\`: this is a
//! per-desktop-session application, and two different users logged into the
//! same machine each get their own profile store and their own hotkey, so they
//! are not a conflict and must not block each other. `Global\` would make the
//! second user's launch silently exit.

use tracing::warn;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// Kernel object name. Changing this string is a compatibility break: a build
/// with a different name will not detect an already-running older build.
const MUTEX_NAME: &str = r"Local\Resonance.SingleInstance";

/// Outcome of trying to claim the single-instance name.
pub enum Instance {
    /// This process claimed the name. The guard must be kept alive for as
    /// long as the process should hold it.
    Only(InstanceGuard),
    /// Another process already holds the name; this process should exit.
    AlreadyRunning,
}

/// Owns the mutex handle for the lifetime of the process.
///
/// The handle is what holds the name, not the `InstanceGuard` value itself:
/// `HANDLE` is a plain `Copy` newtype with no destructor of its own, so
/// nothing releases the name implicitly. This wrapper exists to make the
/// intended lifetime explicit and to close the handle on the orderly exit
/// path. On an abrupt exit the kernel closes it instead, which is equally
/// correct — the name is owned by the handle, and the handle dies with the
/// process.
pub struct InstanceGuard {
    /// `None` when the mutex could not be created at all and this process
    /// decided to run anyway; there is then nothing to close.
    handle: Option<HANDLE>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let Some(handle) = self.handle else {
            return;
        };
        // SAFETY: `handle` was returned by a successful `CreateMutexW` in
        // `acquire` and has not been closed since — this type owns it, is not
        // `Clone`, and hands the handle out to nobody, so this is the only
        // `CloseHandle` call that can ever see it. Closing a mutex handle is
        // valid from any thread regardless of COM apartment; no COM pointer is
        // involved here.
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}

/// Claims the single-instance name, or reports that another process holds it.
///
/// If the mutex cannot be created at all (an unexpected kernel failure, not
/// the "already exists" case), this logs a warning and reports `Only` anyway:
/// refusing to start a working application because a diagnostic aid failed
/// would be the worse outcome of the two.
pub fn acquire() -> Instance {
    acquire_named(MUTEX_NAME)
}

/// The body of [`acquire`], with the kernel object name as a parameter so a
/// test can exercise the real logic against a name of its own instead of the
/// production one (claiming the production name in a test would either
/// interfere with a running instance or fail because of one).
fn acquire_named(name: &str) -> Instance {
    let name = wide_nul(name);

    // `CreateMutexW` reports "the name already existed" only through the
    // thread's last-error value; it still returns a valid, usable handle in
    // that case, so the return value alone cannot distinguish the two. The
    // last-error read therefore has to happen before anything else can
    // overwrite it, which is why both calls sit in one block with no logging,
    // allocation or other call in between.
    //
    // SAFETY: both calls are plain kernel32 entry points with no thread or
    // apartment requirements. `name` is a NUL-terminated UTF-16 buffer owned
    // by this function that outlives the call, so the `PCWSTR` is valid for
    // the whole of it. A null attribute pointer (`None`) requests the default
    // security descriptor, and `false` means the caller does not take
    // ownership of the mutex — only its existence is of interest here, it is
    // never waited on or released.
    let (created, last_error) = unsafe {
        let created = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        (created, GetLastError())
    };

    match created {
        Ok(handle) if last_error == ERROR_ALREADY_EXISTS => {
            // Another process owns the name. Our own handle to the same mutex
            // is useless, and leaving it open would keep the name alive past
            // the real owner's exit, so drop it right here.
            //
            // SAFETY: `handle` was just returned by a successful
            // `CreateMutexW` and has not been stored anywhere or closed; this
            // is its only use.
            unsafe {
                let _ = CloseHandle(handle);
            }
            Instance::AlreadyRunning
        }
        Ok(handle) => Instance::Only(InstanceGuard {
            handle: Some(handle),
        }),
        Err(err) => {
            warn!(
                error = %err,
                "could not create the single-instance mutex, starting without the guard"
            );
            Instance::Only(InstanceGuard { handle: None })
        }
    }
}

/// Copy a string into a NUL-terminated UTF-16 buffer suitable for a `PCWSTR`
/// parameter.
fn wide_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_nul_terminates_and_preserves_the_name() {
        let wide = wide_nul(MUTEX_NAME);
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(
            String::from_utf16_lossy(&wide[..wide.len() - 1]),
            MUTEX_NAME
        );
    }

    #[test]
    fn a_second_acquire_of_the_same_name_reports_already_running() {
        let name = r"Local\Resonance.SingleInstance.Test.SecondAcquire";

        // The first guard is held for the whole test, so the second call sees
        // the name taken.
        let first = acquire_named(name);
        assert!(matches!(first, Instance::Only(_)));

        assert!(matches!(acquire_named(name), Instance::AlreadyRunning));

        drop(first);
    }

    #[test]
    fn the_name_is_released_once_the_guard_is_dropped() {
        let name = r"Local\Resonance.SingleInstance.Test.Release";

        drop(acquire_named(name));

        // Only true if both the guard's `Drop` and the "already running"
        // branch close the handles they own: a single leaked handle would keep
        // the name alive and make this second claim report `AlreadyRunning`.
        assert!(matches!(acquire_named(name), Instance::Only(_)));
    }
}
