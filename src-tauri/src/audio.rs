//! Speechek's temporary mute of the Windows default render endpoint.
//!
//! One setting decides whether a take silences the system while it records.
//! This module owns that silence: nothing else in the shell touches the
//! endpoint, and no other device API is used — a take mutes the *default render
//! endpoint in the `eConsole` role*, the master mute the user sees when they
//! press the mute key, and gives it back when the take is over.
//!
//! The mute is scoped to the capture itself: it is applied once the microphone
//! stream of a dictation is really running (see `capture::session_thread`) and
//! given back wherever that stream ends — a stop, a cancel, a failed start, the
//! arming wake-up, the shell leaving, or the cleanup of a session that a newer
//! dictation replaced. Every operation carries the generation of the dictation
//! it belongs to, and the worker below holds the endpoint for exactly one
//! generation at a time, so a late release left over from a finished dictation
//! can never unmute the take that replaced it. Ownership alone is not enough
//! for the other direction: the endpoint forgets a take the moment its mute is
//! given back — and a take that asked for no mute was never on it — so the
//! worker also remembers the highest generation it admitted and the highest it
//! released, independent of the endpoint. An engage of a generation that was
//! already released opens nothing and mutes nothing, whatever the endpoint
//! looks like by then, and after the shell's exit no engage is admitted at all.
//!
//! Giving back the mute is conditional: the endpoint is only unmuted when it is
//! still the mute *this module* set. A user who unmuted — or unmuted and muted
//! again — while a take ran keeps their choice, which the endpoint's own change
//! notification reports; a mute the user had set before the take is never
//! cleared either. Everything here is best effort: a device that disappeared, a
//! COM call that failed or a worker that did not answer is reported so the pill
//! can show [`MUTE_FAILED`], and the dictation goes on recording regardless.
//!
//! The decision logic (`Book` over the `Endpoint` trait) is plain Rust and is
//! unit-tested; the COM plumbing below it runs on one worker thread, which is
//! where the apartment and the endpoint interfaces live.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, LazyLock};
use std::thread;
use std::time::Duration;

use windows::core::{implement, GUID};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDIO_VOLUME_NOTIFICATION_DATA, IMMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// The one fixed warning a failed mute produces, shown on the pill exactly as
/// written — it says what happened, not why, and never names a device.
pub const MUTE_FAILED: &str = "Не удалось отключить звук во время записи";

/// The event context every `SetMute` of this module carries, so the change
/// notification can tell this module's own mute from one the user made.
const MUTE_CONTEXT: GUID = GUID::from_u128(0x6f1c3f7a_1c9e_4c8b_a0a4_9f2f0f6d3c11);

/// `CLSID_MMDeviceEnumerator` (`{BCDE0395-E52F-467C-8E3D-C4579291692E}`), which
/// the windows crate does not generate for this interface.
const CLSID_MMDEVICE_ENUMERATOR: GUID = GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);

/// A COM call that has not answered by this point is not going to; the capture
/// thread that is winding down is never held longer than this.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);

/* -------------------------------------------------------------------------- */
/* Decision                                                                    */
/* -------------------------------------------------------------------------- */

/// The endpoint's mute, as the session logic needs it.
trait Endpoint {
    /// The endpoint's current mute.
    fn mute(&mut self) -> Result<bool, String>;
    /// Sets the endpoint's mute.
    fn set_mute(&mut self, muted: bool) -> Result<(), String>;
    /// Forgets the changes reported so far: a take that has just written its
    /// own mute owns the endpoint again, and only what happens after that is
    /// somebody else's choice.
    fn forget_changes(&mut self);
    /// Whether somebody changed the mute while this module held it. The
    /// endpoint's change notification reports that; a user who unmuted or
    /// re-muted during a take keeps their choice because of it.
    fn touched(&self) -> bool;
}

/// What this module did to the endpoint for the session that owns it.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Book {
    /// The dictation whose capture holds the endpoint. One take at a time, and
    /// a release of any other generation is a late release: it is ignored.
    owner: Option<u64>,
    /// This module set the endpoint muted, and expects it to still be.
    applied: bool,
    /// The mute the endpoint had before this module touched it. Only meaningful
    /// while `applied` is set.
    original: bool,
}

/// Mutes the endpoint for `generation`, unless the user turned the setting off.
///
/// A session that never released — the capture thread of a stale dictation —
/// is handed over here: when this take wants the same silence and the endpoint
/// is still exactly as that session left it, only the owner moves and the
/// endpoint is not touched; otherwise the stale mute is given back first.
fn mute_for<E: Endpoint>(
    book: &mut Book,
    endpoint: &mut E,
    generation: u64,
    enabled: bool,
) -> Result<(), String> {
    if let Some(owner) = book.owner {
        if owner == generation {
            return Ok(());
        }
        if generation < owner {
            // A thread that was given up on can finish opening its device after
            // a newer dictation already owns the endpoint. Its engage is stale:
            // taking the endpoint over would let its own late release unmute
            // the take that replaced it.
            return Ok(());
        }
        if enabled && !endpoint.touched() {
            // The mute is already on and still this module's: the take that
            // starts now inherits the owner, and with it the original value the
            // stale session recorded.
            book.owner = Some(generation);
            return Ok(());
        }
        give_back(book, endpoint, owner)?;
    }
    if !enabled {
        return Ok(());
    }
    // Read before muting: an endpoint the user had already muted is left as it
    // is, and is never unmuted on the way out.
    let original = endpoint.mute()?;
    if !original {
        // Clear what the endpoint reported before this write: only what happens
        // after the module's own mute is somebody else's choice. A change that
        // lands between the write and a later clear would be erased by it.
        endpoint.forget_changes();
        endpoint.set_mute(true)?;
        book.applied = true;
    } else {
        book.applied = false;
    }
    book.original = original;
    book.owner = Some(generation);
    Ok(())
}

/// Gives the endpoint back for `generation`, and only for it.
///
/// The mute is cleared only when it is still this module's: an endpoint the
/// user had muted, or one whose mute the user changed while the take ran, is
/// left alone. A release that fails keeps the session in the book, so the next
/// engage — or the shell's shutdown — asks for it again instead of forgetting a
/// mute that is still on.
fn give_back<E: Endpoint>(book: &mut Book, endpoint: &mut E, generation: u64) -> Result<(), String> {
    if book.owner != Some(generation) {
        return Ok(());
    }
    if book.applied && !endpoint.touched() && endpoint.mute()? {
        endpoint.set_mute(book.original)?;
    }
    *book = Book::default();
    Ok(())
}

/* -------------------------------------------------------------------------- */
/* Worker                                                                      */
/* -------------------------------------------------------------------------- */

/// The generations the mute worker has already seen, kept for the whole life of
/// the worker and independent of the endpoint and of the book.
///
/// The book forgets a generation as soon as its mute is given back — and a
/// dictation that never engaged at all is never in the book — so nothing about
/// the endpoint can say whether an engage that arrives now belongs to a take
/// that is already over. The marks can: a generation that was released must
/// never silence the system again, whatever the worker happens to be holding
/// when its engage finally arrives.
#[derive(Default, Debug)]
struct Marks {
    /// The highest generation an engage has been processed for. A generation
    /// below it can still be admitted — dictations open their devices
    /// out of order — so this is a high-water mark, not a promise about one
    /// take; it is what the shell's exit drains into `retired`.
    admitted: u64,
    /// The highest generation that was released, or drained by the shell's
    /// exit. A take that reached this point is over: an engage of it that still
    /// arrives — a capture thread that was given up on and is only now
    /// finishing — must not mute the system even though the endpoint it would
    /// have opened has nothing to do with it.
    retired: u64,
    /// The shell has left its capture behind for good; nothing engages after
    /// this, whatever generation it carries.
    closed: bool,
}

impl Marks {
    /// Whether an engage of `generation` may still touch the system. A take
    /// whose generation was already released is over, and so is every take
    /// after the shell's capture was closed.
    fn admits(&self, generation: u64) -> bool {
        !self.closed && generation > self.retired
    }

    /// Records an engage this worker will act on, whatever its setting: the
    /// shell's exit drains every admitted generation into `retired`, so a take
    /// that asked for no silence is fenced exactly like one that did.
    fn admit(&mut self, generation: u64) {
        self.admitted = self.admitted.max(generation);
    }

    /// Records a release, whoever the endpoint belongs to. This is the mark the
    /// fence above reads, so it is written even when there is no endpoint, no
    /// owner, or nothing to give back.
    fn retire(&mut self, generation: u64) {
        self.retired = self.retired.max(generation);
    }

    /// The shell is leaving: everything admitted so far counts as released, and
    /// no engage is ever admitted again.
    fn close(&mut self) {
        self.retired = self.retired.max(self.admitted);
        self.closed = true;
    }
}

/// One instruction for the mute worker, with the channel its answer goes back on.
enum Command {
    /// Mute the default render endpoint for this dictation, when it is enabled.
    Engage {
        generation: u64,
        enabled: bool,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    /// Give the endpoint back, unless a newer dictation already owns it.
    Release {
        generation: u64,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    /// Give back whatever the worker still holds; the shell is leaving.
    Shutdown { reply: mpsc::SyncSender<Result<(), String>> },
}

/// Mutes the system for one dictation's capture, from the pinned settings.
///
/// Called once the microphone stream is running. A failure is reported, never
/// fatal: the caller warns the pill and the dictation records on. An engage of a
/// generation the worker has already released is a no-op — nothing is opened
/// and nothing is muted, whatever the endpoint holds by then — so a capture
/// thread that outlived its dictation cannot silence the system.
pub fn engage(generation: u64, enabled: bool) -> Result<(), String> {
    request(|reply| Command::Engage {
        generation,
        enabled,
        reply,
    })
}

/// Gives the system mute back when the capture of `generation` is over.
///
/// Called for every way that capture can end. A release of a dictation whose
/// mute was already given back — or one a newer dictation does not belong to —
/// changes nothing on the endpoint and succeeds; either way the generation is
/// recorded as released, which is what refuses an engage of it that is still on
/// its way (see [`Marks`]).
pub fn release(generation: u64) -> Result<(), String> {
    request(|reply| Command::Release { generation, reply })
}

/// Best effort for the shell's exit path: gives back a mute whose session
/// thread never made it to its own release, and closes the worker for good —
/// nothing engages after this, whatever generation it carries. Never blocks the
/// exit for long.
pub fn shutdown() {
    if let Err(err) = request(|reply| Command::Shutdown { reply }) {
        eprintln!("speechek: the system mute could not be given back: {err}.");
    }
}

/// Hands one instruction to the worker and waits, but not forever: a stuck
/// device must not hold a capture thread that is winding down.
fn request(build: impl FnOnce(mpsc::SyncSender<Result<(), String>>) -> Command) -> Result<(), String> {
    let sender = commands()?;
    let (reply, answer) = mpsc::sync_channel(1);
    sender
        .send(build(reply))
        .map_err(|_| "the audio mute thread is gone".to_string())?;
    answer
        .recv_timeout(ANSWER_TIMEOUT)
        .map_err(|_| "the audio mute did not answer in time".to_string())?
}

/// The worker, started on the first take that wants a mute and kept for the
/// life of the process: one thread, one apartment, one endpoint at a time.
fn commands() -> Result<&'static mpsc::Sender<Command>, String> {
    static COMMANDS: LazyLock<Result<mpsc::Sender<Command>, String>> = LazyLock::new(|| {
        let (sender, receiver) = mpsc::channel::<Command>();
        let spawned = thread::Builder::new()
            .name("speechek-audio-mute".to_string())
            .spawn(move || serve(receiver));
        match spawned {
            Ok(_) => Ok(sender),
            Err(err) => Err(format!("cannot start the audio mute thread: {err}")),
        }
    });
    match &*COMMANDS {
        Ok(sender) => Ok(sender),
        Err(err) => Err(err.clone()),
    }
}

/// The one thread that talks to the endpoint. The book and the interfaces live
/// here, so the generation guard is this loop's own state and no lock is shared
/// with the capture that is being muted.
fn serve(commands: mpsc::Receiver<Command>) {
    let init_error = match unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        Ok(()) => None,
        Err(err) => Some(format!("cannot open the COM apartment for the audio mute: {err}")),
    };
    let mut endpoint: Option<ComEndpoint> = None;
    let mut book = Book::default();
    let mut marks = Marks::default();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Engage {
                generation,
                enabled,
                reply,
            } => {
                let outcome = match &init_error {
                    Some(err) => {
                        // Nothing can be muted without an apartment, but the
                        // worker still remembers the generation: the fence must
                        // not depend on COM starting up.
                        if marks.admits(generation) {
                            marks.admit(generation);
                        }
                        Err(err.clone())
                    }
                    None => handle_engage(
                        &mut marks,
                        &mut endpoint,
                        &mut book,
                        generation,
                        enabled,
                        ComEndpoint::open,
                    ),
                };
                let _ = reply.send(outcome);
            }
            Command::Release { generation, reply } => {
                let outcome = match &init_error {
                    Some(_) => {
                        marks.retire(generation);
                        Ok(())
                    }
                    None => handle_release(&mut marks, &mut endpoint, &mut book, generation),
                };
                let _ = reply.send(outcome);
            }
            Command::Shutdown { reply } => {
                let outcome = match &init_error {
                    Some(_) => {
                        marks.close();
                        Ok(())
                    }
                    None => handle_shutdown(&mut marks, &mut endpoint, &mut book),
                };
                let _ = reply.send(outcome);
            }
        }
    }
}

/// The engage of one session.
///
/// The interface on hand may still carry the mute of a session that never gave
/// it back — a restore that failed, or a capture thread that never got there.
/// The decision hands it over or gives it back; only when nothing is on hand is
/// the endpoint that is default by now opened for this take, and an endpoint
/// whose first read or write did not work out is not kept: nothing of it is
/// this module's, so the next take opens the default endpoint of that moment
/// instead of an interface that just failed.
///
/// `open` reaches that endpoint; it is a parameter so the decision above can be
/// exercised without COM.
fn handle_engage<E: Endpoint>(
    marks: &mut Marks,
    endpoint: &mut Option<E>,
    book: &mut Book,
    generation: u64,
    enabled: bool,
    open: impl FnOnce() -> Result<E, String>,
) -> Result<(), String> {
    // A take whose generation was already released is over: its engage is
    // refused before anything is opened or written, whatever the endpoint and
    // the book look like by now. A capture thread that was given up on must not
    // silence a system whose dictation is gone, and with nothing holding the
    // endpoint there is nothing else that could stop it.
    if !marks.admits(generation) {
        return Ok(());
    }
    // Recorded whether the setting is on or off: the shell's exit drains every
    // admitted generation into `retired`, so a take that asked for no silence
    // is closed exactly like one that muted.
    marks.admit(generation);
    if let Some(active) = endpoint.as_mut() {
        return mute_for(book, active, generation, enabled);
    }
    // A record without an interface is one a failed restore left behind: there
    // is nothing here to give that mute back through, so it is not kept.
    *book = Book::default();
    if !enabled {
        return Ok(());
    }
    let active = endpoint.insert(open()?);
    if let Err(err) = mute_for(book, active, generation, enabled) {
        // The mute was neither read nor written, and no owner was recorded, so
        // the endpoint and its callback go with the failure.
        *endpoint = None;
        *book = Book::default();
        return Err(err);
    }
    Ok(())
}

/// The release of one session; any other generation is a late release and is
/// ignored, so it cannot give back the mute of the take that replaced it.
fn handle_release<E: Endpoint>(
    marks: &mut Marks,
    endpoint: &mut Option<E>,
    book: &mut Book,
    generation: u64,
) -> Result<(), String> {
    // Recorded before anything else is looked at: this mark is what refuses a
    // late engage of the same generation, so it is written even when there is
    // no endpoint, no owner and nothing to give back.
    marks.retire(generation);
    if book.owner != Some(generation) {
        return Ok(());
    }
    let outcome = match endpoint.as_mut() {
        Some(active) => give_back(book, active, generation),
        None => {
            *book = Book::default();
            Ok(())
        }
    };
    // A restore that failed keeps both the record and the interface: the next
    // take, or the shell's shutdown, asks for the mute again through the same
    // endpoint instead of forgetting one that is still on. Once nothing owns
    // the endpoint, the callback goes and the next take opens the endpoint that
    // is default by then.
    if book.owner.is_none() {
        *endpoint = None;
    }
    outcome
}

/// The shell's exit: give back whatever is still held, whoever it belongs to,
/// and close the worker for good — every generation that asked for a mute is
/// counted as released, and nothing engages after this, whatever arrives.
fn handle_shutdown<E: Endpoint>(
    marks: &mut Marks,
    endpoint: &mut Option<E>,
    book: &mut Book,
) -> Result<(), String> {
    marks.close();
    let outcome = match (book.owner, endpoint.as_mut()) {
        (Some(owner), Some(active)) => give_back(book, active, owner),
        _ => Ok(()),
    };
    *endpoint = None;
    *book = Book::default();
    outcome
}

/* -------------------------------------------------------------------------- */
/* Endpoint                                                                    */
/* -------------------------------------------------------------------------- */

/// The default render endpoint and its mute, as COM hands them out.
struct ComEndpoint {
    volume: IAudioEndpointVolume,
    /// Registered while the endpoint is held, so a mute change the user makes
    /// during a take is never overwritten on the way out.
    notify: IAudioEndpointVolumeCallback,
    touched: Arc<AtomicBool>,
}

impl ComEndpoint {
    fn open() -> Result<Self, String> {
        // SAFETY: every call is made on this module's own worker thread, which
        // has initialized its apartment; the returned interfaces stay alive in
        // `self` for as long as they are used, and are given back before `self`
        // is dropped.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None::<&windows::core::IUnknown>, CLSCTX_ALL)
                    .map_err(|err| format!("cannot reach the audio device enumerator: {err}"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|err| format!("the default playback endpoint is not available: {err}"))?;
            let volume = device
                .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                .map_err(|err| format!("the endpoint volume is not available: {err}"))?;
            let touched = Arc::new(AtomicBool::new(false));
            let notify: IAudioEndpointVolumeCallback = MuteNotify {
                touched: Arc::clone(&touched),
            }
            .into();
            volume
                .RegisterControlChangeNotify(&notify)
                .map_err(|err| format!("cannot watch the endpoint's mute: {err}"))?;
            Ok(Self {
                volume,
                notify,
                touched,
            })
        }
    }
}

impl Endpoint for ComEndpoint {
    fn mute(&mut self) -> Result<bool, String> {
        unsafe { self.volume.GetMute() }
            .map(|muted| muted.as_bool())
            .map_err(|err| format!("cannot read the system mute: {err}"))
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        // SAFETY: the interface is owned by `self`; the context GUID is a
        // constant of this module that outlives the call.
        unsafe { self.volume.SetMute(muted, &MUTE_CONTEXT) }
            .map_err(|err| format!("cannot set the system mute: {err}"))
    }

    fn touched(&self) -> bool {
        self.touched.load(Ordering::SeqCst)
    }

    fn forget_changes(&mut self) {
        self.touched.store(false, Ordering::SeqCst);
    }
}

impl Drop for ComEndpoint {
    fn drop(&mut self) {
        // The callback must not outlive the registration. A failure only means
        // the endpoint is going away anyway.
        unsafe {
            let _ = self.volume.UnregisterControlChangeNotify(&self.notify);
        }
    }
}

/// Watches one endpoint's mute for changes this module did not make.
///
/// A user unmuting during a take — or unmuting and muting again — produces a
/// notification whose event context is not [`MUTE_CONTEXT`]; marking that stops
/// the release from overwriting the user's choice. Volume changes arrive with
/// the mute unchanged and are ignored.
#[implement(IAudioEndpointVolumeCallback)]
struct MuteNotify {
    touched: Arc<AtomicBool>,
}

impl IAudioEndpointVolumeCallback_Impl for MuteNotify_Impl {
    fn OnNotify(
        &self,
        pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA,
    ) -> windows::core::Result<()> {
        // SAFETY: COM passes a pointer to the notification data, valid for the
        // duration of this call; nothing here stores it.
        let data = unsafe { &*pnotify };
        if data.guidEventContext != MUTE_CONTEXT && !data.bMuted.as_bool() {
            self.touched.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/* -------------------------------------------------------------------------- */
/* Tests                                                                       */
/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::{
        give_back, handle_engage, handle_release, handle_shutdown, mute_for, Book, Endpoint, Marks,
    };

    /// An endpoint that records every change and can be told what the user did.
    #[derive(Default)]
    struct Fake {
        muted: bool,
        touched: bool,
        /// Every `set_mute` call, in order.
        sets: Vec<bool>,
        /// Every call, in order, so an ordering inside the logic is visible.
        events: Vec<&'static str>,
        /// The next `set_mute` call fails once.
        fail_set: bool,
        /// `mute()` fails while this is set.
        fail_read: bool,
        /// A change somebody else made is reported while the mute is written.
        touched_on_set: bool,
    }

    impl Endpoint for Fake {
        fn mute(&mut self) -> Result<bool, String> {
            self.events.push("mute");
            if self.fail_read {
                return Err("the mute cannot be read".to_string());
            }
            Ok(self.muted)
        }

        fn set_mute(&mut self, muted: bool) -> Result<(), String> {
            self.events.push(if muted { "set:true" } else { "set:false" });
            if self.fail_set {
                self.fail_set = false;
                return Err("the mute cannot be set".to_string());
            }
            self.muted = muted;
            self.sets.push(muted);
            if self.touched_on_set && muted {
                self.touched = true;
            }
            Ok(())
        }

        fn touched(&self) -> bool {
            self.touched
        }

        fn forget_changes(&mut self) {
            self.events.push("forget");
            self.touched = false;
        }
    }

    /// The take mutes the endpoint while it records and gives it back after.
    #[test]
    fn a_take_mutes_and_gives_the_endpoint_back() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        assert!(fake.muted, "the endpoint records muted");
        assert_eq!(fake.sets, vec![true]);
        assert_eq!(book.owner, Some(1));
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(!fake.muted, "the original mute is back");
        assert_eq!(fake.sets, vec![true, false]);
    }

    /// An endpoint the user had muted before the take is left muted; the take
    /// never clears a mute it did not set.
    #[test]
    fn an_endpoint_the_user_already_muted_is_never_unmuted() {
        let mut book = Book::default();
        let mut fake = Fake {
            muted: true,
            ..Fake::default()
        };
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        assert!(fake.muted);
        assert!(fake.sets.is_empty(), "nothing was changed on the endpoint");
        assert!(!book.applied);
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(fake.muted);
        assert!(fake.sets.is_empty());
    }

    /// The setting off means the endpoint is never read or written.
    #[test]
    fn a_disabled_setting_leaves_the_endpoint_alone() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, false).unwrap();
        assert!(fake.sets.is_empty());
        assert_eq!(book.owner, None);
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(fake.sets.is_empty());
    }

    /// A user who unmutes during the take keeps their choice: the release does
    /// not mute them again.
    #[test]
    fn a_user_unmute_during_the_take_is_not_overwritten() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        // The endpoint's notification reported the user's change.
        fake.touched = true;
        fake.muted = false;
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(!fake.muted, "the user's unmute stands");
        assert_eq!(fake.sets, vec![true], "only the take's own mute was written");
    }

    /// A user who unmutes and mutes again keeps the mute they chose, even
    /// though the endpoint is muted at the release again.
    #[test]
    fn a_user_mute_after_an_unmute_is_left_in_place() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        fake.touched = true;
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(fake.muted);
        assert_eq!(fake.sets, vec![true]);
    }

    /// A session that never released is given back before the next one starts,
    /// and its late release cannot give back the mute of the take that replaced
    /// it.
    #[test]
    fn a_stale_session_cannot_unmute_the_take_that_replaced_it() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        // The thread of dictation 1 never reached its release; dictation 2
        // takes the endpoint over through the same interface.
        mute_for(&mut book, &mut fake, 2, true).unwrap();
        assert_eq!(book.owner, Some(2));
        assert_eq!(fake.sets, vec![true], "dictation 2 found it already muted");
        // The late release of dictation 1 must not give anything back.
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(fake.muted, "dictation 2 keeps the system muted");
        // Its own release does.
        give_back(&mut book, &mut fake, 2).unwrap();
        assert!(!fake.muted);
    }

    /// A take that starts without the mute gives a stale session's mute back
    /// through the interface that session left behind.
    #[test]
    fn a_handover_without_the_setting_gives_the_stale_mute_back() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        // Dictation 2 records with the setting off: the system is not silenced.
        mute_for(&mut book, &mut fake, 2, false).unwrap();
        assert!(!fake.muted, "the stale mute was given back");
        assert_eq!(fake.sets, vec![true, false]);
        assert_eq!(book.owner, None, "dictation 2 owns nothing");
    }

    /// A user who changed the mute while a session was running is not adopted
    /// by the next take, and their choice survives the handover.
    #[test]
    fn a_handover_respects_a_user_change() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        // The user unmuted while the stale session ran, and nobody released it.
        fake.touched = true;
        fake.muted = false;
        // Dictation 2 wants the silence too: it mutes on its own account.
        mute_for(&mut book, &mut fake, 2, true).unwrap();
        assert_eq!(book.owner, Some(2));
        assert!(fake.muted);
        assert_eq!(fake.sets, vec![true, true], "the mute was written again");
        // And gives back its own mute, not the user's earlier state.
        give_back(&mut book, &mut fake, 2).unwrap();
        assert!(!fake.muted);
    }

    /// A release that fails keeps the session in the book, so the mute is asked
    /// for again instead of being forgotten while it is still on.
    #[test]
    fn a_failed_restore_is_reported_and_tried_again() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        fake.fail_set = true;
        assert!(give_back(&mut book, &mut fake, 1).is_err());
        assert_eq!(book.owner, Some(1), "the mute is still unaccounted for");
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(!fake.muted);
        assert_eq!(book.owner, None);
    }

    /// A take that cannot read the endpoint reports it and owns nothing: the
    /// dictation goes on, and no later release touches a mute this take never
    /// set.
    #[test]
    fn a_failed_engage_leaves_the_endpoint_untouched() {
        let mut book = Book::default();
        let mut fake = Fake {
            fail_read: true,
            ..Fake::default()
        };
        assert!(mute_for(&mut book, &mut fake, 1, true).is_err());
        assert_eq!(book.owner, None);
        assert!(fake.sets.is_empty());
    }

    /// The engage of a capture thread that was given up on cannot take the
    /// endpoint over from the dictation that replaced it: neither its engage
    /// nor its late release may unmute the take that is recording now.
    #[test]
    fn an_older_engage_cannot_take_the_endpoint_over() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 2, true).unwrap();
        // Dictation 1 was already given up on, and only now finishes opening
        // its device: its engage must change nothing.
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        assert_eq!(book.owner, Some(2), "dictation 2 keeps the endpoint");
        assert_eq!(fake.sets, vec![true]);
        // The same engage with the setting off must not give the running take's
        // mute back either.
        mute_for(&mut book, &mut fake, 1, false).unwrap();
        assert!(fake.muted, "the running take stays silenced");
        // And the stale thread's own release changes nothing.
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(fake.muted);
        give_back(&mut book, &mut fake, 2).unwrap();
        assert!(!fake.muted);
    }

    /// The endpoint reports what happened before the module's own write as
    /// cleared, and the write comes after that: a change that lands with the
    /// write itself is somebody else's and stays recorded.
    #[test]
    fn the_previous_notifications_are_cleared_before_the_write() {
        let mut book = Book::default();
        let mut fake = Fake::default();
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        assert_eq!(fake.events, vec!["mute", "forget", "set:true"]);
    }

    /// A user change reported while the module writes its mute is not erased:
    /// the release leaves that choice alone.
    #[test]
    fn a_change_reported_with_the_write_is_kept() {
        let mut book = Book::default();
        let mut fake = Fake {
            touched_on_set: true,
            ..Fake::default()
        };
        mute_for(&mut book, &mut fake, 1, true).unwrap();
        assert!(fake.touched(), "the change is still recorded");
        // The user unmuted in the same instant; the release must not mute them.
        fake.muted = false;
        give_back(&mut book, &mut fake, 1).unwrap();
        assert!(!fake.muted, "the user's choice stands");
        assert_eq!(fake.sets, vec![true], "only the take's own mute was written");
    }

    /// An endpoint whose first read failed is not kept: the next take opens the
    /// default endpoint of that moment instead of reusing an interface that is
    /// not this module's.
    #[test]
    fn an_endpoint_that_cannot_be_read_is_dropped() {
        let mut book = Book::default();
        let mut marks = Marks::default();
        let mut endpoint: Option<Fake> = None;
        let outcome = handle_engage(&mut marks, &mut endpoint, &mut book, 1, true, || {
            Ok(Fake {
                fail_read: true,
                ..Fake::default()
            })
        });
        assert!(outcome.is_err());
        assert!(endpoint.is_none(), "an endpoint nothing owns is not kept");
        assert_eq!(book.owner, None);
        // The take that starts now opens an endpoint that answers.
        handle_engage(&mut marks, &mut endpoint, &mut book, 2, true, || {
            Ok(Fake::default())
        })
        .unwrap();
        assert_eq!(book.owner, Some(2));
        assert!(endpoint.as_ref().is_some_and(|active| active.muted));
    }

    /// The same for an endpoint whose first write failed: it is given up on
    /// instead of being cached for a later take.
    #[test]
    fn an_endpoint_that_cannot_be_muted_is_dropped() {
        let mut book = Book::default();
        let mut marks = Marks::default();
        let mut endpoint: Option<Fake> = None;
        let outcome = handle_engage(&mut marks, &mut endpoint, &mut book, 1, true, || {
            Ok(Fake {
                fail_set: true,
                ..Fake::default()
            })
        });
        assert!(outcome.is_err());
        assert!(endpoint.is_none());
        assert_eq!(book.owner, None);
    }

    /// A failed restore recorded for an owner keeps its endpoint, so the next
    /// take — even one that does not want the silence — can give it back
    /// through the same interface.
    #[test]
    fn a_failed_restore_keeps_the_interface_for_the_next_take() {
        let mut book = Book::default();
        let mut marks = Marks::default();
        let mut endpoint: Option<Fake> = None;
        handle_engage(&mut marks, &mut endpoint, &mut book, 1, true, || {
            Ok(Fake::default())
        })
        .unwrap();
        endpoint.as_mut().unwrap().fail_set = true;
        let outcome = handle_release(&mut marks, &mut endpoint, &mut book, 1);
        assert!(outcome.is_err());
        assert!(endpoint.is_some(), "the interface is what can still give it back");
        assert_eq!(book.owner, Some(1));
        // The next take records with the setting off: the stale mute goes back.
        handle_engage(&mut marks, &mut endpoint, &mut book, 2, false, || {
            panic!("the interface on hand is reused")
        })
        .unwrap();
        assert!(!endpoint.as_ref().unwrap().muted);
        assert_eq!(book.owner, None);
    }

    /// A release records its generation even though nothing of it is on hand:
    /// the take was cancelled, or stopped, before its capture thread reached
    /// its engage. The engage that arrives afterwards must not open the default
    /// endpoint, let alone mute it.
    #[test]
    fn an_engage_after_its_release_is_refused_without_an_endpoint() {
        let mut marks = Marks::default();
        let mut book = Book::default();
        let mut endpoint: Option<Fake> = None;
        // The shell released the take before its thread got here; the worker
        // holds nothing for it — no endpoint, no owner.
        handle_release(&mut marks, &mut endpoint, &mut book, 1).unwrap();
        assert_eq!(book.owner, None);
        // Only now does the thread that was given up on reach its engage: it
        // must not open an endpoint, and it must not mute what it would find.
        handle_engage(&mut marks, &mut endpoint, &mut book, 1, true, || {
            panic!("a generation that was released opens no endpoint")
        })
        .unwrap();
        assert!(endpoint.is_none(), "nothing was opened for it");
        assert_eq!(book.owner, None);
        // Nor does the same engage with the setting off open anything.
        handle_engage(&mut marks, &mut endpoint, &mut book, 1, false, || {
            panic!("a generation that was released opens no endpoint")
        })
        .unwrap();
        assert!(endpoint.is_none());
    }

    /// A release closes every generation below it as well: a take whose engage
    /// is still on its way after a newer dictation was already released cannot
    /// mute either, and a dictation that really starts after the release still
    /// can.
    #[test]
    fn a_release_fences_every_older_generation_too() {
        let mut marks = Marks::default();
        let mut book = Book::default();
        let mut endpoint: Option<Fake> = None;
        handle_release(&mut marks, &mut endpoint, &mut book, 2).unwrap();
        handle_engage(&mut marks, &mut endpoint, &mut book, 1, true, || {
            panic!("an older generation cannot engage past a release")
        })
        .unwrap();
        assert!(endpoint.is_none());
        // The next dictation starts after the release and is admitted as usual.
        handle_engage(&mut marks, &mut endpoint, &mut book, 3, true, || {
            Ok(Fake::default())
        })
        .unwrap();
        assert_eq!(book.owner, Some(3));
        assert!(endpoint.as_ref().is_some_and(|active| active.muted));
    }

    /// A late release of a take that was given up on must not give back the
    /// mute of the dictation that replaced it, and must still close its own
    /// generation for any engage that arrives afterwards.
    #[test]
    fn a_late_release_neither_restores_nor_reopens_the_replacement() {
        let mut marks = Marks::default();
        let mut book = Book::default();
        let mut endpoint: Option<Fake> = None;
        handle_engage(&mut marks, &mut endpoint, &mut book, 2, true, || {
            Ok(Fake::default())
        })
        .unwrap();
        assert!(endpoint.as_ref().unwrap().muted);
        // Dictation 1 was cancelled while its device was opening; its release
        // arrives only now, after dictation 2 muted the system.
        handle_release(&mut marks, &mut endpoint, &mut book, 1).unwrap();
        assert!(endpoint.as_ref().unwrap().muted, "dictation 2 stays silenced");
        assert_eq!(book.owner, Some(2), "the endpoint still belongs to dictation 2");
        // And its own engage, arriving after its own release, is refused.
        handle_engage(&mut marks, &mut endpoint, &mut book, 1, true, || {
            panic!("a released generation opens no endpoint")
        })
        .unwrap();
        assert!(endpoint.as_ref().unwrap().muted);
        // Dictation 2's own release gives the system back; the endpoint goes
        // with it, exactly as in `a_take_mutes_and_gives_the_endpoint_back`.
        handle_release(&mut marks, &mut endpoint, &mut book, 2).unwrap();
        assert!(endpoint.is_none(), "the endpoint goes with its own release");
        assert_eq!(book.owner, None);
    }

    /// The shell's exit gives the mute back and closes the worker: neither the
    /// take that was recording nor any later one may engage again.
    #[test]
    fn the_shell_exit_closes_the_worker_for_good() {
        let mut marks = Marks::default();
        let mut book = Book::default();
        let mut endpoint: Option<Fake> = None;
        handle_engage(&mut marks, &mut endpoint, &mut book, 6, true, || {
            Ok(Fake::default())
        })
        .unwrap();
        assert!(endpoint.as_ref().unwrap().muted);
        handle_shutdown(&mut marks, &mut endpoint, &mut book).unwrap();
        assert!(endpoint.is_none(), "the endpoint went with the exit");
        assert_eq!(book.owner, None);
        // A worker of the take that was recording still on its way, and a newer
        // generation, are both refused — nothing opens after the shell left.
        for generation in [6, 7] {
            handle_engage(&mut marks, &mut endpoint, &mut book, generation, true, || {
                panic!("nothing engages after the shell left")
            })
            .unwrap();
        }
        assert!(endpoint.is_none());
    }

    /// The exit counts every generation it saw, setting or no setting, so an
    /// engage that was admitted without a mute is fenced exactly like one that
    /// muted.
    #[test]
    fn the_exit_fences_a_generation_that_never_asked_for_the_mute() {
        let mut marks = Marks::default();
        let mut endpoint: Option<Fake> = None;
        let mut book = Book::default();
        handle_engage(&mut marks, &mut endpoint, &mut book, 4, false, || {
            panic!("the setting is off")
        })
        .unwrap();
        handle_shutdown(&mut marks, &mut endpoint, &mut book).unwrap();
        assert!(!marks.admits(4), "the generation the exit drained is closed");
        assert!(!marks.admits(5), "and so is everything after it");
    }
}
