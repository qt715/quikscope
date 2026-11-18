//! Re-encoding a media file: trimming, muting, converting between video
//! and animation formats, and compositing annotations onto every frame.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;

use super::{EncoderPreference, OutputFormat};
use crate::raster::Image;

/// Part of the source to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trim {
    pub start: Duration,
    pub end: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportOptions {
    pub format: OutputFormat,
    pub trim: Option<Trim>,
    /// Leave audio out (videos only).
    pub mute: bool,
    /// Frame rate for animations; `None` keeps the source rate (capped at 50
    /// for GIF, whose delays have 10 ms resolution).
    pub fps: Option<u32>,
    /// Scale down so the width is at most this (animations); `None` keeps
    /// the source size.
    pub max_width: Option<u32>,
    pub encoder: EncoderPreference,
}

/// Paints annotations onto one decoded frame, BGRA at the source size.
pub type Overlay<'a> = &'a (dyn Fn(&mut Image) + Sync);

/// Encodes `src` into `dst` (written to `<dst>.part`, then renamed).
/// `progress` receives 0.0–1.0; setting `cancel` stops early with an error
/// and removes the partial file.
pub fn export(
    src: &Path,
    dst: &Path,
    options: &ExportOptions,
    overlay: Option<Overlay<'_>>,
    progress: &(dyn Fn(f32) + Sync),
    cancel: &AtomicBool,
) -> Result<()> {
    let _ = (src, dst, options, overlay, progress, cancel);
    todo!("MediaCore")
}
