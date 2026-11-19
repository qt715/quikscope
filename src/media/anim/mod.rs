//! Animated images without FFmpeg: GIF, WebP, AVIF and JPEG XL in both
//! directions, APNG/PNG for reading, and the lossless recording master.
//!
//! Frames keep their own durations end to end: decoders report each frame's
//! display time, encoders take one per frame, so variable-delay animations
//! survive a decode/annotate/encode round trip unchanged (quantized only to
//! the destination format's precision, e.g. 10 ms for GIF).

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use super::OutputFormat;
use crate::raster::Image;

pub mod avif;
pub mod gif;
pub mod jxl;
pub mod master;
pub mod png;
pub mod webp;

/// One fully composited frame (straight-alpha BGRA, the animation's size).
#[derive(Clone, Debug, PartialEq)]
pub struct AnimFrame {
    pub image: Image,
    /// Display start, from the first frame.
    pub pts: Duration,
    /// How long this frame is shown.
    pub duration: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnimInfo {
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    /// Sum of frame durations.
    pub duration: Duration,
}

pub trait AnimDecoder: Send {
    fn info(&self) -> &AnimInfo;
    /// Next composited frame, `Ok(None)` after the last.
    fn next_frame(&mut self) -> Result<Option<AnimFrame>>;
    /// Starts over from the first frame (seeking decodes forward from here,
    /// since frames may build on earlier ones).
    fn rewind(&mut self) -> Result<()>;
}

/// Opens an animated or still GIF, WebP, AVIF, JPEG XL, APNG/PNG or
/// recording master by content. `Ok(None)` when the file is none of these.
/// A still image opens as a one-frame animation with zero duration.
pub fn open_decoder(path: &Path) -> Result<Option<Box<dyn AnimDecoder>>> {
    let _ = path;
    todo!("Animations: sniff magic bytes and dispatch")
}

pub trait AnimEncoder: Send {
    /// Appends a frame (BGRA, the size given at creation) shown for
    /// `duration`.
    fn push(&mut self, image: &Image, duration: Duration) -> Result<()>;
    /// Writes the remaining data. Dropping without `finish` leaves an
    /// incomplete file the caller discards.
    fn finish(self: Box<Self>) -> Result<()>;
}

/// Starts writing `format` (GIF, WebP, AVIF or JPEG XL) to `path` (normally
/// an [`crate::output::OutputFile::part`]). Loops forever.
pub fn create_encoder(
    format: OutputFormat,
    path: &Path,
    width: u32,
    height: u32,
) -> Result<Box<dyn AnimEncoder>> {
    let _ = (format, path, width, height);
    todo!("Animations: dispatch to gif/webp/avif/jxl")
}
