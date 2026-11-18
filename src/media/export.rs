//! Re-encoding a media file: trimming, muting, converting between video
//! and animation formats, and compositing annotations onto every frame.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

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
    use super::{Frame, decode::AudioDecoder, encode::{Graph, Muxer, OutputFile, VideoEncoder}};
    use std::{sync::atomic::Ordering, time::Instant};
    super::init();
    ensure!(src != dst, "Source and export destination must differ");
    let info = super::decode::probe(src)?;
    let start = options.trim.map_or(Duration::ZERO, |t| t.start);
    let end = options.trim.map_or(info.duration, |t| t.end);
    ensure!(end > start, "Export trim must have positive duration");
    let fps = options.fps.unwrap_or(info.fps.round().max(1.0) as u32)
        .min(if options.format == OutputFormat::Gif { 50 } else { 240 });
    ensure!(fps > 0, "Export frame rate must be positive");
    let width = if options.format.is_video() { info.width } else { options.max_width.unwrap_or(info.width).min(info.width) };
    ensure!(width > 0, "Export width must be positive");
    let height = (info.height as f64 * width as f64 / info.width as f64).round().max(1.0) as u32;
    let scale = format!("scale={width}:{height}:flags=area");
    let make_frames = || -> Result<Frames<'_>> { Frames::new(src, start, end, fps, overlay) };
    let mut frames = make_frames()?;
    let first = frames.next()?.context("No frames in export range")?;
    let first_av = Frame::from_image(&first, Instant::now()).into_av();
    let video = match options.format {
        OutputFormat::Mp4 | OutputFormat::Mkv | OutputFormat::Mov =>
            VideoEncoder::select(&first_av, fps, options.encoder, false)?,
        OutputFormat::Gif => {
            let mut palette = Graph::new(&first_av, &format!("{scale},palettegen=stats_mode=diff"))?;
            let mut pass = make_frames()?;
            let mut index = 0_u64;
            while let Some(image) = pass.next()? {
                ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
                let mut frame = Frame::from_image(&image, Instant::now()).into_av();
                frame.set_pts(Some((index as f64 / fps as f64 * 90000.0).round() as i64));
                palette.push(&frame)?;
                index += 1;
                progress((index as f64 / fps as f64 / (end - start).as_secs_f64() * 0.4).min(0.4) as f32);
            }
            palette.flush()?;
            let palette = palette.pull()?.context("FFmpeg palettegen produced no palette")?;
            let graph = Graph::palette(&first_av, &palette, &scale)?;
            VideoEncoder::filtered(&first_av, fps, "gif", graph)?
        }
        OutputFormat::Webp => VideoEncoder::named(&first_av, fps, "libwebp_anim", "bgra", Some(&format!("{scale},format=bgra")))?,
        OutputFormat::Avif => {
            let name = ["libaom-av1", "libsvtav1"].into_iter()
                .find(|name| ffmpeg_next::encoder::find_by_name(name).is_some())
                .context("FFmpeg AVIF export requires libaom-av1 or libsvtav1 and the avif muxer")?;
            VideoEncoder::named(&first_av, fps, name, "yuv420p", Some(&format!("{scale},format=yuv420p")))?
        }
        OutputFormat::Jxl => VideoEncoder::named(&first_av, fps, "libjxl_anim", "rgb24", Some(&format!("{scale},format=rgb24")))?,
    };
    let file = OutputFile::new(dst);
    let mut audio = if options.format.is_video() && !options.mute {
        AudioDecoder::open(src, 48000, 2)?
    } else { None };
    if let Some(audio) = &mut audio && start > Duration::ZERO { audio.seek(start)?; }
    let mut mux = Muxer::new(&file.path, super::record::mux_name(options.format), video, audio.is_some())?;
    let mut image = Some(first);
    let mut index = 0_u64;
    let mut audio_position = 0_usize;
    let audio_end = ((end - start).as_secs_f64() * 48000.0).round() as usize;
    while let Some(frame) = image {
        ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
        let time = Duration::from_secs_f64(index as f64 / fps as f64);
        mux.video(&Frame::from_image(&frame, Instant::now()).into_av(), time)?;
        // Interleave rather than queuing the entire audio track in the muxer.
        if let Some(audio) = &mut audio {
            let target = ((time.as_secs_f64() + 1.0 / fps as f64) * 48000.0).round() as usize;
            while audio_position < target.min(audio_end) {
                let Some((samples, pts)) = audio.next_chunk()? else { break; };
                let at = (pts.saturating_sub(start).as_secs_f64() * 48000.0).round() as usize;
                if at > audio_position {
                    let silence = vec![0.0; (at.min(audio_end) - audio_position) * 2];
                    mux.audio(&silence)?;
                    audio_position = at.min(audio_end);
                }
                let skip = audio_position.saturating_sub(at).min(samples.len() / 2);
                let count = (samples.len() / 2 - skip).min(audio_end.saturating_sub(audio_position));
                mux.audio(&samples[skip * 2..(skip + count) * 2])?;
                audio_position += count;
                if count == 0 && pts >= end { break; }
            }
        }
        index += 1;
        let fraction = (index as f64 / fps as f64 / (end - start).as_secs_f64()).min(1.0) as f32;
        progress(if options.format == OutputFormat::Gif { 0.4 + fraction * 0.6 } else { fraction });
        image = frames.next()?;
    }
    ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
    mux.finish()?;
    file.commit()?;
    progress(1.0);
    Ok(())
}

struct Frames<'a> {
    decoder: super::decode::Decoder,
    current: super::decode::DecodedFrame,
    next: Option<super::decode::DecodedFrame>,
    start: Duration,
    end: Duration,
    fps: u32,
    index: u64,
    overlay: Option<Overlay<'a>>,
}
impl<'a> Frames<'a> {
    fn new(src: &Path, start: Duration, end: Duration, fps: u32, overlay: Option<Overlay<'a>>) -> Result<Self> {
        let mut decoder = super::decode::Decoder::open(src)?;
        if start > Duration::ZERO { decoder.seek(start)?; }
        let current = decoder.next_frame()?.context("Media contains no frames")?;
        let next = decoder.next_frame()?;
        Ok(Self { decoder, current, next, start, end, fps, index: 0, overlay })
    }
    fn next(&mut self) -> Result<Option<Image>> {
        let at = self.start + Duration::from_secs_f64(self.index as f64 / self.fps as f64);
        if at >= self.end { return Ok(None); }
        while self.next.as_ref().is_some_and(|frame| frame.pts <= at) {
            self.current = self.next.take().unwrap();
            self.next = self.decoder.next_frame()?;
        }
        let mut image = self.current.image.clone();
        if let Some(overlay) = self.overlay { overlay(&mut image); }
        self.index += 1;
        Ok(Some(image))
    }
}
