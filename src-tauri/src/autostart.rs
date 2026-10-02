//! Launch at logon: the one current-user `Run` registration this executable
//! owns, read and toggled from the settings window.
//!
//! Windows documents the `Run` key as the place a per-user logon program is
//! registered: the value name is the program's name and the value data is the
//! command line, which the page that documents the key limits to 260
//! characters. That is the whole documented surface, and this module stays
//! inside it:
//!
//! * the value name is exactly the product name the NSIS installer writes, so
//!   the installer's opt-in checkbox and this module's toggle address the same
//!   value;
//! * the command line is this executable's own path, quoted, and a value name
//!   that holds anything else is left untouched and reported unavailable;
//! * nothing is repaired behind the user's back: a value Windows disabled or a
//!   user removed stays that way until the user toggles it in the app.
//!
//! Explorer records disabled entries in an undocumented 12-byte binary value
//! under `...\Explorer\StartupApproved\Run`. The known disabled marker (first
//! byte `0x03`) reads as off; known enabled markers (`0x02`, `0x06`) read as on.
//! An explicit toggle may delete only this executable's recognized marker.
//! Unknown lengths, markers, types and foreign `Run` commands are unavailable
//! and left untouched rather than guessed at.
//! The `Run` key itself is created only by an explicit enable that needs to
//! write a value; reads and disabling open what exists and never create it.
//!
//! The registry is reached through the [`RunRegistry`] seam, so the whole rule
//! set is exercised against an in-memory registry in tests and never against
//! the test runner's own `Run` key.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_BINARY,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

use crate::profile;

/// The current-user logon-program key, exactly as the Run documentation names
/// it.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Explorer's record of which `Run` entries the user disabled or enabled in
/// Task Manager. The subkey is not part of the documented Run contract, so it
/// is read for one fact only and never written wholesale.
const STARTUP_APPROVED_RUN_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// The documented limit for a `Run` command line: at most 260 characters.
const RUN_COMMAND_MAX: usize = 260;

/// The first byte of Explorer's `StartupApproved` binary that means the entry
/// is disabled.
const STARTUP_DISABLED_BYTE: u8 = 0x03;
/// The first bytes Explorer writes for an entry that is (still) enabled. They
/// mean "not disabled"; they are recognized so an explicit off may clear this
/// program's own state, and are otherwise left alone.
const STARTUP_ENABLED_BYTE: u8 = 0x02;
const STARTUP_ENABLED_BYTE_ALT: u8 = 0x06;

/// What reading this executable's registration found, as the window reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutostartState {
    /// Our own `Run` value points at this executable and Windows has not
    /// disabled it.
    Enabled,
    /// There is no value of ours, or Windows disabled the one there is.
    Disabled,
    /// The registration cannot be read, or the name belongs to another program;
    /// nothing is written in either case.
    Unavailable,
}

impl AutostartState {
    /// The triple the settings view carries: `true`, `false`, or `null` when
    /// the state cannot be read or is not ours.
    pub(crate) fn as_option(self) -> Option<bool> {
        match self {
            AutostartState::Enabled => Some(true),
            AutostartState::Disabled => Some(false),
            AutostartState::Unavailable => None,
        }
    }
}

/// Why a toggle could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutostartError {
    /// The name is taken by another program, or the registry could not be read
    /// or trusted: nothing was changed.
    Unavailable,
    /// The registry refused a write, or the result could not be read back after
    /// an attempted rollback.
    Failed,
}

/// Whether this executable is registered to start at logon for the current
/// user, as the registry reports it right now.
pub(crate) fn state() -> Option<bool> {
    let command = match current_command() {
        Ok(command) => command,
        Err(()) => return None,
    };
    read_state(&WindowsRun, value_name(), &command).as_option()
}

/// Turns the registration on or off and returns whether the registry actually
/// changed. The command line is this executable's own quoted path, and the
/// name is never written while it holds a foreign or unreadable value.
pub(crate) fn set_enabled(enabled: bool) -> Result<bool, AutostartError> {
    let command = current_command().map_err(|_| AutostartError::Failed)?;
    write_state(&WindowsRun, value_name(), &command, enabled)
}

/// The `Run` value name: exactly the product name the installer writes.
fn value_name() -> &'static str {
    profile::defaults(profile::ACTIVE).title
}

/// The command line for this executable, quoted, or an error when it cannot be
/// read or would not fit the documented 260-character limit.
fn current_command() -> Result<String, ()> {
    let executable = std::env::current_exe().map_err(|_| ())?;
    let path = executable.to_str().ok_or(())?;
    quoted_command(path)
}

/// The quoted command line for `path`, refused when it exceeds the Run limit.
fn quoted_command(path: &str) -> Result<String, ()> {
    let command = format!("\"{path}\"");
    if command.encode_utf16().count() > RUN_COMMAND_MAX {
        return Err(());
    }
    Ok(command)
}

/// Whether a stored value is this executable's own command line. Windows paths
/// compare case-insensitively, so a differently-cased copy is still ours.
fn same_command(stored: &str, command: &str) -> bool {
    stored.eq_ignore_ascii_case(command)
}

/// What Explorer's `StartupApproved` value says about our entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Approved {
    /// No value of ours: Explorer has not disabled the entry.
    Absent,
    /// A recognized marker for an enabled entry.
    Enabled,
    /// The recognized disabled marker.
    Disabled,
    /// A marker or value type we do not interpret; it is never touched.
    Unknown,
}

/// Reads the registration state without changing anything.
fn read_state<R: RunRegistry>(registry: &R, value: &str, command: &str) -> AutostartState {
    match registry.read_run(value) {
        // No value at all is tracked as "absent".
        Ok(None) => AutostartState::Disabled,
        Ok(Some(stored)) => {
            // A value that is not this executable's command belongs to someone
            // else: it is neither reported as ours nor ever overwritten.
            if !same_command(&stored, command) {
                return AutostartState::Unavailable;
            }
            match read_approved(registry, value) {
                Approved::Disabled => AutostartState::Disabled,
                Approved::Absent | Approved::Enabled => AutostartState::Enabled,
                Approved::Unknown => AutostartState::Unavailable,
            }
        }
        Err(_) => AutostartState::Unavailable,
    }
}

/// Reads Explorer's marker for our value. A read error is as unknown as an
/// unrecognized marker: neither can be trusted, so neither is acted on.
fn read_approved<R: RunRegistry>(registry: &R, value: &str) -> Approved {
    match registry.read_startup_approved(value) {
        Ok(None) => Approved::Absent,
        Ok(Some(bytes)) if bytes.len() == 12 => match bytes[0] {
            STARTUP_DISABLED_BYTE => Approved::Disabled,
            STARTUP_ENABLED_BYTE | STARTUP_ENABLED_BYTE_ALT => Approved::Enabled,
            _ => Approved::Unknown,
        },
        Ok(Some(_)) | Err(_) => Approved::Unknown,
    }
}

/// The writes one attempt actually performed, so a rollback touches exactly
/// those values and never one it did not write.
#[derive(Clone, Copy, Default)]
struct Touched {
    /// The `Run` value was written by this attempt.
    wrote_run: bool,
    /// The recognized `StartupApproved` marker was deleted by this attempt.
    deleted_approved: bool,
}

/// Writes the registration and reports whether it changed. The draft revision
/// only moves on a real change, so this is the one place that decision is made.
fn write_state<R: RunRegistry>(
    registry: &R,
    value: &str,
    command: &str,
    enabled: bool,
) -> Result<bool, AutostartError> {
    let current = read_state(registry, value, command);
    if current == AutostartState::Unavailable {
        return Err(AutostartError::Unavailable);
    }
    let desired = if enabled {
        AutostartState::Enabled
    } else {
        AutostartState::Disabled
    };
    if current == desired {
        return Ok(false);
    }
    // An unrecognized marker is never interpreted, deleted or rewritten.
    let approved = read_approved(registry, value);
    if approved == Approved::Unknown {
        return Err(AutostartError::Unavailable);
    }
    let prior_run = registry
        .read_run(value)
        .map_err(|_| AutostartError::Unavailable)?;
    let prior_approved = registry
        .read_startup_approved(value)
        .map_err(|_| AutostartError::Unavailable)?;
    // The value captured for the rollback has to be ours too: another owner
    // that slipped in after the state read must never be deleted or rewritten
    // as if it belonged to this program.
    if let Some(stored) = prior_run.as_deref() {
        if !same_command(stored, command) {
            return Err(AutostartError::Unavailable);
        }
    }
    // Ownership is revalidated immediately before the mutation section, so a
    // replacement between the capture and the first write is refused before
    // anything is touched.
    if read_state(registry, value, command) == AutostartState::Unavailable {
        return Err(AutostartError::Unavailable);
    }
    // Enabling rewrites the command line only when it is not already exactly
    // ours: clearing the marker is enough otherwise, and a `Run` write the
    // system refuses must not block an enable that needs none.
    let rewrite_run = if enabled {
        prior_run.as_deref() != Some(command)
    } else {
        true
    };
    let mut touched = Touched::default();
    let step = mutate(registry, value, command, enabled, approved, rewrite_run, &mut touched);
    if step.is_ok() && read_state(registry, value, command) == desired {
        return Ok(true);
    }
    // The attempt did not leave the registration as asked: whatever it did
    // touch is put back independently, and the effective state is read once
    // more so the answer never contradicts what Windows will do. A state that
    // ended up as asked is an honest success; anything else is a failure,
    // including a rollback that could not be completed.
    let _ = restore(
        registry,
        value,
        command,
        prior_run.as_deref(),
        prior_approved.as_deref(),
        touched,
    );
    if read_state(registry, value, command) == desired {
        return Ok(true);
    }
    Err(AutostartError::Failed)
}

/// The ordered mutation section. Enabling writes the value it points at
/// before clearing the marker, so a marker that is cleared can never leave a
/// value that was never written; disabling removes the value before the
/// marker, for the same reason.
fn mutate<R: RunRegistry>(
    registry: &R,
    value: &str,
    command: &str,
    enabled: bool,
    approved: Approved,
    rewrite_run: bool,
    touched: &mut Touched,
) -> Result<(), String> {
    if enabled {
        if rewrite_run {
            registry.write_run(value, command)?;
            touched.wrote_run = true;
        }
        if approved == Approved::Disabled {
            registry.delete_startup_approved(value)?;
            touched.deleted_approved = true;
        }
    } else {
        registry.delete_run(value)?;
        if approved != Approved::Absent {
            registry.delete_startup_approved(value)?;
            touched.deleted_approved = true;
        }
    }
    Ok(())
}

/// Puts the captured previous values back after a failed attempt. The marker
/// is restored first because it is what makes an entry effective, and each
/// value is only put back while it still holds exactly what this attempt
/// wrote: a value another owner replaced or removed in the meantime is left
/// alone and never overwritten.
fn restore<R: RunRegistry>(
    registry: &R,
    value: &str,
    command: &str,
    prior_run: Option<&str>,
    prior_approved: Option<&[u8]>,
    touched: Touched,
) -> Result<(), String> {
    let mut failure = None;
    if touched.deleted_approved {
        // The marker goes back only while our deletion is still visible; one
        // written by another owner in the meantime stays untouched.
        if let Ok(None) = registry.read_startup_approved(value) {
            if let Some(data) = prior_approved {
                if let Err(error) = registry.write_startup_approved(value, data) {
                    failure = Some(error);
                }
            }
        }
    }
    if touched.wrote_run {
        // The value is undone only while it still holds what this attempt
        // wrote; a replacement or removal by another owner is theirs to keep.
        let ours = match registry.read_run(value) {
            Ok(Some(stored)) => same_command(&stored, command),
            Ok(None) | Err(_) => false,
        };
        if ours {
            let undone = match prior_run {
                Some(prior) => registry.write_run(value, prior),
                None => registry.delete_run(value),
            };
            if failure.is_none() {
                failure = undone.err();
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The narrow set of registry operations this module needs, so the rule set
/// above is testable without touching a real hive.
pub(crate) trait RunRegistry {
    /// The command line stored under `value`, or `None` when it is absent.
    fn read_run(&self, value: &str) -> Result<Option<String>, String>;
    /// Stores `command` under `value`, creating the key when it is missing.
    fn write_run(&self, value: &str, command: &str) -> Result<(), String>;
    /// Removes `value`; a value that is already absent is not an error.
    fn delete_run(&self, value: &str) -> Result<(), String>;
    /// The raw bytes of `value` under `StartupApproved\Run`, or `None`.
    fn read_startup_approved(&self, value: &str) -> Result<Option<Vec<u8>>, String>;
    /// Restores `value` under `StartupApproved\Run` to captured bytes.
    fn write_startup_approved(&self, value: &str, data: &[u8]) -> Result<(), String>;
    /// Removes `value` under `StartupApproved\Run`.
    fn delete_startup_approved(&self, value: &str) -> Result<(), String>;
}

/// The real current-user registry.
struct WindowsRun;

impl RunRegistry for WindowsRun {
    fn read_run(&self, value: &str) -> Result<Option<String>, String> {
        let Some(key) = open(HKEY_CURRENT_USER, RUN_KEY, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        read_string(key.0, value)
    }

    fn write_run(&self, value: &str, command: &str) -> Result<(), String> {
        // An explicit enable is the one path allowed to create the key.
        let key = open_or_create(HKEY_CURRENT_USER, RUN_KEY, KEY_SET_VALUE)?;
        write_bytes(key.0, value, REG_SZ, &wide_bytes(command))
    }

    fn delete_run(&self, value: &str) -> Result<(), String> {
        let Some(key) = open(HKEY_CURRENT_USER, RUN_KEY, KEY_SET_VALUE)? else {
            return Ok(());
        };
        delete_value(key.0, value)
    }

    fn read_startup_approved(&self, value: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(key) = open(HKEY_CURRENT_USER, STARTUP_APPROVED_RUN_KEY, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        read_binary(key.0, value)
    }

    fn write_startup_approved(&self, value: &str, data: &[u8]) -> Result<(), String> {
        let Some(key) = open(HKEY_CURRENT_USER, STARTUP_APPROVED_RUN_KEY, KEY_SET_VALUE)? else {
            return Ok(());
        };
        write_bytes(key.0, value, REG_BINARY, data)
    }

    fn delete_startup_approved(&self, value: &str) -> Result<(), String> {
        let Some(key) = open(HKEY_CURRENT_USER, STARTUP_APPROVED_RUN_KEY, KEY_SET_VALUE)? else {
            return Ok(());
        };
        delete_value(key.0, value)
    }
}

/// An open registry key, closed when it drops.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Opens `subkey` under `root` with `access`. A missing key is `Ok(None)`, not
/// a failure: there is simply nothing there.
fn open(root: HKEY, subkey: &str, access: REG_SAM_FLAGS) -> Result<Option<Key>, String> {
    let name = wide(subkey);
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            root,
            PCWSTR(name.as_ptr()),
            None,
            access,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(error_text(status));
    }
    Ok(Some(Key(key)))
}

/// Opens or creates `subkey` under `root` with `access`. Only the explicit
/// enable path uses this: reads and disabling open what already exists, so
/// nothing is ever created behind the user's back.
fn open_or_create(root: HKEY, subkey: &str, access: REG_SAM_FLAGS) -> Result<Key, String> {
    let name = wide(subkey);
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            root,
            PCWSTR(name.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            access,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(error_text(status));
    }
    Ok(Key(key))
}

/// Reads one value's type and raw bytes, or `None` when it is absent.
fn query(key: HKEY, value: &str) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, String> {
    let name = wide(value);
    let mut value_type = REG_VALUE_TYPE(0);
    let mut size: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return Err(error_text(status));
    }
    let mut buffer = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(error_text(status));
    }
    buffer.truncate(size as usize);
    Ok(Some((value_type, buffer)))
}

/// Reads a `REG_SZ` value as text; any other type is refused.
fn read_string(key: HKEY, value: &str) -> Result<Option<String>, String> {
    let Some((value_type, bytes)) = query(key, value)? else {
        return Ok(None);
    };
    if value_type != REG_SZ {
        return Err("unexpected registry value type".to_owned());
    }
    if bytes.len() % 2 != 0 {
        return Err("malformed registry string".to_owned());
    }
    let mut units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    Ok(Some(String::from_utf16_lossy(&units)))
}

/// Reads a `REG_BINARY` value; any other type is refused.
fn read_binary(key: HKEY, value: &str) -> Result<Option<Vec<u8>>, String> {
    match query(key, value)? {
        None => Ok(None),
        Some((value_type, bytes)) if value_type == REG_BINARY => Ok(Some(bytes)),
        Some(_) => Err("unexpected registry value type".to_owned()),
    }
}

/// Writes a value of the given type.
fn write_bytes(
    key: HKEY,
    value: &str,
    value_type: REG_VALUE_TYPE,
    data: &[u8],
) -> Result<(), String> {
    let name = wide(value);
    let status = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, value_type, Some(data)) };
    if status != ERROR_SUCCESS {
        return Err(error_text(status));
    }
    Ok(())
}

/// Deletes a value; a value that is already absent is not an error.
fn delete_value(key: HKEY, value: &str) -> Result<(), String> {
    let name = wide(value);
    let status = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(error_text(status))
    }
}

/// A fixed, non-secret description of a failed registry call.
fn error_text(status: WIN32_ERROR) -> String {
    format!("registry error {}", status.0)
}

/// A NUL-terminated UTF-16 copy of `value`, for the wide Win32 calls.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A `REG_SZ` payload: UTF-16 little-endian with a trailing NUL.
fn wide_bytes(value: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((value.len() + 1) * 2);
    for unit in value.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A registry that lives in memory, so no test ever reads or writes the
    /// runner's real `Run` key.
    #[derive(Default)]
    struct MemoryRegistry {
        run: RefCell<Option<String>>,
        approved: RefCell<Option<Vec<u8>>>,
        /// The modeled `Run` key exists; only an explicit enable creates it.
        run_key_present: Cell<bool>,
        /// How many value writes had to create the modeled key.
        run_key_creates: Cell<u32>,
        /// How many value writes were attempted, denied or not.
        run_writes: Cell<u32>,
        /// Writes are refused, as a key that denies `KEY_SET_VALUE` would.
        deny_run_write: Cell<bool>,
        /// Deleting the marker is refused.
        deny_approved_delete: Cell<bool>,
        /// Deleting the marker reports success without changing anything.
        drop_approved_delete: Cell<bool>,
        /// Counts down reads of the value; on the last one another owner
        /// replaces it with a foreign command.
        replace_run_on_read: Cell<u32>,
        /// Every access fails, as an unreadable hive would.
        broken: Cell<bool>,
        /// Writes report success but change nothing, to exercise the readback.
        mute_writes: Cell<bool>,
    }

    impl MemoryRegistry {
        fn with_run(command: &str) -> Self {
            let registry = Self::default();
            registry.run_key_present.set(true);
            *registry.run.borrow_mut() = Some(command.to_owned());
            registry
        }

        /// Arms the value read that is `remaining` reads away: that read sees
        /// a foreign command where ours used to be, and keeps it.
        fn replace_run_after(&self, remaining: u32) {
            self.replace_run_on_read.set(remaining);
        }
    }

    impl RunRegistry for MemoryRegistry {
        fn read_run(&self, _value: &str) -> Result<Option<String>, String> {
            if self.broken.get() {
                return Err("broken".to_owned());
            }
            let remaining = self.replace_run_on_read.get();
            if remaining != 0 {
                self.replace_run_on_read
                    .set(if remaining == 1 { 0 } else { remaining - 1 });
                if remaining == 1 {
                    *self.run.borrow_mut() = Some(FOREIGN.to_owned());
                }
            }
            Ok(self.run.borrow().clone())
        }

        fn write_run(&self, _value: &str, command: &str) -> Result<(), String> {
            if self.broken.get() {
                return Err("broken".to_owned());
            }
            self.run_writes.set(self.run_writes.get() + 1);
            if self.deny_run_write.get() {
                return Err("denied".to_owned());
            }
            if !self.run_key_present.get() {
                self.run_key_present.set(true);
                self.run_key_creates.set(self.run_key_creates.get() + 1);
            }
            if self.mute_writes.get() {
                return Ok(());
            }
            *self.run.borrow_mut() = Some(command.to_owned());
            Ok(())
        }

        fn delete_run(&self, _value: &str) -> Result<(), String> {
            if self.broken.get() {
                return Err("broken".to_owned());
            }
            if !self.run_key_present.get() {
                return Ok(());
            }
            if self.mute_writes.get() {
                return Ok(());
            }
            *self.run.borrow_mut() = None;
            Ok(())
        }

        fn read_startup_approved(&self, _value: &str) -> Result<Option<Vec<u8>>, String> {
            if self.broken.get() {
                return Err("broken".to_owned());
            }
            Ok(self.approved.borrow().clone())
        }

        fn write_startup_approved(&self, _value: &str, data: &[u8]) -> Result<(), String> {
            if self.broken.get() {
                return Err("broken".to_owned());
            }
            if self.mute_writes.get() {
                return Ok(());
            }
            *self.approved.borrow_mut() = Some(data.to_vec());
            Ok(())
        }

        fn delete_startup_approved(&self, _value: &str) -> Result<(), String> {
            if self.broken.get() {
                return Err("broken".to_owned());
            }
            if self.deny_approved_delete.get() {
                return Err("denied".to_owned());
            }
            if self.drop_approved_delete.get() || self.mute_writes.get() {
                return Ok(());
            }
            *self.approved.borrow_mut() = None;
            Ok(())
        }
    }

    const VALUE: &str = "Speechek Test";
    const COMMAND: &str = "\"C:\\Apps\\Speechek\\speechek.exe\"";
    const FOREIGN: &str = "\"C:\\other\\program.exe\"";

    #[test]
    fn absent_registration_reads_as_disabled() {
        let registry = MemoryRegistry::default();
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Disabled);
        assert_eq!(AutostartState::Disabled.as_option(), Some(false));
    }

    #[test]
    fn own_value_without_marker_reads_as_enabled() {
        let registry = MemoryRegistry::with_run(COMMAND);
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Enabled);
        assert_eq!(AutostartState::Enabled.as_option(), Some(true));
    }

    #[test]
    fn own_value_is_matched_without_regard_to_case() {
        let registry = MemoryRegistry::with_run(&COMMAND.to_ascii_uppercase());
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Enabled);
    }

    #[test]
    fn foreign_value_is_unavailable_and_never_touched() {
        let registry = MemoryRegistry::with_run("\"C:\\other\\program.exe\"");
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Unavailable);
        assert_eq!(AutostartState::Unavailable.as_option(), None);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Unavailable)
        );
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, false),
            Err(AutostartError::Unavailable)
        );
        assert_eq!(registry.run.borrow().as_deref(), Some("\"C:\\other\\program.exe\""));
    }

    #[test]
    fn disabled_marker_reads_as_disabled_and_on_clears_only_it() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Disabled);
        assert_eq!(write_state(&registry, VALUE, COMMAND, true), Ok(true));
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Enabled);
        assert_eq!(registry.approved.borrow().as_deref(), None);
        assert_eq!(registry.run.borrow().as_deref(), Some(COMMAND));
        assert_eq!(registry.run_writes.get(), 0);
    }

    #[test]
    fn unknown_marker_is_unavailable_and_preserved() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![0x07, 0xAB, 0xCD]);
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Unavailable);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Unavailable)
        );
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[0x07, 0xAB, 0xCD][..])
        );
        assert_eq!(registry.run.borrow().as_deref(), Some(COMMAND));
    }

    #[test]
    fn truncated_disabled_marker_is_not_deleted() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_DISABLED_BYTE]);
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Unavailable);
        assert_eq!(write_state(&registry, VALUE, COMMAND, true), Err(AutostartError::Unavailable));
        assert_eq!(registry.approved.borrow().as_deref(), Some(&[STARTUP_DISABLED_BYTE][..]));
    }

    #[test]
    fn enabling_from_absent_writes_and_is_idempotent() {
        let registry = MemoryRegistry::default();
        assert_eq!(write_state(&registry, VALUE, COMMAND, true), Ok(true));
        assert_eq!(registry.run.borrow().as_deref(), Some(COMMAND));
        assert_eq!(write_state(&registry, VALUE, COMMAND, true), Ok(false));
    }

    #[test]
    fn disabling_clears_run_and_recognized_state() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Enabled);
        assert_eq!(write_state(&registry, VALUE, COMMAND, false), Ok(true));
        assert_eq!(registry.run.borrow().as_deref(), None);
        assert_eq!(registry.approved.borrow().as_deref(), None);
        assert_eq!(write_state(&registry, VALUE, COMMAND, false), Ok(false));
    }

    #[test]
    fn unreadable_registry_is_unavailable() {
        let registry = MemoryRegistry::default();
        registry.broken.set(true);
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Unavailable);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Unavailable)
        );
    }

    #[test]
    fn readback_mismatch_reports_failure_and_rolls_back() {
        let registry = MemoryRegistry::default();
        registry.mute_writes.set(true);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Failed)
        );
        assert_eq!(registry.run.borrow().as_deref(), None);
    }

    #[test]
    fn command_line_is_quoted_and_bounded() {
        assert_eq!(quoted_command("C:\\a.exe").unwrap(), "\"C:\\a.exe\"");
        assert_eq!(quoted_command(&"x".repeat(RUN_COMMAND_MAX + 1)), Err(()));
        let edge = "y".repeat(RUN_COMMAND_MAX - 2);
        assert_eq!(quoted_command(&edge).unwrap().encode_utf16().count(), RUN_COMMAND_MAX);
        assert_eq!(quoted_command(&"🎙".repeat(130)), Err(()));
    }

    /// Enabling an entry whose command line is already exact must not rewrite
    /// the `Run` value: with a key that denies writes, clearing the marker
    /// alone is still a successful enable.
    #[test]
    fn enabling_unchanged_own_run_clears_the_marker_without_a_run_write() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.deny_run_write.set(true);
        assert_eq!(write_state(&registry, VALUE, COMMAND, true), Ok(true));
        assert_eq!(registry.run_writes.get(), 0);
        assert_eq!(registry.approved.borrow().as_deref(), None);
        assert_eq!(registry.run.borrow().as_deref(), Some(COMMAND));
    }

    /// When the value really has to be written and the key refuses it, the
    /// marker is not touched first, so the entry stays effectively off.
    #[test]
    fn refused_run_write_leaves_the_disabled_marker_and_the_entry_off() {
        let registry = MemoryRegistry::default();
        registry.run_key_present.set(true);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.deny_run_write.set(true);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Failed)
        );
        assert_eq!(registry.run_writes.get(), 1);
        assert_eq!(registry.run.borrow().as_deref(), None);
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..])
        );
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Disabled);
    }

    /// A foreign command that replaced ours between the state read and the
    /// capture must never be deleted by the toggle.
    #[test]
    fn captured_foreign_replacement_is_refused_without_touching_it() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.replace_run_after(2);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, false),
            Err(AutostartError::Unavailable)
        );
        assert_eq!(registry.run.borrow().as_deref(), Some(FOREIGN));
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..])
        );
    }

    /// A foreign command that replaces ours after the capture but before the
    /// mutation is caught by the revalidation and refused.
    #[test]
    fn replacement_after_capture_is_refused_by_the_revalidation() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.replace_run_after(3);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, false),
            Err(AutostartError::Unavailable)
        );
        assert_eq!(registry.run.borrow().as_deref(), Some(FOREIGN));
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..])
        );
    }

    /// A rollback must not overwrite a value another owner put there while
    /// the failed write was unwinding.
    #[test]
    fn rollback_skips_a_value_replaced_by_another_owner() {
        let registry = MemoryRegistry::default();
        registry.run_key_present.set(true);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.deny_approved_delete.set(true);
        registry.replace_run_after(4);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Failed)
        );
        assert_eq!(registry.run.borrow().as_deref(), Some(FOREIGN));
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..])
        );
    }

    /// An enable on a hive without the per-user `Run` key creates it.
    #[test]
    fn explicit_enable_creates_the_missing_run_key() {
        let registry = MemoryRegistry::default();
        assert!(!registry.run_key_present.get());
        assert_eq!(write_state(&registry, VALUE, COMMAND, true), Ok(true));
        assert_eq!(registry.run_key_creates.get(), 1);
        assert!(registry.run_key_present.get());
        assert_eq!(registry.run.borrow().as_deref(), Some(COMMAND));
    }

    /// Reading or disabling never creates the `Run` key.
    #[test]
    fn read_and_disable_never_create_the_run_key() {
        let registry = MemoryRegistry::default();
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Disabled);
        assert_eq!(write_state(&registry, VALUE, COMMAND, false), Ok(false));
        assert_eq!(registry.run_key_creates.get(), 0);
        assert!(!registry.run_key_present.get());
    }

    /// A disable whose value removal landed but whose marker clear was refused
    /// is reported by the state Windows will act on.
    #[test]
    fn disable_with_a_refused_marker_clear_reports_the_effective_off_state() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.deny_approved_delete.set(true);
        assert_eq!(write_state(&registry, VALUE, COMMAND, false), Ok(true));
        assert_eq!(registry.run.borrow().as_deref(), None);
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[STARTUP_ENABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..])
        );
    }

    /// A marker whose deletion silently did not stick is left as it is, and
    /// the failed toggle leaves the entry effectively off.
    #[test]
    fn marker_whose_delete_did_not_stick_is_left_alone() {
        let registry = MemoryRegistry::with_run(COMMAND);
        *registry.approved.borrow_mut() = Some(vec![STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        registry.drop_approved_delete.set(true);
        assert_eq!(
            write_state(&registry, VALUE, COMMAND, true),
            Err(AutostartError::Failed)
        );
        assert_eq!(registry.run_writes.get(), 0);
        assert_eq!(registry.run.borrow().as_deref(), Some(COMMAND));
        assert_eq!(
            registry.approved.borrow().as_deref(),
            Some(&[STARTUP_DISABLED_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..])
        );
        assert_eq!(read_state(&registry, VALUE, COMMAND), AutostartState::Disabled);
    }
}
