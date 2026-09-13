//! "Start with Windows" support: one value under the per-user `Run` key.
//!
//! The setting itself lives in the profile store like every other setting and
//! is reduced by the state manager; this module owns only the side effect that
//! state cannot perform — writing the registry. That split is deliberate:
//! `resonance-state` stays platform-independent and testable off Windows, and
//! the binary that ties the threads together is the one place that touches
//! machine-wide configuration.
//!
//! `HKCU\...\Run` is used rather than `HKLM`, a scheduled task, or a Startup
//! folder shortcut: it needs no elevation, is per-user (matching the profile
//! store, which is per-user too), and is the location Task Manager's Startup
//! tab shows, so a user who disables the entry there is disabling something
//! they can see. That last point is why [`apply`] is also called at startup to
//! reconcile: Windows itself offers the user a way to remove the entry behind
//! the application's back.
//!
//! Nothing here is load-bearing. Every failure is reported to the caller to be
//! logged and then ignored — an application that refused to start because it
//! could not write an autostart entry would be trading a working program for a
//! convenience.

use std::fmt;
use std::io;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ,
};

/// The per-user autostart key, relative to `HKEY_CURRENT_USER`.
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Name of the value this application owns under [`RUN_KEY_PATH`]. Changing it
/// would orphan the entry written by an older build, which would then keep
/// launching the application forever with nothing able to remove it.
const VALUE_NAME: &str = "Resonance";

/// The mode the autostart entry launches. Explicitly *not* `--run` or
/// `--dump-events`: those are debugging modes that hold a console and print to
/// stdout, and a sign-in that silently started one of them would look like a
/// hang. `--overlay` is the real application.
const LAUNCH_MODE_ARG: &str = "--overlay";

/// Why an autostart change could not be applied.
///
/// Every variant is something a caller logs and carries on from; none of them
/// justify failing a startup or dropping a user's setting.
#[derive(Debug)]
pub enum AutostartError {
    /// The path of the running executable could not be determined, so there is
    /// nothing meaningful to write.
    ExecutablePath(io::Error),
    /// A registry call failed. `operation` names the call so a log line points
    /// at the failing step rather than just at this module.
    Registry {
        operation: &'static str,
        code: WIN32_ERROR,
    },
}

impl fmt::Display for AutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AutostartError::ExecutablePath(err) => {
                write!(f, "could not determine the executable path: {err}")
            }
            AutostartError::Registry { operation, code } => {
                write!(f, "{operation} failed with Win32 error {}", code.0)
            }
        }
    }
}

impl std::error::Error for AutostartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AutostartError::ExecutablePath(err) => Some(err),
            AutostartError::Registry { .. } => None,
        }
    }
}

/// Makes the registry match `enabled`: writes the autostart entry when true,
/// removes it when false.
///
/// Idempotent in both directions. Enabling twice rewrites the same value, and
/// disabling when no entry exists succeeds without doing anything — "the entry
/// is absent" is the requested state, and whether this call is what made it
/// absent is not information a caller can use.
pub fn apply(enabled: bool) -> Result<(), AutostartError> {
    if enabled {
        write_value(VALUE_NAME, &launch_command()?)
    } else {
        remove_value(VALUE_NAME)
    }
}

/// The command line the autostart entry runs: the current executable's path,
/// quoted, followed by the mode argument.
///
/// The quoting is not cosmetic. An unquoted path containing a space is parsed
/// by `CreateProcess` as a shorter path plus arguments, so an installation
/// under `C:\Program Files\...` would either launch the wrong file or fail;
/// quoting makes the path one token whatever it contains.
fn launch_command() -> Result<String, AutostartError> {
    let exe = std::env::current_exe().map_err(AutostartError::ExecutablePath)?;
    Ok(format_command(&exe))
}

/// Split out of [`launch_command`] so the formatting can be tested against
/// paths this machine does not have, notably one containing spaces.
fn format_command(exe: &Path) -> String {
    format!("\"{}\" {LAUNCH_MODE_ARG}", exe.display())
}

/// An open registry key that closes itself.
///
/// `HKEY` is a plain `Copy` newtype around a pointer with no destructor, so
/// nothing releases it implicitly; every early return below would leak the key
/// without this wrapper.
struct RunKey(HKEY);

impl Drop for RunKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `RegCreateKeyExW` or
        // `RegOpenKeyExW` in this module. This type is not `Clone`, never hands
        // the raw key out, and is the only thing that closes it, so this call
        // sees a live key exactly once. Registry calls carry no COM apartment
        // or thread-affinity requirement; no COM pointer is involved.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Opens the `Run` key, creating it if it does not exist.
fn create_run_key() -> Result<RunKey, AutostartError> {
    let path = wide_nul(RUN_KEY_PATH);
    let mut key = HKEY(std::ptr::null_mut());

    // SAFETY: `path` is a NUL-terminated UTF-16 buffer owned by this function
    // and outlives the call, so the `PCWSTR` is valid throughout. `&mut key`
    // is a live, writable `HKEY` slot. `None` for the class and the security
    // attributes requests the defaults, and the disposition output (whether the
    // key was created or opened) is not needed: either outcome is equally
    // acceptable here.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };

    if status.is_err() {
        return Err(AutostartError::Registry {
            operation: "RegCreateKeyExW",
            code: status,
        });
    }
    Ok(RunKey(key))
}

/// Opens the `Run` key without creating it.
///
/// Returns `Ok(None)` when the key does not exist, which is not an error on
/// the removal path: no key means no value to remove.
fn open_run_key(access: REG_SAM_FLAGS) -> Result<Option<RunKey>, AutostartError> {
    let path = wide_nul(RUN_KEY_PATH);
    let mut key = HKEY(std::ptr::null_mut());

    // SAFETY: as in `create_run_key` — `path` is a NUL-terminated UTF-16 buffer
    // that outlives the call, and `&mut key` is a live `HKEY` slot. `None` for
    // the options parameter is the documented "reserved, must be zero" value.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            access,
            &mut key,
        )
    };

    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status.is_err() {
        return Err(AutostartError::Registry {
            operation: "RegOpenKeyExW",
            code: status,
        });
    }
    Ok(Some(RunKey(key)))
}

/// Writes one `REG_SZ` value under the `Run` key.
///
/// The value name is a parameter rather than the constant so a test can write
/// a value of its own without touching the one the running application owns.
fn write_value(value_name: &str, command: &str) -> Result<(), AutostartError> {
    let key = create_run_key()?;
    let name = wide_nul(value_name);
    let data = reg_sz_bytes(command);

    // SAFETY: `name` and `data` are owned by this function and outlive the
    // call. `data` is the value's full byte representation including its
    // terminating NUL, which is what `REG_SZ` requires — the length is taken
    // from the slice, so it cannot disagree with the pointer. `key` is live:
    // the `RunKey` guard is still in scope and drops only after this returns.
    let status = unsafe {
        RegSetValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            REG_SZ,
            Some(data.as_slice()),
        )
    };

    if status.is_err() {
        return Err(AutostartError::Registry {
            operation: "RegSetValueExW",
            code: status,
        });
    }
    Ok(())
}

/// Removes one value from the `Run` key, treating "it was not there" as
/// success. See [`apply`] for why that is the right reading.
fn remove_value(value_name: &str) -> Result<(), AutostartError> {
    let Some(key) = open_run_key(KEY_SET_VALUE)? else {
        return Ok(());
    };
    let name = wide_nul(value_name);

    // SAFETY: `name` is a NUL-terminated UTF-16 buffer owned by this function
    // and outlives the call; `key` is live for the same reason as in
    // `write_value`.
    let status = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };

    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if status.is_err() {
        return Err(AutostartError::Registry {
            operation: "RegDeleteValueW",
            code: status,
        });
    }
    Ok(())
}

/// A string as the bytes of a NUL-terminated UTF-16 `REG_SZ` value.
///
/// Built byte by byte in little-endian order rather than by reinterpreting a
/// `Vec<u16>` buffer: that keeps the conversion free of `unsafe` and of any
/// alignment assumption, at a cost that is irrelevant for a path-length string
/// written at most once per launch.
fn reg_sz_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((text.len() + 1) * 2);
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Copy a string into a NUL-terminated UTF-16 buffer for a `PCWSTR` parameter.
fn wide_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{RegQueryValueExW, KEY_QUERY_VALUE, REG_VALUE_TYPE};

    /// Reads a `REG_SZ` value back from the `Run` key, or `None` if it is
    /// absent. Test-only: the application never needs to read the entry, it
    /// only asserts a desired state onto it.
    fn read_value(value_name: &str) -> Option<String> {
        let key = open_run_key(KEY_QUERY_VALUE).expect("opening the Run key for reading")?;
        let name = wide_nul(value_name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut size: u32 = 0;

        // SAFETY: `name` is a NUL-terminated UTF-16 buffer owned by this
        // function and outlives the call. Passing a null data pointer with a
        // live size slot is the documented way to ask for the required buffer
        // size without writing any data. `key` is live: its `RunKey` guard is
        // in scope. Registry calls have no thread or apartment requirement.
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return None;
        }
        assert!(status.is_ok(), "sizing query failed: {}", status.0);
        assert_eq!(kind, REG_SZ, "test value should be a REG_SZ");

        let mut buffer = vec![0u8; size as usize];

        // SAFETY: same key and name as the sizing call above, both still live.
        // `buffer` is a live allocation of exactly `size` bytes and `size` is
        // passed unchanged alongside it, so the pointer and the length agree;
        // the call writes at most that many bytes and updates `size` with how
        // many it actually wrote.
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                None,
                Some(buffer.as_mut_ptr()),
                Some(&mut size),
            )
        };
        assert!(status.is_ok(), "read query failed: {}", status.0);

        let units: Vec<u16> = buffer[..size as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let units = units.strip_suffix(&[0]).unwrap_or(&units);
        Some(String::from_utf16_lossy(units))
    }

    #[test]
    fn the_command_quotes_the_path_and_selects_the_overlay_mode() {
        let command = format_command(Path::new(r"C:\Program Files\Resonance\resonance-app.exe"));
        assert_eq!(
            command,
            r#""C:\Program Files\Resonance\resonance-app.exe" --overlay"#
        );
    }

    #[test]
    fn reg_sz_bytes_is_utf16_with_a_terminator() {
        // "Hi" plus the NUL terminator: three UTF-16 units, six bytes.
        assert_eq!(reg_sz_bytes("Hi"), vec![b'H', 0, b'i', 0, 0, 0]);
    }

    #[test]
    fn wide_nul_terminates_and_preserves_the_text() {
        let wide = wide_nul(RUN_KEY_PATH);
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(
            String::from_utf16_lossy(&wide[..wide.len() - 1]),
            RUN_KEY_PATH
        );
    }

    /// Exercises the real registry, exactly like the single-instance tests
    /// exercise the real kernel object namespace: the value of these functions
    /// is entirely in what Windows does with them, so a mocked registry would
    /// test the mock. The value name is deliberately *not* `Resonance` — it is
    /// distinct and obviously temporary, so this test can never disturb the
    /// entry a real installation owns, even when run on the developer's own
    /// machine while the application is installed.
    #[test]
    fn a_value_round_trips_through_the_real_run_key_and_is_removed_again() {
        let value_name = "Resonance.Test.RoundTrip";
        let command = r#""C:\some path\resonance-app.exe" --overlay"#;

        // Left over from an aborted earlier run, if any.
        remove_value(value_name).expect("pre-test cleanup");
        assert_eq!(read_value(value_name), None, "should start absent");

        write_value(value_name, command).expect("writing the test value");
        assert_eq!(read_value(value_name).as_deref(), Some(command));

        // Enabling twice must not fail or duplicate anything.
        write_value(value_name, command).expect("rewriting the test value");
        assert_eq!(read_value(value_name).as_deref(), Some(command));

        remove_value(value_name).expect("removing the test value");
        assert_eq!(read_value(value_name), None, "should end absent");

        // Removing something already absent is success, not an error.
        remove_value(value_name).expect("removing an absent value");
    }
}
