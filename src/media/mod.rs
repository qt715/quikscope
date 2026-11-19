//! Video and animation: recording sessions, encoding, decoding and export,
//! on FFmpeg's libraries (libavcodec, libavformat, libavfilter, libavutil).
//!
//! Platform backends produce [`Frame`]s through a [`VideoSource`] and audio
//! through [`AudioSource`]s; a [`record::Session`] timestamps, encodes and
//! muxes them on its own threads. Frames may be CPU BGRA or hardware frames
//! (DRM PRIME, D3D11, VideoToolbox); the encoder maps them to whichever
//! encoder works (see `encode`).

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

pub mod anim;
pub mod audio;
pub mod codec;
pub mod container;
pub mod convert;
pub mod decode;
pub mod encode;
pub mod export;
mod frame;
pub mod record;

pub use frame::{Frame, PixelFormat, Pixels};

/// Audio device id meaning "what the default output plays" (desktop audio).
pub const DEFAULT_OUTPUT: &str = "default-output";
/// Audio device id meaning "the default microphone".
pub const DEFAULT_INPUT: &str = "default-input";

/// Initializes FFmpeg once (log routing into `log`, device registration).
/// Cheap after the first call.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(e) = ffmpeg_next::init() {
            log::error!("FFmpeg initialization failed: {e}");
        }
        encode::route_logs();
    });
}

/// What a recording produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Video with optional audio: MP4, MKV or MOV.
    Video,
    /// Silent looping animation: GIF, WebP, AVIF or JPEG XL.
    Animation,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Animation => "animation",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "video" => Kind::Video,
            "animation" | "gif" | "anim" => Kind::Animation,
            _ => return None,
        })
    }
}

/// File format of a recording or export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    /// H.264 + AAC in MP4.
    Mp4,
    /// H.264 + Opus in Matroska.
    Mkv,
    /// H.264 + AAC in QuickTime.
    Mov,
    Gif,
    Webp,
    Avif,
    Jxl,
}

impl OutputFormat {
    pub const VIDEO: [OutputFormat; 3] = [OutputFormat::Mp4, OutputFormat::Mkv, OutputFormat::Mov];
    pub const ANIMATION: [OutputFormat; 4] = [
        OutputFormat::Gif,
        OutputFormat::Webp,
        OutputFormat::Avif,
        OutputFormat::Jxl,
    ];

    pub fn parse(s: &str) -> Option<OutputFormat> {
        Some(match s.to_ascii_lowercase().as_str() {
            "mp4" => OutputFormat::Mp4,
            "mkv" | "matroska" => OutputFormat::Mkv,
            "mov" | "quicktime" => OutputFormat::Mov,
            "gif" => OutputFormat::Gif,
            "webp" => OutputFormat::Webp,
            "avif" => OutputFormat::Avif,
            "jxl" | "jpegxl" => OutputFormat::Jxl,
            _ => return None,
        })
    }

    /// The format a file name's extension names, if any.
    pub fn from_path(path: &Path) -> Option<OutputFormat> {
        OutputFormat::parse(path.extension()?.to_str()?)
    }

    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Mkv => "mkv",
            OutputFormat::Mov => "mov",
            OutputFormat::Gif => "gif",
            OutputFormat::Webp => "webp",
            OutputFormat::Avif => "avif",
            OutputFormat::Jxl => "jxl",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "video/mp4",
            OutputFormat::Mkv => "video/x-matroska",
            OutputFormat::Mov => "video/quicktime",
            OutputFormat::Gif => "image/gif",
            OutputFormat::Webp => "image/webp",
            OutputFormat::Avif => "image/avif",
            OutputFormat::Jxl => "image/jxl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "MP4",
            OutputFormat::Mkv => "MKV",
            OutputFormat::Mov => "MOV",
            OutputFormat::Gif => "GIF",
            OutputFormat::Webp => "WebP",
            OutputFormat::Avif => "AVIF",
            OutputFormat::Jxl => "JPEG XL",
        }
    }

    pub fn is_video(self) -> bool {
        matches!(
            self,
            OutputFormat::Mp4 | OutputFormat::Mkv | OutputFormat::Mov
        )
    }

    pub fn kind(self) -> Kind {
        if self.is_video() {
            Kind::Video
        } else {
            Kind::Animation
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

/// Encoder choice: `Auto` tries the GPU encoders that match the frames and
/// falls back to software; `Software` never touches the GPU encoders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EncoderPreference {
    #[default]
    Auto,
    Software,
}

impl EncoderPreference {
    pub fn parse(s: &str) -> Option<EncoderPreference> {
        Some(match s {
            "auto" => EncoderPreference::Auto,
            "software" => EncoderPreference::Software,
            _ => return None,
        })
    }
}

/// A source of screen frames, opened by the platform for one recording.
/// Dropping it stops the capture and releases its buffers.
pub trait VideoSource: Send {
    /// Pixel size of every frame this source produces.
    fn size(&self) -> (u32, u32);
    /// Blocks until the next frame, delivering at most the requested frame
    /// rate. A static screen may deliver nothing for a while: sources wait
    /// for damage, and the session repeats the last frame as needed. Returns
    /// `Ok(None)` when no new frame arrived within `timeout` (the session then
    /// checks whether it was stopped). The first frame after opening and
    /// after [`set_paused(false)`](Self::set_paused) is captured whether or
    /// not anything changed, so the recording never shows a stale picture.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;
    /// While paused the source stops capturing (no compositor copies, no
    /// GPU work) and `next_frame` only waits; called from the thread that
    /// calls `next_frame`.
    fn set_paused(&mut self, paused: bool) {
        let _ = paused;
    }
}

/// Interleaved 32-bit float samples captured starting at `captured`.
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub captured: Instant,
}

/// Sample rate and channel count of an [`AudioSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSpec {
    pub rate: u32,
    pub channels: u16,
}

/// A capturing audio device (or a capture's own audio).
pub trait AudioSource: Send {
    fn spec(&self) -> AudioSpec;
    /// Blocks for the next chunk; `Ok(None)` when nothing arrived within
    /// `timeout`.
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>>;
}

/// An audio device recordings can include.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AudioDevice {
    /// Stable id for the config file and `--audio`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// `true` for an output's loopback/monitor (desktop audio), `false` for
    /// a microphone or other input.
    pub output: bool,
}

/// Facts about a media file, for opening it.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration: Duration,
    /// Average frame rate (0 when unknown).
    pub fps: f64,
    /// Number of video frames when the container says so.
    pub frames: Option<u64>,
    pub has_audio: bool,
    /// `Video` for files with a video codec stream and/or audio,
    /// `Animation` for animated image formats (GIF, animated WebP/AVIF/JXL,
    /// APNG). A single still image probes as an animation with one frame.
    pub kind: Kind,
}

impl MediaInfo {
    /// More than one frame.
    pub fn is_animated(&self) -> bool {
        self.frames.is_none_or(|n| n > 1) && self.duration > Duration::ZERO
    }
}
