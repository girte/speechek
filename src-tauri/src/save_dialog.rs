//! The one native "save as" dialog the shell shows.
//!
//! The recording history exports a decrypted FLAC copy through the standard
//! Windows save dialog, so the user picks the folder, the name and the
//! overwrite decision in the surface they already know. The dialog is a COM
//! object, which is why this module owns its apartment: [`pick_save_path`] runs
//! on a thread of its own (see `history_save_audio`) and enters a
//! single-threaded apartment there, because the shell's other COM user - the
//! audio mute - lives in a multithreaded one.
//!
//! A cancelled dialog is not a failure: the caller gets `Ok(None)` and the page
//! says nothing. Every other error is the shell's, and it is the caller that
//! turns it into a catalog message.

use std::path::PathBuf;

use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileSaveDialog, IFileSaveDialog, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, SIGDN_FILESYSPATH,
};

/// `ERROR_CANCELLED` as the dialog reports the user closing it.
const ERROR_CANCELLED_HRESULT: HRESULT = HRESULT::from_win32(ERROR_CANCELLED.0);

/// The COM apartment this thread entered, released on drop.
///
/// `CoInitializeEx` answers `RPC_E_CHANGED_MODE` for a thread that is already in
/// another apartment: that thread cannot use this dialog, but it also did not
/// enter anything on our behalf, so only a successful call is released.
struct Apartment {
    entered: bool,
}

impl Apartment {
    fn enter() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
        Ok(Self { entered: true })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.entered {
            unsafe { CoUninitialize() };
        }
    }
}

/// Shows the save dialog for one FLAC export and returns the chosen path.
///
/// `default_name` is the file name the dialog starts with, `title` its window
/// title and `filter_label` the label of the single `*.flac` filter, all
/// rendered by the caller in the interface language. `Ok(None)` means the user
/// cancelled; the file is never written here.
pub(crate) fn pick_save_path(
    owner: HWND,
    default_name: &str,
    title: &str,
    filter_label: &str,
) -> windows::core::Result<Option<PathBuf>> {
    let _apartment = Apartment::enter()?;

    let dialog: IFileSaveDialog = unsafe {
        CoCreateInstance(&FileSaveDialog, None::<&windows::core::IUnknown>, CLSCTX_INPROC_SERVER)?
    };

    // The name and the label outlive the calls that borrow them, so they are
    // kept in this frame for as long as the dialog is shown.
    let name = wide(default_name);
    let title = wide(title);
    let label = wide(filter_label);
    let extension = wide("flac");

    unsafe {
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
            pszName: PCWSTR(label.as_ptr()),
            pszSpec: w!("*.flac"),
        }])?;
        dialog.SetDefaultExtension(PCWSTR(extension.as_ptr()))?;
        dialog.SetFileName(PCWSTR(name.as_ptr()))?;
        dialog.SetTitle(PCWSTR(title.as_ptr()))?;
        dialog.SetOptions(dialog.GetOptions()? | FOS_OVERWRITEPROMPT | FOS_FORCEFILESYSTEM)?;
    }

    match unsafe { dialog.Show(Some(owner)) } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_CANCELLED_HRESULT => return Ok(None),
        Err(error) => return Err(error),
    }

    let item = unsafe { dialog.GetResult()? };
    let display = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? };
    let path = unsafe { display.to_string() };
    unsafe { CoTaskMemFree(Some(display.0 as *const core::ffi::c_void)) };
    Ok(Some(PathBuf::from(path?)))
}

/// A NUL-terminated UTF-16 copy of `text`.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
