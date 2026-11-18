//! A recording session: pulls frames from a [`VideoSource`] and audio from
//! [`AudioSource`]s on its own threads, timestamps them against one clock,
//! encodes and muxes. Pause excludes time from the output; stop finalizes.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use super::{EncoderPreference, OutputFormat, VideoSource};
use crate::raster::Image;

/// What to record into.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    /// Final file. Videos are written here directly (as `<path>.part` until
    /// finished); animations are first recorded losslessly to `master` and
    /// encoded into `path` when the session stops.
    pub path: PathBuf,
    pub format: OutputFormat,
    /// Lossless intermediate for animations (FFV1 in Matroska), kept after
    /// the session so the annotator can re-export without generation loss.
    /// Ignored for videos.
    pub master: PathBuf,
    pub fps: u32,
    /// Audio device ids for videos (see [`super::audio::devices`]); ignored
    /// for animations.
    pub audio: Vec<String>,
    pub encoder: EncoderPreference,
}

/// A finished recording.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: PathBuf,
    pub format: OutputFormat,
    /// For animations the lossless master; for videos the same as `path`.
    pub master: PathBuf,
    pub width: u32,
    pub height: u32,
    /// Recorded time, pauses excluded.
    pub duration: Duration,
    /// A representative frame (for notifications and the clipboard preview).
    pub thumbnail: Image,
    /// Encoder actually used, e.g. "h264_nvenc" or "libx264", for logs.
    pub encoder: String,
}

/// A running recording. Methods take `&self` so the daemon can share it.
pub struct Session {
    _private: (),
}

impl Session {
    /// Opens the output, starts audio capture and the encoder threads, and
    /// begins pulling frames from `source`. Fails before returning when no
    /// encoder accepts the frames or an audio device cannot be opened.
    pub fn start(config: SessionConfig, source: Box<dyn VideoSource>) -> Result<Session> {
        let _ = (config, source);
        todo!("MediaCore")
    }

    /// Frame size of the recording.
    pub fn size(&self) -> (u32, u32) {
        todo!("MediaCore")
    }

    pub fn pause(&self) {
        todo!("MediaCore")
    }

    pub fn resume(&self) {
        todo!("MediaCore")
    }

    pub fn is_paused(&self) -> bool {
        todo!("MediaCore")
    }

    /// Recorded time so far, pauses excluded.
    pub fn elapsed(&self) -> Duration {
        todo!("MediaCore")
    }

    /// An error that ended capture early (the output disappeared, the
    /// encoder failed). The session keeps what it has; `stop` still
    /// finalizes it.
    pub fn failure(&self) -> Option<String> {
        todo!("MediaCore")
    }

    /// Stops capturing and finalizes the file (encoding the animation from
    /// its master). Blocks; call it off the UI thread.
    pub fn stop(self) -> Result<Recorded> {
        todo!("MediaCore")
    }

    /// Stops and deletes everything written.
    pub fn cancel(self) {
        todo!("MediaCore")
    }
}
