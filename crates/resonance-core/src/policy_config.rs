//! Default endpoint switching through the undocumented `IPolicyConfig`.
//!
//! Windows exposes no supported API for changing the default audio endpoint
//! programmatically. Every tool in this product class drives the same private
//! coclass, `PolicyConfigClient`, and its `IPolicyConfig` interface. Because the
//! interface is undocumented it has no published header: the vtable layout below
//! is reconstructed from independent public reimplementations that have been
//! shipping against it for years, and it is the layout — not the parameter
//! types — that is load-bearing. A missing or extra slot before
//! `SetDefaultEndpoint` does not fail to compile; it silently dispatches the
//! call to a neighbouring function.
//!
//! Because this is unsupported API that a future Windows release may change or
//! remove, everything here sits behind the `switching` cargo feature and the
//! caller degrades to "profiles only, no switching" when the coclass cannot be
//! created.
//!
//! Every type in this module holds a COM interface pointer, so none of them are
//! `Send`. That is the compiler-enforced half of the rule that COM pointers live
//! only on the audio core thread: a `PolicyConfig` cannot be moved to the UI or
//! state threads even by accident.

// The interface methods keep the Windows spelling of their names, which is what
// makes them checkable against the references the layout was taken from. The
// `#[interface]` macro rejects any non-doc attribute on the trait it expands, so
// the allowance has to sit at module scope rather than on the declaration.
#![allow(non_snake_case)]

use std::ffi::c_void;

use tracing::{debug, warn};
use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia, ERole};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
// `IUnknown_Vtbl` is not named anywhere below, but the `#[interface]` macro
// refers to the parent interface's vtable type unqualified, so it has to be in
// scope here.
use windows_core::{
    interface, Error as ComError, IUnknown, IUnknown_Vtbl, Result as ComResult, GUID, HRESULT,
    PCWSTR,
};

use crate::messages::{EndpointId, RoleSet};

/// CLSID of the `PolicyConfigClient` coclass that implements `IPolicyConfig`.
const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// The undocumented interface used to change the default audio endpoint.
///
/// Slot order, cross-checked against three independent public implementations
/// that agree exactly (EarTrumpet's `IPolicyConfig.cs`, the `audioswitch`
/// `IPolicyConfig.h` header, and `AudioEndPointLibrary`'s `PolicyConfig.h`):
///
/// | Slot | Method |
/// |------|--------|
/// | 0-2  | `IUnknown` (`QueryInterface`, `AddRef`, `Release`) |
/// | 3    | `GetMixFormat` |
/// | 4    | `GetDeviceFormat` |
/// | 5    | `ResetDeviceFormat` |
/// | 6    | `SetDeviceFormat` |
/// | 7    | `GetProcessingPeriod` |
/// | 8    | `SetProcessingPeriod` |
/// | 9    | `GetShareMode` |
/// | 10   | `SetShareMode` |
/// | 11   | `GetPropertyValue` |
/// | 12   | `SetPropertyValue` |
/// | 13   | `SetDefaultEndpoint` |
/// | 14   | `SetEndpointVisibility` |
///
/// Ten interface methods therefore precede `SetDefaultEndpoint`. One widely
/// copied C# binding (`coreaudio-dotnet`) omits `ResetDeviceFormat` and so
/// places `SetDefaultEndpoint` one slot too early; it is a known outlier and is
/// not followed here.
///
/// Only `SetDefaultEndpoint` is ever called. The preceding methods exist purely
/// to occupy their slots, so their parameter lists are documentation rather than
/// contract: they carry the arity and rough shape reported by the C++ headers,
/// with the structure pointers left opaque because a slot that is never invoked
/// cannot misuse them.
///
/// The methods are `unsafe` because calling any of them dispatches through a
/// vtable whose layout is asserted by this declaration rather than verified by
/// the compiler.
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
// SAFETY: declaring this trait asserts a vtable layout that no compiler can
// check. The assertion is the slot table above: three `IUnknown` slots followed
// by the twelve methods in exactly this order, which is the layout three
// independent implementations agree on. Every method dispatches through that
// vtable, so the declaration is sound only while the order is right; the object
// behind it is created by `PolicyConfig::new` on the audio core thread, is not
// `Send`, and is released on that same thread.
pub unsafe trait IPolicyConfig: IUnknown {
    /// Slot 3. Never called; present to position the slots that follow.
    fn GetMixFormat(&self, device_id: PCWSTR, format: *mut *mut c_void) -> HRESULT;
    /// Slot 4. Never called.
    fn GetDeviceFormat(&self, device_id: PCWSTR, default: i32, format: *mut *mut c_void)
        -> HRESULT;
    /// Slot 5. Never called. Absent from some third-party bindings; omitting it
    /// would shift every following slot by one.
    fn ResetDeviceFormat(&self, device_id: PCWSTR) -> HRESULT;
    /// Slot 6. Never called.
    fn SetDeviceFormat(
        &self,
        device_id: PCWSTR,
        endpoint_format: *mut c_void,
        mix_format: *mut c_void,
    ) -> HRESULT;
    /// Slot 7. Never called.
    fn GetProcessingPeriod(
        &self,
        device_id: PCWSTR,
        default: i32,
        default_period: *mut i64,
        min_period: *mut i64,
    ) -> HRESULT;
    /// Slot 8. Never called.
    fn SetProcessingPeriod(&self, device_id: PCWSTR, period: *mut i64) -> HRESULT;
    /// Slot 9. Never called.
    fn GetShareMode(&self, device_id: PCWSTR, share_mode: *mut c_void) -> HRESULT;
    /// Slot 10. Never called.
    fn SetShareMode(&self, device_id: PCWSTR, share_mode: *mut c_void) -> HRESULT;
    /// Slot 11. Never called.
    fn GetPropertyValue(
        &self,
        device_id: PCWSTR,
        key: *const c_void,
        value: *mut c_void,
    ) -> HRESULT;
    /// Slot 12. Never called.
    fn SetPropertyValue(
        &self,
        device_id: PCWSTR,
        key: *const c_void,
        value: *mut c_void,
    ) -> HRESULT;
    /// Slot 13. The only method Resonance calls: make `device_id` the default
    /// endpoint for one role.
    ///
    /// Declared with a `Result` return so the generated wrapper converts the
    /// underlying `HRESULT` for us; the ABI is still a plain `HRESULT`.
    fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> windows_core::Result<()>;
    /// Slot 14. Never called; declared so the interface ends where the
    /// references say it ends.
    fn SetEndpointVisibility(&self, device_id: PCWSTR, visible: i32) -> HRESULT;
}

/// The slot count is the one part of the layout a machine can check.
///
/// The generated vtable is `#[repr(C)]` and starts with the three `IUnknown`
/// slots, so a correct declaration is exactly fifteen function pointers wide. If
/// someone adds or removes a method above — the precise mistake that makes
/// `SetDefaultEndpoint` dispatch to its neighbour at runtime — this stops the
/// build instead of letting it through. It cannot detect methods that are
/// present but in the wrong order; only the references cross-checked above
/// establish that.
const _: () = {
    let expected = 15 * size_of::<*const c_void>();
    assert!(
        size_of::<IPolicyConfig_Vtbl>() == expected,
        "IPolicyConfig vtable must hold IUnknown's 3 slots plus the interface's 12"
    );
};

/// Owner of the `IPolicyConfig` instance used to switch the default endpoint.
///
/// Created on, used on and dropped on the audio core thread. It holds a COM
/// interface pointer and is therefore not `Send`, which is what stops it from
/// reaching any other thread.
pub struct PolicyConfig {
    policy: IPolicyConfig,
}

impl PolicyConfig {
    /// Create the `PolicyConfigClient` instance.
    ///
    /// This doubles as the startup smoke check: the call fails if the coclass is
    /// not registered or no longer answers to this IID, which is how a Windows
    /// release that withdraws the interface announces itself. A success proves
    /// the coclass exists and supports the IID; it cannot prove that the vtable
    /// slots are still in the order declared above.
    ///
    /// Must be called on the audio core thread, after COM has been initialised
    /// there.
    pub fn new() -> ComResult<Self> {
        // SAFETY: runs on the audio core thread, which has already entered the
        // MTA. `POLICY_CONFIG_CLIENT` is a `'static` constant, so the pointer is
        // valid for the whole call, and `None` requests no aggregation. The
        // returned interface is owned by this struct, which is not `Send`, so
        // both its use and its release stay on this thread.
        let policy: IPolicyConfig =
            unsafe { CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }?;
        Ok(Self { policy })
    }

    /// Make `id` the default render endpoint for every role selected in
    /// `roles`.
    ///
    /// Windows models the three roles independently, so this is up to three
    /// calls. Each is attempted even if an earlier one failed — a partial switch
    /// is still better than none, and stopping early would leave the roles
    /// inconsistent with no way for the caller to tell how far it got. The first
    /// error is returned once all selected roles have been attempted.
    pub fn set_default(&self, id: &EndpointId, roles: RoleSet) -> ComResult<()> {
        let selected = selected_roles(roles);
        if selected.is_empty() {
            debug!(%id, "SetDefaultEndpoint with an empty role set, nothing to do");
            return Ok(());
        }

        let device_id = wide_nul(id);
        let mut first_error: Option<ComError> = None;

        for (role, role_name) in selected {
            // SAFETY: runs on the audio core thread that owns `self.policy`.
            // `device_id` is a NUL-terminated UTF-16 buffer owned by this
            // function that outlives the call, and `ERole` is a transparent
            // wrapper over the `i32` the interface expects. This is the one
            // undocumented vtable slot Resonance dispatches through; its
            // position is asserted by the declaration above.
            match unsafe {
                self.policy
                    .SetDefaultEndpoint(PCWSTR(device_id.as_ptr()), role)
            } {
                Ok(()) => debug!(%id, role = role_name, "default endpoint set"),
                Err(e) => {
                    warn!(%id, role = role_name, error = %e, "SetDefaultEndpoint failed for this role");
                    first_error.get_or_insert(e);
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// The `ERole` values a `RoleSet` selects, paired with a name for logging.
fn selected_roles(roles: RoleSet) -> Vec<(ERole, &'static str)> {
    [
        (roles.console, eConsole, "console"),
        (roles.multimedia, eMultimedia, "multimedia"),
        (roles.communications, eCommunications, "communications"),
    ]
    .into_iter()
    .filter_map(|(selected, role, name)| selected.then_some((role, name)))
    .collect()
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
    fn role_set_maps_to_the_matching_com_roles() {
        assert_eq!(selected_roles(RoleSet::default()), Vec::new());

        let all = selected_roles(RoleSet::all());
        assert_eq!(
            all,
            vec![
                (eConsole, "console"),
                (eMultimedia, "multimedia"),
                (eCommunications, "communications"),
            ]
        );

        let only_communications = RoleSet {
            console: false,
            multimedia: false,
            communications: true,
        };
        assert_eq!(
            selected_roles(only_communications),
            vec![(eCommunications, "communications")],
            "a role that was not selected must not be switched"
        );
    }

    #[test]
    fn device_ids_are_terminated_for_the_com_call() {
        let id = "{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}";
        let wide = wide_nul(id);
        assert_eq!(wide.len(), id.len() + 1);
        assert_eq!(wide.last(), Some(&0), "the buffer must be NUL-terminated");
        assert_eq!(String::from_utf16(&wide[..wide.len() - 1]).unwrap(), id);
    }
}
