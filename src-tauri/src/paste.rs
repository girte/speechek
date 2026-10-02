//! Clipboard insertion for the dictated text, adapted from Handy (MIT).
//!
//! Upstream: <https://github.com/cjpais/Handy> — `src-tauri/src/paste_tx/mod.rs`,
//! `src-tauri/src/paste_tx/windows.rs` and the Windows arm of
//! `src-tauri/src/input.rs::send_paste_ctrl_v`, at commit
//! `8f9cf53cd1410cda26beea39ff802ac306e39585`. MIT licensed, © CJ Pais; the full
//! notice ships in `third_party/Handy.LICENSE` and is served as
//! `/licenses/Handy.LICENSE`.
//!
//! A dictation ends with the transcript either in the window the user is looking
//! at, or — when that cannot be trusted — only in the clipboard, for one manual
//! paste. Handy's receipt-sequenced paste is what makes "in the window" mean
//! something: the transcript is published as a *delayed-render* promise
//! (`SetClipboardData(CF_UNICODETEXT, NULL)`) owned by a hidden message-only
//! window, the `Ctrl+V` chord is sent, and Windows reports the clipboard read
//! back to the owner as `WM_RENDERFORMAT`. Only a read after the chord counts:
//! an eager clipboard manager reading the promise the moment it appears is not a
//! paste, and SendInput's return value says nothing at all about the target.
//!
//! What speechek does differently from the upstream algorithm:
//!
//! - **the transcript is kept.** Settlement materializes it as ordinary
//!   `CF_UNICODETEXT` under the same ownership instead of restoring a snapshot,
//!   so Handy's snapshot, bitmap and restore machinery is gone: what is left in
//!   the clipboard is exactly the text the user dictated, and it outlives this
//!   process;
//! - **the outcome is reported.** The transaction answers its caller instead of
//!   finishing in the background, and it tells three cases apart: the target
//!   read the transcript, only the clipboard has it, and someone else owns the
//!   clipboard now so their copy was left alone;
//! - **one chord and nothing else.** No `Ctrl+Shift+V`, no `Shift+Insert`, no
//!   auto-submit Enter;
//! - **a foreground check surrounds the chord**, because `Ctrl+V` goes to
//!   whoever owns the foreground when Windows dispatches it — not to the window
//!   that was focused a moment earlier.
//!
//! Everything here blocks: a transaction waits for the operating system to
//! report a read, for a quiet period after the last one, and at most
//! [`SETTLE_TIMEOUT`]. Callers own their threading.

use std::sync::{mpsc, Arc, Once};
use std::thread;
use std::time::{Duration, Instant};

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use parking_lot::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GlobalFree, SetLastError, ERROR_SUCCESS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardOwner, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, GetWindowLongPtrW, GetWindowThreadProcessId, KillTimer, PostQuitMessage,
    RegisterClassW, SetTimer, SetWindowLongPtrW, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_TIMER, WNDCLASSW,
};

/// The transcript is not left as a promise forever: after this long without a
/// decision the promise becomes ordinary text whether or not anyone read it.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(8);

/// One read is not proof that a paste finished: applications read the clipboard
/// more than once (Chromium probes, then reads), so the transcript is settled
/// only once reads have stopped for this long.
const QUIET_PERIOD: Duration = Duration::from_millis(200);

/// A chord that could not be sent produces no legitimate read, so the promise is
/// settled quickly instead of waiting out the whole timeout. The half second
/// covers a chord that only partly went out.
const FAILED_CHORD_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the modifier stays down after the `V` click. Chords released too
/// quickly are dropped by applications that poll global keyboard state (Handy
/// added this hold for exactly that reason); nothing else changes the chord.
const CHORD_HOLD_MS: u64 = 100;

/// `VK_V`: the chord is a virtual key, not a character, so it is the same chord
/// on a Russian or Dvorak layout.
const VK_V: u32 = 0x56;

/// While a transaction waits for its read receipt it has to pump messages, so
/// its decisions are driven by a timer on its own window.
const TICK_MS: u32 = 25;
const TIMER_ID: usize = 1;

/// The clipboard is a shared resource: Windows refuses to open it while another
/// process holds it, and that contention is routine. Writes therefore retry
/// briefly instead of reporting a failure the user would see.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(20);

/// The hidden window that owns the clipboard promise. One class serves every
/// transaction; each transaction has its own window on its own thread.
const CLASS_NAME: PCWSTR = w!("SpeechekPasteWindow");

/// What the caller has reported about the paste chord.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chord {
    /// The caller is still deciding whether a chord may be sent.
    Pending,
    /// The chord was sent; a read after `injected_at` is evidence of a paste.
    Sent,
    /// The chord could not be sent.
    Failed,
    /// No chord was sent on purpose: there was no target, or it lost the
    /// foreground before the chord would have been sent.
    Skipped,
}

/// What the worker concluded about a transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Decision {
    /// A newer transaction took the clipboard over; nothing may be written.
    Cancelled,
    /// Another window owns the clipboard now.
    TakenOver,
    /// The target read the transcript after the chord.
    Inserted,
    /// Only the clipboard has the transcript.
    CopiedFallback,
}

/// How one transaction ended, as the renderer sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Settlement {
    /// The target read the transcript after the chord: it was pasted there, and
    /// the clipboard still holds the same text. Nothing on screen says so.
    Inserted,
    /// No chord was sent (no target, or a foreground that moved) or no read was
    /// observed in time: the transcript is on the clipboard, for one manual
    /// paste.
    CopiedFallback,
}

impl Settlement {
    /// The word the renderer receives. These two are the only outcomes it knows
    /// how to show; anything else is a failure.
    pub fn as_str(self) -> &'static str {
        match self {
            Settlement::Inserted => "inserted",
            Settlement::CopiedFallback => "copied_fallback",
        }
    }
}

/// One transaction, shared between the calling thread and the worker thread that
/// owns the hidden clipboard window. Every field is written once, by the thread
/// that observed it, under `Job::tx`.
struct Tx {
    /// When the promise appeared on the clipboard; the deadline runs from here.
    published_at: Instant,
    chord: Chord,
    /// When the chord was sent. A read *before* this is an eager third party
    /// (clipboard manager, antivirus) reacting to the promise itself.
    injected_at: Option<Instant>,
    /// Every read the owner was asked to render, so the quiet period can be
    /// measured from the last one.
    reads: Vec<Instant>,
    /// The target owned the foreground immediately before *and* immediately
    /// after the chord.
    foreground_held: bool,
    /// Another window emptied or replaced the clipboard.
    taken_over: bool,
    /// A newer transaction replaced this one; the newer one owns the clipboard.
    cancelled: bool,
    /// The worker has decided and the clipboard is final for this transaction.
    settled: bool,
    /// Sequence number of our own last write to the clipboard. Any other number
    /// means somebody else wrote to it since.
    own_sequence: u32,
    /// The outcome the caller is waiting for, stored before the worker's message
    /// loop ends so it can be reported from there.
    result: Option<Result<Settlement, String>>,
}

impl Tx {
    fn new() -> Self {
        Self {
            published_at: Instant::now(),
            chord: Chord::Pending,
            injected_at: None,
            reads: Vec::new(),
            foreground_held: false,
            taken_over: false,
            cancelled: false,
            settled: false,
            own_sequence: 0,
            result: None,
        }
    }

    /// The most recent read that happened while, or after, the chord was sent.
    fn last_read(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.reads.iter().copied().rev().find(|at| *at >= injected)
    }
}

/// The state of one transaction.
struct Job {
    tx: Mutex<Tx>,
    /// The transcript, kept for the whole transaction: the promise is rendered
    /// from here whenever the clipboard asks for the data.
    text: Arc<String>,
}

/// The transaction that currently owns the clipboard promise, if any. A new
/// transaction replaces it, and the replaced one must not write anything: the
/// clipboard belongs to the newer transaction by then.
static PENDING: Mutex<Option<Arc<Job>>> = Mutex::new(None);

/// How the worker's publish step went, as the caller needs to know it.
enum Startup {
    /// The promise is on the clipboard; the caller may send the chord.
    Published,
    /// There is nothing to wait for: the text is already ordinary clipboard text.
    Settled(Settlement),
    /// Neither happened, and the clipboard holds no dictation text at all.
    Unavailable(String),
}

/* -------------------------------------------------------------------------- */
/* Public entry points                                                         */
/* -------------------------------------------------------------------------- */

/// The window in the foreground right now, when Windows reports one.
pub fn foreground_window() -> Option<HWND> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd)
}

/// The process a window belongs to, when Windows says.
fn window_process(hwnd: HWND) -> Option<u32> {
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    (process != 0).then_some(process)
}

/// One dictation: `text` reaches `target` through the clipboard, or it is left
/// in the clipboard alone for one manual paste.
///
/// `target` is the window the caller wants the text in, as it stands at this
/// moment. A `None` target — or a target that does not own the foreground when
/// the chord would be sent, or that loses it while the chord is being sent —
/// never receives a `Ctrl+V`; the transcript is published and settled in the
/// clipboard instead, which is reported as [`Settlement::CopiedFallback`].
///
/// [`Settlement::Inserted`] is only returned after Windows reported a clipboard
/// read to the owner *after* the chord, the reads went quiet, the foreground
/// stayed on the target across the chord, and the transcript was materialized as
/// ordinary `CF_UNICODETEXT` while the clipboard was still ours.
///
/// An `Err` means the clipboard was taken by somebody else (their copy was left
/// untouched), or the text could not be written to it at all.
///
/// Blocking: waits for the read receipt, the quiet period, and at most
/// [`SETTLE_TIMEOUT`]. Never run it on a thread that has to answer messages.
pub fn transact(text: Arc<String>, target: Option<HWND>) -> Result<Settlement, String> {
    let job = Arc::new(Job {
        tx: Mutex::new(Tx::new()),
        text,
    });
    let (ready_tx, ready_rx) = mpsc::channel();
    let (settled_tx, settled_rx) = mpsc::channel();
    let worker = job.clone();
    thread::Builder::new()
        .name("speechek-paste".to_string())
        .spawn(move || pump(worker, ready_tx, settled_tx))
        .map_err(|err| format!("cannot start the clipboard worker ({err})"))?;

    match ready_rx.recv() {
        Ok(Ok(Startup::Published)) => {}
        Ok(Ok(Startup::Settled(settlement))) => return Ok(settlement),
        Ok(Ok(Startup::Unavailable(err))) => return Err(err),
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            return Err("the clipboard worker stopped before the text was published".to_string())
        }
    }

    // The chord: only a target that owns the foreground may get one, and only if
    // it still does right before the keystrokes and right after them. The pre-
    // check is the caller's own read, so a foreground that moved between the two
    // calls below cannot be mistaken for the target.
    let chord_target =
        target.filter(|target| foreground_window().is_some_and(|front| front == *target));
    match chord_target {
        None => {
            let mut tx = job.tx.lock();
            tx.chord = Chord::Skipped;
        }
        Some(target) => {
            {
                let mut tx = job.tx.lock();
                tx.chord = Chord::Sent;
                // Marked before the keystrokes go out: a fast target may
                // legitimately read while the chord is still held.
                tx.injected_at = Some(Instant::now());
            }
            let sent = send_paste_chord();
            let mut tx = job.tx.lock();
            if let Err(err) = sent {
                note(&format!("the paste chord could not be sent ({err})"));
                tx.chord = Chord::Failed;
            }
            tx.foreground_held =
                tx.chord == Chord::Sent && foreground_window().is_some_and(|front| front == target);
        }
    }

    match settled_rx.recv() {
        Ok(result) => result,
        Err(_) => Err("the clipboard worker stopped without settling the text".to_string()),
    }
}

/// [`transact`] for whichever window owns the foreground at this moment, as long
/// as it is not speechek's own: the foreground is read *inside* the transaction,
/// so the window that is active when the text is ready receives it.
///
/// A window whose process cannot be established is not trusted with a chord
/// either; like `own_process` itself and a missing foreground, it leaves the
/// transcript in the clipboard alone.
pub fn transact_foreground(text: Arc<String>, own_process: u32) -> Result<Settlement, String> {
    let target = foreground_window()
        .filter(|hwnd| window_process(*hwnd).is_some_and(|process| process != own_process));
    transact(text, target)
}

/* -------------------------------------------------------------------------- */
/* The transaction's two threads                                               */
/* -------------------------------------------------------------------------- */

/// The worker: it owns the hidden window, the clipboard promise and the wait for
/// a read receipt. The calling thread only decides whether a chord may be sent.
fn pump(
    job: Arc<Job>,
    ready: mpsc::Sender<Result<Startup, String>>,
    settled: mpsc::Sender<Result<Settlement, String>>,
) {
    let hwnd = match create_window() {
        Ok(hwnd) => hwnd,
        Err(err) => {
            let _ = ready.send(Ok(Startup::Unavailable(err)));
            return;
        }
    };
    unsafe {
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Arc::into_raw(job.clone()) as *const Job as isize,
        );
    }

    let startup = start(hwnd, &job);
    if let Startup::Published = startup {
        let _ = ready.send(Ok(Startup::Published));
        run_message_loop(hwnd);
        let result = job.tx.lock().result.take().unwrap_or_else(|| {
            Err("the clipboard window closed before the text settled".to_string())
        });
        let _ = settled.send(result);
    } else {
        // Nothing was published, so there is nothing to wait for: the caller
        // gets the whole outcome here.
        let _ = ready.send(Ok(startup));
    }

    unsafe { destroy_window(hwnd) };
}

/// Publishes the promise and arms the timer that settles it. Everything that can
/// go wrong here is answered before the caller is told to send a chord.
fn start(hwnd: HWND, job: &Arc<Job>) -> Startup {
    // A previous transaction's promise is taken over by this one; that worker is
    // told to stop, and must not write, because the clipboard is ours to fill.
    flush_pending();

    let sequence = match unsafe { publish(hwnd) } {
        Ok(sequence) => sequence,
        Err(err) => return recover(hwnd, job, err),
    };
    job.tx.lock().own_sequence = sequence;

    // The promise is live from here on, so a newer transaction can take it over
    // even if this transaction never gets any further.
    *PENDING.lock() = Some(job.clone());

    // The timer is the only thing that can settle the promise, so a transaction
    // without one is not started at all.
    if unsafe { SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None) } == 0 {
        release_pending(job);
        return recover(
            hwnd,
            job,
            "cannot watch the clipboard promise (SetTimer)".to_string(),
        );
    }

    note("published the transcript as a clipboard promise");
    Startup::Published
}

/// The promise could not be set up. `publish` may already have emptied the
/// clipboard, so it can be ours and empty: write the transcript as ordinary text
/// when that is possible — the user is left with a clipboard, not with nothing.
fn recover(hwnd: HWND, job: &Job, err: String) -> Startup {
    match unsafe { materialize(hwnd, job) } {
        Ok(()) => Startup::Settled(Settlement::CopiedFallback),
        Err(_) => Startup::Unavailable(err),
    }
}

fn run_message_loop(hwnd: HWND) {
    let mut message = MSG::default();
    loop {
        let received = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if !received.as_bool() {
            break;
        }
        unsafe {
            let _ = DispatchMessageW(&message);
        }
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_ID);
    }
}

/// The window is the only thing that keeps a transaction alive: once it goes, so
/// does the promise (Windows asks for the data one last time on the way out, in
/// `WM_RENDERALLFORMATS`, when the clipboard is still ours).
unsafe fn destroy_window(hwnd: HWND) {
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Job;
    let _ = DestroyWindow(hwnd);
    if !raw.is_null() {
        drop(Arc::from_raw(raw));
    }
}

/* -------------------------------------------------------------------------- */
/* Deciding                                                                    */
/* -------------------------------------------------------------------------- */

/// One decision point of a transaction. `None` means "keep waiting".
fn decide(tx: &Tx, now: Instant) -> Option<Decision> {
    if tx.cancelled {
        return Some(Decision::Cancelled);
    }
    if tx.taken_over {
        return Some(Decision::TakenOver);
    }
    let elapsed = now.duration_since(tx.published_at);
    match tx.chord {
        // The caller is still deciding whether a chord may be sent. It normally
        // reports within milliseconds; a caller that never reports must not
        // strand the promise in the clipboard.
        Chord::Pending => (elapsed >= SETTLE_TIMEOUT).then_some(Decision::CopiedFallback),
        // Nothing was sent, and nothing can arrive.
        Chord::Skipped => Some(Decision::CopiedFallback),
        Chord::Failed => (elapsed >= FAILED_CHORD_TIMEOUT).then_some(Decision::CopiedFallback),
        Chord::Sent => match tx.last_read() {
            // Reads went quiet: this is as close to "the target pasted it" as
            // the operating system lets anyone get. It is still a read, not a
            // paste, so the foreground check has the last word.
            Some(last) if now.duration_since(last) >= QUIET_PERIOD => Some(if tx.foreground_held {
                Decision::Inserted
            } else {
                Decision::CopiedFallback
            }),
            // No read inside the deadline: the target never took the data, so
            // nothing was pasted and the transcript is only in the clipboard.
            _ if elapsed >= SETTLE_TIMEOUT => Some(Decision::CopiedFallback),
            _ => None,
        },
    }
}

/// Settles the transaction: decide once, write the clipboard once, answer the
/// caller once, and end the worker's message loop.
fn on_timer(hwnd: HWND, job: &Job) {
    let decision = {
        let tx = job.tx.lock();
        decide(&tx, Instant::now())
    };
    let Some(decision) = decision else {
        return;
    };
    if std::mem::replace(&mut job.tx.lock().settled, true) {
        // Windows can deliver one more timer tick before the loop ends; nothing
        // may be settled (or written) twice.
        return;
    }

    let result = match decision {
        Decision::Cancelled => Err("a newer dictation replaced this text".to_string()),
        Decision::TakenOver => {
            Err("another window took the clipboard; that copy was left as it is".to_string())
        }
        Decision::Inserted => unsafe { materialize(hwnd, job) }.map(|()| Settlement::Inserted),
        Decision::CopiedFallback => {
            unsafe { materialize(hwnd, job) }.map(|()| Settlement::CopiedFallback)
        }
    };
    match &result {
        Ok(Settlement::Inserted) => note("the target read the transcript after the chord"),
        Ok(Settlement::CopiedFallback) => {
            note("no confirmed paste; the transcript stays in the clipboard")
        }
        Err(err) => note(err),
    }

    release_pending(job);
    job.tx.lock().result = Some(result);
    unsafe { PostQuitMessage(0) };
}

/// A new transaction takes the clipboard over from an older one. The older
/// worker keeps its text and reports its own outcome, but it must not write:
/// the clipboard belongs to the newer transaction from here on.
fn flush_pending() {
    let previous = PENDING.lock().take();
    if let Some(previous) = previous {
        previous.tx.lock().cancelled = true;
    }
}

/// Gives up the pending slot when this transaction is the one holding it.
fn release_pending(job: &Job) {
    let mut slot = PENDING.lock();
    let ours = slot
        .as_ref()
        .is_some_and(|pending| Arc::as_ptr(pending) as *const Job == job as *const Job);
    if ours {
        *slot = None;
    }
}

/* -------------------------------------------------------------------------- */
/* The clipboard                                                               */
/* -------------------------------------------------------------------------- */

/// Publishes the transcript as a promise: a delayed-render `CF_UNICODETEXT` entry
/// that Windows asks the owner for (`WM_RENDERFORMAT`) once somebody reads it,
/// plus the clipboard-history and cloud-sync opt-outs Chrome uses for incognito
/// copies. Without the opt-outs a clipboard manager would read the promise the
/// moment it appears, which is exactly the read that must not be mistaken for a
/// paste.
///
/// The delayed handle is NULL, and `SetClipboardData` returns NULL for it: success
/// is only distinguishable from failure through the thread error, which therefore
/// has to be cleared first.
unsafe fn publish(hwnd: HWND) -> Result<u32, String> {
    open_clipboard(hwnd)?;
    let published = publish_formats();
    let closed = CloseClipboard();
    published?;
    closed.map_err(|err| format!("cannot close the clipboard ({err})"))?;
    Ok(GetClipboardSequenceNumber())
}

/// Everything `publish` does while the clipboard is open, split out so the
/// clipboard is closed on every path.
unsafe fn publish_formats() -> Result<(), String> {
    EmptyClipboard().map_err(|err| format!("cannot empty the clipboard ({err})"))?;

    for (name, value) in [
        ("ExcludeClipboardContentFromMonitorProcessing", 1u32),
        ("CanIncludeInClipboardHistory", 0u32),
        ("CanUploadToCloudClipboard", 0u32),
    ] {
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let format = RegisterClipboardFormatW(PCWSTR(name.as_ptr()));
        if format == 0 {
            continue;
        }
        let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>()) else {
            continue;
        };
        let pointer = GlobalLock(handle) as *mut u32;
        if pointer.is_null() {
            let _ = GlobalFree(Some(handle));
            continue;
        }
        *pointer = value;
        let _ = GlobalUnlock(handle);
        if SetClipboardData(format, Some(HANDLE(handle.0))).is_err() {
            let _ = GlobalFree(Some(handle));
        }
    }

    SetLastError(ERROR_SUCCESS);
    if let Err(err) = SetClipboardData(CF_UNICODETEXT.0 as u32, None) {
        if err.code().is_err() {
            return Err(format!("cannot promise the text to the clipboard ({err})"));
        }
    }
    Ok(())
}

/// Turns the promise into ordinary Unicode clipboard text: what the user is left
/// with, readable by every application and by clipboard history without asking
/// this process for anything.
///
/// The write only happens while the clipboard is still ours. A copy the user made
/// during the transaction — or another transaction's promise — is never
/// clobbered; the caller reports the error instead.
unsafe fn materialize(hwnd: HWND, job: &Job) -> Result<(), String> {
    open_clipboard(hwnd)?;
    let outcome = write_text(hwnd, job);
    let _ = CloseClipboard();
    outcome
}

/// The guarded write, with the clipboard already open and designated as ours.
unsafe fn write_text(hwnd: HWND, job: &Job) -> Result<(), String> {
    let ours = GetClipboardOwner()
        .map(|owner| owner == hwnd)
        .unwrap_or(false);
    let sequence = GetClipboardSequenceNumber();
    let expected = job.tx.lock().own_sequence;
    if !ours || (expected != 0 && sequence != expected) {
        return Err("another window took the clipboard before the text could be kept".to_string());
    }
    EmptyClipboard().map_err(|err| format!("cannot empty the clipboard ({err})"))?;
    render_text(job)
}

/// Writes the transcript as plain `CF_UNICODETEXT`. The clipboard must be open and
/// ours; the same bytes are written whenever a reader asks for the promise and
/// when the transaction settles, so a reader never sees anything else.
unsafe fn render_text(job: &Job) -> Result<(), String> {
    let wide: Vec<u16> = job.text.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)
        .map_err(|err| format!("cannot allocate the clipboard text ({err})"))?;
    let pointer = GlobalLock(handle) as *mut u16;
    if pointer.is_null() {
        let _ = GlobalFree(Some(handle));
        return Err("cannot lock the clipboard text".to_string());
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len());
    let _ = GlobalUnlock(handle);
    if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(handle.0))).is_err() {
        let _ = GlobalFree(Some(handle));
        return Err("cannot put the text on the clipboard".to_string());
    }
    // Our own write moved the sequence number; the guard compares against the
    // last one we wrote, not against the promise's.
    job.tx.lock().own_sequence = GetClipboardSequenceNumber();
    Ok(())
}

/// Opens the clipboard with this window as its owner, retrying while another
/// process holds it. Opening it with the owner is what lets the following
/// `EmptyClipboard` and `SetClipboardData` succeed: opening with `NULL` would
/// clear the owner and make the write fail.
unsafe fn open_clipboard(hwnd: HWND) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..OPEN_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(OPEN_RETRY);
        }
        match OpenClipboard(Some(hwnd)) {
            Ok(()) => return Ok(()),
            Err(err) => last = err.to_string(),
        }
    }
    Err(format!("cannot open the clipboard ({last})"))
}

/* -------------------------------------------------------------------------- */
/* The chord                                                                   */
/* -------------------------------------------------------------------------- */

/// Sends the one chord speechek uses: `Ctrl+V`, with the modifier held for
/// [`CHORD_HOLD_MS`] after the `V` click. The modifier is released on every path,
/// so a failed chord never leaves `Ctrl` held down.
fn send_paste_chord() -> Result<(), String> {
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|err| format!("cannot reach the keyboard ({err})"))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|err| format!("cannot hold Ctrl ({err})"))?;
    let clicked = enigo.key(Key::Other(VK_V), Direction::Click);
    if clicked.is_ok() {
        thread::sleep(Duration::from_millis(CHORD_HOLD_MS));
    }
    let released = enigo.key(Key::Control, Direction::Release);
    clicked.map_err(|err| format!("cannot press V ({err})"))?;
    released.map_err(|err| format!("cannot release Ctrl ({err})"))?;
    Ok(())
}

/* -------------------------------------------------------------------------- */
/* The hidden clipboard owner                                                  */
/* -------------------------------------------------------------------------- */

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // A consumer read the promise. The system opened the clipboard for us;
        // its handle is the read receipt, and the data is rendered on the spot so
        // the reader gets the transcript rather than an empty handle.
        WM_RENDERFORMAT => {
            if let Some(job) = job_of(hwnd) {
                job.tx.lock().reads.push(Instant::now());
                if wparam.0 as u32 == CF_UNICODETEXT.0 as u32 {
                    let _ = render_text(job);
                }
            }
            LRESULT(0)
        }
        // Sent when the owner window goes away while a promise is still on the
        // clipboard: not a read, so no receipt. The clipboard has to be opened
        // (and still be ours) before the data can be rendered.
        WM_RENDERALLFORMATS => {
            if let Some(job) = job_of(hwnd) {
                if OpenClipboard(Some(hwnd)).is_ok() {
                    if GetClipboardOwner()
                        .map(|owner| owner == hwnd)
                        .unwrap_or(false)
                    {
                        let _ = render_text(job);
                    }
                    let _ = CloseClipboard();
                }
            }
            LRESULT(0)
        }
        // The clipboard was emptied or taken over. Emptying the clipboard we own
        // ourselves is not a takeover: only another window becoming the owner is,
        // and that is what the owner check tells apart.
        WM_DESTROYCLIPBOARD => {
            if let Some(job) = job_of(hwnd) {
                let theirs = GetClipboardOwner()
                    .map(|owner| owner != hwnd)
                    .unwrap_or(false);
                if theirs {
                    job.tx.lock().taken_over = true;
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if let Some(job) = job_of(hwnd) {
                on_timer(hwnd, job);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// The transaction a window belongs to; none until the worker stored it.
unsafe fn job_of(hwnd: HWND) -> Option<&'static Job> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Job).as_ref()
}

/// Creates the hidden message-only window that owns the clipboard promise.
fn create_window() -> Result<HWND, String> {
    let hinstance = unsafe {
        GetModuleHandleW(PCWSTR::null())
            .map(|module| HINSTANCE(module.0))
            .map_err(|err| format!("cannot reach the process module ({err})"))?
    };
    ensure_window_class(hinstance);
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("speechek paste"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
        .map_err(|err| format!("cannot create the clipboard window ({err})"))
    }
}

/// One class for every transaction; message-only windows of a class can be
/// created from several threads at once, so this runs exactly once.
fn ensure_window_class(hinstance: HINSTANCE) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&class);
        }
    });
}

/// The release build has no console; this is what a developer running the debug
/// build sees of a transaction.
fn note(message: &str) {
    eprintln!("speechek: paste: {message}");
}
