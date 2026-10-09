//! The streaming FLAC codec of the development diagnostics: the lossless
//! archive of the take's PCM16 16 kHz mono stream, and its independent check.
//!
//! Everything here is memory-only and disk-free: the encoder stages one fixed
//! block at a time and hands each encoded frame to a [`FrameCommit`], which is
//! where the storage half admits it against the quota and writes it. Nothing
//! accumulates a whole recording, and no `Vec<i32>` of the take ever exists.
//!
//! The layout is the one the archive's own reader expects:
//!
//! * `fLaC` + one STREAMINFO block, 42 bytes, written before the first frame
//!   with zero frame sizes, zero sample count and a zero digest (a header an
//!   interrupted run can still be parsed through), and patched in place when
//!   the take's tail has been committed,
//! * `16_000` Hz, one channel, 16 bits per sample, 2048 samples per block,
//! * the last block shorter than 2048 samples, exactly as long as the PCM really
//!   is — never padded, trimmed, normalized or resampled.
//!
//! The check is [`verify`]: claxon decodes the committed file and it compares
//! the stream information, the block layout, every frame's own fields, the
//! exact sample count and the digest of the samples claxon decoded with what
//! the encoder committed. The decoder is independent of the encoder — a
//! mangled sample, a shifted block boundary or a stale header fails the check —
//! while the digest itself is flacenc's [`Context`] on both sides, so there is
//! one MD5 in the archive and no second, hand-written one to trust.
//!
//! Sources for every flacenc 0.5.1 and claxon 0.4.3 call below:
//!
//! * <https://docs.rs/flacenc/0.5.1/flacenc/coding/fn.encode_fixed_size_frame.html>
//!   — `encode_fixed_size_frame(&Verified<Encoder>, &FrameBuf, usize, &StreamInfo)`;
//!   the block size of the frame is `FrameBuf::filled_size()`, so the short
//!   tail is encoded as a short block rather than padded.
//! * <https://docs.rs/flacenc/0.5.1/flacenc/source/trait.Fill.html> —
//!   `fill_le_bytes(bytes, bytes_per_sample)` accepts a short input and sets
//!   `filled_size`; <https://docs.rs/flacenc/0.5.1/flacenc/source/struct.Context.html>
//!   — `Context::fill_le_bytes` is the MD5 and sample count of the committed
//!   PCM, and `md5_digest()` is what the STREAMINFO field must carry.
//! * <https://docs.rs/flacenc/0.5.1/flacenc/component/struct.StreamInfo.html> —
//!   `update_frame_info`, `set_block_sizes`, `set_total_samples`,
//!   `set_md5_digest`;
//!   <https://docs.rs/flacenc/0.5.1/src/flacenc/component/datatype.rs.html#516-525>
//!   — `update_frame_info` raises the block/frame bounds and the sample count
//!   itself, which is why the digest and the final count are set last.
//! * <https://docs.rs/flacenc/0.5.1/flacenc/bitsink/type.ByteSink.html> —
//!   the reusable byte buffer;
//!   <https://docs.rs/flacenc/0.5.1/flacenc/component/trait.BitRepr.html> —
//!   `Frame::write`/`Stream::write`.
//! * <https://docs.rs/claxon/0.4.3/claxon/struct.FlacReader.html> —
//!   `streaminfo()`, `blocks()`;
//!   <https://docs.rs/claxon/0.4.3/claxon/frame/struct.Block.html> —
//!   `duration()`, `channels()`, `time()`, `channel()`, `into_buffer()`, so one
//!   buffer decodes every block;
//!   <https://docs.rs/claxon/0.4.3/claxon/metadata/struct.StreamInfo.html> —
//!   the fields the check compares.
//! * <https://www.rfc-editor.org/rfc/rfc9639#section-8.2> — STREAMINFO and the
//!   last block, which may be shorter than the minimum block size; a block
//!   shorter than 16 samples is legal in the final position.
//!
//! The digest: the header carries the digest the encoder's `Context` computed
//! over the committed PCM bytes, and [`verify`] recomputes it with a second
//! `Context` over the independently decoded samples — claxon's decoder and a
//! sample-for-sample comparison, never the header's own claim. `verify_pcm`
//! goes one step further and compares every decoded sample with the PCM the
//! caller says was committed, which is what the tests exercise.

use std::io::Read;

use claxon::FlacReader;
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use super::DiagnosticCode;

/// Sampling rate of the archived stream, in Hz.
pub(crate) const SAMPLE_RATE: usize = 16_000;
/// Channels of the archived stream.
pub(crate) const CHANNELS: usize = 1;
/// Bits per sample of the archived stream.
pub(crate) const BITS_PER_SAMPLE: usize = 16;
/// Bytes one sample takes in the little-endian PCM the capture produces.
pub(crate) const BYTES_PER_SAMPLE: usize = BITS_PER_SAMPLE / 8;
/// Inter-channel samples one FLAC block carries.
pub(crate) const BLOCK_SAMPLES: usize = 2_048;
/// Bytes of one full block of PCM.
pub(crate) const BLOCK_BYTES: usize = BLOCK_SAMPLES * BYTES_PER_SAMPLE;

const ZERO_MD5: [u8; 16] = [0; 16];

/// Where one encoded frame goes. The storage half implements this with the
/// exact quota admission and the file write; the tests with a byte vector. The
/// encoder calls it *before* it counts the frame: a frame whose commit fails is
/// never part of the take's own statistics, so what the take reports is exactly
/// what the file holds.
pub(crate) trait FrameCommit {
    fn write_frame(&mut self, encoded: &[u8]) -> Result<(), DiagnosticCode>;
}

/// What a finished encoder produced: the same-size header that has to replace
/// the one on disk, and the two values the independent check compares.
pub(crate) struct Finished {
    pub(crate) header: Vec<u8>,
    pub(crate) samples: u64,
    pub(crate) md5: [u8; 16],
}

/// The streaming encoder of one take: a fixed block of PCM, one frame buffer,
/// one context and one reusable byte sink. No whole recording, no frame list.
pub(crate) struct FlacEncoder {
    config: Verified<config::Encoder>,
    frame_buf: FrameBuf,
    /// The bytes of the block being filled, at most [`BLOCK_BYTES`].
    staged: Vec<u8>,
    /// How many of them are real (the last block of a take may be short).
    staged_filled: usize,
    context: Context,
    stream_info: StreamInfo,
    sink: ByteSink,
    frames: u64,
    samples: u64,
    /// The header the file was opened with, so the patched one can be checked
    /// to be exactly as long.
    header: Vec<u8>,
}

impl FlacEncoder {
    /// A fresh encoder for 16 kHz mono PCM16, 2048 samples per block. The
    /// working stream information starts at the stream's format and the block
    /// bounds, with no frame committed yet.
    pub(crate) fn new() -> Result<Self, DiagnosticCode> {
        let config = config::Encoder::default()
            .into_verified()
            .map_err(|_| DiagnosticCode::Codec)?;
        let frame_buf = FrameBuf::with_size(CHANNELS, BLOCK_SAMPLES)
            .map_err(|_| DiagnosticCode::Codec)?;
        let mut stream_info = StreamInfo::new(SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE)
            .map_err(|_| DiagnosticCode::Codec)?;
        stream_info
            .set_block_sizes(BLOCK_SAMPLES, BLOCK_SAMPLES)
            .map_err(|_| DiagnosticCode::Codec)?;
        // The working stats keep the constructor's frame-size sentinel: the
        // minimum starts at `u32::MAX` and every `update_frame_info` lowers it,
        // so the patched header carries the smallest and largest frame really
        // written. A zero minimum here would stick for the whole take — every
        // later `min(0, size)` is zero — which is why the sentinel is removed
        // from the header's copy below instead of from the working stats.
        let mut encoder = Self {
            config,
            frame_buf,
            staged: vec![0u8; BLOCK_BYTES],
            staged_filled: 0,
            context: Context::new(BITS_PER_SAMPLE, CHANNELS),
            stream_info,
            sink: ByteSink::new(),
            frames: 0,
            samples: 0,
            header: Vec::new(),
        };
        encoder.header = serialize_header(&initial_header_info(&encoder.stream_info)?)?;
        Ok(encoder)
    }

    /// The header the file is opened with: the stream's format, zero frame
    /// sizes, zero samples and a zero digest. An interrupted run leaves a file
    /// a reader can still parse, and it is never a claim about audio.
    pub(crate) fn initial_header(&self) -> &[u8] {
        &self.header
    }

    /// How many inter-channel samples have really been committed.
    pub(crate) fn samples(&self) -> u64 {
        self.samples
    }

    /// How many frames have really been committed.
    pub(crate) fn frames(&self) -> u64 {
        self.frames
    }

    /// Stages one piece of little-endian PCM16 and encodes every block it
    /// completes. The piece must be whole samples: a trailing odd byte would
    /// silently change every following sample, so it is refused instead.
    pub(crate) fn push_pcm<C: FrameCommit>(
        &mut self,
        pcm: &[u8],
        commit: &mut C,
    ) -> Result<(), DiagnosticCode> {
        if pcm.len() % BYTES_PER_SAMPLE != 0 {
            return Err(DiagnosticCode::Codec);
        }
        let mut rest = pcm;
        while !rest.is_empty() {
            let room = BLOCK_BYTES - self.staged_filled;
            let take = room.min(rest.len());
            self.staged[self.staged_filled..self.staged_filled + take]
                .copy_from_slice(&rest[..take]);
            self.staged_filled += take;
            rest = &rest[take..];
            if self.staged_filled == BLOCK_BYTES {
                self.encode_block(commit)?;
            }
        }
        Ok(())
    }

    /// Commits the short tail, and returns the header that has to replace the
    /// one on disk. The block bounds go back to the stream's block size first,
    /// then the sample count, and the digest last: `update_frame_info` keeps
    /// moving the bounds and the count itself.
    pub(crate) fn finish<C: FrameCommit>(
        &mut self,
        commit: &mut C,
    ) -> Result<Finished, DiagnosticCode> {
        self.encode_block(commit)?;
        let mut info = self.stream_info.clone();
        info.set_block_sizes(BLOCK_SAMPLES, BLOCK_SAMPLES)
            .map_err(|_| DiagnosticCode::Codec)?;
        info.set_total_samples(self.samples as usize);
        info.set_md5_digest(&self.context.md5_digest());
        let header = serialize_header(&info)?;
        if header.len() != self.header.len() {
            // The patch is an in-place overwrite: a header that changed size
            // would leave the frame it already wrote unreadable.
            return Err(DiagnosticCode::Codec);
        }
        Ok(Finished {
            header,
            samples: self.samples,
            md5: self.context.md5_digest(),
        })
    }

    /// One block: encode it, hand it to the commit, and only count it once the
    /// commit returned. The frame number is the compression-block index the
    /// fixed blocking strategy requires — never the capture's own sequence.
    fn encode_block<C: FrameCommit>(&mut self, commit: &mut C) -> Result<(), DiagnosticCode> {
        let filled = self.staged_filled;
        if filled == 0 {
            // An empty block cannot be encoded: the block-size specification
            // has no bit pattern for zero samples.
            return Ok(());
        }
        self.frame_buf
            .fill_le_bytes(&self.staged[..filled], BYTES_PER_SAMPLE)
            .map_err(|_| DiagnosticCode::Codec)?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.frame_buf,
            self.frames as usize,
            &self.stream_info,
        )
        .map_err(|_| DiagnosticCode::Codec)?;
        self.sink.clear();
        frame
            .write(&mut self.sink)
            .map_err(|_| DiagnosticCode::Codec)?;
        if self.sink.len() % 8 != 0 {
            return Err(DiagnosticCode::Codec);
        }
        commit.write_frame(self.sink.as_slice())?;
        // The frame is on its way to the file: now it is part of the stream.
        self.stream_info.update_frame_info(&frame);
        self.context
            .fill_le_bytes(&self.staged[..filled], BYTES_PER_SAMPLE)
            .map_err(|_| DiagnosticCode::Codec)?;
        self.frames += 1;
        self.samples += (filled / (BYTES_PER_SAMPLE * CHANNELS)) as u64;
        self.staged_filled = 0;
        Ok(())
    }
}

/// Serializes `fLaC` and one last STREAMINFO block. 42 bytes, always: the
/// patched header is written over the previous one in place.
fn serialize_header(info: &StreamInfo) -> Result<Vec<u8>, DiagnosticCode> {
    let stream = Stream::with_stream_info(info.clone());
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|_| DiagnosticCode::Codec)?;
    Ok(sink.as_slice().to_vec())
}

/// The stats the file is opened with: the format and the block bounds, zero
/// frame sizes, zero samples and a zero digest. The working `StreamInfo` keeps
/// its own sentinels, so this is always a copy — a header a reader may parse,
/// and never a claim about audio.
fn initial_header_info(working: &StreamInfo) -> Result<StreamInfo, DiagnosticCode> {
    let mut info = working.clone();
    info.set_frame_sizes(0, 0)
        .map_err(|_| DiagnosticCode::Codec)?;
    info.set_block_sizes(BLOCK_SAMPLES, BLOCK_SAMPLES)
        .map_err(|_| DiagnosticCode::Codec)?;
    info.set_total_samples(0);
    info.set_md5_digest(&ZERO_MD5);
    Ok(info)
}

/// Decodes the committed stream and checks it against what the encoder
/// committed: the format, the block layout, every frame's own fields, the exact
/// number of samples and the digest of the decoded samples. The header's digest
/// and count are compared too, so a stale header fails instead of passing
/// because the frames happen to agree with it.
///
/// There is exactly one MD5 in the archive and it is flacenc's [`Context`]:
/// the encoder hashes the PCM it commits and this check hashes the samples
/// claxon decoded. What is independent of the encoder is the decoder and the
/// layout check, not a second digest written by hand.
pub(crate) fn verify<R: Read>(
    reader: R,
    samples: u64,
    md5: &[u8; 16],
) -> Result<(), DiagnosticCode> {
    walk(reader, samples, *md5, |_, _| Ok(()))
}

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
/// The same check with the strongest comparison the archive can make: the
/// digest is recomputed over the PCM the caller says was committed, and every
/// decoded sample has to equal that PCM in order. The tests use this; the
/// writer cannot, because it deliberately never keeps a whole take's PCM.
pub(crate) fn verify_pcm<R: Read>(
    reader: R,
    samples: u64,
    pcm_le: &[u8],
) -> Result<(), DiagnosticCode> {
    if pcm_le.len() % BYTES_PER_SAMPLE != 0 || pcm_le.len() / BYTES_PER_SAMPLE != samples as usize
    {
        return Err(DiagnosticCode::Decode);
    }
    let mut context = Context::new(BITS_PER_SAMPLE, CHANNELS);
    context
        .fill_le_bytes(pcm_le, BYTES_PER_SAMPLE)
        .map_err(|_| DiagnosticCode::Codec)?;
    let md5 = context.md5_digest();
    let mut checked = 0usize;
    walk(reader, samples, md5, |_, value| {
        // A file that carries more samples than the caller says were committed
        // is refused, never indexed past the end of the PCM.
        let at = checked * BYTES_PER_SAMPLE;
        checked += 1;
        let Some(pair) = pcm_le.get(at..at + BYTES_PER_SAMPLE) else {
            return Err(DiagnosticCode::Decode);
        };
        let expected = i16::from_le_bytes([pair[0], pair[1]]) as i32;
        if expected == value {
            Ok(())
        } else {
            Err(DiagnosticCode::Decode)
        }
    })
}

/// One decode of the whole stream with the caller's own comparison of every
/// sample, in order. `md5` is the digest the decoded samples have to hash to.
/// The layout is checked as it is read: one channel, blocks of at most
/// [`BLOCK_SAMPLES`], no short block except the last, and a full block's frame
/// number equal to its own index.
fn walk<R: Read>(
    reader: R,
    samples: u64,
    md5: [u8; 16],
    mut want: impl FnMut(usize, i32) -> Result<(), DiagnosticCode>,
) -> Result<(), DiagnosticCode> {
    let mut reader = FlacReader::new(reader).map_err(|_| DiagnosticCode::Decode)?;
    let info = reader.streaminfo();
    if info.sample_rate != SAMPLE_RATE as u32
        || info.channels != CHANNELS as u32
        || info.bits_per_sample != BITS_PER_SAMPLE as u32
        || info.min_block_size != BLOCK_SAMPLES as u16
        || info.max_block_size != BLOCK_SAMPLES as u16
    {
        return Err(DiagnosticCode::Decode);
    }
    // claxon's own metadata check only requires `min <= max`, and it models both
    // frame sizes as optional. A missing one is the zero sentinel of the initial
    // header; a present one must be a real byte count, so `0` is refused too and
    // the pair must be ordered.
    match (info.min_frame_size, info.max_frame_size) {
        (Some(min), Some(max)) if min > 0 && max > 0 && min <= max => {}
        _ => return Err(DiagnosticCode::Decode),
    }
    if info.samples != Some(samples) {
        return Err(DiagnosticCode::Decode);
    }
    if info.md5sum != md5 {
        return Err(DiagnosticCode::HashMismatch);
    }
    let full_blocks = samples / BLOCK_SAMPLES as u64;
    let tail = samples % BLOCK_SAMPLES as u64;
    let expected_frames = full_blocks + u64::from(tail > 0);

    let mut blocks = reader.blocks();
    let mut buffer: Vec<i32> = Vec::new();
    let mut decoded: u64 = 0;
    let mut frames: u64 = 0;
    let mut previous_short = false;
    let mut context = Context::new(BITS_PER_SAMPLE, CHANNELS);
    loop {
        let Some(block) = blocks
            .read_next_or_eof(buffer)
            .map_err(|_| DiagnosticCode::Decode)?
        else {
            break;
        };
        let duration = block.duration() as u64;
        if block.channels() != CHANNELS as u32
            || duration == 0
            || duration > BLOCK_SAMPLES as u64
            || previous_short
        {
            return Err(DiagnosticCode::Decode);
        }
        // claxon derives a fixed-block frame's time as `block_size *
        // frame_number` — the frame's OWN block size, per its
        // `BlockTime::FrameNumber` arm — so a short last block yields
        // `tail_size * index`, never `2048 * index`. Dividing by the duration
        // therefore recovers the frame number exactly, and it has to be this
        // frame's own position: a frame numbered past its place fails, the
        // short tail included.
        if block.time() != frames * duration {
            return Err(DiagnosticCode::Decode);
        }
        // The layout is exactly the fixed blocking the archive claims: full
        // blocks, then at most one short final block of the real tail.
        let expected = if duration == BLOCK_SAMPLES as u64 {
            duration
        } else if decoded + duration == samples {
            tail
        } else {
            return Err(DiagnosticCode::Decode);
        };
        if duration != expected {
            return Err(DiagnosticCode::Decode);
        }
        let values = block.channel(0);
        for (offset, &value) in values.iter().enumerate() {
            if !(-32_768..=32_767).contains(&value) {
                return Err(DiagnosticCode::Decode);
            }
            want((decoded + offset as u64) as usize, value)?;
        }
        context
            .fill_interleaved(values)
            .map_err(|_| DiagnosticCode::Codec)?;
        decoded += duration;
        frames += 1;
        previous_short = duration != BLOCK_SAMPLES as u64;
        buffer = block.into_buffer();
    }
    if frames == 0 || frames != expected_frames || decoded != samples {
        return Err(DiagnosticCode::Decode);
    }
    if context.md5_digest() != md5 {
        return Err(DiagnosticCode::HashMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The archive's own file, in memory: the initial header, the frames in
    /// order, and the patched header written over the first bytes of it.
    struct Bytes {
        bytes: Vec<u8>,
        frames: usize,
    }

    impl Bytes {
        fn new(header: &[u8]) -> Self {
            Self {
                bytes: header.to_vec(),
                frames: 0,
            }
        }

        fn patch(&mut self, header: &[u8]) {
            self.bytes[..header.len()].copy_from_slice(header);
        }
    }

    impl FrameCommit for Bytes {
        fn write_frame(&mut self, encoded: &[u8]) -> Result<(), DiagnosticCode> {
            self.frames += 1;
            self.bytes.extend_from_slice(encoded);
            Ok(())
        }
    }

    /// Deterministic PCM16: zero, both extremes, a slow saw and an alternating
    /// pair, so no codec path rests on one lone shape. Every sample is exact.
    fn pcm(samples: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples * BYTES_PER_SAMPLE);
        for index in 0..samples {
            let value: i16 = match index % 8 {
                0 => 0,
                1 => i16::MIN,
                2 => i16::MAX,
                3 => 1,
                4 => -1,
                5 => 16_000,
                6 => ((index % 1_000) as i32 - 500) as i16,
                _ => (((index * 37) % 32_768) as i32 - 16_384) as i16,
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// Encodes a whole take the way the writer does: one header, the frames,
    /// then the same-size patch.
    fn encode(pcm: &[u8]) -> (Bytes, Finished) {
        let mut encoder = FlacEncoder::new().expect("a fresh encoder");
        let header = encoder.initial_header().to_vec();
        let mut file = Bytes::new(&header);
        encoder.push_pcm(pcm, &mut file).expect("the whole take");
        let finished = encoder.finish(&mut file).expect("a final header");
        assert_eq!(
            finished.header.len(),
            header.len(),
            "the patch is exactly as long as the header it replaces"
        );
        file.patch(&finished.header);
        assert_eq!(file.frames as u64, encoder.frames());
        (file, finished)
    }

    /// The samples one decode of `bytes` yields, in order.
    fn decoded(bytes: &[u8]) -> Vec<i32> {
        let mut reader = FlacReader::new(bytes).expect("a readable stream");
        let mut blocks = reader.blocks();
        let mut buffer: Vec<i32> = Vec::new();
        let mut samples = Vec::new();
        while let Some(block) = blocks
            .read_next_or_eof(buffer)
            .expect("a decodable block")
        {
            samples.extend_from_slice(block.channel(0));
            buffer = block.into_buffer();
        }
        samples
    }

    #[test]
    fn every_take_length_round_trips_sample_for_sample() {
        for samples in [
            1usize,
            15,
            511,
            512,
            2_047,
            2_048,
            2_049,
            2_048 * 3,
            7_500 * 512 + 37,
        ] {
            let waveform = pcm(samples);
            let (file, finished) = encode(&waveform);
            assert_eq!(finished.samples, samples as u64, "{samples} samples");
            assert!(file.bytes.len() > 42, "a header and its frames");

            // The strongest check the archive can make: the digest recomputes
            // over the committed PCM and every decoded sample is compared with
            // it, in order.
            verify_pcm(&file.bytes[..], finished.samples, &waveform)
                .unwrap_or_else(|code| panic!("{samples} samples: {code:?}"));

            // And the same walk again with the test's own decoder: every
            // sample, in order, against the PCM that was handed over.
            let expected: Vec<i32> = waveform
                .chunks_exact(BYTES_PER_SAMPLE)
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]]) as i32)
                .collect();
            assert_eq!(
                decoded(&file.bytes),
                expected,
                "{samples} samples, sample by sample"
            );

            // The header itself: the format, the block bounds and the digest.
            let info = FlacReader::new(&file.bytes[..])
                .expect("a readable stream")
                .streaminfo();
            assert_eq!(info.sample_rate, SAMPLE_RATE as u32);
            assert_eq!(info.channels, CHANNELS as u32);
            assert_eq!(info.bits_per_sample, BITS_PER_SAMPLE as u32);
            assert_eq!(info.min_block_size, BLOCK_SAMPLES as u16);
            assert_eq!(info.max_block_size, BLOCK_SAMPLES as u16);
            assert_eq!(info.samples, Some(samples as u64));
            assert_eq!(info.md5sum, finished.md5);
            assert_ne!(info.md5sum, ZERO_MD5, "the digest is a real one");

            // The layout: full blocks of 2048 and one short tail, never a
            // padded block and never a doubled count.
            let mut reader = FlacReader::new(&file.bytes[..]).expect("a readable stream");
            let mut blocks = reader.blocks();
            let mut buffer: Vec<i32> = Vec::new();
            let mut durations: Vec<u32> = Vec::new();
            while let Some(block) = blocks
                .read_next_or_eof(buffer)
                .expect("a decodable block")
            {
                durations.push(block.duration());
                buffer = block.into_buffer();
            }
            let full = samples / BLOCK_SAMPLES;
            let tail = samples % BLOCK_SAMPLES;
            assert_eq!(durations.len(), full + usize::from(tail > 0));
            assert!(durations[..full].iter().all(|d| *d as usize == BLOCK_SAMPLES));
            if tail > 0 {
                assert_eq!(*durations.last().unwrap() as usize, tail, "the real tail");
            }
            assert_eq!(
                durations.iter().map(|d| *d as usize).sum::<usize>(),
                samples,
                "no sample is padded or repeated"
            );
        }
    }

    #[test]
    fn the_same_take_in_pieces_writes_the_same_file() {
        let waveform = pcm(4_097);
        let (whole, _) = encode(&waveform);
        // Pieces that never line up with a block: the encoder stages one block
        // at a time, so where a piece ends must not matter at all.
        for pieces in [1usize, 3, 4, 5, 8, 64] {
            let mut encoder = FlacEncoder::new().expect("a fresh encoder");
            let header = encoder.initial_header().to_vec();
            let mut file = Bytes::new(&header);
            for chunk in waveform.chunks(pieces * BYTES_PER_SAMPLE) {
                encoder.push_pcm(chunk, &mut file).expect("a piece");
            }
            let finished = encoder.finish(&mut file).expect("a final header");
            file.patch(&finished.header);
            assert_eq!(file.bytes, whole.bytes, "{pieces} samples per push");
            assert_eq!(file.frames as u64, encoder.frames());
            assert_eq!(finished.samples, 4_097);
            verify_pcm(&file.bytes[..], finished.samples, &waveform).expect("still exact");
        }
    }

    #[test]
    fn the_check_refuses_a_mangled_file_or_a_wrong_claim() {
        let waveform = pcm(5_000);
        let (file, finished) = encode(&waveform);
        verify_pcm(&file.bytes[..], finished.samples, &waveform).expect("the good take");

        // A claim about more samples than the file holds.
        assert!(verify_pcm(&file.bytes[..], finished.samples + 1, &waveform).is_err());

        // A PCM that is not the one the digest covers: the header's own claim
        // and the recomputed one disagree.
        let mut wrong = waveform.clone();
        wrong[100] ^= 0x01;
        assert_eq!(
            verify_pcm(&file.bytes[..], finished.samples, &wrong),
            Err(DiagnosticCode::HashMismatch)
        );

        // A byte the disk changed in the middle of a frame.
        let mut mangled = file.bytes.clone();
        let at = mangled.len() / 2;
        mangled[at] ^= 0xff;
        assert!(verify_pcm(&mangled[..], finished.samples, &waveform).is_err());

        // A byte the disk changed in the header's own digest.
        let mut header = file.bytes.clone();
        header[30] ^= 0xff;
        assert_eq!(
            verify_pcm(&header[..], finished.samples, &waveform),
            Err(DiagnosticCode::HashMismatch)
        );
    }

    #[test]
    fn a_piece_of_half_a_sample_is_refused() {
        let mut encoder = FlacEncoder::new().expect("a fresh encoder");
        let header = encoder.initial_header().to_vec();
        let mut file = Bytes::new(&header);
        let refused = encoder.push_pcm(&[0u8; 3], &mut file);
        assert_eq!(refused, Err(DiagnosticCode::Codec));
        assert_eq!(file.frames, 0, "nothing reached the file");
        assert_eq!(file.bytes.len(), header.len());
        assert_eq!(encoder.samples(), 0);
    }

    #[test]
    fn the_header_of_an_interrupted_run_is_never_a_verified_archive() {
        let encoder = FlacEncoder::new().expect("a fresh encoder");
        let header = encoder.initial_header().to_vec();

        // What the file is opened with: the format, the block bounds, no frame
        // sizes, no samples and a zero digest.
        let info = FlacReader::new(&header[..])
            .expect("a readable header")
            .streaminfo();
        assert_eq!(info.sample_rate, SAMPLE_RATE as u32);
        assert_eq!(info.channels, CHANNELS as u32);
        assert_eq!(info.bits_per_sample, BITS_PER_SAMPLE as u32);
        assert_eq!(info.min_block_size, BLOCK_SAMPLES as u16);
        assert_eq!(info.max_block_size, BLOCK_SAMPLES as u16);
        assert_eq!(info.md5sum, ZERO_MD5);
        assert!(matches!(info.samples, None | Some(0)));
        assert_eq!(encoder.samples(), 0);
        assert_eq!(encoder.frames(), 0);

        // And the check refuses it however it is claimed: the sentinel header
        // is never published as audio.
        assert!(verify_pcm(&header[..], 0, &[]).is_err());
        assert!(verify(&header[..], 0, &ZERO_MD5).is_err());
    }
}
