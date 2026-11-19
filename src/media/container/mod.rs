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

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

pub mod mkv;
pub mod mp4;

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
    /// keyframe (about once a second), flushed to disk as written, so a crash
    /// loses at most the last fragment. Otherwise the file is written as
    /// fragments and rewritten into a classic MP4 with `moov` first on
    /// [`Muxer::finish`].
    Mp4 { fragmented: bool },
    /// QuickTime brand; always finished as a classic file like
    /// `Mp4 { fragmented: false }`.
    Mov,
    /// Clusters per keyframe with unknown-size segment while writing; cues
    /// and sizes are written on [`Muxer::finish`]. A crashed file still plays.
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
        Container::Mp4 { .. } | Container::Mov => mp4::Mp4Muxer::create(path, container, tracks)
            .map(|m| Box::new(m) as Box<dyn Muxer>),
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
    /// keyframe at or before `to`, and per audio track the last packet
    /// starting at or before `to`. Callers decode forward to `to`.
    fn seek(&mut self, to: Duration) -> Result<()>;
}

/// Opens an MP4/MOV/M4V or Matroska/WebM file by content (not extension).
/// `Ok(None)` when the file is neither.
pub fn open_demuxer(path: &Path) -> Result<Option<Box<dyn Demuxer>>> {
    let _ = path;
    todo!("Containers: sniff ftyp/EBML magic, open mp4::Mp4Demuxer or mkv::MkvDemuxer")
}
