//! The installer's own request for this shell to leave.
//!
//! A released NSIS installer must not kill a working copy of Speechek: it asks
//! the one process it confirmed to quit first, waits a bounded time and only
//! then forces that process. The asking side is a manual-reset event with the
//! logical name `Local\Speechek.Quit.<pid>` — one backslash in the name itself,
//! which the string literal below escapes as `\\`. The PID in the name keeps
//! the request inside the session it was confirmed for: a running shell of
//! another build or another user is never addressed, and the installer only
//! ever opens and signals an event that this side created.
//!
//! The handle lives here for the whole runtime, in managed state, and is never
//! closed while the watcher may still poll it. [`QuitSignal::is_signaled`] is
//! the one hot call: it performs a zero-timeout wait and must not allocate,
//! format or take a lock, because the watcher runs it on every tick.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// The manual-reset event a confirmed installer signals before it falls back to
/// forcing this process.
pub(crate) struct QuitSignal {
    /// The owned Win32 event handle. An integer on purpose: a `HANDLE` is a raw
    /// pointer and neither `Send` nor `Sync`, while this wrapper has to live in
    /// managed state and be read from the watcher thread.
    handle: isize,
}

impl QuitSignal {
    /// Creates this process's quit event, once.
    ///
    /// A name that already exists is refused instead of adopted. The name
    /// carries this process's PID, so an existing object is a leftover of
    /// another lifetime — it may even be signaled already — and adopting it
    /// could start the shell with a quit pending. An installer that finds no
    /// fresh event simply waits out its own deadline.
    pub(crate) fn new() -> Result<Self, String> {
        let name: Vec<u16> = format!("Local\\Speechek.Quit.{}", std::process::id())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // A successful `CreateEventW` that found an existing name answers with
        // the same handle as one that created the event; the last error is the
        // only thing that tells them apart, and it is only meaningful if it was
        // cleared first.
        let event = unsafe {
            SetLastError(ERROR_SUCCESS);
            CreateEventW(None, true, false, PCWSTR(name.as_ptr()))
        }
        .map_err(|err| format!("the installer quit event could not be created: {err}"))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let _ = unsafe { CloseHandle(event) };
            return Err(
                "an installer quit event of this process name already exists; \
                 the installer falls back to its own deadline"
                    .to_string(),
            );
        }
        Ok(Self {
            handle: event.0 as isize,
        })
    }

    /// Whether the installer signaled this process to leave. A zero-timeout
    /// wait and nothing else: the watcher polls this.
    pub(crate) fn is_signaled(&self) -> bool {
        unsafe {
            WaitForSingleObject(HANDLE(self.handle as *mut core::ffi::c_void), 0) == WAIT_OBJECT_0
        }
    }
}

impl Drop for QuitSignal {
    fn drop(&mut self) {
        // Best effort: the process is ending and the object dies with its last
        // handle either way.
        let _ = unsafe { CloseHandle(HANDLE(self.handle as *mut core::ffi::c_void)) };
    }
}

#[cfg(test)]
mod tests {
    use super::QuitSignal;

    /// One process owns one quit event: it starts quiet, and a second creation
    /// of the same name is refused instead of adopting an object of another
    /// lifetime.
    #[test]
    fn the_quit_event_is_created_once_and_starts_quiet() {
        let signal = QuitSignal::new().expect("a free name creates the event");
        assert!(!signal.is_signaled(), "nothing has signaled the exit yet");
        assert!(
            QuitSignal::new().is_err(),
            "the same process name is only ever created once"
        );
    }
}
