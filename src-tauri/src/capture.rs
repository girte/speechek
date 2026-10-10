//! Native microphone capture for the overlay and the built-in laboratory.
//!
//! Windows keeps the microphone for the window that is allowed to show the
//! permission prompt, and the recording overlay never takes the foreground: a
//! `getUserMedia` call made there stays pending forever. The shell therefore
//! captures the pinned input device — or the system one — itself, through
//! `cpal`, and hands the renderer the exact frames the shared recorder would
//! have produced on its own: 16 kHz mono PCM16, little-endian, in 512-sample
//! chunks (~32 ms), tagged with the take they belong to and numbered from 1.
//!
//! A take belongs to exactly one owner: a dictation generation, or one take of
//! the laboratory. The owner decides the window every frame and every error
//! goes to, and which stop may end the capture — a stop of the other owner
//! never reaches into it. Only one take ever holds the microphone, and its
//! reservation is in the slot before its worker exists, so a second start of
//! any owner is refused from before the first device is opened.
//!
//! Nothing is written anywhere: a frame is emitted to the window of its take
//! and dropped, so the next take starts from an empty buffer.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio;
use crate::i18n::{self, Language, MessageId, UiError, UiMessage};
use crate::{LAB_LABEL, OVERLAY_LABEL};

#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::diagnostics::{
    CaptureArchive, CaptureAttempt, CaptureStarted, ContentKind, Diagnostics, Event, Outcome,
    Owner, SessionHandle, SessionKey,
};

/// Emitted for every captured frame, to the window of the take it belongs to.
const EVENT_PCM_FRAME: &str = "speechek:pcm-frame";
/// Emitted when the microphone cannot be opened, or stops while it captures,
/// to the window of the take it belongs to.
const EVENT_MIC_ERROR: &str = "speechek:mic-error";
/// Emitted when the system mute of the take could not be set or given back.
/// This is a warning, not a failure: the capture goes on recording.
const EVENT_MUTE_WARNING: &str = "speechek:mute-warning";

/// The recorder's own rate. The providers only ever see 16 kHz.
const RATE_HZ: u32 = 16_000;
/// One frame is 512 samples, the chunk the recorder passes around. Every frame
/// of a take is this size except the very last one, which the flush may leave
/// short; the diagnostics use the same number to price a lost frame, so this is
/// the one place that size is written down.
pub(crate) const FRAME_SAMPLES: usize = 512;
/// A device that has not answered in this long is not going to answer.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// The stable codes capture failures cross the command and event boundary
/// with. The reason itself is the descriptor's key; these only keep the
/// categories apart.
const MICROPHONE_BUSY: &str = "MICROPHONE_BUSY";
const MICROPHONE_ERROR: &str = "MICROPHONE_ERROR";
const CAPTURE_ENDED: &str = "CAPTURE_ENDED";
const CAPTURE_FAILED: &str = "CAPTURE_FAILED";

/// A microphone failure without arguments.
fn mic_failure(key: MessageId) -> UiError {
    UiError::new(MICROPHONE_ERROR, UiMessage::new(key))
}

/// A microphone failure whose one placeholder is the named value. An operating
/// system or driver string is copied in as data — it is never translated and
/// never read again as a template.
fn mic_failure_arg(key: MessageId, name: &'static str, value: Value) -> UiError {
    UiError::new(MICROPHONE_ERROR, UiMessage::new(key).with_arg(name, value))
}

/// A take that ended while its device was still opening: its capture must not
/// start, record or silence anything.
fn ended_while_opening() -> UiError {
    UiError::new(
        CAPTURE_ENDED,
        UiMessage::new(MessageId::CaptureEndedWhileOpening),
    )
}

/// The session thread needs far less than this to close down after a stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the session thread waits for the next callback's samples.
const IDLE_POLL: Duration = Duration::from_millis(2);

/// Formats the shell samples. Anything else a device likes is replaced by one
/// of these whenever the device offers it.
const SAMPLED: [SampleFormat; 3] = [SampleFormat::F32, SampleFormat::I16, SampleFormat::U16];

/// Who one native capture records for: the dictation of a generation, or one
/// take of the built-in laboratory. The owner is fixed for the whole life of a
/// take — every frame, every error and the only stop that may end it follow it.
///
/// A dictation is numbered by the shell's session generation and its events go
/// to the overlay. A laboratory take is numbered by the laboratory's own
/// generation and its events go to the laboratory window. The two number spaces
/// are independent, so the variant, and never the number alone, decides which
/// window hears a take and whose stop reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOwner {
    Dictation(u64),
    Lab(u64),
}

impl CaptureOwner {
    /// The generation this take was numbered with, in its own space.
    fn generation(self) -> u64 {
        match self {
            Self::Dictation(generation) | Self::Lab(generation) => generation,
        }
    }

    /// The window every frame, every error and the mute notice of this take go
    /// to.
    fn label(self) -> &'static str {
        match self {
            Self::Dictation(_) => OVERLAY_LABEL,
            Self::Lab(_) => LAB_LABEL,
        }
    }

    /// Whether this take belongs to a dictation. Only a dictation may touch the
    /// system mute: the mute bookkeeping is numbered by dictation generations,
    /// and a laboratory take never silences anything.
    fn is_dictation(self) -> bool {
        matches!(self, Self::Dictation(_))
    }

    /// Whether both owners name takes of the same kind, however their numbers
    /// differ. A stop of another take of the same kind is the ordinary stale
    /// stop — a newer take of the same window owns the microphone — while a
    /// stop of the other kind is a capture this owner must never touch.
    fn same_kind(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Dictation(_), Self::Dictation(_)) | (Self::Lab(_), Self::Lab(_))
        )
    }

    /// How the worker thread is named for this take.
    fn thread_name(self) -> String {
        match self {
            Self::Dictation(generation) => format!("speechek-capture-{generation}"),
            Self::Lab(generation) => format!("speechek-lab-capture-{generation}"),
        }
    }
}

/// A captured frame: the take it belongs to, its 1-based sequence, and the
/// little-endian PCM16 samples as base64.
#[derive(Clone, Serialize)]
struct Frame {
    generation: u64,
    sequence: u64,
    data: String,
}

/// One line the renderer shows for a capture of a dictation: why the microphone
/// is not coming back, or why the system mute did not work out. The renderer
/// decides how it reads; this is the reason, never a secret. `last` marks the
/// line that arrives after the take is already gone — the shell cancelled it and
/// the sound it silenced was not given back — which the renderer shows on its own
/// and brings the pill back for.
#[derive(Clone, Serialize)]
struct CaptureNotice {
    generation: u64,
    /// The English diagnostic for logs and for callers that predate the
    /// catalog; the current language renders `ui`.
    message: String,
    /// The semantic reason this notice carries; one reason keeps one key in
    /// every language.
    ui: UiMessage,
    last: bool,
}

/* -------------------------------------------------------------------------- */
/* Frames                                                                      */
/* -------------------------------------------------------------------------- */

/// One device sample as a -1.0..=1.0 level, whatever the device speaks.
trait Level {
    fn level(self) -> f32;
}

impl Level for f32 {
    #[inline]
    fn level(self) -> f32 {
        self
    }
}

impl Level for i16 {
    #[inline]
    fn level(self) -> f32 {
        f32::from(self) / 32_768.0
    }
}

impl Level for u16 {
    #[inline]
    fn level(self) -> f32 {
        (f32::from(self) - 32_768.0) / 32_768.0
    }
}

/// The level of one output sample as PCM16.
fn pcm16(level: f32) -> i16 {
    (level.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// How many frames a fresh callback buffer is sized for. WASAPI hands out a few
/// milliseconds at a time, so a buffer of this size is never resized.
const BUFFER_FRAMES: usize = 2_048;

/// One callback's mono buffer, taken from the session thread's pool. The audio
/// thread never waits for one and never frees one: it takes the next buffer the
/// session thread has finished with, and only a pool that ran dry allocates.
struct Buffers {
    free: Receiver<Vec<f32>>,
    /// Capacity of a fresh buffer, in samples.
    size: usize,
}

impl Buffers {
    fn take(&self) -> Vec<f32> {
        self.free
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(self.size))
    }
}

/// Averages one interleaved callback buffer down to mono frames.
fn downmix<T>(channels: usize, data: &[T], mono: &mut Vec<f32>)
where
    T: Level + Copy,
{
    mono.clear();
    mono.reserve(data.len() / channels);
    for frame in data.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for sample in frame {
            sum += sample.level();
        }
        mono.push(sum / channels as f32);
    }
}

/// Hands one callback's interleaved samples to the session thread as mono
/// frames, in a buffer the thread then hands back. The audio thread only
/// copies; resampling, framing and the events all happen on the session thread.
fn sink<T>(
    channels: usize,
    samples: Sender<Vec<f32>>,
    free: Receiver<Vec<f32>>,
    size: usize,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static
where
    T: SizedSample + Level,
{
    let pool = Buffers { free, size };
    move |data: &[T], _| {
        let mut mono = pool.take();
        downmix(channels, data, &mut mono);
        // The only way this fails is a session that is already gone.
        let _ = samples.send(mono);
    }
}

/// Where the framed PCM of one take goes.
///
/// Every production frame goes to the window of the take; the diagnostics
/// fixture collects the same frames in memory instead. The emitter is a type
/// parameter, so a frame is never handed over through a trait object.
trait FrameEmitter {
    /// Why a frame could not be handed over. Only the log reads it.
    type Error: std::fmt::Display;

    fn emit(&mut self, owner: CaptureOwner, frame: Frame) -> Result<(), Self::Error>;
}

/// The window of the take: what every production capture emits to.
struct WindowEmitter {
    app: AppHandle,
}

impl FrameEmitter for WindowEmitter {
    type Error = tauri::Error;

    fn emit(&mut self, owner: CaptureOwner, frame: Frame) -> Result<(), Self::Error> {
        self.app.emit_to(owner.label(), EVENT_PCM_FRAME, frame)
    }
}

/// How far one device attempt got, in microseconds since its device was asked
/// for: zero for a step that never happened.
///
/// Only the diagnostics read this. A build without them carries an empty
/// placeholder, so an attempt costs nothing there.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
#[derive(Clone, Copy)]
struct AttemptTiming {
    /// The moment the attempt began.
    began: Instant,
    /// The rate and the channel count of the device that answered; zero until
    /// a device did.
    source_rate: u32,
    source_channels: u16,
    /// How long the device took to answer, to run and to deliver its first
    /// frame.
    open_us: u64,
    play_us: u64,
    first_frame_us: u64,
}

#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
impl AttemptTiming {
    fn start() -> Self {
        Self {
            began: Instant::now(),
            source_rate: 0,
            source_channels: 0,
            open_us: 0,
            play_us: 0,
            first_frame_us: 0,
        }
    }

    /// The device answered: its rate and its channels are known from here on.
    fn opened(&mut self, input: &Input) {
        self.source_rate = input.rate;
        self.source_channels = input.channels as u16;
        self.open_us = self.elapsed_us();
    }

    /// The stream is running.
    fn played(&mut self) {
        self.play_us = self.elapsed_us();
    }

    /// The first frame of the attempt is out.
    fn framed(&mut self) {
        self.first_frame_us = self.elapsed_us();
    }

    fn elapsed_us(&self) -> u64 {
        self.began.elapsed().as_micros() as u64
    }
}

/// The archive of one capture, as its encoder sees it: the handle of the take,
/// looked up once, and the archive taken lazily on the very first frame.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
struct DiagCapture {
    /// The registered take, or `None` for a sink that is disabled or does not
    /// know this capture: nothing is archived then, and nothing is counted.
    session: Option<SessionHandle>,
    /// Taken on the first frame, never before: a capture that fails before it
    /// records has no archive to close, and leaves nothing behind for the
    /// system fallback of its take to trip over.
    archive: Option<CaptureArchive>,
    /// The take's capture is over: no later frame may open an archive behind
    /// it.
    closed: bool,
}

#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
impl DiagCapture {
    fn of(session: Option<SessionHandle>) -> Self {
        Self {
            session,
            archive: None,
            closed: false,
        }
    }

    /// A capture with no take to archive: the emitter-only constructor's
    /// starting point, before the worker attaches the handle it looked up.
    fn none() -> Self {
        Self::of(None)
    }

    /// One produced frame, from the very bytes the frame is built from and
    /// before its base64 form goes out. The archive is taken on this first
    /// call: a take that never produced a frame never gets one.
    fn push(&mut self, sequence: u64, pcm_le: &[u8]) {
        let Some(session) = &self.session else {
            return;
        };
        if self.closed {
            // The capture is over: a frame that arrives now is not part of it,
            // and it must not open an archive the take has already closed.
            return;
        }
        let archive = self.archive.get_or_insert_with(|| session.capture());
        archive.push(sequence, pcm_le);
    }

    /// The emit of one frame failed: the frame never reached its window.
    fn emit_failed(&mut self) {
        if let Some(session) = &self.session {
            session.emit_failed();
        }
    }

    /// The capture is over: its last sequence is published and the writer is
    /// told to flush the tail. This never encodes, verifies or joins anything.
    fn finish(&mut self, last_sequence: u64, outcome: Outcome) {
        self.closed = true;
        match &mut self.archive {
            Some(archive) => archive.finish(last_sequence, outcome),
            // Nothing was ever captured: the take's capture is over all the
            // same, and no empty archive is opened for it.
            None => {
                if let Some(session) = &self.session {
                    session.end_capture(outcome);
                }
            }
        }
    }
}

/// The take a capture of `owner` archives under, as the worker finds it: the
/// sink the shell manages, and the key of this very take inside it. `None`
/// means there is nothing to archive — the sink is disabled, or this take was
/// never registered.
///
/// The lookup happens once, before the first device is opened, and the handle
/// then lives in the encoder for the whole take: no frame ever touches the
/// registry.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn diagnostics_session(app: &AppHandle, owner: CaptureOwner) -> Option<SessionHandle> {
    let key = SessionKey {
        owner: match owner {
            CaptureOwner::Dictation(_) => Owner::Dictation,
            CaptureOwner::Lab(_) => Owner::Lab,
        },
        generation: owner.generation(),
    };
    app.try_state::<Arc<Diagnostics>>()?.lookup(key)
}

/// A start command that failed before any worker existed: the take's capture
/// never began, so it is over at once, with no audio and no archive. It is
/// closed as the lowest outcome there is, so the terminal the shell really gave
/// this take — a cancel, an error, a window closing — is the one that stands.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_start_failed(session: &Option<SessionHandle>) {
    if let Some(session) = session {
        session.end_capture(Outcome::Abandoned);
    }
}

/// What a refused start does to the take's capture.
///
/// A refusal that names another take — the other window's capture holding the
/// microphone — leaves this start's take with a capture that never began, and
/// that capture is closed here. A refusal of *this very* take is not that: the
/// take it names is already arming or recording, and its capture, its frames
/// and its later end all stay its own.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_refused_start(refusal: &ClaimError, session: &Option<SessionHandle>) {
    if !matches!(refusal, ClaimError::SameTake(_)) {
        note_start_failed(session);
    }
}

/// One device attempt, as the journal records it: the numbers of the attempt
/// and, for one that failed, the catalog key of its reason. Never the device's
/// name or its driver's words.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn attempt_event(
    attempt_index: u32,
    fallback: bool,
    timing: &AttemptTiming,
    ui_key: Option<MessageId>,
) -> Event {
    Event::CaptureAttempt(CaptureAttempt {
        attempt_index,
        source_rate: timing.source_rate,
        source_channels: timing.source_channels,
        fallback,
        ui_key,
        open_us: timing.open_us,
        play_us: timing.play_us,
        first_frame_us: timing.first_frame_us,
    })
}

/// The catalog key of a failed attempt: what the renderer would have shown for
/// it. An attempt that ended because the take was over has no reason of its
/// own.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn failure_key(failure: &AttemptError) -> Option<MessageId> {
    match failure {
        AttemptError::Fallback(error) | AttemptError::Fatal(error) => Some(error.ui.key),
        AttemptError::Ended(_) => None,
    }
}

/// The outcome and the full descriptor of a failed attempt. The driver's own
/// words travel with the descriptor, so a device that would not open, refused
/// the format or was held by another stream can be read back without
/// reproducing the failure. Nothing of it is archived before the caller
/// redacts it against the take's own ring.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn failure_reason(failure: &AttemptError) -> (&'static str, &UiError) {
    match failure {
        AttemptError::Fallback(error) => ("fallback", error),
        AttemptError::Fatal(error) => ("fatal", error),
        AttemptError::Ended(error) => ("ended", error),
    }
}

/// The attempt that recorded, as the journal records it: the rate and the
/// channels of the device that really captured, and how long it took to open,
/// to run and to deliver its first frame.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn started_event(timing: &AttemptTiming) -> Event {
    Event::CaptureStarted(CaptureStarted {
        source_rate: timing.source_rate,
        source_channels: timing.source_channels,
        open_us: timing.open_us,
        play_us: timing.play_us,
        first_frame_us: timing.first_frame_us,
    })
}

/// Turns the device's mono frames into the recorder's frames: 16 kHz mono
/// PCM16, in 512-sample chunks, emitted as they fill up.
///
/// The resampler is a linear interpolator and it keeps its read position across
/// callbacks, so where a callback happens to end never moves a frame boundary.
struct Encoder<E: FrameEmitter = WindowEmitter> {
    /// Where the frames of this take go.
    emitter: E,
    /// The take this encoder frames.
    owner: CaptureOwner,
    /// Sequence of the last emitted frame; zero before the first one.
    sequence: u64,
    /// Source frames still needed for the next interpolation step.
    pending: VecDeque<f32>,
    /// Where the next output sample sits in `pending`, in source frames.
    position: f64,
    /// Source frames per output sample.
    ratio: f64,
    /// PCM16 samples of the frame being filled.
    chunk: Vec<i16>,
    /// Little-endian bytes of `chunk`, reused for every frame.
    bytes: Vec<u8>,
    /// Whether the window of this take has already been reported as
    /// unreachable, so a take cannot fill the console with one line per frame.
    complained: bool,
    /// The archive of this take, for the builds that keep one.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    diagnostics: DiagCapture,
}

impl<E: FrameEmitter> Encoder<E> {
    /// The encoder of one take, framing into `emitter`.
    fn with_emitter(emitter: E, owner: CaptureOwner, source_rate: u32) -> Self {
        Self {
            emitter,
            owner,
            sequence: 0,
            pending: VecDeque::new(),
            position: 0.0,
            ratio: f64::from(source_rate) / f64::from(RATE_HZ),
            chunk: Vec::with_capacity(FRAME_SAMPLES),
            bytes: Vec::with_capacity(FRAME_SAMPLES * 2),
            complained: false,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            diagnostics: DiagCapture::none(),
        }
    }

    /// Feeds one callback's worth of mono source frames.
    fn push(&mut self, frames: &[f32]) {
        self.pending.extend(frames.iter().copied());
        // Two source frames are needed for one interpolation step.
        while self.position + 1.0 < self.pending.len() as f64 {
            let index = self.position as usize;
            let fraction = (self.position - index as f64) as f32;
            let first = self.pending[index];
            let second = self.pending[index + 1];
            self.sample(first + (second - first) * fraction);
            self.position += self.ratio;
        }
        // Frames before the next output sample are never needed again. The
        // buffer can end before that sample does: dropping all of it and
        // keeping the remainder in `position` puts the next callback's frames
        // exactly where they belong.
        let consumed = (self.position as usize).min(self.pending.len());
        if consumed > 0 {
            self.pending.drain(..consumed).for_each(drop);
            self.position -= consumed as f64;
        }
    }

    /// Sends what a stopped capture already resampled. The source frames that
    /// were still waiting for their neighbour are dropped with the stream: they
    /// are less than one output sample, and guessing them would only add noise.
    fn flush(&mut self) {
        if !self.chunk.is_empty() {
            self.frame();
        }
    }

    fn sample(&mut self, level: f32) {
        self.chunk.push(pcm16(level));
        if self.chunk.len() >= FRAME_SAMPLES {
            self.frame();
        }
    }

    fn frame(&mut self) {
        self.sequence += 1;
        let sequence = self.sequence;
        self.bytes.clear();
        for sample in &self.chunk {
            self.bytes.extend_from_slice(&sample.to_le_bytes());
        }
        // The archive sees the very bytes this frame is built from, before the
        // window does: a frame the window never gets is still a frame of the
        // take, and a gap the archive can account for.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        self.diagnostics.push(sequence, &self.bytes);
        let data = BASE64.encode(&self.bytes);
        let frame = Frame {
            generation: self.owner.generation(),
            sequence,
            data,
        };
        if let Err(err) = self.emitter.emit(self.owner, frame) {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            self.diagnostics.emit_failed();
            if !self.complained {
                self.complained = true;
                eprintln!(
                    "speechek: cannot hand captured frames to the {} window: {err}.",
                    self.owner.label()
                );
            }
        }
        self.chunk.clear();
    }
}

impl Encoder<WindowEmitter> {
    /// The production encoder of one take: its frames go to the window of the
    /// take. The take's diagnostics handle is attached by the worker, from the
    /// one lookup it made before the device was opened.
    fn new(app: &AppHandle, owner: CaptureOwner, source_rate: u32) -> Self {
        Self::with_emitter(WindowEmitter { app: app.clone() }, owner, source_rate)
    }
}

#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
impl<E: FrameEmitter> Encoder<E> {
    /// Attaches the registered take this capture archives under. A sink that is
    /// disabled, or one that does not know the key, leaves it unattached and
    /// archives nothing.
    fn with_session(&mut self, session: Option<SessionHandle>) {
        self.diagnostics = DiagCapture::of(session);
    }
    fn finish_diagnostics(&mut self, outcome: Outcome) {
        let last = self.sequence;
        self.diagnostics.finish(last, outcome);
    }
}

/* -------------------------------------------------------------------------- */
/* Diagnostics fixture                                                         */
/* -------------------------------------------------------------------------- */

/// One persistent encoder whose frames are collected instead of emitted: the
/// headless seam a real capture is driven through, with no `AppHandle` and no
/// microphone.
///
/// The encoder is created once and kept across every batch, so its sequence
/// and its resampler position are the ones a live capture would have.
#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
pub(crate) struct DiagnosticsFramingFixture {
    encoder: Encoder<CollectEmitter>,
    session: SessionHandle,
    finished: bool,
}

/// Collects the frames of one take, base64 and all, exactly as they would have
/// been emitted to its window. An emitter the fixture switched to failing keeps
/// nothing: it is the window that can no longer be reached.
#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
#[derive(Default)]
struct CollectEmitter {
    /// The frames handed over so far.
    frames: Vec<(u64, Vec<u8>)>,
    /// Whether the window refuses them, as an unreachable window does.
    failing: bool,
}

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
impl FrameEmitter for CollectEmitter {
    type Error = &'static str;

    fn emit(&mut self, _owner: CaptureOwner, frame: Frame) -> Result<(), Self::Error> {
        if self.failing {
            return Err("the window of this take cannot be reached");
        }
        let bytes = BASE64
            .decode(frame.data.as_bytes())
            .expect("an emitted frame carries base64 PCM");
        self.frames.push((frame.sequence, bytes));
        Ok(())
    }
}

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
impl DiagnosticsFramingFixture {
    /// A take of `session`, fed source frames at `source_rate`.
    pub(crate) fn new(key: SessionKey, source_rate: u32, session: SessionHandle) -> Self {
        let owner = match key.owner {
            Owner::Dictation => CaptureOwner::Dictation(key.generation),
            Owner::Lab => CaptureOwner::Lab(key.generation),
        };
        let mut encoder = Encoder::with_emitter(CollectEmitter::default(), owner, source_rate);
        encoder.with_session(Some(session.clone()));
        Self {
            encoder,
            session,
            finished: false,
        }
    }

    /// One batch of mono source frames, at the fixture's source rate.
    pub(crate) fn push(&mut self, source: &[f32]) {
        self.encoder.push(source);
    }

    /// The frames emitted since the last call: their 1-based sequence and the
    /// PCM16 little-endian samples the window would have received.
    pub(crate) fn take_frames(&mut self) -> Vec<(u64, Vec<u8>)> {
        std::mem::take(&mut self.encoder.emitter.frames)
    }

    /// The frames the writer has taken out of this take's queue: the watermark
    /// a driver waits for between batches, so a push never outruns the pool.
    pub(crate) fn consumed_watermark(&self) -> u64 {
        self.session.audio_consumed()
    }

    /// The window of the take becomes unreachable: every frame from here on is
    /// produced and archived, but never handed over.
    pub(crate) fn fail_emits(&mut self) {
        self.encoder.emitter.failing = true;
    }

    /// The installation of this capture was refused: the worker drops it
    /// exactly as it stands. Whatever the archive already holds goes with it —
    /// the first frame of an attempt that recorded, but a take that never
    /// framed leaves nothing behind at all.
    pub(crate) fn abandon_take(self) {}

    /// Ends the take exactly once: the tail the capture was still holding goes
    /// out, and the archive publishes its last sequence.
    pub(crate) fn finish(&mut self, outcome: Outcome) {
        assert!(!self.finished, "the fixture ends one take exactly once");
        self.finished = true;
        self.encoder.flush();
        self.encoder.finish_diagnostics(outcome);
    }
}

/* -------------------------------------------------------------------------- */
/* Device                                                                      */
/* -------------------------------------------------------------------------- */

/// A cpal refusal that says the device itself is gone — and not that its format
/// is unsupported, that another stream holds it, or that Windows refused the
/// access. Only a gone device is worth the system fallback before the first
/// frame; nothing else about a device is repaired by opening another one.
trait Gone {
    fn gone(&self) -> bool;
}

impl Gone for cpal::DefaultStreamConfigError {
    fn gone(&self) -> bool {
        matches!(self, Self::DeviceNotAvailable)
    }
}

impl Gone for cpal::SupportedStreamConfigsError {
    fn gone(&self) -> bool {
        matches!(self, Self::DeviceNotAvailable)
    }
}

impl Gone for cpal::BuildStreamError {
    fn gone(&self) -> bool {
        matches!(self, Self::DeviceNotAvailable)
    }
}

impl Gone for cpal::PlayStreamError {
    fn gone(&self) -> bool {
        matches!(self, Self::DeviceNotAvailable)
    }
}

impl Gone for cpal::StreamError {
    fn gone(&self) -> bool {
        matches!(self, Self::DeviceNotAvailable)
    }
}

/// Why one attempt at a device ended, and what the session thread should do
/// about it.
#[derive(Debug)]
enum OpenError {
    /// The device the settings named is gone: before the first frame, exactly
    /// one retry with the system device may repair this.
    Fallback(UiError),
    /// No other device repairs this: the take fails with this reason.
    Fatal(UiError),
}

impl OpenError {
    fn fallback(error: UiError) -> Self {
        Self::Fallback(error)
    }

    fn fatal(error: UiError) -> Self {
        Self::Fatal(error)
    }

    /// The failure of one device attempt: its reason, and whether the system
    /// fallback may repair it.
    fn device(error: UiError, gone: bool) -> Self {
        if gone {
            Self::Fallback(error)
        } else {
            Self::Fatal(error)
        }
    }
}

/// A failure the session thread has to see, as the stream reports it.
#[derive(Debug)]
enum StreamFailure {
    /// The device is no longer available. Before the first frame this is what
    /// the system fallback may repair; after it, the take ends.
    Unavailable(UiError),
    /// Anything else: the take ends with this reason.
    Fatal(UiError),
}

impl StreamFailure {
    /// The driver's own words for the failure, classified by what another
    /// device could repair.
    fn of(err: cpal::StreamError) -> Self {
        let error = mic_failure_arg(
            MessageId::MicStreamStopped,
            "detail",
            Value::String(err.to_string()),
        );
        if err.gone() {
            Self::Unavailable(error)
        } else {
            Self::Fatal(error)
        }
    }

    /// The reason this take ends with, whichever way the stream failed.
    fn into_error(self) -> UiError {
        match self {
            Self::Unavailable(error) | Self::Fatal(error) => error,
        }
    }
}

/// The device a capture records from, and the format it captures at.
struct Input {
    device: cpal::Device,
    /// The id the driver reports for this device, for the answer of the start
    /// command; a device the driver cannot name still records.
    used_device_id: Option<String>,
    /// The name the driver gives the device, for the pill's fallback line.
    name: String,
    config: StreamConfig,
    format: SampleFormat,
    rate: u32,
    channels: usize,
}

/// The device the settings pinned, parsed back into the host's own id. An id
/// this shell cannot read is a device it cannot find: the same system fallback
/// as a device Windows no longer lists, and never a failed take.
fn parse_selected(id: &str) -> Result<cpal::DeviceId, OpenError> {
    cpal::DeviceId::from_str(id).map_err(|_| {
        OpenError::fallback(mic_failure(MessageId::MicSelectedUnreadableId))
    })
}

/// Opens the device a capture records from: the one the settings pinned, or
/// the system input device.
///
/// A selected device that is gone — unparsable, no longer listed, or not an
/// input device — is a reason for the caller to try the system device once. A
/// device that opens but refuses the take — its format, its access, another
/// stream holding it — is the take's own failure, and the system device is
/// never tried for it.
fn open(selected: Option<&str>) -> Result<Input, OpenError> {
    let host = cpal::default_host();
    let device = match selected {
        Some(id) => {
            let parsed = parse_selected(id)?;
            host.device_by_id(&parsed).ok_or_else(|| {
                OpenError::fallback(mic_failure(MessageId::MicSelectedMissing))
            })?
        }
        None => host
            .default_input_device()
            .ok_or_else(|| OpenError::fatal(mic_failure(MessageId::MicNoSystemDevice)))?,
    };
    if !device.supports_input() {
        return Err(match selected {
            Some(_) => OpenError::fallback(mic_failure(MessageId::MicSelectedNotInput)),
            None => OpenError::fatal(mic_failure(MessageId::MicNoSystemDevice)),
        });
    }
    let supported = supported_config(&device)?;
    let format = supported.sample_format();
    let rate = supported.sample_rate();
    if rate == 0 {
        return Err(OpenError::fatal(mic_failure(MessageId::MicZeroSampleRate)));
    }
    let channels = usize::from(supported.channels()).max(1);
    Ok(Input {
        used_device_id: device.id().ok().map(|id| id.to_string()),
        name: device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_default(),
        config: supported.config(),
        device,
        format,
        rate,
        channels,
    })
}

/// The device's own configuration when it is one the shell samples, and
/// otherwise the first one it does offer, at the rate that device prefers.
fn supported_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, OpenError> {
    let default = device.default_input_config().map_err(|err| {
        OpenError::device(
            mic_failure_arg(
                MessageId::MicNoDefaultFormat,
                "detail",
                Value::String(err.to_string()),
            ),
            err.gone(),
        )
    })?;
    if SAMPLED.contains(&default.sample_format()) {
        return Ok(default);
    }
    let fallback = device
        .supported_input_configs()
        .map_err(|err| {
            OpenError::device(
                mic_failure_arg(MessageId::MicNoFormats, "detail", Value::String(err.to_string())),
                err.gone(),
            )
        })?
        .find(|range| SAMPLED.contains(&range.sample_format()))
        .map(|range| {
            let rate = range.max_sample_rate();
            range.with_sample_rate(rate)
        });
    fallback.ok_or_else(|| {
        OpenError::fatal(mic_failure_arg(
            MessageId::MicUnreadableSamples,
            "format",
            Value::String(default.sample_format().to_string()),
        ))
    })
}

/// Opens the stream for the format the device reported, so no conversion
/// happens outside this module. A stopped stream reports itself to the session
/// thread instead of killing it from the audio thread.
fn build(
    input: &Input,
    samples: Sender<Vec<f32>>,
    free: Receiver<Vec<f32>>,
    errors: Sender<StreamFailure>,
) -> Result<cpal::Stream, OpenError> {
    match input.format {
        SampleFormat::F32 => build_typed::<f32>(input, samples, free, errors),
        SampleFormat::I16 => build_typed::<i16>(input, samples, free, errors),
        SampleFormat::U16 => build_typed::<u16>(input, samples, free, errors),
        other => Err(OpenError::fatal(mic_failure_arg(
            MessageId::MicUnreadableFormat,
            "format",
            Value::String(other.to_string()),
        ))),
    }
}

fn build_typed<T>(
    input: &Input,
    samples: Sender<Vec<f32>>,
    free: Receiver<Vec<f32>>,
    errors: Sender<StreamFailure>,
) -> Result<cpal::Stream, OpenError>
where
    T: SizedSample + Level,
{
    let on_error = move |err: cpal::StreamError| {
        let _ = errors.send(StreamFailure::of(err));
    };
    let size = input.channels * BUFFER_FRAMES;
    input
        .device
        .build_input_stream(
            &input.config,
            sink::<T>(input.channels, samples, free, size),
            on_error,
            None,
        )
        .map_err(|err| {
            OpenError::device(
                mic_failure_arg(
                    MessageId::MicCannotOpen,
                    "detail",
                    Value::String(err.to_string()),
                ),
                err.gone(),
            )
        })
}

/* -------------------------------------------------------------------------- */
/* Session                                                                     */
/* -------------------------------------------------------------------------- */

/// One running capture, as the command thread sees it.
struct Session {
    /// The take this stream was opened for. A stop of another owner never
    /// reaches into it.
    owner: CaptureOwner,
    /// Identity of the reservation this recording took over. Only whoever holds
    /// the same ticket may take this session back out of the slot directly;
    /// every other end goes through a stop of its owner.
    ticket: Arc<()>,
    stop: Sender<()>,
    /// The sequence of the last emitted frame, sent once the tail is out, or
    /// the reason the stream failed before it could be read whole.
    landed: Receiver<Result<u64, UiError>>,
    /// The take's diagnostics handle, cached when the recording was installed:
    /// a command that gives up on a stalled worker publishes the capture end
    /// from here, without reaching for the registry again.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    diagnostics: Option<SessionHandle>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Stops the device, sends what is left of the capture and reports the last
    /// frame sequence.
    ///
    /// The count arrives only after every frame of the session has been queued
    /// for the window of its take, so a renderer that waits for it cannot lose
    /// the tail. A stream that failed on its own keeps that failure: a tail a
    /// renderer cannot trust is never handed over as if it were whole. A
    /// session that does not report itself in time is left detached: a driver
    /// that stopped answering must not be able to hold this thread forever. A
    /// worker that never came back never published this take's capture end
    /// either, so the command publishes it here instead — the take is closed as
    /// its own error, and a frame the stalled worker later produces is a hole
    /// of a capture that is already over.
    fn finish(mut self) -> Result<u64, UiError> {
        let _ = self.stop.send(());
        let landed = self.landed.recv_timeout(STOP_TIMEOUT);
        if !matches!(landed, Err(RecvTimeoutError::Timeout)) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        match landed {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                // The worker never reached its tail, so the take's capture end
                // is published here — as the take's own error — and a frame the
                // stalled worker finally produces is a hole of a capture that
                // is already over. Nothing about the driver, the renderer or
                // this wait changes.
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                if let Some(session) = &self.diagnostics {
                    session.abandon_capture(Outcome::Error);
                }
                Err(UiError::new(CAPTURE_FAILED, UiMessage::new(MessageId::MicStopTimeout)))
            }
            Err(RecvTimeoutError::Disconnected) => {
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                if let Some(session) = &self.diagnostics {
                    session.abandon_capture(Outcome::Error);
                }
                Err(UiError::new(CAPTURE_FAILED, UiMessage::new(MessageId::CaptureThreadLost)))
            }
        }
    }

    /// Ends a capture that is already handing its take over, without waiting
    /// for its driver or its renderer.
    ///
    /// Only the bounded installer exit uses this: the stop is sent — the worker
    /// closes its stream at the next read — and the thread's handle is dropped
    /// instead of joined, so a driver that stopped answering cannot hold the
    /// exit. A normal dictation keeps [`Session::finish`], which waits for the
    /// tail a renderer still needs.
    fn abandon(self) {
        let _ = self.stop.send(());
        // The remaining fields — the landed receiver and the thread's handle —
        // are dropped here: nothing waits for the tail, and a hung worker is
        // left to the process ending.
    }
}

/// The reservation of a take whose device is still opening. It is in the slot
/// from before the worker exists, so a second start — of either owner — finds
/// the microphone taken while this one is still waiting on its driver, and a
/// stop of this very take can end it without waiting for that driver.
struct Opening {
    owner: CaptureOwner,
    /// The admission this take was started under, kept so the recording can be
    /// installed only while the shell's promise still holds — compared under
    /// the same lock as the installation itself.
    admission: u64,
    /// Identity of this reservation. The worker installs its recording only
    /// while the slot still holds *this* reservation, so a stop that took it
    /// can never be overtaken by a device that answered late.
    ticket: Arc<()>,
    /// The stop channel's only sender until the recording takes it over. Its
    /// end is what tells a worker whose reservation vanished to close instead
    /// of recording.
    stop: Sender<()>,
    /// The take's diagnostics handle, cached from before the worker existed, so
    /// the recording keeps it and a stop that has to give up on a stalled
    /// worker can still publish this take's capture end.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    diagnostics: Option<SessionHandle>,
    /// Filled in right after the worker is spawned, while the reservation is
    /// still held.
    thread: Option<JoinHandle<()>>,
}

impl Opening {
    /// Ends a take whose device is still opening, without waiting for the
    /// driver: the worker is told to close, and the slot entry — and with it
    /// the shell's only handle on the thread — is released right away. The
    /// worker closes whatever it opened and never installs itself.
    fn cancel(self) {
        let _ = self.stop.send(());
    }
}

/// What one take holds in the shell's single capture slot.
enum Active {
    /// A device is being opened, or its first frame is on its way: the slot is
    /// claimed, nothing is recorded yet.
    Opening(Opening),
    /// The stream has delivered its first frame: every frame from here on
    /// belongs to the take that owns this session.
    Recording(Session),
}

impl Active {
    fn owner(&self) -> CaptureOwner {
        match self {
            Self::Opening(opening) => opening.owner,
            Self::Recording(session) => session.owner,
        }
    }
}

/// What a stop of one owner may do to the slot it finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopRight {
    /// The slot holds this very take: the stop may end it.
    Mine,
    /// The slot holds a newer take of the same window: nothing of the stopping
    /// take is left, and the newer capture is never touched.
    Stale,
    /// The slot holds the capture of the other window: this stop must never
    /// reach into it.
    Foreign,
    /// Nothing captures at all.
    Idle,
}

fn stop_right(slot: &Option<Active>, owner: CaptureOwner) -> StopRight {
    match slot {
        None => StopRight::Idle,
        Some(active) if active.owner() == owner => StopRight::Mine,
        Some(active) if active.owner().same_kind(owner) => StopRight::Stale,
        Some(_) => StopRight::Foreign,
    }
}

/// Why a start could not claim the slot, and whether the take it found there is
/// this very one — already capturing — or another take holding the microphone.
///
/// The distinction is what keeps a repeated start harmless: the take it names
/// is already running, and nothing about its capture may be closed just because
/// the start was refused. Only a refusal of a take that never began hands the
/// caller a take whose capture is really over.
#[derive(Debug)]
enum ClaimError {
    /// This very take already claims the slot: it is arming or recording, and
    /// its capture is its own to finish.
    SameTake(UiError),
    /// Another take — of the other window — holds the microphone.
    Busy(UiError),
}

impl ClaimError {
    /// The reason the start command is answered with.
    fn into_error(self) -> UiError {
        match self {
            Self::SameTake(error) | Self::Busy(error) => error,
        }
    }
}

/// Ends a take that was left in the slot by an earlier generation of the same
/// window, outside the lock that held it. A leftover is a bug somewhere else —
/// every end path of a take closes its capture — but the microphone must be
/// free for the next take all the same.
fn end_leftover(active: Active) {
    eprintln!(
        "speechek: closing the capture of {:?}, left behind before the next take.",
        active.owner()
    );
    match active {
        Active::Recording(session) => {
            let _ = session.finish();
        }
        Active::Opening(opening) => opening.cancel(),
    }
}

/// The shell's single native capture. Handed to Tauri as managed state so the
/// commands, the watcher and the shutdown all see the same stream.
#[derive(Default)]
pub struct NativeCapture {
    slot: Mutex<Option<Active>>,
    /// Bumped every time the shell closes admission for a capture. A take whose
    /// device is still opening reads it before it records, silences or warns,
    /// so a close that happened while its driver was answering is never
    /// overtaken by that take installing itself.
    closed: AtomicU64,
}

impl NativeCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims the slot for a take of `owner` to be opened next, and hands back
    /// a leftover take of the same window so the caller can end it outside this
    /// lock. The other window's capture — and this very take, already claimed —
    /// are refused.
    fn claim(&self, owner: CaptureOwner) -> Result<Option<Active>, ClaimError> {
        let mut slot = self.slot.lock();
        match stop_right(&slot, owner) {
            StopRight::Idle => Ok(None),
            StopRight::Mine => Err(ClaimError::SameTake(same_take_error(owner))),
            StopRight::Stale => Ok(slot.take()),
            StopRight::Foreign => Err(ClaimError::Busy(busy_error(owner))),
        }
    }

    /// Puts the reservation of a take into the slot it claimed, or refuses
    /// because another take claimed the microphone in between.
    fn reserve(&self, opening: Opening) -> Result<(), UiError> {
        let mut slot = self.slot.lock();
        if slot.is_some() {
            return Err(busy_error(opening.owner));
        }
        *slot = Some(Active::Opening(opening));
        Ok(())
    }

    /// Takes the active capture a stop of `owner` may end, or reports why it may
    /// not: the other window's capture is refused, a newer take of the same
    /// window leaves nothing behind, and a take that is still opening its device
    /// is handed over to be closed without waiting for its driver.
    fn take_for_stop(&self, owner: CaptureOwner) -> Result<Option<Active>, UiError> {
        let mut slot = self.slot.lock();
        match stop_right(&slot, owner) {
            StopRight::Mine => Ok(slot.take()),
            // A newer take of the same window owns the microphone now: this
            // stop has no stream of its own left, and the newer one is not
            // touched.
            StopRight::Stale => Ok(None),
            StopRight::Foreign => Err(busy_error(owner)),
            StopRight::Idle => Ok(None),
        }
    }

    /// Replaces the reservation of this very take with its recording. The same
    /// lock holds both, so no stop can find the slot empty between the two, and
    /// no other take can slip in. The take's own admission is compared here as
    /// well: a shell that closed its capture while the driver was answering
    /// never gets a recording installed behind it.
    ///
    /// `diagnostics` is the take's own handle, cached beside the recording so a
    /// stop that has to give up on a stalled worker can still publish this
    /// take's capture end.
    fn install(
        &self,
        ticket: &Arc<()>,
        admission: u64,
        owner: CaptureOwner,
        landed: Receiver<Result<u64, UiError>>,
    ) -> Result<(), ()> {
        let mut slot = self.slot.lock();
        if !current(&self.closed, admission) {
            return Err(());
        }
        let opening = match slot.take() {
            Some(Active::Opening(opening))
                if opening.admission == admission && Arc::ptr_eq(&opening.ticket, ticket) =>
            {
                opening
            }
            other => {
                *slot = other;
                return Err(());
            }
        };
        // The recording takes the reservation's parts over one by one, so the
        // take's own handle travels with it and nothing has to carry a gate
        // inside a pattern.
        *slot = Some(Active::Recording(Session {
            owner,
            ticket: Arc::clone(ticket),
            stop: opening.stop,
            landed,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            diagnostics: opening.diagnostics,
            thread: opening.thread,
        }));
        Ok(())
    }

    /// Takes this very take's reservation out of the slot, leaving nothing
    /// behind for its worker to install into.
    fn withdraw(&self, owner: CaptureOwner, ticket: &Arc<()>) -> bool {
        let mut slot = self.slot.lock();
        let mine = matches!(
            slot.as_ref(),
            Some(Active::Opening(opening))
                if opening.owner == owner && Arc::ptr_eq(&opening.ticket, ticket)
        );
        if mine {
            if let Some(Active::Opening(opening)) = slot.take() {
                opening.cancel();
            }
        }
        mine
    }

    /// Hands the reservation its worker's handle, once the worker exists. A
    /// reservation that was taken in the meantime — a stop that arrived while
    /// the thread was being spawned — leaves the handle with the caller, which
    /// simply drops it: the worker is already closing.
    fn attach_worker(&self, owner: CaptureOwner, ticket: &Arc<()>, worker: JoinHandle<()>) {
        let mut slot = self.slot.lock();
        if let Some(Active::Opening(opening)) = slot.as_mut() {
            if opening.owner == owner && Arc::ptr_eq(&opening.ticket, ticket) {
                opening.thread = Some(worker);
            }
        }
    }

    /// Ends whatever this very take left in the slot after the command gave up
    /// on it: the reservation if its worker is still opening the device, or the
    /// recording that was installed in the very instant the command stopped
    /// waiting. Both are named by the ticket the caller holds, so no other take
    /// — not even a newer one of the same window that claimed the microphone in
    /// between — is ever touched, and a take that is already out of the slot is
    /// left alone.
    fn abandon(&self, owner: CaptureOwner, ticket: &Arc<()>) {
        if self.withdraw(owner, ticket) {
            return;
        }
        let installed = {
            let mut slot = self.slot.lock();
            let ours = matches!(
                slot.as_ref(),
                Some(Active::Recording(session))
                    if session.owner == owner && Arc::ptr_eq(&session.ticket, ticket)
            );
            if ours {
                slot.take()
            } else {
                None
            }
        };
        if let Some(Active::Recording(session)) = installed {
            let _ = session.finish();
        }
    }

    /// Ends whatever is capturing without waiting for it: the bounded exit an
    /// installer asked for.
    ///
    /// The admission moves before the slot is touched — a take that is still
    /// opening its device sees the close and never installs itself — and a
    /// recording is stopped without the wait [`Self::take_for_stop`] and
    /// [`shutdown`] give a normal end. The installer's own deadline is what
    /// bounds this.
    fn end_for_exit(&self) {
        self.closed.fetch_add(1, Ordering::SeqCst);
        let active = self.slot.lock().take();
        match active {
            Some(Active::Recording(session)) => session.abandon(),
            // A take still opening its device is closed through its own stop:
            // the worker reads it, gives the device back and installs nothing.
            Some(Active::Opening(opening)) => opening.cancel(),
            None => {}
        }
    }
}

/// The reason a start of `owner` gets when the microphone is already capturing
/// that very take.
fn same_take_error(owner: CaptureOwner) -> UiError {
    let key = match owner {
        CaptureOwner::Dictation(_) => MessageId::CaptureSameTakeDictation,
        CaptureOwner::Lab(_) => MessageId::CaptureSameTakeLab,
    };
    UiError::new(MICROPHONE_BUSY, UiMessage::new(key))
}

/// The reason a start or a stop of `owner` gets when the microphone is busy
/// with the other window's capture: the laboratory and a dictation never share
/// a take.
fn busy_error(owner: CaptureOwner) -> UiError {
    let key = match owner {
        CaptureOwner::Dictation(_) => MessageId::CaptureBusyDictation,
        CaptureOwner::Lab(_) => MessageId::CaptureBusyLab,
    };
    UiError::new(MICROPHONE_BUSY, UiMessage::new(key))
}

/// The shell's promise that a capture may open its microphone, as it stands at
/// this very moment.
///
/// Every way a take ends — a cancel, an arming that was given up on, a finished
/// session, the laboratory closing its window or taking a stop, the shell
/// leaving — closes whatever is capturing, which moves this value *before* it
/// stops or releases anything. A caller reads the promise under the session
/// lock and carries it into [`start`] as the capture's admission; a capture
/// whose token has moved since then opens no stream, silences nothing and
/// reports itself as already over.
pub fn admission(app: &AppHandle) -> u64 {
    app.state::<NativeCapture>().closed.load(Ordering::SeqCst)
}

/// Closes the shell's admission without touching any capture: a take whose
/// reservation is withdrawn separately must not install itself behind the
/// caller's back, however long its driver takes to answer. The laboratory's
/// stop uses this before its owner-scoped [`stop`], under the shell's session
/// lock, so the generation it closes can never regain a microphone.
pub fn close_admission(app: &AppHandle) {
    app.state::<NativeCapture>().closed.fetch_add(1, Ordering::SeqCst);
}

/// Whether a capture that was admitted with `admission` still holds the shell's
/// promise: the token has not moved since the admission was read.
fn current(closed: &AtomicU64, admission: u64) -> bool {
    closed.load(Ordering::SeqCst) == admission
}

/// Whether a capture thread that was admitted with `admission` must treat its
/// take as over before it records, silences or warns.
///
/// `waiting` is whether the renderer may still be waiting for the handshake's
/// answer. While it is, a stop that is already queued is a verdict of its own:
/// only a take that is over queues one. Once the answer has gone out the take's
/// capture is real, and the queued stop is the ordinary end of a recording —
/// left for the capture loop to see, not consumed here.
///
/// The token is read here and not only the stop channel: a cancel closes the
/// capture *before* it queues the stop, so a take that was given up on while
/// its handshake was on its way refuses to silence anything even though
/// nothing has reached the channel yet.
fn is_stopping(closed: &AtomicU64, admission: u64, waiting: bool, control: &Receiver<()>) -> bool {
    !waiting
        || !current(closed, admission)
        || matches!(control.try_recv(), Ok(()) | Err(TryRecvError::Disconnected))
}

/// What a started take was really recorded with, as its start command answers
/// it.
///
/// The stream is running and its first frame has been handed to the window of
/// the take by the time this is sent, so the renderer may feed its recorder.
/// The renderer reads `fallbackDevice` and `usedDeviceId` for it. `used_device_id`
/// is the id the driver reports for the device that was really opened: the
/// settings may name a device that is gone, and this is what captured instead —
/// without ever changing those settings. `fallback_device` is the name of the
/// system device that took over from a selected one that was not there; a take
/// that recorded with the device it asked for leaves it empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStart {
    pub fallback_device: Option<String>,
    pub used_device_id: Option<String>,
}

/// One capture whose stream has delivered its first frame: the handshake may
/// answer, and every frame from here on belongs to the take that started it.
/// The stream, its buffers and its resampler live here, so the take that ends
/// them ends all of it at once.
struct Opened {
    /// What the start command is answered with once this capture is in the
    /// slot. A capture that never gets there — its reservation was withdrawn
    /// while its driver was answering — is never answered at all.
    start: CaptureStart,
    /// How far this attempt got, in microseconds: what the take's start record
    /// carries.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    timing: AttemptTiming,
    encoder: Encoder,
    stream: cpal::Stream,
    samples: Receiver<Vec<f32>>,
    free: Sender<Vec<f32>>,
    errors: Receiver<StreamFailure>,
}

/// A device attempt that ended before its first frame reached the overlay, and
/// what the session thread should do about it.
enum AttemptError {
    /// The selected device is gone: one retry with the system device may still
    /// record this take.
    Fallback(UiError),
    /// The device answered and refused the take: no other microphone repairs
    /// that.
    Fatal(UiError),
    /// The attempt never recorded anything because the take is already over.
    /// No device failed, so no failure is reported for it.
    Ended(UiError),
}

impl From<OpenError> for AttemptError {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::Fallback(error) => Self::Fallback(error),
            OpenError::Fatal(error) => Self::Fatal(error),
        }
    }
}

/// What a failed attempt leaves to do.
#[derive(Debug)]
enum Retry {
    /// The selected device is gone: the system device is tried once, in its
    /// place. Carries the reason, for the log only.
    System(UiError),
    /// The take ends before its first frame, with this reason for its command.
    End(UiError),
    /// The take was already over: nothing failed, so nothing is reported.
    Ended(UiError),
}

/// The fallback policy, decided without a device: the system device replaces
/// the selected one only when the selected one is gone, and only once. A device
/// that answered and refused the take — its format, its access, another stream
/// holding it — ends the take; no other microphone repairs that.
fn retry(failure: AttemptError, already_fell_back: bool) -> Retry {
    match failure {
        AttemptError::Fallback(error) if !already_fell_back => Retry::System(error),
        AttemptError::Fallback(error) | AttemptError::Fatal(error) => Retry::End(error),
        AttemptError::Ended(error) => Retry::Ended(error),
    }
}

/// Where one take's handshake stands.
enum HandshakeState {
    /// No answer has been sent and the renderer may still be waiting: the
    /// command's timeout, the shell closing the capture and a stop that is
    /// already queued are each the whole verdict.
    Waiting,
    /// The answer is on its way. The take's capture is real now: a stop that
    /// lands from here on is the ordinary end of a recording, not the
    /// handshake's business.
    Answered,
    /// Nobody waits for an answer any more: the renderer gave the command up.
    /// The take records and stops like any other, and its answer goes nowhere.
    Abandoned,
}

/// One take's handshake: the channel the command waits on, and where it stands.
///
/// The command answers only once the stream has delivered its first frame *and*
/// the take's reservation has been replaced by its recording, so a take that
/// ended while its device was opening is never answered and never installs
/// itself behind the shell's back. It is only because the worker really installs
/// before answering that [`Handshake::ready`] can answer with a plain
/// [`CaptureStart`].
struct Handshake {
    /// The command's end of this handshake. At most one answer ever travels
    /// through it, and failing to hand it over is the renderer having given the
    /// command up.
    ready: SyncSender<Result<CaptureStart, UiError>>,
    state: HandshakeState,
}

impl Handshake {
    fn new(ready: SyncSender<Result<CaptureStart, UiError>>) -> Self {
        Self {
            ready,
            state: HandshakeState::Waiting,
        }
    }

    /// Whether the take is already over at this very moment — read before
    /// anything is recorded or silenced.
    ///
    /// While the answer is still waiting, the renderer giving up, the shell
    /// closing the capture and a stop that is already queued are each the whole
    /// verdict (see [`is_stopping`]); once the answer has gone out, the take's
    /// capture is real and a stop is just the end of its recording.
    fn over(&self, closed: &AtomicU64, admission: u64, control: &Receiver<()>) -> bool {
        is_stopping(
            closed,
            admission,
            matches!(self.state, HandshakeState::Waiting),
            control,
        )
    }

    /// Answers the waiting command that the take's first frame is out, and
    /// reports whether anybody was still waiting.
    ///
    /// Nobody waiting is the take's whole verdict: the renderer has given the
    /// command up, so this capture records nothing further and is closed by the
    /// worker instead of starting behind its back.
    fn ready(&mut self, answer: CaptureStart) -> Result<(), ()> {
        if !matches!(self.state, HandshakeState::Waiting) {
            return Err(());
        }
        self.state = HandshakeState::Answered;
        if self.ready.send(Ok(answer)).is_err() {
            self.state = HandshakeState::Abandoned;
            return Err(());
        }
        Ok(())
    }

    /// The capture is over before its answer ever went out: the command learns
    /// that the attempt failed, not that it may record. An answer that already
    /// went out is not taken back — there is nothing to say twice.
    fn fail(&mut self, error: UiError) -> Result<(), ()> {
        if !matches!(self.state, HandshakeState::Waiting) {
            return Err(());
        }
        self.state = HandshakeState::Abandoned;
        self.ready.send(Err(error)).map_err(|_| ())
    }
}

/// One attempt at recording from one device, from opening it to the frame that
/// lets the handshake answer.
struct Attempt<'a> {
    app: &'a AppHandle,
    /// The take this attempt frames for: its number tags every frame, and its
    /// window receives them.
    owner: CaptureOwner,
    /// The device the settings pinned, or `None` for the system input device.
    selected: Option<&'a str>,
    /// Whether this attempt is the system device standing in for a selected one
    /// that was gone: the renderer is told which device really captured.
    fallback: bool,
    /// Which device attempt this is, counting from one: the device the take
    /// asked for is the first, the system device standing in for it the second.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    attempt_index: u32,
    /// The take's diagnostics handle, looked up once by the worker. Nothing is
    /// recorded about this attempt without it, and it redacts every body of the
    /// attempt against the ring it remembered when the take began.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    session: Option<&'a SessionHandle>,
    closed: &'a AtomicU64,
    admission: u64,
    control: &'a Receiver<()>,
    handshake: &'a mut Handshake,
}

impl Attempt<'_> {
    /// Records from one device until the first of its frames has reached the
    /// window of the take — the point after which the capture may be installed
    /// and the handshake may answer.
    ///
    /// Nothing is emitted before that point, and nothing of a failed attempt
    /// survives it: the stream is closed with the returned error, taking its
    /// queued buffers, its resampler position and its frame sequence with it.
    /// The retry therefore starts from an empty encoder and sequence zero.
    fn run(self) -> Result<Opened, AttemptError> {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        let mut timing = AttemptTiming::start();
        if self.handshake.over(self.closed, self.admission, self.control) {
            return Err(AttemptError::Ended(ended_while_opening()));
        }
        let input = match open(self.selected) {
            Ok(input) => input,
            Err(error) => {
                let failure = AttemptError::from(error);
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                self.note_attempt(&timing, Some(&failure));
                return Err(failure);
            }
        };
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        timing.opened(&input);
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        self.note_resolution(&input);
        if self.handshake.over(self.closed, self.admission, self.control) {
            // The device answered, but the take is over: it is given back
            // without a stream ever running, so nothing of it is recorded.
            return Err(AttemptError::Ended(ended_while_opening()));
        }
        let (samples_tx, samples_rx) = mpsc::channel::<Vec<f32>>();
        let (free_tx, free_rx) = mpsc::channel::<Vec<f32>>();
        let (errors_tx, errors_rx) = mpsc::channel::<StreamFailure>();
        // Two buffers wait before the first callback, so even the very first
        // ones find one instead of allocating on the audio thread.
        for _ in 0..2 {
            let _ = free_tx.send(Vec::with_capacity(input.channels * BUFFER_FRAMES));
        }
        let mut encoder = Encoder::new(self.app, self.owner, input.rate);
        // The handle was looked up once, before this device was touched: the
        // encoder keeps it for the whole take, so no frame ever asks the
        // registry anything.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        encoder.with_session(self.session.cloned());
        let stream = match build(&input, samples_tx, free_rx, errors_tx) {
            Ok(stream) => stream,
            Err(error) => {
                let failure = AttemptError::from(error);
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                self.note_attempt(&timing, Some(&failure));
                return Err(failure);
            }
        };
        if let Err(err) = stream.play() {
            let error = mic_failure_arg(
                MessageId::MicStreamDidNotStart,
                "detail",
                Value::String(err.to_string()),
            );
            let failure = if err.gone() {
                AttemptError::Fallback(error)
            } else {
                AttemptError::Fatal(error)
            };
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            self.note_attempt(&timing, Some(&failure));
            return Err(failure);
        }
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        timing.played();
        // The stream runs, but the take has not recorded anything yet: the
        // handshake only answers once a frame of it has really been emitted.
        let deadline = Instant::now() + OPEN_TIMEOUT;
        loop {
            if self.handshake.over(self.closed, self.admission, self.control) {
                return Err(AttemptError::Ended(ended_while_opening()));
            }
            drain(&samples_rx, &free_tx, &mut encoder);
            if encoder.sequence > 0 {
                break;
            }
            if let Ok(failure) = errors_rx.try_recv() {
                let failure = match failure {
                    StreamFailure::Unavailable(error) => AttemptError::Fallback(error),
                    StreamFailure::Fatal(error) => AttemptError::Fatal(error),
                };
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                self.note_attempt(&timing, Some(&failure));
                return Err(failure);
            }
            if Instant::now() >= deadline {
                let failure = AttemptError::Fatal(mic_failure_arg(
                    MessageId::MicNoFrame,
                    "seconds",
                    Value::from(OPEN_TIMEOUT.as_secs()),
                ));
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                self.note_attempt(&timing, Some(&failure));
                return Err(failure);
            }
            thread::sleep(IDLE_POLL);
        }
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        timing.framed();
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        self.note_attempt(&timing, None);
        // The first frame is out: this attempt may be installed, and only the
        // worker that holds the reservation may answer the command from here on.
        // Only a system device standing in for a selected one is named: a take
        // that recorded with the device it asked for leaves that line empty.
        let fallback_device = self.fallback.then(|| input.name);
        Ok(Opened {
            start: CaptureStart {
                fallback_device,
                used_device_id: input.used_device_id,
            },
            encoder,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            timing,
            stream,
            samples: samples_rx,
            free: free_tx,
            errors: errors_rx,
        })
    }

    /// The device this attempt resolved, as the archive records it: the id the
    /// settings named and the id, the name and the source format of the device
    /// that really answered. A take that recorded from a device nobody asked
    /// for — the system device standing in for a selected one that was gone —
    /// or at a format its settings did not name can be told apart from the
    /// settings alone.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn note_resolution(&self, input: &Input) {
        let Some(session) = self.session else {
            return;
        };
        let body = serde_json::json!({
            "stage": "opened",
            "attempt": self.attempt_index,
            "fallback": self.fallback,
            "requested_device": self.selected,
            "resolved_device_id": input.used_device_id,
            "resolved_device_name": input.name,
            "source_format": input.format.to_string(),
            "source_rate": input.rate,
            "source_channels": input.channels,
        });
        let _ = session.redacted_content_json(ContentKind::CaptureContext, &body);
    }

    /// One device attempt, as the take's journal records it: the numbers of the
    /// attempt and, for one that failed, the catalog key of its reason. The
    /// device's own name and its driver's words never travel through here.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn note_attempt(&self, timing: &AttemptTiming, failure: Option<&AttemptError>) {
        if let Some(session) = self.session {
            session.event(attempt_event(
                self.attempt_index,
                self.fallback,
                timing,
                failure.and_then(failure_key),
            ));
        }
        // A failed attempt leaves the reason the numeric event cannot carry:
        // the descriptor the renderer would have shown, with the driver's own
        // words in it, redacted against the take's ring before it is archived.
        let Some((outcome, error)) = failure.map(failure_reason) else {
            return;
        };
        let Some(session) = self.session else {
            return;
        };
        let body = serde_json::json!({
            "stage": "failed",
            "attempt": self.attempt_index,
            "fallback": self.fallback,
            "requested_device": self.selected,
            "outcome": outcome,
            "code": error.code,
            "message": error.message,
            "ui": { "key": error.ui.key, "args": error.ui.args },
        });
        let _ = session.redacted_content_json(ContentKind::CaptureContext, &body);
    }
}

/// Feeds every sample the stream has delivered into the encoder, handing each
/// buffer straight back to the audio thread. The pool never grows past the
/// callbacks in flight, and neither side ever waits: the channel is unbounded.
/// Answers whether anything arrived, so a caller only sleeps when it has
/// nothing to do.
fn drain(samples: &Receiver<Vec<f32>>, free: &Sender<Vec<f32>>, encoder: &mut Encoder) -> bool {
    let mut got = false;
    while let Ok(frames) = samples.try_recv() {
        got = true;
        encoder.push(&frames);
        let _ = free.send(frames);
    }
    got
}

/// Starts capturing the microphone of one take.
///
/// `owner` is the take this capture belongs to: the dictation of a generation,
/// or one take of the built-in laboratory. It decides the window every frame and
/// every error goes to, and the only stop that may end this capture. At most one
/// take of either owner holds the microphone: a start while another take is
/// reserved or recording is refused before a device is opened; a take of the
/// same window left behind by an earlier generation is ended first.
///
/// `input_device` is the device the take pinned: the id the settings named at
/// that moment, or `None` for the system input device. It is resolved against
/// the system again here, at every open, so a device that was unplugged since
/// the settings were written is a fallback instead of a silent take — see
/// [`open`] and the single retry in [`session_thread`].
///
/// The command answers only once the stream has delivered its first frame *and*
/// the take's reservation has been replaced by its recording: the shell has
/// really started recording, and the renderer may feed its recorder.
/// [`CaptureStart`] names the device that captured, including the system device
/// that took over from a selected one that was gone. A device that never
/// delivers a frame within [`OPEN_TIMEOUT`] fails the command and is closed
/// behind it.
///
/// `mute_audio` is the pinned setting of a dictation: when it is set, the
/// system's default playback endpoint is muted for as long as the stream really
/// runs, and given back wherever it ends. A mute that cannot be applied or
/// given back only warns the pill; it never fails the capture. A laboratory
/// take never touches the system mute, whatever this says.
///
/// `admission` is the token the caller read under the session lock — see
/// [`admission`]. It is compared again here, before a thread is spawned, and at
/// every step of the handshake and once the stream runs, so a take that ended
/// while its microphone was opening opens no stream and silences nothing,
/// however long the driver took.
pub fn start(
    app: &AppHandle,
    owner: CaptureOwner,
    input_device: Option<&str>,
    mute_audio: bool,
    admission: u64,
) -> Result<CaptureStart, UiError> {
    let state = app.state::<NativeCapture>();
    // The take's diagnostics handle, looked up once, here, so a start that
    // fails before any device is opened still closes the take's capture instead
    // of leaving it open for a capture that never began. A build without the
    // diagnostics looks nothing up.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    let session = diagnostics_session(app, owner);
    if !current(&state.closed, admission) {
        // The take is over already: no thread is started for it, so no device
        // is opened and no mute can be engaged behind the shell's back.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_start_failed(&session);
        return Err(ended_error(owner));
    }

    // A take of this same window left in the slot by an earlier generation is
    // ended before this reservation is claimed; the other window's capture
    // refuses this start outright.
    //
    // A refusal of this very take is not a failure of the take's capture: the
    // take that holds the slot is already arming or recording — a repeated
    // start — and its capture, its frames and its later end all belong to it.
    // Only a refusal that names another take leaves this start's take without a
    // capture of its own, and only that one is closed here.
    let leftover = match state.claim(owner) {
        Ok(leftover) => leftover,
        Err(refusal) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_refused_start(&refusal, &session);
            return Err(refusal.into_error());
        }
    };
    if let Some(leftover) = leftover {
        end_leftover(leftover);
    }

    // The pinned device id travels to the thread: resolving it right before the
    // device is opened is what makes a device that disappeared since the
    // settings were written a fallback instead of a dead take.
    let selected = input_device.map(str::to_owned);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<CaptureStart, UiError>>(1);
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (landed_tx, landed_rx) = mpsc::channel::<Result<u64, UiError>>();
    let ticket = Arc::new(());
    // The reservation is in the slot before the worker exists: a second start of
    // either owner finds the microphone taken, and a stop can end this take
    // without waiting for a driver.
    if let Err(error) = state.reserve(Opening {
        owner,
        admission,
        ticket: Arc::clone(&ticket),
        stop: stop_tx,
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        diagnostics: session.clone(),
        thread: None,
    }) {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_start_failed(&session);
        return Err(error);
    }

    let worker_ticket = Arc::clone(&ticket);
    let worker = app.clone();
    let spawned = thread::Builder::new()
        .name(owner.thread_name())
        .spawn(move || {
            session_thread(
                worker,
                owner,
                worker_ticket,
                selected,
                mute_audio,
                admission,
                ready_tx,
                stop_rx,
                landed_tx,
                landed_rx,
            )
        });
    let worker_thread = match spawned {
        Ok(worker_thread) => worker_thread,
        Err(err) => {
            // No worker holds the reservation, so it is taken back here: the
            // microphone must not stay claimed by a thread that never existed.
            state.withdraw(owner, &ticket);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_start_failed(&session);
            return Err(mic_failure_arg(
                MessageId::CaptureThreadStart,
                "detail",
                Value::String(err.to_string()),
            ));
        }
    };
    state.attach_worker(owner, &ticket, worker_thread);

    match ready_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(opened)) => Ok(opened),
        // The worker withdrew its own reservation before answering, so there is
        // nothing left to take back here.
        Ok(Err(error)) => Err(error),
        Err(RecvTimeoutError::Timeout) => {
            // No frame in time. The reservation is taken back before the stop is
            // queued: a worker whose driver answers late finds its slot entry
            // gone and closes the device instead of installing itself, so this
            // command never waits on a driver and the slot never stays claimed.
            // A recording that was installed in the very instant the wait
            // expired is closed here through its own take.
            state.abandon(owner, &ticket);
            Err(mic_failure_arg(
                MessageId::MicStartTimeout,
                "seconds",
                Value::from(OPEN_TIMEOUT.as_secs()),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            state.abandon(owner, &ticket);
            Err(UiError::new(CAPTURE_FAILED, UiMessage::new(MessageId::CaptureThreadStopped)))
        }
    }
}

/// Stops the capture of one take and returns the sequence of its last frame, or
/// zero when there is nothing to stop: a stop the shell queued while the
/// renderer was still arming finds no stream, and has no frames to wait for.
///
/// Only the take `owner` names is affected. A stop of the other window — a
/// dictation command naming a laboratory take, or the reverse — is refused
/// outright; a stop of an older take of the same window leaves the newer capture
/// alone and answers zero. A take whose device is still opening is closed
/// without waiting for its driver, so a stop can never hang on a device that
/// does not answer.
pub fn stop(app: &AppHandle, owner: CaptureOwner) -> Result<u64, UiError> {
    let state = app.state::<NativeCapture>();
    match state.take_for_stop(owner)? {
        // The recording publishes its own capture end: its tail on the way out,
        // or — a worker that stopped answering — the abandon its own finish
        // writes when the tail never comes.
        Some(Active::Recording(session)) => session.finish(),
        // A take whose device has not answered yet is ended without waiting for
        // it: the worker reads the stop, closes whatever it opened and never
        // installs itself.
        Some(Active::Opening(opening)) => {
            opening.cancel();
            Ok(0)
        }
        None => Ok(0),
    }
}

/// Closes whatever is capturing, whoever asked for it. Used when a dictation is
/// over for reasons the stream cannot see — the renderer finishing the session,
/// an arming that was given up on — and when the shell is torn down for good.
///
/// The admission moves before the slot is touched, so a take that is still
/// opening its device sees the close and ends instead of starting behind the
/// shell's back.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<NativeCapture>();
    // Before the slot is touched: a capture that is still opening its device
    // sees this and closes itself instead of starting behind the shell's back.
    state.closed.fetch_add(1, Ordering::SeqCst);
    let active = state.slot.lock().take();
    match active {
        Some(Active::Recording(session)) => {
            if let Err(err) = session.finish() {
                eprintln!("speechek: {err}.");
            }
        }
        // A take still opening its device is closed through its own stop: the
        // worker reads it, gives the device back and installs nothing.
        Some(Active::Opening(opening)) => opening.cancel(),
        None => {}
    }
}

/// Closes whatever is capturing without waiting for a driver or a renderer.
///
/// This is the bounded exit an installer asked for: [`shutdown`] remains the
/// ordinary teardown, which may wait up to [`STOP_TIMEOUT`] for the tail a
/// renderer is still reading, while the installer has a deadline of its own and
/// a hung stream must not be part of it.
pub(crate) fn shutdown_for_exit(app: &AppHandle) {
    app.state::<NativeCapture>().end_for_exit();
}

/// Holds a worker's reservation while it opens its device: unless the worker
/// installed its recording, the slot entry is taken back when the worker ends —
/// however it ends, a driver failure or a panic included. The microphone is
/// therefore never left claimed by a thread that is gone.
struct Reservation<'a> {
    state: &'a NativeCapture,
    owner: CaptureOwner,
    ticket: Arc<()>,
    /// Set once the worker's recording has taken the reservation's place; the
    /// slot then belongs to the take and must not be touched here.
    installed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.installed {
            self.state.withdraw(self.owner, &self.ticket);
        }
    }
}

/// Why a start answered that nothing was opened, for a take that was already
/// over before its device was touched.
fn ended_error(owner: CaptureOwner) -> UiError {
    let key = match owner {
        CaptureOwner::Dictation(_) => MessageId::CaptureEndedDictation,
        CaptureOwner::Lab(_) => MessageId::CaptureEndedLab,
    };
    UiError::new(CAPTURE_ENDED, UiMessage::new(key))
}

/// Runs one take's capture on its own thread: the device, the stream and the
/// encoder all live and die here, which is what keeps a stuck driver from
/// holding on to the shell.
///
/// The worker holds the reservation it was started under until its recording is
/// installed in the slot; whatever way it ends before that, [`Reservation`]
/// takes the entry back. An installation that finds the reservation withdrawn
/// means the take was stopped — or the shell closed its capture — while the
/// driver was still answering: the device is closed at once, and neither the
/// command nor the window hears about a capture that never started.
///
/// This is also where the system mute of a dictation lives: it is applied once
/// the stream really runs, and given back right here wherever the thread ends —
/// the stop of a dictation, an arming that was given up on, the shell's
/// shutdown. The module holds the endpoint for one generation at a time, so this
/// release can never touch the mute of the take that replaced it. A laboratory
/// take never touches the mute at all: `audio::Marks` is numbered by dictation
/// generations, and a laboratory number must never be recorded as a take that
/// silenced the system.
///
/// The admission token the shell read under its session lock is compared again
/// here — at every step before the first frame, and again once the recording is
/// installed — so a take the shell gave up on while its driver was opening
/// records nothing and silences nothing, even when no stop has reached the
/// channel yet (see [`is_stopping`]).
fn session_thread(
    app: AppHandle,
    owner: CaptureOwner,
    ticket: Arc<()>,
    selected: Option<String>,
    mute_audio: bool,
    admission: u64,
    ready: SyncSender<Result<CaptureStart, UiError>>,
    control: Receiver<()>,
    landed: Sender<Result<u64, UiError>>,
    answers: Receiver<Result<u64, UiError>>,
) {
    let state = app.state::<NativeCapture>();
    // The take's diagnostics handle, looked up once, before the first device is
    // asked for anything: every attempt, the start record and the archive of
    // this capture read it from here, and it redacts every body against the ring
    // it remembered when the take began — never a ring read later, when a save
    // may have replaced the running revision. A build without the diagnostics
    // looks nothing up.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    let session = diagnostics_session(&app, owner);
    let mut reservation = Reservation {
        state: &state,
        owner,
        ticket,
        installed: false,
    };
    let mut handshake = Handshake::new(ready);

    // The system device is tried when the settings named none, and once more
    // when the device they named is gone. Only then: a selected device that
    // answered and refused the take is the take's failure, not a reason to
    // record from another microphone.
    let mut attempt_selected = selected;
    let mut fallback = false;
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    let mut attempt_index = 0u32;
    let mut opened = loop {
        if handshake.over(&state.closed, admission, &control) {
            // The take was given up on before this attempt recorded anything:
            // nobody is waiting for an answer about a capture that must not
            // start, so there is nothing left to report. No archive was ever
            // opened for it, so its capture is closed here instead — as the
            // lowest outcome there is, so the terminal the shell really gave
            // this take is the one that stands.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(session) = &session {
                session.end_capture(Outcome::Abandoned);
            }
            let _ = handshake.fail(ended_error(owner));
            return;
        }
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        {
            attempt_index += 1;
        }
        let attempt = Attempt {
            app: &app,
            owner,
            selected: attempt_selected.as_deref(),
            fallback,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            attempt_index,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            session: session.as_ref(),
            closed: &state.closed,
            admission,
            control: &control,
            handshake: &mut handshake,
        };
        match attempt.run() {
            Ok(opened) => break opened,
            Err(failure) => match retry(failure, fallback) {
                Retry::System(reason) => {
                    eprintln!(
                        "speechek: the chosen microphone could not start; trying the system one: {reason}."
                    );
                    attempt_selected = None;
                    fallback = true;
                }
                Retry::End(error) => {
                    eprintln!("speechek: {error}.");
                    // The take never recorded a frame, so it has no archive:
                    // its capture is closed here, and a failure of the device
                    // is the take's own error.
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    if let Some(session) = &session {
                        session.end_capture(Outcome::Error);
                    }
                    let _ = handshake.fail(error);
                    return;
                }
                Retry::Ended(error) => {
                    // The take is over, not the device: the command that may
                    // still be waiting learns that nothing started, and no
                    // failure is reported for a take the shell left behind. A
                    // stop that arrived while the device was answering is this
                    // case too, so nothing but the shell's own terminal is
                    // recorded for such a take.
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    if let Some(session) = &session {
                        session.end_capture(Outcome::Abandoned);
                    }
                    let _ = handshake.fail(error);
                    return;
                }
            },
        }
    };

    // The recording takes the reservation's place before the command is
    // answered: from here on a stop finds the session it may end, and no stop
    // can find the slot empty in between. A reservation that is gone — or a
    // shell that closed its capture while the driver was answering — means the
    // device is closed with this return, and nothing is recorded, silenced or
    // installed.
    if state
        .install(&reservation.ticket, admission, owner, answers)
        .is_err()
    {
        // The take was stopped — or the shell closed its capture — while the
        // driver was answering. The first frame of this attempt was already
        // archived, and the archive of an installation that never happened is
        // dropped here: it publishes that frame as an abandoned capture of a
        // take that never started.
        let _ = handshake.fail(ended_error(owner));
        return;
    }
    reservation.installed = true;

    // The take recorded: its start record carries the rate and the channels of
    // the device that really captured, and how long it took to open, to run
    // and to deliver the frame the command was answered with.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(session) = &session {
        session.event(started_event(&opened.timing));
    }

    // The answer may only go out for an installed recording: a renderer that
    // gave the command up — it timed out, or the take was stopped in the same
    // instant — gets no capture that records behind its back.
    let answered = handshake.ready(opened.start).is_ok();

    // A stop that found the session, an answer nobody heard, the shell closing
    // its capture: the take is over and must neither record nor silence.
    let stopped = !answered
        || !current(&state.closed, admission)
        || matches!(control.try_recv(), Ok(()) | Err(TryRecvError::Disconnected));

    // The first frame is out, so a dictation may silence the system: the pinned
    // setting decides, and a failure is a warning on the pill, never a reason to
    // stop recording. A take that was already over when its answer went out must
    // not silence anything: its engage carries the setting off, and the mute
    // worker refuses an engage of a generation it has already released — see
    // `audio::Marks` — so this call cannot silence a system whose take is gone.
    // Nothing is warned about it either: the shell has moved on from this take.
    // The mute is given back at the end of this thread, whichever way it ends.
    if owner.is_dictation() {
        if let Err(cause) = audio::engage(owner.generation(), mute_audio && !stopped) {
            eprintln!("speechek: {cause}.");
            if !stopped {
                mute_warning(&app, owner.generation());
            }
        }
    }

    let mut failure = None;
    if !stopped {
        loop {
            let idle = !drain(&opened.samples, &opened.free, &mut opened.encoder);
            if let Ok(reason) = opened.errors.try_recv() {
                failure = Some(reason.into_error());
                break;
            }
            match control.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {}
            }
            if idle {
                thread::sleep(IDLE_POLL);
            }
        }
    }

    // The device is released before the tail is sent, so no callback can append
    // to a capture that is already closing.
    drop(opened.stream);
    // Everything the stream had delivered is still this take's: the buffers
    // that were already queued are drained into the encoder before its tail is
    // flushed.
    drain(&opened.samples, &opened.free, &mut opened.encoder);
    opened.encoder.flush();
    // The capture is over, and this is the last thing the take's archive hears
    // from this thread: the last sequence is published and the writer is told
    // to flush the tail, *before* the stop command is answered below, so a stop
    // that returns the last sequence is already followed by this take's capture
    // end. The publish is atomic and never encodes, verifies or joins anything,
    // so the stop deadline above is the one it always was.
    //
    // The outcome here is deliberately neutral. A take's terminal belongs to
    // the shell — a cancelled dictation, a laboratory window closing, the shell
    // leaving — and a capture that simply ran to its own stop claims nothing:
    // it is published as the lowest outcome there is, so only a real terminal
    // can ever rise above it. A stream that failed on its own is the one thing
    // this thread knows for certain, and that is an error.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    {
        let outcome = if failure.is_some() {
            Outcome::Error
        } else {
            Outcome::Abandoned
        };
        opened.encoder.finish_diagnostics(outcome);
    }
    // A device that failed while the take was recording is the other failure
    // the numeric events cannot carry: the same descriptor the renderer is
    // about to be shown, redacted against the ring this capture was started
    // under, so a stream that stopped on its own can be read back with the
    // driver's own words in it.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let (Some(error), Some(session)) = (&failure, &session) {
        let body = serde_json::json!({
            "stage": "stream-failed",
            "outcome": "error",
            "code": error.code,
            "message": error.message,
            "ui": { "key": error.ui.key, "args": error.ui.args },
        });
        let _ = session.redacted_content_json(ContentKind::CaptureContext, &body);
    }
    if let Some(error) = &failure {
        mic_error(&app, owner, error);
        eprintln!("speechek: {error}.");
    }
    // The capture of a dictation is over, so the silence ends with it. A release
    // that fails is the same warning as an engage that failed, and changes
    // nothing else. A laboratory take never engaged, so it never releases.
    if owner.is_dictation() {
        if let Err(cause) = audio::release(owner.generation()) {
            eprintln!("speechek: {cause}.");
            mute_warning(&app, owner.generation());
        }
    }
    // A stream that failed is never reported as a whole tail: the renderer must
    // not transcribe a capture whose end it cannot trust.
    let _ = landed.send(match failure {
        Some(error) => Err(error),
        None => Ok(opened.encoder.sequence),
    });
}

/// Tells the window of `owner` that its microphone is gone. The renderer
/// decides what the user reads; this is the reason, never a secret.
fn mic_error(app: &AppHandle, owner: CaptureOwner, error: &UiError) {
    let mut ui = error.ui.clone();
    crate::redact_capture_ui(app, owner, &mut ui);
    let notice = CaptureNotice {
        generation: owner.generation(),
        message: i18n::render(Language::En, &ui).into_owned(),
        ui,
        last: false,
    };
    if let Err(err) = app.emit_to(owner.label(), EVENT_MIC_ERROR, notice) {
        eprintln!(
            "speechek: cannot hand a microphone error to the {} window: {err}.",
            owner.label()
        );
    }
}

/// Tells the overlay that the system mute of this take did not work out.
///
/// The fixed sentence is what the user reads; the reason only reaches the log.
/// The take keeps recording and the pill only gains a line, so this is the one
/// shell message that never ends a dictation. Only a dictation has a mute, so
/// this is never sent to the laboratory window.
pub(crate) fn mute_warning(app: &AppHandle, generation: u64) {
    notice(app, generation, false);
}

/// The same sentence for a take that is already over: the shell cancelled it and
/// the sound it silenced was not given back.
///
/// Its pill is down by then, so this line is marked as the take's last: the
/// renderer paints it on its own and then asks the shell to bring the pill back
/// for it — see [`crate::mute_warning_shown`].
pub(crate) fn mute_warning_after_take(app: &AppHandle, generation: u64) {
    notice(app, generation, true);
}

/// Hands one notice to the overlay; a failure only reaches the log.
fn notice(app: &AppHandle, generation: u64, last: bool) {
    let mut ui = audio::mute_failed();
    crate::redact_capture_ui(app, CaptureOwner::Dictation(generation), &mut ui);
    let warning = CaptureNotice {
        generation,
        message: i18n::render(Language::En, &ui).into_owned(),
        ui,
        last,
    };
    if let Err(err) = app.emit_to(OVERLAY_LABEL, EVENT_MUTE_WARNING, warning) {
        eprintln!("speechek: cannot hand a mute warning to the overlay: {err}.");
    }
}

/* -------------------------------------------------------------------------- */
/* Tests                                                                       */
/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::{
        current, is_stopping, parse_selected, retry, stop_right, Active, AttemptError,
        CaptureOwner, CaptureStart, Gone, Handshake, MessageId, NativeCapture, OpenError, Opening,
        Reservation, Retry, Session, StopRight, StreamFailure, UiError, UiMessage,
    };
    use cpal::{
        BackendSpecificError, BuildStreamError, DefaultStreamConfigError, PlayStreamError,
        StreamError, SupportedStreamConfigsError,
    };
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// A failure whose reason is one catalog key: a test builds errors here
    /// instead of spelling prose, and never asserts on the reason's text or
    /// key — only on the behavior the error travels through.
    fn failure(code: &'static str, key: MessageId) -> UiError {
        UiError::new(code, UiMessage::new(key))
    }

    /// Which direction the fallback policy sent a failure: the policy is the
    /// behavior under test, not the reason's wording.
    fn retry_kind(outcome: Retry) -> &'static str {
        match outcome {
            Retry::System(_) => "system",
            Retry::End(_) => "end",
            Retry::Ended(_) => "ended",
        }
    }

    /// A dictation that was admitted and then cancelled before its capture
    /// started must not be treated as one that may record or silence.
    #[test]
    fn a_cancelled_admission_stops_the_capture() {
        let closed = AtomicU64::new(3);
        // The promise the command read while it held the session lock.
        let admission = closed.load(Ordering::SeqCst);
        let (_stop, control) = mpsc::channel::<()>();
        assert!(current(&closed, admission));
        assert!(
            !is_stopping(&closed, admission, true, &control),
            "an admitted take whose handshake is still waiting records"
        );

        // A cancel closes whatever is capturing *before* it queues the stop: the
        // token moves, so the worker refuses to record and to mute from here on
        // even though nothing has reached the stop channel yet.
        closed.fetch_add(1, Ordering::SeqCst);
        assert!(!current(&closed, admission));
        assert!(
            is_stopping(&closed, admission, true, &control),
            "the take is over although no stop is queued"
        );
    }

    /// The queued stop, a renderer that is no longer listening and a stop
    /// channel whose sender is gone each end the take on their own.
    #[test]
    fn every_way_a_capture_ends_stops_the_worker() {
        let closed = AtomicU64::new(0);
        let (stop, control) = mpsc::channel::<()>();
        assert!(!is_stopping(&closed, 0, true, &control));
        assert!(
            is_stopping(&closed, 0, false, &control),
            "nobody waits for the handshake"
        );
        stop.send(()).unwrap();
        assert!(
            is_stopping(&closed, 0, true, &control),
            "a queued stop ends the take"
        );
        drop(stop);
        assert!(
            is_stopping(&closed, 0, true, &control),
            "a stop channel whose sender is gone is the same verdict"
        );
    }

    /// One handshake whose answer nobody has read yet.
    fn new_handshake() -> (Handshake, mpsc::Receiver<Result<CaptureStart, UiError>>) {
        let (ready, answers) = mpsc::sync_channel(1);
        (Handshake::new(ready), answers)
    }

    /// The answer names the device that really captured — including the system
    /// device that stood in for one that was gone — and it goes out exactly
    /// once. A queued stop ends the take only while the answer is still waiting;
    /// once it is out, the stop is the recording's own end and stays for the
    /// capture loop to see.
    #[test]
    fn the_handshake_answers_once_with_the_device_that_really_captured() {
        let closed = AtomicU64::new(4);
        let start = CaptureStart {
            fallback_device: Some("Microphone (USB)".to_string()),
            used_device_id: Some("wasapi:card-one".to_string()),
        };

        let (mut handshake, answers) = new_handshake();
        let (_stop, control) = mpsc::channel::<()>();
        assert!(
            !handshake.over(&closed, 4, &control),
            "a waiting handshake has started nothing yet"
        );
        assert_eq!(
            handshake.ready(start.clone()),
            Ok(()),
            "the first frame is what answers"
        );
        let answer = answers.recv_timeout(Duration::from_millis(50))
            .expect("the capture answers before the deadline")
            .expect("the first frame starts capture");
        assert_eq!(answer, start, "the answer carries the device that captured");
        assert_eq!(
            handshake.ready(start.clone()),
            Err(()),
            "one answer is the whole handshake"
        );
        assert!(
            handshake.over(&closed, 4, &control),
            "an answered take is past its handshake"
        );

        // A stop that is already queued ends a take whose answer has not gone
        // out: only a take that is over queues one.
        let (handshake, _answers) = new_handshake();
        let (stop, control) = mpsc::channel::<()>();
        stop.send(()).expect("the handshake's stop channel lives");
        assert!(
            handshake.over(&closed, 4, &control),
            "a queued stop ends the waiting take"
        );

        // After the answer the same stop is the recording's own end: the
        // handshake does not consume it, so the capture loop still sees it.
        let (mut handshake, _answers) = new_handshake();
        let (stop, control) = mpsc::channel::<()>();
        assert_eq!(handshake.ready(start.clone()), Ok(()));
        stop.send(()).expect("the handshake's stop channel lives");
        assert!(
            handshake.over(&closed, 4, &control),
            "a take that answered is over for the handshake"
        );
        assert!(
            matches!(control.try_recv(), Ok(())),
            "the stop that ends the recording is left for the capture loop"
        );

        // Nobody waiting for the answer: the capture must not start, and a
        // failure reported afterwards is not a second answer.
        let (mut handshake, answers) = new_handshake();
        drop(answers);
        assert_eq!(
            handshake.ready(start.clone()),
            Err(()),
            "nobody heard the first frame"
        );
        let (_stop, control) = mpsc::channel::<()>();
        assert!(handshake.over(&closed, 4, &control));
        assert_eq!(
            handshake.fail(failure("MICROPHONE_ERROR", MessageId::MicNoSystemDevice)),
            Err(())
        );

        // A failure before the first frame answers the command.
        let (mut handshake, answers) = new_handshake();
        assert_eq!(
            handshake.fail(failure("MICROPHONE_ERROR", MessageId::MicNoSystemDevice)),
            Ok(())
        );
        let answer = answers
            .recv_timeout(Duration::from_millis(50))
            .expect("the failure answer arrives");
        assert!(
            answer.is_err(),
            "a failure before the first frame answers the command"
        );

        // A take that was given up on while its device was opening never starts.
        let (handshake, _answers) = new_handshake();
        let (_stop, control) = mpsc::channel::<()>();
        closed.fetch_add(1, Ordering::SeqCst);
        assert!(handshake.over(&closed, 4, &control), "the promise moved");
    }

    /// The fallback policy, decided without a microphone: the system device
    /// stands in only for a selected device that is gone, and only once. Access,
    /// format and busyness of a device that exists are the take's own failure,
    /// and a take that is already over reports nothing at all.
    #[test]
    fn the_system_device_only_replaces_a_selected_one_that_is_gone() {
        let gone = || failure("MICROPHONE_ERROR", MessageId::MicNoSystemDevice);
        assert_eq!(
            retry_kind(retry(AttemptError::Fallback(gone()), false)),
            "system",
            "a selected device that is gone is replaced by the system one"
        );
        assert_eq!(
            retry_kind(retry(AttemptError::Fallback(gone()), true)),
            "end",
            "the system device is tried exactly once"
        );
        assert_eq!(
            retry_kind(retry(AttemptError::Fatal(gone()), false)),
            "end",
            "a device that exists and refuses the take is not replaced"
        );
        assert_eq!(
            retry_kind(retry(AttemptError::Fatal(gone()), true)),
            "end",
            "a system device that refused the take ends it"
        );
        let over = || failure("CAPTURE_ENDED", MessageId::CaptureEndedDictation);
        assert_eq!(
            retry_kind(retry(AttemptError::Ended(over()), false)),
            "ended",
            "a take that is over reports no failure"
        );
    }

    /// Only a device that is really gone earns the system fallback: a refusal
    /// of the take — an unsupported format, a denied access, a device another
    /// stream holds — never does, whatever the message says.
    #[test]
    fn only_a_gone_device_falls_back() {
        assert!(BuildStreamError::DeviceNotAvailable.gone());
        assert!(!BuildStreamError::StreamConfigNotSupported.gone());
        assert!(!BuildStreamError::BackendSpecific {
            err: BackendSpecificError {
                description: "AUDCLNT_E_DEVICE_IN_USE".to_string(),
            },
        }
        .gone());
        assert!(DefaultStreamConfigError::DeviceNotAvailable.gone());
        assert!(!DefaultStreamConfigError::StreamTypeNotSupported.gone());
        assert!(SupportedStreamConfigsError::DeviceNotAvailable.gone());
        assert!(!SupportedStreamConfigsError::InvalidArgument.gone());
        assert!(PlayStreamError::DeviceNotAvailable.gone());

        assert!(
            matches!(
                StreamFailure::of(StreamError::DeviceNotAvailable),
                StreamFailure::Unavailable(_)
            ),
            "a device that disappeared mid-stream is the same verdict"
        );
        assert!(
            matches!(
                StreamFailure::of(StreamError::BackendSpecific {
                    err: BackendSpecificError {
                        description: "the stream broke".to_string(),
                    },
                }),
                StreamFailure::Fatal(_)
            ),
            "a stream that just broke is not a reason to open another device"
        );
    }

    /// The device id the settings wrote is the one this shell parses back, and
    /// an id it cannot parse is a device that is gone — never a failed take.
    #[test]
    fn a_selected_device_id_round_trips_or_falls_back() {
        let parsed = parse_selected("wasapi:card-one").expect("a device id the settings accept");
        assert_eq!(parsed.to_string(), "wasapi:card-one");
        let case = parse_selected("WASAPI:card-one").expect("the host matches case-insensitively");
        assert_eq!(case.to_string(), "wasapi:card-one");
        assert!(
            matches!(parse_selected("card-one"), Err(OpenError::Fallback(_))),
            "an id without a host is a device this shell cannot find"
        );
        assert!(
            matches!(parse_selected(""), Err(OpenError::Fallback(_))),
            "an empty id is a device this shell cannot find"
        );
    }

    /* ---------------------------------------------------------------------- */
    /* Owners                                                                  */
    /* ---------------------------------------------------------------------- */

    /// One running capture of `owner`, as the slot sees it: a real worker that
    /// reports `tail` once its own stop arrives.
    fn captured(owner: CaptureOwner, tail: Result<u64, UiError>) -> Session {
        let (stop, control) = mpsc::channel::<()>();
        let (landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        let worker = thread::spawn(move || {
            let _ = control.recv();
            let _ = landed_tx.send(tail);
        });
        Session {
            owner,
            ticket: Arc::new(()),
            stop,
            landed,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            diagnostics: None,
            thread: Some(worker),
        }
    }

    /// One reservation of `owner` in the slot, before any worker exists for it;
    /// the returned receiver is that reservation's stop channel. The admission
    /// is the token a caller reads while it holds the session lock.
    fn reserved(
        state: &NativeCapture,
        owner: CaptureOwner,
        admission: u64,
    ) -> (Arc<()>, mpsc::Receiver<()>) {
        let ticket = Arc::new(());
        let (stop, control) = mpsc::channel::<()>();
        state
            .reserve(Opening {
                owner,
                admission,
                ticket: Arc::clone(&ticket),
                stop,
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                diagnostics: None,
                thread: None,
            })
            .expect("the slot is free");
        (ticket, control)
    }

    /// A stop reaches only its own take: the other window's capture is refused
    /// outright, a newer take of the same window is left alone, and the take
    /// that owns the slot is the one that may be ended.
    #[test]
    fn a_stop_reaches_only_its_own_take() {
        let state = NativeCapture::default();
        *state.slot.lock() = Some(Active::Recording(captured(CaptureOwner::Dictation(4), Ok(9))));

        assert_eq!(
            stop_right(&state.slot.lock(), CaptureOwner::Dictation(4)),
            StopRight::Mine
        );
        assert_eq!(
            stop_right(&state.slot.lock(), CaptureOwner::Dictation(5)),
            StopRight::Stale,
            "an older generation of the same window owns no stream any more"
        );
        assert_eq!(
            stop_right(&state.slot.lock(), CaptureOwner::Lab(4)),
            StopRight::Foreign,
            "the same number in the other window is a different take"
        );

        // A laboratory stop may not reach into a dictation's capture, whatever
        // number it carries: the slot keeps recording its own take.
        assert!(state.take_for_stop(CaptureOwner::Lab(4)).is_err());
        assert_eq!(
            state.slot.lock().as_ref().map(Active::owner),
            Some(CaptureOwner::Dictation(4)),
            "the refused stop left the capture alone"
        );
        // A stale stop of the same window has nothing of its own and takes
        // nothing from the newer take.
        assert!(matches!(
            state.take_for_stop(CaptureOwner::Dictation(5)),
            Ok(None)
        ));
        let mine = state
            .take_for_stop(CaptureOwner::Dictation(4))
            .expect("its own stop takes the capture");
        assert_eq!(mine.map(|active| active.owner()), Some(CaptureOwner::Dictation(4)));
        // The take is out of the slot now, so a second stop finds nothing.
        assert!(matches!(
            state.take_for_stop(CaptureOwner::Dictation(4)),
            Ok(None)
        ));
    }

    /// The microphone is claimed before a worker exists: a second start of the
    /// same take, a start of the other window and a second reservation are all
    /// refused while one take is still opening its device, and a stop of that
    /// take hands the microphone to the next one immediately.
    #[test]
    fn a_second_start_finds_the_microphone_taken() {
        let state = NativeCapture::default();
        let (ticket, control) = reserved(&state, CaptureOwner::Lab(1), 0);

        assert!(
            state.claim(CaptureOwner::Lab(1)).is_err(),
            "the same laboratory take is already opening"
        );
        assert!(
            state.claim(CaptureOwner::Dictation(7)).is_err(),
            "a dictation never shares the microphone with the laboratory"
        );
        assert!(
            state
                .reserve(Opening {
                    owner: CaptureOwner::Dictation(7),
                    admission: 0,
                    ticket: Arc::new(()),
                    stop: mpsc::channel().0,
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    diagnostics: None,
                    thread: None,
                })
                .is_err(),
            "the slot is held by another take"
        );

        // The laboratory's stop takes its own reservation and tells the worker
        // to close; the slot is free for the next take right away, without
        // waiting for the driver of the one that was stopped.
        assert!(state.withdraw(CaptureOwner::Lab(1), &ticket));
        assert!(matches!(
            control.try_recv(),
            Ok(()) | Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(state.slot.lock().is_none());
        assert!(
            state.claim(CaptureOwner::Dictation(7)).is_ok(),
            "the stopped reservation hands the microphone to the next take"
        );
    }

    /// A take that is stopped while its device is still opening can never
    /// install itself, and neither can one whose shell closed the capture while
    /// the driver was answering: the slot stays free, and a worker that ends
    /// without installing gives its reservation back on its own.
    #[test]
    fn a_withdrawn_reservation_can_never_install_itself() {
        let state = NativeCapture::default();
        let (ticket, control) = reserved(&state, CaptureOwner::Dictation(3), 0);
        assert!(state.withdraw(CaptureOwner::Dictation(3), &ticket));

        // A stop of another take — another generation of the same window, or
        // the other window — cannot take this reservation, and a reservation
        // that was already taken cannot be taken twice.
        assert!(!state.withdraw(CaptureOwner::Dictation(4), &ticket));
        assert!(!state.withdraw(CaptureOwner::Lab(3), &ticket));
        assert!(!state.withdraw(CaptureOwner::Dictation(3), &ticket));

        let _ = control;

        // The worker's late answer is refused: nothing is installed behind the
        // stop, and the slot stays empty.
        assert!(state
            .install(
                &ticket,
                0,
                CaptureOwner::Dictation(3),
                mpsc::channel::<Result<u64, UiError>>().1,
            )
            .is_err());
        assert!(state.slot.lock().is_none());

        // A shell that closed its capture while the driver was answering never
        // gets a recording installed: the admission is compared under the same
        // lock as the installation itself.
        let (ticket, _control) = reserved(&state, CaptureOwner::Lab(4), 0);
        state.closed.fetch_add(1, Ordering::SeqCst);
        let (_landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        assert!(state.install(&ticket, 0, CaptureOwner::Lab(4), landed).is_err());
        assert!(
            state.slot.lock().is_some(),
            "the reservation stays the take's, so its own worker gives it back"
        );
        assert!(state.withdraw(CaptureOwner::Lab(4), &ticket));

        // A worker that ends without installing gives its reservation back,
        // whatever way it ended.
        let (ticket, _control) = reserved(&state, CaptureOwner::Lab(2), 0);
        {
            let _guard = Reservation {
                state: &state,
                owner: CaptureOwner::Lab(2),
                ticket,
                installed: false,
            };
        }
        assert!(
            state.slot.lock().is_none(),
            "a worker that is gone leaves no reservation"
        );

        // A worker that installed keeps the slot: the recording is the slot now,
        // and the guard must not take it back.
        let admission = state.closed.load(Ordering::SeqCst);
        let (ticket, _control) = reserved(&state, CaptureOwner::Lab(3), admission);
        let (_landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        assert!(state
            .install(&ticket, admission, CaptureOwner::Lab(3), landed)
            .is_ok());
        {
            let _guard = Reservation {
                state: &state,
                owner: CaptureOwner::Lab(3),
                ticket: Arc::clone(&ticket),
                installed: true,
            };
        }
        assert!(
            matches!(state.slot.lock().as_ref(), Some(Active::Recording(_))),
            "an installed recording is left alone"
        );
    }

    /// The bounded exit ends a capture without waiting for its driver or its
    /// renderer: the stop is sent, the slot is freed and the admission moves, so
    /// a take that is still opening can never claim the microphone behind this.
    /// The recording's tail is not consumed — nothing on this path waits on the
    /// landed channel.
    #[test]
    fn the_bounded_exit_ends_a_capture_without_waiting_for_it() {
        let state = NativeCapture::default();
        let (stop, control) = mpsc::channel::<()>();
        let (landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&dropped);
        let worker = thread::spawn(move || {
            let _ = control.recv();
            // The exit dropped the receiver instead of reading the tail, so the
            // worker's own report fails exactly when nothing waited for it.
            if landed_tx.send(Ok(12)).is_err() {
                observed.store(true, Ordering::SeqCst);
            }
        });
        *state.slot.lock() = Some(Active::Recording(Session {
            owner: CaptureOwner::Dictation(6),
            ticket: Arc::new(()),
            stop,
            landed,
            thread: Some(worker),
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            diagnostics: None,
        }));

        state.end_for_exit();
        assert!(
            state.slot.lock().is_none(),
            "the exit took the capture out of the shell's hands"
        );
        // The admission moved before the slot was touched: a take that was
        // still opening its device never installs itself behind the exit.
        assert!(!current(&state.closed, 0), "the exit closed the admission");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !dropped.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "the exit sent the stop and gave up the tail instead of waiting for it"
        );

        // A reservation that was still opening is closed through its own stop,
        // and the shell's admission never comes back.
        let (_ticket, control) = reserved(&state, CaptureOwner::Lab(1), 0);
        state.end_for_exit();
        assert!(state.slot.lock().is_none(), "the opening take is gone");
        assert!(matches!(
            control.try_recv(),
            Ok(()) | Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(
            !current(&state.closed, 0),
            "the exit closed the admission for good"
        );
    }

    /// A command that gave up on its take cleans up only what that very take
    /// installed: a newer recording that claimed the microphone in the meantime
    /// is left exactly as it is.
    #[test]
    fn a_command_that_gave_up_touches_only_its_own_take() {
        let state = NativeCapture::default();
        // A newer take of the same window owns the microphone now.
        let (newer, _control) = reserved(&state, CaptureOwner::Lab(2), 0);
        let (landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        // The worker that would report the tail is gone with the command, so
        // the cleanup does not wait on it.
        drop(landed_tx);
        assert!(state.install(&newer, 0, CaptureOwner::Lab(2), landed).is_ok());

        // The older command gives up on a ticket that names nothing any more:
        // the newer recording is not touched.
        let older = Arc::new(());
        state.abandon(CaptureOwner::Lab(1), &older);
        assert_eq!(
            state.slot.lock().as_ref().map(Active::owner),
            Some(CaptureOwner::Lab(2)),
            "the newer recording is left alone"
        );

        // Its own ticket ends exactly its own leftover, whoever claimed the
        // microphone in between.
        state.abandon(CaptureOwner::Lab(2), &newer);
        assert!(
            state.slot.lock().is_none(),
            "its own leftover is closed and the slot is free"
        );
    }

    /// A capture thread that is gone, or whose stream failed, is never reported
    /// as a whole tail: the renderer must not transcribe a broken end.
    #[test]
    fn a_broken_tail_is_an_error_not_an_empty_capture() {
        let (stop, _control) = mpsc::channel::<()>();
        let (landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        drop(landed_tx);
        let session = Session {
            owner: CaptureOwner::Dictation(1),
            ticket: Arc::new(()),
            stop,
            landed,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            diagnostics: None,
            thread: None,
        };
        assert!(
            session.finish().is_err(),
            "a worker that is gone reports nothing whole"
        );

        let failed = captured(
            CaptureOwner::Dictation(1),
            Err(failure("MICROPHONE_ERROR", MessageId::MicStreamStopped)),
        )
        .finish();
        assert!(failed.is_err(), "a stream that failed keeps its reason");
        assert_eq!(
            captured(CaptureOwner::Dictation(1), Ok(12))
                .finish()
                .expect("a capture that ended normally reports its last frame"),
            12,
        );
    }

    /// Every take speaks to its own window and never to the other one, and the
    /// two number spaces are independent: the same number names a different
    /// take in each window.
    #[test]
    fn each_owner_speaks_to_its_own_window() {
        assert_eq!(CaptureOwner::Dictation(3).label(), crate::OVERLAY_LABEL);
        assert_eq!(CaptureOwner::Lab(3).label(), crate::LAB_LABEL);
        assert_eq!(CaptureOwner::Dictation(3).generation(), 3);
        assert_eq!(CaptureOwner::Lab(3).generation(), 3);
        assert!(CaptureOwner::Dictation(3).is_dictation());
        assert!(!CaptureOwner::Lab(3).is_dictation());
        assert!(CaptureOwner::Dictation(3).same_kind(CaptureOwner::Dictation(9)));
        assert!(!CaptureOwner::Dictation(3).same_kind(CaptureOwner::Lab(3)));
        assert_ne!(CaptureOwner::Dictation(3), CaptureOwner::Lab(3));
    }

    /* ---------------------------------------------------------------------- */
    /* The diagnostics framing seam                                            */
    /* ---------------------------------------------------------------------- */

    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    use super::{note_refused_start, pcm16, ClaimError, CollectEmitter, DiagnosticsFramingFixture, Encoder};
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    use crate::diagnostics::{
        AudioStatus, Diagnostics, Mode, Outcome, Owner, Policy, SessionHandle, SessionKey,
    };

    /// A registered take of a sink that keeps no files: the framing seams need
    /// a handle, not a disk.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn scratch_session() -> (Arc<Diagnostics>, SessionHandle) {
        let sink = Diagnostics::for_test(
            &std::env::temp_dir().join("speechek-capture-framing"),
            Policy {
                max_age: Duration::from_secs(600),
                max_bytes: 1_000_000,
            },
            std::time::SystemTime::UNIX_EPOCH,
        );
        let handle = scratch_take(&sink, Owner::Dictation, 1);
        (sink, handle)
    }

    /// Another registered take of the same sink, named by its own owner and
    /// generation: the key a refusal of *another* take is answered with, so a
    /// test can tell the take it names from the one that is already running.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn scratch_take(sink: &Arc<Diagnostics>, owner: Owner, generation: u64) -> SessionHandle {
        sink.begin(SessionKey { owner, generation }, Mode::Live)
            .expect("a take of this sink")
    }

    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn framing(handle: &SessionHandle, source_rate: u32) -> DiagnosticsFramingFixture {
        DiagnosticsFramingFixture::new(handle.key(), source_rate, handle.clone())
    }

    /// A deterministic source signal: no randomness, and no silence that a
    /// resampler could hide a mistake in.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn signal(len: usize) -> Vec<f32> {
        (0..len)
            .map(|index| ((index % 64) as f32 - 32.0) / 32.0)
            .collect()
    }

    /// The PCM16 little-endian bytes of `source`, as a frame carries them.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn pcm_bytes(source: &[f32]) -> Vec<u8> {
        source
            .iter()
            .flat_map(|level| pcm16(*level).to_le_bytes())
            .collect()
    }

    /// At the recorder's own rate the resampler is the identity: one batch of
    /// source frames becomes the frames of the same samples, byte for byte,
    /// with nothing padded, trimmed or normalised.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn the_encoder_frames_the_exact_samples_it_is_given() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        let source = signal(513);
        fixture.push(&source);

        let frames = fixture.take_frames();
        assert_eq!(frames.len(), 1, "one full frame; the rest still waits");
        assert_eq!(frames[0].0, 1, "frames are numbered from one");
        assert_eq!(frames[0].1, pcm_bytes(&source[..512]));
    }

    /// The encoder is created once and kept: where a batch happens to end never
    /// moves a frame boundary, and the sequence never restarts.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn one_encoder_keeps_its_resampler_across_batches() {
        let source = signal(8_000);

        let (_sink, handle) = scratch_session();
        let mut whole = framing(&handle, 48_000);
        whole.push(&source);
        let in_one = whole.take_frames();

        let (_sink, handle) = scratch_session();
        let mut split = framing(&handle, 48_000);
        for chunk in source.chunks(97) {
            split.push(chunk);
        }
        let in_batches = split.take_frames();

        assert!(!in_one.is_empty(), "the source really does frame");
        assert_eq!(in_one, in_batches, "the batch edges moved no frame");
        let sequences: Vec<u64> = in_one.iter().map(|(sequence, _)| *sequence).collect();
        assert_eq!(
            sequences,
            (1..=in_one.len() as u64).collect::<Vec<_>>(),
            "one encoder numbers its frames straight through"
        );
    }

    /// The tail a stop leaves behind goes out exactly once, and it carries the
    /// samples that were really captured: the last source frame waited for a
    /// neighbour that never came, and went with the stream.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn the_tail_goes_out_once_and_keeps_the_captured_samples() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        let source = signal(600);
        fixture.push(&source);
        let mut frames = fixture.take_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, pcm_bytes(&source[..512]));

        fixture.finish(Outcome::Success);
        let tail = fixture.take_frames();
        assert_eq!(tail.len(), 1, "the tail is the only frame left");
        assert_eq!(tail[0].0, 2, "the tail keeps the sequence it follows");
        assert_eq!(
            tail[0].1,
            pcm_bytes(&source[512..599]),
            "the tail is what was captured, with no padding and no repeat"
        );
        frames.extend(tail);

        let emitted: Vec<u8> = frames.iter().flat_map(|(_, bytes)| bytes.clone()).collect();
        assert_eq!(
            emitted,
            pcm_bytes(&source[..599]),
            "one source frame — the one that waited for its neighbour — is all that is lost"
        );
    }


    /// An installation that is refused drops the encoder with the frame it
    /// already emitted: that frame is kept, and the capture of a take that
    /// never started is over all the same.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn a_refused_installation_keeps_the_frame_it_archived() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        fixture.push(&signal(513));
        assert_eq!(fixture.take_frames().len(), 1);

        fixture.abandon_take();

        let counters = handle.capture_counters();
        assert_eq!(counters.produced_frames, 1, "the first frame is kept");
        assert_eq!(counters.last_sequence, Some(1));
        assert!(
            handle.capture_finished(),
            "the capture of a take that was never installed is over"
        );
    }

    /// A take whose device never delivered a frame has no archive at all: its
    /// drop closes nothing, so the system fallback of the attempt is free to
    /// try the next device.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn an_attempt_that_never_framed_leaves_no_archive_behind() {
        let (_sink, handle) = scratch_session();
        let fixture = framing(&handle, 16_000);
        fixture.abandon_take();

        assert_eq!(handle.capture_counters().produced_frames, 0);
        assert!(
            !handle.capture_finished(),
            "no frame was ever captured, so there is no capture to close"
        );
    }

    /// Once the shell has closed the capture, a frame the encoder still
    /// produces is a hole: it is counted, it never reaches the archive, and it
    /// does not open one behind the take's back.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn a_late_frame_after_the_capture_closed_is_a_gap() {
        let (_sink, handle) = scratch_session();
        let mut encoder = Encoder::with_emitter(
            CollectEmitter::default(),
            CaptureOwner::Dictation(1),
            16_000,
        );
        encoder.with_session(Some(handle.clone()));
        encoder.push(&signal(513));
        assert_eq!(handle.capture_counters().produced_frames, 1);

        // The shell cancelled the take while this encoder was still running.
        handle.end_capture(Outcome::Cancelled);
        encoder.push(&signal(513));

        let counters = handle.capture_counters();
        assert_eq!(counters.produced_frames, 2, "the frame was still produced");
        assert_eq!(
            counters.dropped_frames, 1,
            "but it never reached the archive"
        );
        assert!(counters.has_gaps());
        assert_eq!(counters.last_sequence, Some(2));
        assert_eq!(
            handle.audio_status(),
            AudioStatus::Recording,
            "the take keeps the one archive it took"
        );
    }

    /// Once the capture has published its last sequence, nothing may open an
    /// archive behind it: a frame that arrives later is not part of the take.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn a_frame_after_the_capture_finished_is_ignored() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        fixture.push(&signal(513));
        fixture.finish(Outcome::Success);
        let counters = handle.capture_counters();

        fixture.push(&signal(513));

        assert_eq!(handle.capture_counters(), counters, "nothing moved");
    }

    /// A window that cannot be reached loses nothing of the take: the frame is
    /// still produced and still archived, and the sequence goes on.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn the_archive_takes_a_frame_the_window_never_got() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        fixture.fail_emits();
        fixture.push(&signal(513));

        assert!(fixture.take_frames().is_empty(), "the window kept nothing");
        let counters = handle.capture_counters();
        assert_eq!(counters.produced_frames, 1);
        assert_eq!(counters.emit_failed_frames, 1);
        assert_eq!(counters.last_sequence, Some(1));
        assert!(!counters.has_gaps(), "the archive really took the frame");
    }

    /// A writer that never catches up costs frames, never the capture: the
    /// sequence keeps running through the holes.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn dropped_frames_never_move_the_capture_watermark() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        fixture.push(&signal(512 * 600));

        let counters = handle.capture_counters();
        assert!(
            counters.produced_frames > 256,
            "far more frames than the pool holds"
        );
        assert!(counters.dropped_frames > 0, "the queue really ran dry");
        assert_eq!(
            counters.last_sequence,
            Some(counters.produced_frames),
            "the capture keeps its own watermark through the holes"
        );
    }

    /* ---------------------------------------------------------------------- */
    /* A refused start and a stalled stop                                       */
    /* ---------------------------------------------------------------------- */

    /// A repeated start of a take that is already capturing is refused, and the
    /// running capture is left exactly as it was: its frames keep arriving, its
    /// archive is never closed behind it, and its own tail still publishes.
    ///
    /// This is the bug this test guards: the refusal of this very take is not a
    /// failure of *its* capture, so nothing may be closed for it.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn a_duplicate_start_never_closes_the_running_capture() {
        let (sink, handle) = scratch_session();
        let state = NativeCapture::default();
        let (_ticket, _control) = reserved(&state, CaptureOwner::Dictation(1), 0);

        let mut fixture = framing(&handle, 16_000);
        fixture.push(&signal(513));
        assert_eq!(fixture.take_frames().len(), 1);

        // The same take starts again while its capture is still running. The
        // start runs the very decision the command path runs on a refusal: the
        // running capture must survive it untouched.
        let refusal = state
            .claim(CaptureOwner::Dictation(1))
            .err()
            .expect("a repeated start of the same take is refused");
        assert!(
            matches!(refusal, ClaimError::SameTake(_)),
            "the slot holds this very take, not another one"
        );
        note_refused_start(&refusal, &Some(handle.clone()));
        assert!(
            state.slot.lock().is_some(),
            "the recording is still the shell's"
        );
        assert!(
            !handle.capture_finished(),
            "the duplicate start closed a capture that was already running"
        );
        // A start of the other window is refused as well, and that refusal is
        // the one that really ends a take's capture: the take it names never
        // began. Its owner and its number both differ from the dictation that
        // holds the slot, so this refusal is not the same-take verdict however
        // the two number spaces are read. The take a caller hands to the
        // decision is its own — the lab take — so only that one's capture ends
        // and the running dictation is left exactly as it was.
        let lab = scratch_take(&sink, Owner::Lab, 7);
        let foreign = state
            .claim(CaptureOwner::Lab(7))
            .err()
            .expect("the other window's start is refused");
        assert!(!matches!(foreign, ClaimError::SameTake(_)));
        note_refused_start(&foreign, &Some(lab.clone()));
        assert!(
            lab.capture_finished(),
            "the refused start ends the capture of its own take"
        );
        assert!(
            !handle.capture_finished(),
            "the other window's refusal names another take"
        );

        // The real capture went on: its next frame is archived, not a hole, and
        // its sequence kept running through the refusals.
        let source = signal(513);
        fixture.push(&source);
        let second = fixture.take_frames();
        assert_eq!(second.len(), 1, "the refusal left the framing alone");
        assert_eq!(
            second[0].1.len(),
            1_024,
            "a whole frame of the take's own samples"
        );
        assert_eq!(second[0].0, 2, "the sequence went straight on");

        // The take's own end still publishes, and the short tail the capture
        // was still holding goes out with it: the tail is exactly the one
        // captured sample that did not complete a frame, so every captured
        // sample reached the archive and none of them became a hole.
        fixture.finish(Outcome::Success);
        let tail = fixture.take_frames();
        assert_eq!(tail.len(), 1, "the stopped capture sends its short tail");
        assert_eq!(
            tail[0].1,
            pcm_bytes(&source[511..512]),
            "the tail is the captured sample a full frame could not hold"
        );
        let counters = handle.capture_counters();
        assert!(
            handle.capture_finished(),
            "the take's own capture end stands"
        );
        assert_eq!(
            counters.last_sequence,
            Some(tail[0].0),
            "the published watermark is the tail's own frame"
        );
        assert_eq!(
            counters.produced_samples,
            512 + 512 + 1,
            "two whole frames and the one captured tail sample"
        );
        assert_eq!(counters.dropped_frames, 0, "the whole capture is archived");
    }

    /// A stop that gives up on a worker which never sends its tail publishes the
    /// take's capture end itself: the take is no longer open for frames that
    /// cannot belong to it, while its observed counters and watermark stay the
    /// ones it really produced.
    #[test]
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn a_stalled_stop_publishes_the_capture_end() {
        let (_sink, handle) = scratch_session();
        let mut fixture = framing(&handle, 16_000);
        fixture.push(&signal(513));
        let produced = handle.capture_counters();

        // One running capture whose worker holds the tail back: the stop is
        // sent and the command waits out its whole deadline.
        let (stop, _control) = mpsc::channel::<()>();
        let (landed_tx, landed) = mpsc::channel::<Result<u64, UiError>>();
        let session = Session {
            owner: CaptureOwner::Dictation(1),
            ticket: Arc::new(()),
            stop,
            landed,
            diagnostics: Some(handle.clone()),
            thread: None,
        };
        // The sender is kept alive, so the wait ends on its deadline and not on
        // a dropped channel: exactly the stall the deadline exists for.
        assert!(session.finish().is_err(), "the tail never came");
        drop(landed_tx);

        assert!(
            handle.capture_finished(),
            "the capture is over even though the worker never reported"
        );
        let after = handle.capture_counters();
        assert_eq!(
            after.produced_frames, produced.produced_frames,
            "the observed frames are the ones really produced"
        );
        assert_eq!(after.last_sequence, produced.last_sequence);

        // A frame the stalled worker finally hands over is a hole of a capture
        // that is already over — it never reopens one.
        fixture.push(&signal(513));
        let late = handle.capture_counters();
        assert_eq!(late.dropped_frames, 1, "the late frame is a gap");
        assert_eq!(late.produced_frames, produced.produced_frames + 1);
    }
}
