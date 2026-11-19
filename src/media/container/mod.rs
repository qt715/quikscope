//! Reading and writing MP4/MOV (ISO BMFF, via `mp4-atom`) and Matroska
//! (`mkv-element` for writing, `matroska-demuxer` for reading) without FFmpeg.
//!
//! Conventions at this API, for both directions:
//! - H.264 packet data is **Annex-B**. Muxers convert to length-prefixed
//!   samples and build `avcC` from the SPS/PPS of the first keyframe; demuxers
//!   convert back and insert the `avcC` SPS/PPS before every keyframe, so a
//!   decoder can start at any keyframe.
//! - Timestamps are relative to the file's presentation start, shared by all
//!   tracks (edit lists and per-track start offsets applied), so an audio
//!   track that starts two seconds after the picture still does.
//! - Opus packet times count the encoder's pre-skip as part of the stream
//!   (see [`crate::media::codec::opus`]): muxers store the pre-skip (MP4 edit
//!   list and `dOps`, Matroska `CodecDelay`) without shifting packets, and
//!   demuxers do not shift Opus packets by it either; the decoder trims it.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

pub mod mkv;
pub mod mp4;
mod nal;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Opus,
    /// Read-only: AAC-LC in third-party files.
    Aac,
    /// Read-only here; animated AVIF writes its `av01` track through
    /// [`mp4`]'s box helpers.
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackKind {
    Video { width: u32, height: u32, fps: f64 },
    Audio { rate: u32, channels: u16 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub codec: Codec,
    pub kind: TrackKind,
    /// Demuxed: what the file stores (`avcC`, OpusHead, AudioSpecificConfig,
    /// `av1C`). Muxing: OpusHead for Opus, empty for H.264.
    pub codec_private: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    /// Index into the track list.
    pub track: usize,
    pub data: Vec<u8>,
    pub pts: Duration,
    /// Required for audio when muxing; demuxers report it when known.
    pub duration: Option<Duration>,
    pub keyframe: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// `fragmented`: init segment, then a `moof`/`mdat` fragment per video
    /// keyframe (about once a second), handed to the OS as written and
    /// synced to disk every few seconds, so a crash loses at most the last
    /// fragment and a power loss the last seconds. Otherwise the file is
    /// written as fragments and rewritten into a classic MP4 with `moov`
    /// first on [`Muxer::finish`].
    Mp4 { fragmented: bool },
    /// QuickTime brand; always finished as a classic file like
    /// `Mp4 { fragmented: false }`.
    Mov,
    /// Clusters per keyframe with unknown-size segment while writing (synced
    /// to disk like fragments); cues and sizes are written on
    /// [`Muxer::finish`]. A crashed file still plays.
    Mkv,
}

pub trait Muxer: Send {
    /// Packets of each track in increasing pts order; tracks may interleave
    /// loosely (the muxer buffers up to a fragment).
    fn write(&mut self, packet: Packet) -> Result<()>;
    /// Completes the file. A muxer dropped without `finish` leaves whatever
    /// it flushed so far (for fragmented MP4 and MKV, a playable prefix).
    fn finish(self: Box<Self>) -> Result<()>;
}

/// Creates `path` (normally an [`crate::output::OutputFile::part`]).
pub fn create_muxer(path: &Path, container: Container, tracks: &[Track]) -> Result<Box<dyn Muxer>> {
    match container {
        Container::Mp4 { .. } | Container::Mov => {
            mp4::Mp4Muxer::create(path, container, tracks).map(|m| Box::new(m) as Box<dyn Muxer>)
        }
        Container::Mkv => {
            mkv::MkvMuxer::create(path, tracks).map(|m| Box::new(m) as Box<dyn Muxer>)
        }
    }
}

pub trait Demuxer: Send {
    fn tracks(&self) -> &[Track];
    /// Presentation length (longest track).
    fn duration(&self) -> Duration;
    /// Next packet of any track, in file order.
    fn next_packet(&mut self) -> Result<Option<Packet>>;
    /// Repositions so that, per video track, the next packet is the last
    /// keyframe at or before `to`, and per audio track the packet playing at
    /// `to` (or the next one: a track that has ended before `to` resumes
    /// with nothing). Callers decode forward to `to`.
    fn seek(&mut self, to: Duration) -> Result<()>;
}

/// Opens an MP4/MOV/M4V or Matroska/WebM file by content (not extension).
/// `Ok(None)` when the file is neither.
pub fn open_demuxer(path: &Path) -> Result<Option<Box<dyn Demuxer>>> {
    let mut head = [0u8; 8];
    let mut file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..])? {
            0 => return Ok(None),
            n => filled += n,
        }
    }
    drop(file);
    Ok(match sniff(&head) {
        Some(Format::Mp4) => Some(Box::new(mp4::Mp4Demuxer::open(path)?)),
        Some(Format::Matroska) => Some(Box::new(mkv::MkvDemuxer::open(path)?)),
        None => None,
    })
}

#[derive(Debug, PartialEq)]
enum Format {
    Mp4,
    Matroska,
}

/// Recognises the first eight bytes of a file: the EBML magic, or a first
/// ISO BMFF box that files start with (QuickTime files may open with
/// `wide`/`free`/`mdat` instead of `ftyp`).
fn sniff(head: &[u8; 8]) -> Option<Format> {
    if head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(Format::Matroska);
    }
    let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let kind = &head[4..8];
    // Size 1 announces a 64-bit size; 0 runs to the end of the file.
    let plausible = size == 0 || size == 1 || size >= 8;
    let known = [
        b"ftyp", b"moov", b"wide", b"free", b"skip", b"mdat", b"styp",
    ];
    (plausible && known.iter().any(|k| kind == *k)).then_some(Format::Mp4)
}

/// `d` in units of `1 / timescale`, rounded to nearest.
fn to_ticks(d: Duration, timescale: u32) -> u64 {
    let ticks = (d.as_nanos() * u128::from(timescale) + 500_000_000) / 1_000_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

/// Signed `ticks / timescale` in nanoseconds.
fn ticks_to_nanos(ticks: i128, timescale: u32) -> i128 {
    ticks * 1_000_000_000 / i128::from(timescale.max(1))
}

/// A non-negative nanosecond count as a [`Duration`] (negative clamps to 0).
fn nanos(ns: i128) -> Duration {
    let ns = u128::try_from(ns).unwrap_or(0);
    Duration::new(
        u64::try_from(ns / 1_000_000_000).unwrap_or(u64::MAX),
        (ns % 1_000_000_000) as u32,
    )
}

/// Nominal frame rate from frame intervals in units of
/// `1 / units_per_second`: the mean of the intervals below 1.5 times the
/// shortest. Recordings skip pictures while the screen is static, so the
/// overall average understates the rate they were made at, while whole-tick
/// rounding (33 and 34 ms at 30 fps) still averages out. 0 without any
/// interval.
fn nominal_fps(intervals: impl Iterator<Item = u64> + Clone, units_per_second: f64) -> f64 {
    let Some(shortest) = intervals.clone().filter(|&i| i > 0).min() else {
        return 0.0;
    };
    let limit = u128::from(shortest) * 3;
    let (sum, count) = intervals
        .filter(|&i| i > 0 && u128::from(i) * 2 < limit)
        .fold((0u128, 0u64), |(sum, count), i| {
            (sum + u128::from(i), count + 1)
        });
    units_per_second * count as f64 / sum as f64
}
