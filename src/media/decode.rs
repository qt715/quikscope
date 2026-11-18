//! Reading media files: probing, decoding video/animation frames to BGRA
//! and audio to interleaved f32, with seeking.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use super::MediaInfo;
use crate::raster::Image;

/// Probes `path`: dimensions, duration, frame rate, audio, and whether it
/// is a video or an (animated) image.
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let _ = path;
    todo!("MediaCore")
}

/// One decoded frame.
pub struct DecodedFrame {
    /// BGRA at the decoder's output size.
    pub image: Image,
    /// Presentation time from the start of the stream.
    pub pts: Duration,
    /// How long it is shown (the next frame's pts minus this one's).
    pub duration: Duration,
}

/// Sequential video frame decoder with seeking.
pub struct Decoder {
    _private: (),
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Decoder> {
        let _ = path;
        todo!("MediaCore")
    }

    pub fn info(&self) -> &MediaInfo {
        todo!("MediaCore")
    }

    /// Scales decoded frames to `width`×`height` (area filtering), e.g. for
    /// previews; the default is the native size.
    pub fn set_output_size(&mut self, width: u32, height: u32) {
        let _ = (width, height);
        todo!("MediaCore")
    }

    /// Positions the decoder so the next frame is the one showing at `to`.
    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let _ = to;
        todo!("MediaCore")
    }

    /// The next frame, or `None` at the end.
    pub fn next_frame(&mut self) -> Result<Option<DecodedFrame>> {
        todo!("MediaCore")
    }
}

/// Sequential audio decoder resampled to a fixed rate and channel count.
pub struct AudioDecoder {
    _private: (),
}

impl AudioDecoder {
    /// `None` when the file has no audio stream.
    pub fn open(path: &Path, rate: u32, channels: u16) -> Result<Option<AudioDecoder>> {
        let _ = (path, rate, channels);
        todo!("MediaCore")
    }

    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let _ = to;
        todo!("MediaCore")
    }

    /// Next interleaved samples and the time of the first one; `None` at the end.
    pub fn next_chunk(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        todo!("MediaCore")
    }
}

/// A frame from the middle of the file, scaled to fit `max_side`.
pub fn thumbnail(path: &Path, max_side: u32) -> Result<Image> {
    let _ = (path, max_side);
    todo!("MediaCore")
}
