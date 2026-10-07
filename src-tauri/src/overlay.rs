//! Windows overlay placement adapted from Handy by CJ Pais (MIT).
//! Source: https://github.com/cjpais/Handy/blob/main/src-tauri/src/overlay.rs
//! License: ../../third_party/Handy.LICENSE

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, POINT};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::i18n::{MessageId, UiError, UiMessage};

/// The recording pill is 224×48 inside a 240×64 window, leaving an 8 px
/// transparent edge while the meter, status and timer sit 12 px apart.
const NORMAL_PILL: (f64, f64) = (240.0, 64.0);
/// The notice pill — a failed dictation's reason, or the sound a cancelled take
/// could not give back: the wider 344×48 card inside a 360×64 window, so a
/// sentence worth acting on reads at length. The next recording returns the
/// window to [`NORMAL_PILL`].
const NOTICE_PILL: (f64, f64) = (360.0, 64.0);
/// Distance from the work-area bottom to the pill, in logical points.
const BOTTOM_OFFSET: f64 = 40.0;
/// How long a notice that takes the pill down with it — a failed dictation's
/// reason, or the sound a cancelled one could not give back — stays up before
/// the pill goes away.
const ERROR_DWELL: Duration = Duration::from_secs(4);
/// Window label of the recording pill.
const OVERLAY: &str = "overlay";
/// Bumped by every `show`; a delayed hide or a late error may only touch the
/// window while it still belongs to that dictation.
static GENERATION: AtomicU64 = AtomicU64::new(0);
// Serialize a show with the generation check and hide: a stale hide cannot race a new show.
static PLACEMENT: Mutex<()> = Mutex::new(());

fn native_handle(window: &tauri::WebviewWindow) -> Result<HWND, UiError> {
    let handle = window.hwnd().map_err(|error| {
        UiError::new(
            "OVERLAY_HANDLE_FAILED",
            UiMessage::new(MessageId::OverlayHandleFailed)
                .with_arg("detail", serde_json::Value::from(error.to_string())),
        )
    })?;
    // Tauri and our direct windows dependency may use different windows crate versions.
    Ok(HWND(handle.0 as _))
}

fn within(point: POINT, position: &PhysicalPosition<i32>, size: &PhysicalSize<u32>) -> bool {
    point.x >= position.x
        && point.y >= position.y
        && (point.x as i64) < position.x as i64 + size.width as i64
        && (point.y as i64) < position.y as i64 + size.height as i64
}

/// The Windows "Make text bigger" slider (Settings ▸ Accessibility ▸ Text size)
/// as a factor. It is a second scale axis next to display scaling, and WebView2
/// applies it to the page itself, so the window has to grow with it or the card
/// would keep its physical size inside an oversized frame. The value is absent
/// until the slider leaves 100%.
fn text_scale_factor() -> f64 {
    let key: Vec<u16> = r"Software\Microsoft\Accessibility"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "TextScaleFactor"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut percent = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut percent as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return 1.0;
    }
    // Windows offers 100..225; anything else is not a scale this shell can place.
    (percent as f64 / 100.0).clamp(1.0, 2.25)
}

/// The window's rectangle for `window_size` in the destination monitor's
/// physical pixels, so nothing is converted through the window's previous-monitor
/// DPI. The window grows with the Windows text scale and keeps the card's logical
/// proportions; the offset stays DPI-only, like the card's distance from the
/// screen edge. The compact recording size and the wider notice size share this
/// one path, so both scale the same way.
fn bounds(app: &AppHandle, window_size: (f64, f64)) -> Result<(i32, i32, i32, i32), String> {
    let mut cursor = POINT::default();
    let has_cursor = unsafe { GetCursorPos(&mut cursor) }.is_ok();
    let monitor = app
        .available_monitors()
        .map_err(|error| format!("Overlay monitors: {error}"))?
        .into_iter()
        .find(|monitor| has_cursor && within(cursor, monitor.position(), monitor.size()))
        .or_else(|| app.primary_monitor().ok().flatten())
        .ok_or_else(|| "No display is available for the recording overlay".to_string())?;
    let scale = monitor.scale_factor();
    let content_scale = scale * text_scale_factor();
    let width = (window_size.0 * content_scale).round().max(1.0) as i32;
    let height = (window_size.1 * content_scale).round().max(1.0) as i32;
    let work = monitor.work_area();
    let x = work.position.x + (work.size.width as i32 - width) / 2;
    let y =
        work.position.y + work.size.height as i32 - height - (BOTTOM_OFFSET * scale).round() as i32;
    Ok((x, y, width, height))
}

fn position(window: &tauri::WebviewWindow, rectangle: (i32, i32, i32, i32)) -> Result<(), String> {
    let (x, y, width, height) = rectangle;
    unsafe {
        SetWindowPos(
            native_handle(window).map_err(|error| error.to_string())?,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(|error| format!("Cannot position recording overlay: {error}"))
}

/// Places the window on the monitor under the cursor, sizes it to `window_size`
/// and shows it without taking the foreground, so the window the dictation
/// belongs to keeps it. Every reveal names the size it needs, so a notice never
/// inherits the compact width of a recording, or the other way around.
fn reveal(app: &AppHandle, window_size: (f64, f64)) -> Result<tauri::WebviewWindow, String> {
    let window = app
        .get_webview_window(OVERLAY)
        .ok_or_else(|| "Recording overlay is unavailable".to_string())?;
    let rectangle = bounds(app, window_size)?;
    // Physical destination-monitor coordinates avoid Tauri's old-monitor DPI conversion.
    position(&window, rectangle)?;
    window
        .show()
        .map_err(|error| format!("Cannot show recording overlay: {error}"))?;
    // WM_DPICHANGED can move the first placement after show; reassert position and Z-order.
    position(&window, rectangle)?;
    unsafe {
        SetWindowPos(
            native_handle(&window).map_err(|error| error.to_string())?,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
        )
    }
    .map_err(|error| format!("Cannot keep recording overlay on top: {error}"))?;
    Ok(window)
}

pub fn create(app: &AppHandle) -> Result<(), UiError> {
    if app.get_webview_window(OVERLAY).is_some() {
        return Ok(());
    }
    let window = WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::App("overlay.html".into()))
        .title(crate::localized(
            crate::current_language(app),
            crate::i18n::MessageId::OverlayTitle,
        ))
        // The first size only: every reveal sets the size its state needs.
        .inner_size(NORMAL_PILL.0, NORMAL_PILL.1)
        .decorations(false)
        .resizable(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|error| {
            UiError::new(
                "OVERLAY_CREATE_FAILED",
                UiMessage::new(MessageId::OverlayWindowCreateFailed)
                    .with_arg("detail", serde_json::Value::from(error.to_string())),
            )
        })?;
    let hwnd = native_handle(&window)?;
    unsafe {
        let styles = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            styles | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
        );
    }
    Ok(())
}

/// Shows the pill for the dictation that is starting — always at
/// [`NORMAL_PILL`], whatever size a notice left behind — and hands out the
/// generation every later native call has to carry.
pub fn show(app: &AppHandle) -> Result<u64, String> {
    let _guard = PLACEMENT
        .lock()
        .map_err(|_| "Overlay placement lock poisoned".to_string())?;
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    reveal(app, NORMAL_PILL)?;
    Ok(generation)
}

/// Puts the pill away right now, before the transcript is handed to the window
/// it belongs to: nothing of that hand-over is meant to be visible. This is also
/// what ends the dictation after its error dwell.
///
/// The generation decides whether this hide may touch the window at all: a
/// delayed hide left over from a finished dictation never takes down the pill of
/// the one that replaced it.
pub fn hide_now(app: &AppHandle, generation: u64) {
    let Ok(_guard) = PLACEMENT.lock() else {
        return;
    };
    if GENERATION.load(Ordering::SeqCst) != generation {
        return;
    }
    if let Some(window) = app.get_webview_window(OVERLAY) {
        let _ = window.hide();
    }
}

/// Brings the pill back with the reason the dictation failed, and takes it away
/// again after [`ERROR_DWELL`] — unless a newer dictation owns the window by
/// then. The renderer has already written the reason into the pill; all the
/// shell does is make it visible.
pub fn show_error(app: &AppHandle, generation: u64) {
    show_for(app, generation);
}

/// Brings the pill back for the one warning that can outlive its take: the system
/// sound a cancelled dictation could not give back. The renderer has already
/// written the sentence into the pill — this is only the reveal, with the same
/// dwell, so a notice of a finished take is read instead of drawn into a hidden
/// window.
pub fn show_warning(app: &AppHandle, generation: u64) {
    show_for(app, generation);
}

/// Reveals the pill at [`NOTICE_PILL`] for [`ERROR_DWELL`] and puts it away
/// again, unless a newer dictation owns the window by then.
fn show_for(app: &AppHandle, generation: u64) {
    match PLACEMENT.lock() {
        Ok(_guard) => {
            if GENERATION.load(Ordering::SeqCst) != generation {
                // A dictation that started meanwhile owns the pill now; a notice
                // of the one before it must not cover it — nor resize its window.
                return;
            }
            // The check above ran before this resize: a late notice never widens
            // the compact window of the recording that replaced it.
            if let Err(error) = reveal(app, NOTICE_PILL) {
                eprintln!("speechek: cannot show the dictation notice: {error}.");
                return;
            }
        }
        Err(_) => return,
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(ERROR_DWELL);
        hide_now(&app, generation);
    });
}
