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
use tauri::{AppHandle, Emitter, Manager};

use crate::audio;
use crate::{LAB_LABEL, OVERLAY_LABEL};

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
/// One frame is 512 samples, the chunk the recorder passes around.
const FRAME_SAMPLES: usize = 512;
/// A device that has not answered in this long is not going to answer.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// Windows reporting no default microphone at all. There is nothing for the
/// system fallback to try, so this always fails the take.
const NO_SYSTEM_DEVICE: &str = "Windows reports no default microphone. Connect one, or set a default input device (and allow speechek to use it in the Windows privacy settings).";
/// A take that ended while its device was still opening: its capture must not
/// start, record or silence anything.
const ENDED_WHILE_OPENING: &str = "this dictation ended while its microphone was opening.";
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
    message: String,
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

/// Turns the device's mono frames into the recorder's frames: 16 kHz mono
/// PCM16, in 512-sample chunks, emitted as they fill up.
///
/// The resampler is a linear interpolator and it keeps its read position across
/// callbacks, so where a callback happens to end never moves a frame boundary.
struct Encoder {
    app: AppHandle,
    /// The take this encoder frames and the window its frames go to.
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
}

impl Encoder {
    fn new(app: AppHandle, owner: CaptureOwner, source_rate: u32) -> Self {
        Self {
            app,
            owner,
            sequence: 0,
            pending: VecDeque::new(),
            position: 0.0,
            ratio: f64::from(source_rate) / f64::from(RATE_HZ),
            chunk: Vec::with_capacity(FRAME_SAMPLES),
            bytes: Vec::with_capacity(FRAME_SAMPLES * 2),
            complained: false,
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
        let data = BASE64.encode(&self.bytes);
        let frame = Frame {
            generation: self.owner.generation(),
            sequence,
            data,
        };
        if let Err(err) = self.app.emit_to(self.owner.label(), EVENT_PCM_FRAME, frame) {
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
    Fallback(String),
    /// No other device repairs this: the take fails with this message.
    Fatal(String),
}

impl OpenError {
    fn fallback(message: impl Into<String>) -> Self {
        Self::Fallback(message.into())
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self::Fatal(message.into())
    }

    /// The failure of one device attempt: its message, and whether the system
    /// fallback may repair it.
    fn device(message: String, gone: bool) -> Self {
        if gone {
            Self::Fallback(message)
        } else {
            Self::Fatal(message)
        }
    }
}

/// A failure the session thread has to see, as the stream reports it.
#[derive(Debug, PartialEq, Eq)]
enum StreamFailure {
    /// The device is no longer available. Before the first frame this is what
    /// the system fallback may repair; after it, the take ends.
    Unavailable(String),
    /// Anything else: the take ends with this message.
    Fatal(String),
}

impl StreamFailure {
    /// The driver's own words for the failure, classified by what another
    /// device could repair.
    fn of(err: cpal::StreamError) -> Self {
        let message = format!("Windows stopped the microphone stream: {err}");
        if err.gone() {
            Self::Unavailable(message)
        } else {
            Self::Fatal(message)
        }
    }

    fn message(self) -> String {
        match self {
            Self::Unavailable(message) | Self::Fatal(message) => message,
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
        OpenError::fallback(
            "the chosen microphone is not a device id this shell can read".to_string(),
        )
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
                OpenError::fallback(
                    "the chosen microphone is no longer connected: Windows does not list it"
                        .to_string(),
                )
            })?
        }
        None => host
            .default_input_device()
            .ok_or_else(|| OpenError::fatal(NO_SYSTEM_DEVICE.to_string()))?,
    };
    if !device.supports_input() {
        return Err(match selected {
            Some(_) => OpenError::fallback(
                "the chosen microphone cannot record: Windows does not offer it as an input device"
                    .to_string(),
            ),
            None => OpenError::fatal(NO_SYSTEM_DEVICE.to_string()),
        });
    }
    let supported = supported_config(&device)?;
    let format = supported.sample_format();
    let rate = supported.sample_rate();
    if rate == 0 {
        return Err(OpenError::fatal(
            "the microphone reports a sample rate of zero".to_string(),
        ));
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
            format!("Windows did not report a default microphone format: {err}"),
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
                format!("Windows did not report the microphone formats: {err}"),
                err.gone(),
            )
        })?
        .find(|range| SAMPLED.contains(&range.sample_format()))
        .map(|range| {
            let rate = range.max_sample_rate();
            range.with_sample_rate(rate)
        });
    fallback.ok_or_else(|| {
        OpenError::fatal(format!(
            "the microphone only offers {} samples, which the shell cannot read",
            default.sample_format()
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
        other => Err(OpenError::fatal(format!(
            "the microphone speaks {other}, which the shell cannot read"
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
                format!("the microphone cannot be opened for capture: {err}"),
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
    landed: Receiver<Result<u64, String>>,
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
    /// that stopped answering must not be able to hold this thread forever.
    fn finish(mut self) -> Result<u64, String> {
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
                Err("the microphone did not stop within five seconds".to_string())
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err("the capture thread ended before it reported its frames".to_string())
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
    fn claim(&self, owner: CaptureOwner) -> Result<Option<Active>, String> {
        let mut slot = self.slot.lock();
        match stop_right(&slot, owner) {
            StopRight::Idle => Ok(None),
            StopRight::Mine => Err(same_take_message(owner)),
            StopRight::Stale => Ok(slot.take()),
            StopRight::Foreign => Err(busy_message(owner)),
        }
    }

    /// Puts the reservation of a take into the slot it claimed, or refuses
    /// because another take claimed the microphone in between.
    fn reserve(&self, opening: Opening) -> Result<(), String> {
        let mut slot = self.slot.lock();
        if slot.is_some() {
            return Err(busy_message(opening.owner));
        }
        *slot = Some(Active::Opening(opening));
        Ok(())
    }

    /// Takes the active capture a stop of `owner` may end, or reports why it may
    /// not: the other window's capture is refused, a newer take of the same
    /// window leaves nothing behind, and a take that is still opening its device
    /// is handed over to be closed without waiting for its driver.
    fn take_for_stop(&self, owner: CaptureOwner) -> Result<Option<Active>, String> {
        let mut slot = self.slot.lock();
        match stop_right(&slot, owner) {
            StopRight::Mine => Ok(slot.take()),
            // A newer take of the same window owns the microphone now: this
            // stop has no stream of its own left, and the newer one is not
            // touched.
            StopRight::Stale => Ok(None),
            StopRight::Foreign => Err(busy_message(owner)),
            StopRight::Idle => Ok(None),
        }
    }

    /// Replaces the reservation of this very take with its recording. The same
    /// lock holds both, so no stop can find the slot empty between the two, and
    /// no other take can slip in. The take's own admission is compared here as
    /// well: a shell that closed its capture while the driver was answering
    /// never gets a recording installed behind it.
    fn install(
        &self,
        ticket: &Arc<()>,
        admission: u64,
        owner: CaptureOwner,
        landed: Receiver<Result<u64, String>>,
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
        let Opening { stop, thread, .. } = opening;
        *slot = Some(Active::Recording(Session {
            owner,
            ticket: Arc::clone(ticket),
            stop,
            landed,
            thread,
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

/// The message a start of `owner` gets when the microphone is already capturing
/// that very take.
fn same_take_message(owner: CaptureOwner) -> String {
    match owner {
        CaptureOwner::Dictation(_) => {
            "the microphone is already capturing this dictation".to_string()
        }
        CaptureOwner::Lab(_) => "микрофон уже записывает эту лабораторную запись".to_string(),
    }
}

/// The message a start or a stop of `owner` gets when the microphone is busy
/// with the other window's capture: the laboratory and a dictation never share
/// a take.
fn busy_message(owner: CaptureOwner) -> String {
    match owner {
        CaptureOwner::Dictation(_) => {
            "the microphone is recording for another window of Speechek".to_string()
        }
        CaptureOwner::Lab(_) => {
            "микрофон занят записью другого окна Speechek".to_string()
        }
    }
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
    Fallback(String),
    /// The device answered and refused the take: no other microphone repairs
    /// that.
    Fatal(String),
    /// The attempt never recorded anything because the take is already over.
    /// No device failed, so no failure is reported for it.
    Ended(String),
}

impl From<OpenError> for AttemptError {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::Fallback(message) => Self::Fallback(message),
            OpenError::Fatal(message) => Self::Fatal(message),
        }
    }
}

/// What a failed attempt leaves to do.
#[derive(Debug, PartialEq, Eq)]
enum Retry {
    /// The selected device is gone: the system device is tried once, in its
    /// place. Carries the reason, for the log only.
    System(String),
    /// The take ends before its first frame, with this message for its command.
    End(String),
    /// The take was already over: nothing failed, so nothing is reported.
    Ended(String),
}

/// The fallback policy, decided without a device: the system device replaces
/// the selected one only when the selected one is gone, and only once. A device
/// that answered and refused the take — its format, its access, another stream
/// holding it — ends the take; no other microphone repairs that.
fn retry(failure: AttemptError, already_fell_back: bool) -> Retry {
    match failure {
        AttemptError::Fallback(reason) if !already_fell_back => Retry::System(reason),
        AttemptError::Fallback(message) | AttemptError::Fatal(message) => Retry::End(message),
        AttemptError::Ended(message) => Retry::Ended(message),
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
    ready: SyncSender<Result<CaptureStart, String>>,
    state: HandshakeState,
}

impl Handshake {
    fn new(ready: SyncSender<Result<CaptureStart, String>>) -> Self {
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
    fn fail(&mut self, message: String) -> Result<(), ()> {
        if !matches!(self.state, HandshakeState::Waiting) {
            return Err(());
        }
        self.state = HandshakeState::Abandoned;
        self.ready.send(Err(message)).map_err(|_| ())
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
        if self.handshake.over(self.closed, self.admission, self.control) {
            return Err(AttemptError::Ended(ENDED_WHILE_OPENING.to_string()));
        }
        let input = open(self.selected).map_err(AttemptError::from)?;
        if self.handshake.over(self.closed, self.admission, self.control) {
            // The device answered, but the take is over: it is given back
            // without a stream ever running, so nothing of it is recorded.
            return Err(AttemptError::Ended(ENDED_WHILE_OPENING.to_string()));
        }
        let (samples_tx, samples_rx) = mpsc::channel::<Vec<f32>>();
        let (free_tx, free_rx) = mpsc::channel::<Vec<f32>>();
        let (errors_tx, errors_rx) = mpsc::channel::<StreamFailure>();
        // Two buffers wait before the first callback, so even the very first
        // ones find one instead of allocating on the audio thread.
        for _ in 0..2 {
            let _ = free_tx.send(Vec::with_capacity(input.channels * BUFFER_FRAMES));
        }
        let mut encoder = Encoder::new(self.app.clone(), self.owner, input.rate);
        let stream = build(&input, samples_tx, free_rx, errors_tx).map_err(AttemptError::from)?;
        if let Err(err) = stream.play() {
            let message = format!("the microphone stream did not start: {err}");
            return Err(if err.gone() {
                AttemptError::Fallback(message)
            } else {
                AttemptError::Fatal(message)
            });
        }
        // The stream runs, but the take has not recorded anything yet: the
        // handshake only answers once a frame of it has really been emitted.
        let deadline = Instant::now() + OPEN_TIMEOUT;
        loop {
            if self.handshake.over(self.closed, self.admission, self.control) {
                return Err(AttemptError::Ended(ENDED_WHILE_OPENING.to_string()));
            }
            drain(&samples_rx, &free_tx, &mut encoder);
            if encoder.sequence > 0 {
                break;
            }
            if let Ok(failure) = errors_rx.try_recv() {
                return Err(match failure {
                    StreamFailure::Unavailable(message) => AttemptError::Fallback(message),
                    StreamFailure::Fatal(message) => AttemptError::Fatal(message),
                });
            }
            if Instant::now() >= deadline {
                return Err(AttemptError::Fatal(format!(
                    "the microphone did not deliver a single frame within {} seconds",
                    OPEN_TIMEOUT.as_secs()
                )));
            }
            thread::sleep(IDLE_POLL);
        }
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
            stream,
            samples: samples_rx,
            free: free_tx,
            errors: errors_rx,
        })
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
) -> Result<CaptureStart, String> {
    let state = app.state::<NativeCapture>();
    if !current(&state.closed, admission) {
        // The take is over already: no thread is started for it, so no device
        // is opened and no mute can be engaged behind the shell's back.
        return Err(ended_message(owner));
    }

    // A take of this same window left in the slot by an earlier generation is
    // ended before this reservation is claimed; the other window's capture
    // refuses this start outright.
    if let Some(leftover) = state.claim(owner)? {
        end_leftover(leftover);
    }

    // The pinned device id travels to the thread: resolving it right before the
    // device is opened is what makes a device that disappeared since the
    // settings were written a fallback instead of a dead take.
    let selected = input_device.map(str::to_owned);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<CaptureStart, String>>(1);
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (landed_tx, landed_rx) = mpsc::channel::<Result<u64, String>>();
    let ticket = Arc::new(());
    // The reservation is in the slot before the worker exists: a second start of
    // either owner finds the microphone taken, and a stop can end this take
    // without waiting for a driver.
    state.reserve(Opening {
        owner,
        admission,
        ticket: Arc::clone(&ticket),
        stop: stop_tx,
        thread: None,
    })?;

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
            return Err(format!("cannot start the capture thread: {err}"));
        }
    };
    state.attach_worker(owner, &ticket, worker_thread);

    match ready_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(opened)) => Ok(opened),
        // The worker withdrew its own reservation before answering, so there is
        // nothing left to take back here.
        Ok(Err(message)) => Err(message),
        Err(RecvTimeoutError::Timeout) => {
            // No frame in time. The reservation is taken back before the stop is
            // queued: a worker whose driver answers late finds its slot entry
            // gone and closes the device instead of installing itself, so this
            // command never waits on a driver and the slot never stays claimed.
            // A recording that was installed in the very instant the wait
            // expired is closed here through its own take.
            state.abandon(owner, &ticket);
            Err(format!(
                "the microphone did not start within {} seconds",
                OPEN_TIMEOUT.as_secs()
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            state.abandon(owner, &ticket);
            Err("the capture thread stopped before the device opened".to_string())
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
pub fn stop(app: &AppHandle, owner: CaptureOwner) -> Result<u64, String> {
    let state = app.state::<NativeCapture>();
    match state.take_for_stop(owner)? {
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
fn ended_message(owner: CaptureOwner) -> String {
    match owner {
        CaptureOwner::Dictation(_) => {
            "this dictation ended before its microphone could be opened.".to_string()
        }
        CaptureOwner::Lab(_) => "эта запись завершилась до того, как микрофон был открыт".to_string(),
    }
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
    ready: SyncSender<Result<CaptureStart, String>>,
    control: Receiver<()>,
    landed: Sender<Result<u64, String>>,
    answers: Receiver<Result<u64, String>>,
) {
    let state = app.state::<NativeCapture>();
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
    let mut opened = loop {
        if handshake.over(&state.closed, admission, &control) {
            // The take was given up on before this attempt recorded anything:
            // nobody is waiting for an answer about a capture that must not
            // start, so there is nothing left to report.
            let _ = handshake.fail(ended_message(owner));
            return;
        }
        let attempt = Attempt {
            app: &app,
            owner,
            selected: attempt_selected.as_deref(),
            fallback,
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
                Retry::End(message) => {
                    eprintln!("speechek: {message}.");
                    let _ = handshake.fail(message);
                    return;
                }
                Retry::Ended(message) => {
                    // The take is over, not the device: the command that may
                    // still be waiting learns that nothing started, and no
                    // failure is reported for a take the shell left behind.
                    let _ = handshake.fail(message);
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
        let _ = handshake.fail(ended_message(owner));
        return;
    }
    reservation.installed = true;

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
                failure = Some(reason.message());
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
    if let Some(message) = &failure {
        mic_error(&app, owner, message);
        eprintln!("speechek: {message}.");
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
        Some(message) => Err(message),
        None => Ok(opened.encoder.sequence),
    });
}

/// Tells the window of `owner` that its microphone is gone. The renderer
/// decides what the user reads; this is the reason, never a secret.
fn mic_error(app: &AppHandle, owner: CaptureOwner, message: &str) {
    let error = CaptureNotice {
        generation: owner.generation(),
        message: message.to_string(),
        last: false,
    };
    if let Err(err) = app.emit_to(owner.label(), EVENT_MIC_ERROR, error) {
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
    let warning = CaptureNotice {
        generation,
        message: audio::MUTE_FAILED.to_string(),
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
        CaptureOwner, CaptureStart, Gone, Handshake, NativeCapture, OpenError, Opening,
        Reservation, Retry, Session, StopRight, StreamFailure,
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
    fn new_handshake() -> (Handshake, mpsc::Receiver<Result<CaptureStart, String>>) {
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
        assert_eq!(
            answers.recv_timeout(Duration::from_millis(50)),
            Ok(Ok(start.clone())),
            "the answer carries the device that captured"
        );
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
        assert_eq!(handshake.fail("the microphone is gone".to_string()), Err(()));

        // A failure before the first frame answers the command with its reason.
        let (mut handshake, answers) = new_handshake();
        assert_eq!(
            handshake.fail("the chosen microphone is gone".to_string()),
            Ok(())
        );
        assert_eq!(
            answers.recv_timeout(Duration::from_millis(50)),
            Ok(Err("the chosen microphone is gone".to_string()))
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
        assert_eq!(
            retry(AttemptError::Fallback("gone".to_string()), false),
            Retry::System("gone".to_string()),
            "a selected device that is gone is replaced by the system one"
        );
        assert_eq!(
            retry(AttemptError::Fallback("gone too".to_string()), true),
            Retry::End("gone too".to_string()),
            "the system device is tried exactly once"
        );
        assert_eq!(
            retry(AttemptError::Fatal("another stream holds it".to_string()), false),
            Retry::End("another stream holds it".to_string()),
            "a device that exists and refuses the take is not replaced"
        );
        assert_eq!(
            retry(AttemptError::Fatal("no default microphone".to_string()), true),
            Retry::End("no default microphone".to_string()),
            "a system device that refused the take ends it"
        );
        assert_eq!(
            retry(AttemptError::Ended("the take is over".to_string()), false),
            Retry::Ended("the take is over".to_string()),
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
    fn captured(owner: CaptureOwner, tail: Result<u64, String>) -> Session {
        let (stop, control) = mpsc::channel::<()>();
        let (landed_tx, landed) = mpsc::channel::<Result<u64, String>>();
        let worker = thread::spawn(move || {
            let _ = control.recv();
            let _ = landed_tx.send(tail);
        });
        Session {
            owner,
            ticket: Arc::new(()),
            stop,
            landed,
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
                mpsc::channel::<Result<u64, String>>().1,
            )
            .is_err());
        assert!(state.slot.lock().is_none());

        // A shell that closed its capture while the driver was answering never
        // gets a recording installed: the admission is compared under the same
        // lock as the installation itself.
        let (ticket, _control) = reserved(&state, CaptureOwner::Lab(4), 0);
        state.closed.fetch_add(1, Ordering::SeqCst);
        let (_landed_tx, landed) = mpsc::channel::<Result<u64, String>>();
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
        let (_landed_tx, landed) = mpsc::channel::<Result<u64, String>>();
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
        let (landed_tx, landed) = mpsc::channel::<Result<u64, String>>();
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
        let (landed_tx, landed) = mpsc::channel::<Result<u64, String>>();
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
        let (landed_tx, landed) = mpsc::channel::<Result<u64, String>>();
        drop(landed_tx);
        let session = Session {
            owner: CaptureOwner::Dictation(1),
            ticket: Arc::new(()),
            stop,
            landed,
            thread: None,
        };
        assert!(
            session.finish().is_err(),
            "a worker that is gone reports nothing whole"
        );

        assert_eq!(
            captured(CaptureOwner::Dictation(1), Err("the microphone is gone".to_string()))
                .finish(),
            Err("the microphone is gone".to_string()),
            "a stream that failed keeps its reason"
        );
        assert_eq!(
            captured(CaptureOwner::Dictation(1), Ok(12)).finish(),
            Ok(12),
            "a capture that ended normally reports its last frame"
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

}
