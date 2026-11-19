//! Re-encoding a media file: trimming, muting, converting between video
//! and animation formats, and compositing annotations onto every frame.
//! Also home of the H.264 and Opus track writers that recordings share.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

use super::codec::{self, VideoEncoder, VideoEncoderConfig, Yuv420, YuvLayout, opus};
use super::container::{self, Codec, Container, Muxer, Packet, Track, TrackKind};
use super::decode::{AudioDecoder, DecodedFrame, Decoder};
use super::{EncoderPreference, OutputFormat, PixelFormat, anim, convert};
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
    /// Resample to this rate; `None` keeps every source frame with its own
    /// duration (variable-delay animations stay exact, within the format's
    /// precision, e.g. 10 ms for GIF).
    pub fps: Option<u32>,
    /// MP4 layout (see [`crate::media::container::Container::Mp4`]).
    pub fragmented: bool,
    /// Scale down so the width is at most this (animations); `None` keeps
    /// the source size.
    pub max_width: Option<u32>,
    pub encoder: EncoderPreference,
}

/// Paints annotations onto one decoded frame, BGRA at the source size.
pub type Overlay<'a> = &'a (dyn Fn(&mut Image) + Sync);

/// Encodes `src` into `dst` and commits it, returning the saved path.
/// `progress` receives 0.0–1.0; setting `cancel` stops early with an error,
/// and dropping `dst` uncommitted removes the partial file.
///
/// The output covers exactly the trim range: the frame showing at its start
/// opens it, the last frame is cut at its end, and audio keeps its offset
/// from the picture.
pub fn export(
    src: &Path,
    dst: crate::output::OutputFile,
    options: &ExportOptions,
    overlay: Option<Overlay<'_>>,
    progress: &(dyn Fn(f32) + Sync),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    ensure!(
        src != dst.path(),
        "Source and export destination must differ"
    );
    let mut frames = Frames::open(src, options, overlay)?;
    let total = frames.end - frames.start;
    let report = |at: Duration| progress((at.as_secs_f64() / total.as_secs_f64()).min(1.0) as f32);
    if options.format.is_video() {
        export_video(src, dst.part(), options, &mut frames, &report, cancel)?;
    } else {
        let (width, height) = frames.size;
        let mut encoder = anim::create_encoder(options.format, dst.part(), width, height)?;
        while let Some(frame) = frames.next()? {
            ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
            encoder.push(&frame.image, frame.duration)?;
            report(frame.pts + frame.duration);
        }
        ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
        encoder.finish()?;
    }
    progress(1.0);
    dst.commit()
}

fn export_video(
    src: &Path,
    part: &Path,
    options: &ExportOptions,
    frames: &mut Frames<'_>,
    report: &dyn Fn(Duration),
    cancel: &AtomicBool,
) -> Result<()> {
    let (width, height) = (frames.size.0 & !1, frames.size.1 & !1);
    ensure!(
        width > 0 && height > 0,
        "A {}x{} picture is too small for video",
        frames.size.0,
        frames.size.1
    );
    let mut video = H264Track::new(VIDEO_TRACK, width, height, frames.rate, options.encoder)?;
    let mut audio = if options.mute {
        None
    } else {
        AudioExport::open(src, frames.start, frames.end)?
    };
    let mut tracks = vec![video.track()];
    tracks.extend(audio.as_ref().map(|a| a.track.track()));
    let mut mux = container::create_muxer(
        part,
        container_for(options.format, options.fragmented)?,
        &tracks,
    )?;
    let total = frames.end - frames.start;
    while let Some(frame) = frames.next()? {
        ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
        let picture = convert::to_yuv420(
            &frame.image.data,
            frame.image.stride(),
            PixelFormat::Bgra,
            width,
            height,
            video.layout(),
        );
        video.push(&picture, frame.pts, false, &mut *mux)?;
        // Audio follows the picture closely so the muxer never queues much.
        if let Some(audio) = &mut audio {
            audio.pump(frame.pts + AUDIO_LEAD, &mut *mux)?;
        }
        report(frame.pts + frame.duration);
    }
    ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
    video.finish(total, &mut *mux)?;
    if let Some(mut audio) = audio {
        audio.pump(total, &mut *mux)?;
        audio.track.finish(&mut *mux)?;
    }
    mux.finish()
}

/// How far audio is written ahead of the picture while exporting.
const AUDIO_LEAD: Duration = Duration::from_millis(500);

/// The source's audio from the trim start, written as Opus.
struct AudioExport {
    decoder: AudioDecoder,
    /// The first chunk, read ahead to place the track.
    first: Option<Vec<f32>>,
    track: OpusTrack,
    /// Length of the export; audio past it is cut.
    total: Duration,
    done: bool,
}

impl AudioExport {
    /// `None` without audio in the trim range.
    fn open(src: &Path, start: Duration, end: Duration) -> Result<Option<Self>> {
        let Some(mut decoder) = AudioDecoder::open(src, opus::RATE, 2)? else {
            return Ok(None);
        };
        if start > Duration::ZERO {
            decoder.seek(start)?;
        }
        let Some((samples, pts)) = decoder.next_chunk()? else {
            return Ok(None);
        };
        if pts >= end {
            return Ok(None);
        }
        // Audio that starts after the picture keeps its offset; the decoder
        // returns contiguous samples from here on.
        let offset = sample_time(frames_before(pts - start.min(pts)));
        Ok(Some(Self {
            decoder,
            first: Some(samples),
            track: OpusTrack::new(AUDIO_TRACK, offset)?,
            total: end - start,
            done: false,
        }))
    }

    /// Writes audio up to `until` (export time).
    fn pump(&mut self, until: Duration, mux: &mut dyn Muxer) -> Result<()> {
        while !self.done && self.track.position() < until {
            let samples = match self.first.take() {
                Some(samples) => samples,
                None => match self.decoder.next_chunk()? {
                    Some((samples, _)) => samples,
                    None => {
                        self.done = true;
                        break;
                    }
                },
            };
            let room =
                frames_before(self.total).saturating_sub(frames_before(self.track.position()));
            let take = (samples.len() / 2).min(room as usize);
            self.track.push(&samples[..take * 2], mux)?;
            self.done = take < samples.len() / 2;
        }
        Ok(())
    }
}

/// Number of 48 kHz frames that start before `at`.
fn frames_before(at: Duration) -> u64 {
    (at.as_nanos() * u128::from(opus::RATE)).div_ceil(1_000_000_000) as u64
}

/// Time of 48 kHz frame `frame`.
fn sample_time(frame: u64) -> Duration {
    Duration::from_nanos((u128::from(frame) * 1_000_000_000 / u128::from(opus::RATE)) as u64)
}

/// One frame of the export: what shows from `pts` (export time) for
/// `duration`.
struct OutFrame {
    image: Image,
    pts: Duration,
    duration: Duration,
}

/// Source frames in the trim range, with the overlay painted at the source
/// size and then scaled to the output size.
struct Frames<'a> {
    decoder: Decoder,
    start: Duration,
    end: Duration,
    /// Output rate; `None` keeps every source frame with its own timing.
    fps: Option<u32>,
    /// Rate a video export's encoder is set up for (bitrate, keyframe
    /// interval): the output grid, else the source's nominal rate, never its
    /// average (which a variable-rate recording's static stretches lower).
    rate: u32,
    size: (u32, u32),
    overlay: Option<Overlay<'a>>,
    /// Grid mode: the prepared frame showing now.
    current: Option<DecodedFrame>,
    /// The next source frame, not yet prepared.
    upcoming: Option<DecodedFrame>,
    index: u64,
}

impl<'a> Frames<'a> {
    fn open(src: &Path, options: &ExportOptions, overlay: Option<Overlay<'a>>) -> Result<Self> {
        let mut decoder = Decoder::open(src)?;
        let info = decoder.info().clone();
        let start = options.trim.map_or(Duration::ZERO, |t| t.start);
        let end = options
            .trim
            .map_or(info.duration, |t| t.end.min(info.duration));
        ensure!(end > start, "Export trim must have positive duration");
        let limit = if options.format == OutputFormat::Gif {
            50
        } else {
            240
        };
        let fps = options.fps.map(|fps| fps.min(limit));
        ensure!(fps != Some(0), "Export frame rate must be positive");
        if start > Duration::ZERO {
            decoder.seek(start)?;
        }
        let first = decoder
            .next_frame()?
            .context("No frames in the export range")?;
        // The overlay may crop: its output on the first frame sets the size.
        let (source_width, source_height) = match overlay {
            Some(overlay) => {
                let mut probe = first.image.clone();
                overlay(&mut probe);
                (probe.width, probe.height)
            }
            None => (first.image.width, first.image.height),
        };
        ensure!(
            source_width > 0 && source_height > 0,
            "The annotations leave an empty picture"
        );
        let rate = fps.unwrap_or(if info.fps > 0.0 {
            info.fps.round().clamp(1.0, 240.0) as u32
        } else {
            30
        });
        let (width, height) = if options.format.is_video() {
            let even = (source_width & !1, source_height & !1);
            let fitted = if even.0 > 0 && even.1 > 0 {
                encodable_size(even, rate, options.encoder)
            } else {
                even
            };
            if fitted == even {
                // Odd sizes are cropped when converting, never scaled.
                (source_width, source_height)
            } else {
                log::warn!(
                    "exporting {source_width}x{source_height} as {}x{}: no H.264 encoder takes the full size",
                    fitted.0,
                    fitted.1
                );
                fitted
            }
        } else {
            let width = options
                .max_width
                .unwrap_or(source_width)
                .clamp(1, source_width);
            let height =
                ((source_height as f64 * width as f64 / source_width as f64).round() as u32).max(1);
            (width, height)
        };
        Ok(Self {
            decoder,
            start,
            end,
            fps,
            rate,
            size: (width, height),
            overlay,
            current: None,
            upcoming: Some(first),
            index: 0,
        })
    }

    /// Paints the overlay at the source size, then scales.
    fn prepare(&self, mut frame: DecodedFrame) -> DecodedFrame {
        if let Some(overlay) = self.overlay {
            overlay(&mut frame.image);
        }
        if (frame.image.width, frame.image.height) != self.size {
            frame.image = convert::resize(&frame.image, self.size.0, self.size.1);
        }
        frame
    }

    /// The next source frame, unprepared.
    fn next_raw(&mut self) -> Result<Option<DecodedFrame>> {
        match self.upcoming.take() {
            Some(frame) => Ok(Some(frame)),
            None => self.decoder.next_frame(),
        }
    }

    fn next(&mut self) -> Result<Option<OutFrame>> {
        match self.fps {
            None => self.next_source_frame(),
            Some(fps) => self.next_tick(fps),
        }
    }

    /// Every source frame once, cut to the trim range.
    fn next_source_frame(&mut self) -> Result<Option<OutFrame>> {
        while let Some(frame) = self.next_raw()? {
            let from = frame.pts.max(self.start);
            let to = (frame.pts + frame.duration).min(self.end);
            if from >= self.end {
                break;
            }
            if to <= from {
                continue;
            }
            let frame = self.prepare(frame);
            return Ok(Some(OutFrame {
                image: frame.image,
                pts: from - self.start,
                duration: to - from,
            }));
        }
        Ok(None)
    }

    /// The frame showing at each tick of an `fps` grid from the trim start.
    fn next_tick(&mut self, fps: u32) -> Result<Option<OutFrame>> {
        let offset = Duration::from_nanos(self.index * 1_000_000_000 / u64::from(fps));
        let at = self.start + offset;
        if at >= self.end {
            return Ok(None);
        }
        // Only the newest source frame at or before the tick is shown:
        // frames it replaces within the tick are never painted or scaled.
        let mut newest = None;
        if self.current.is_none() {
            let Some(first) = self.next_raw()? else {
                return Ok(None);
            };
            newest = Some(first);
        }
        loop {
            if self.upcoming.is_none() {
                self.upcoming = self.decoder.next_frame()?;
            }
            match self.upcoming.take() {
                Some(next) if next.pts <= at => newest = Some(next),
                other => {
                    self.upcoming = other;
                    break;
                }
            }
        }
        if let Some(frame) = newest {
            self.current = Some(self.prepare(frame));
        }
        self.index += 1;
        let next = Duration::from_nanos(self.index * 1_000_000_000 / u64::from(fps));
        Ok(Some(OutFrame {
            image: self.current.as_ref().expect("set above").image.clone(),
            pts: offset,
            duration: (self.start + next).min(self.end) - at,
        }))
    }
}

/// Track numbers of the files quikscope writes.
pub(super) const VIDEO_TRACK: usize = 0;
pub(super) const AUDIO_TRACK: usize = 1;

/// The container a video format is written in.
pub(super) fn container_for(format: OutputFormat, fragmented: bool) -> Result<Container> {
    Ok(match format {
        OutputFormat::Mp4 => Container::Mp4 { fragmented },
        OutputFormat::Mov => Container::Mov,
        OutputFormat::Mkv => Container::Mkv,
        OutputFormat::Gif | OutputFormat::Webp | OutputFormat::Avif | OutputFormat::Jxl => {
            bail!("{format} is not a video format")
        }
    })
}

/// The largest size with `size`'s aspect ratio (to rounding, both sides
/// even) that an H.264 encoder allowed by `preference` takes at `fps`: `size`
/// itself unless it is beyond every encoder (an ultrawide or multi-monitor
/// capture past H.264 level 5.2 without a GPU encoder for it). `size` is
/// even and not empty.
pub(super) fn encodable_size(
    (width, height): (u32, u32),
    fps: u32,
    preference: EncoderPreference,
) -> (u32, u32) {
    let fits = |w: u32, h: u32| codec::h264_fits(w, h, fps, preference);
    if fits(width, height) {
        return (width, height);
    }
    let height_for = |w: u32| {
        let h = (u64::from(height) * u64::from(w) + u64::from(width) / 2) / u64::from(width);
        (h as u32 & !1).max(2)
    };
    // Bisects the width in steps of 2; 2 pixels wide always fits.
    let (mut good, mut bad) = (1, width / 2);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if fits(mid * 2, height_for(mid * 2)) {
            good = mid;
        } else {
            bad = mid;
        }
    }
    (good * 2, height_for(good * 2))
}

/// Shortest step between two video timestamps: they are kept in whole
/// milliseconds, the precision of Matroska, so every container sees them
/// strictly increasing.
const TICK: Duration = Duration::from_millis(1);

/// Writes an H.264 track with variable frame timing: each picture at its
/// own time, a keyframe at least every [`VideoEncoderConfig::keyframe_interval`]
/// worth of time (so files stay seekable however rarely pictures arrive),
/// and each packet lasting until the next.
pub(super) struct H264Track {
    encoder: Box<dyn VideoEncoder>,
    width: u32,
    height: u32,
    fps: u32,
    track: usize,
    key_interval: Duration,
    last_pts: Option<Duration>,
    last_key: Option<Duration>,
    /// The latest packet, written once the next one gives its duration.
    held: Option<Packet>,
}

impl H264Track {
    /// `width` and `height` even.
    pub(super) fn new(
        track: usize,
        width: u32,
        height: u32,
        fps: u32,
        preference: EncoderPreference,
    ) -> Result<Self> {
        let config = VideoEncoderConfig::for_screen(width, height, fps);
        Ok(Self {
            encoder: codec::h264_encoder(&config, preference)?,
            width,
            height,
            fps,
            track,
            key_interval: Duration::from_secs_f64(
                f64::from(config.keyframe_interval) / f64::from(fps.max(1)),
            ),
            last_pts: None,
            last_key: None,
            held: None,
        })
    }

    pub(super) fn name(&self) -> &str {
        self.encoder.name()
    }

    pub(super) fn layout(&self) -> YuvLayout {
        self.encoder.input_layout()
    }

    pub(super) fn key_interval(&self) -> Duration {
        self.key_interval
    }

    /// Time of the latest picture.
    pub(super) fn last_pts(&self) -> Option<Duration> {
        self.last_pts
    }

    pub(super) fn track(&self) -> Track {
        Track {
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: self.width,
                height: self.height,
                fps: f64::from(self.fps),
            },
            codec_private: Vec::new(),
        }
    }

    /// Encodes `picture` shown from `at`, which is rounded down to whole
    /// milliseconds and moved after the previous picture if needed. Returns
    /// the time used.
    pub(super) fn push(
        &mut self,
        picture: &Yuv420,
        at: Duration,
        keyframe: bool,
        mux: &mut dyn Muxer,
    ) -> Result<Duration> {
        let mut pts = Duration::from_millis(u64::try_from(at.as_millis()).unwrap_or(u64::MAX));
        if let Some(last) = self.last_pts
            && pts <= last
        {
            pts = last + TICK;
        }
        let keyframe = keyframe
            || self
                .last_key
                .is_none_or(|key| pts >= key + self.key_interval);
        if keyframe {
            self.last_key = Some(pts);
        }
        self.last_pts = Some(pts);
        for frame in self.encoder.encode(picture, pts, keyframe)? {
            self.write(frame, mux)?;
        }
        Ok(pts)
    }

    fn write(&mut self, frame: codec::EncodedFrame, mux: &mut dyn Muxer) -> Result<()> {
        if frame.keyframe {
            self.last_key = self.last_key.max(Some(frame.pts));
        }
        let packet = Packet {
            track: self.track,
            data: frame.data,
            pts: frame.pts,
            duration: None,
            keyframe: frame.keyframe,
        };
        if let Some(mut held) = self.held.replace(packet) {
            held.duration = Some(frame.pts.saturating_sub(held.pts));
            mux.write(held)?;
        }
        Ok(())
    }

    /// Drains the encoder; the last picture shows until `end`. A failed
    /// drain still writes the packet already held, so the file keeps every
    /// picture encoded before the failure.
    pub(super) fn finish(&mut self, end: Duration, mux: &mut dyn Muxer) -> Result<()> {
        let drained = self.encoder.finish().and_then(|frames| {
            frames
                .into_iter()
                .try_for_each(|frame| self.write(frame, mux))
        });
        let held = self.finish_held(end, mux);
        drained.and(held)
    }

    /// Writes the packet held back for its duration, without touching the
    /// encoder (which may have failed): what salvaging a recording keeps.
    pub(super) fn finish_held(&mut self, end: Duration, mux: &mut dyn Muxer) -> Result<()> {
        if let Some(mut held) = self.held.take() {
            held.duration = Some(end.saturating_sub(held.pts).max(TICK));
            mux.write(held)?;
        }
        Ok(())
    }
}

/// Writes 48 kHz stereo as an Opus track of 20 ms packets whose times count
/// samples from `start` exactly. Packet times include the encoder's pre-skip
/// (the container convention), so the first sample pushed plays at `start`.
pub(super) struct OpusTrack {
    encoder: opus::Encoder,
    track: usize,
    start: Duration,
    /// Samples not yet making a whole packet.
    pending: Vec<f32>,
    packets: u64,
    /// Frames pushed.
    frames: u64,
}

impl OpusTrack {
    pub(super) fn new(track: usize, start: Duration) -> Result<Self> {
        Ok(Self {
            encoder: opus::Encoder::new(opus::BITRATE)?,
            track,
            start,
            pending: Vec::with_capacity(opus::FRAME * 2),
            packets: 0,
            frames: 0,
        })
    }

    pub(super) fn track(&self) -> Track {
        Track {
            codec: Codec::Opus,
            kind: TrackKind::Audio {
                rate: opus::RATE,
                channels: 2,
            },
            codec_private: self.encoder.head(),
        }
    }

    /// Where the samples pushed so far end.
    pub(super) fn position(&self) -> Duration {
        self.start + sample_time(self.frames)
    }

    /// Appends interleaved stereo samples.
    pub(super) fn push(&mut self, samples: &[f32], mux: &mut dyn Muxer) -> Result<()> {
        self.frames += (samples.len() / 2) as u64;
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= opus::FRAME * 2 {
            self.encode_packet(mux)?;
        }
        Ok(())
    }

    fn encode_packet(&mut self, mux: &mut dyn Muxer) -> Result<()> {
        let data = self.encoder.encode(&self.pending[..opus::FRAME * 2])?;
        self.pending.drain(..opus::FRAME * 2);
        let packet_length = sample_time(opus::FRAME as u64);
        mux.write(Packet {
            track: self.track,
            data,
            pts: self.start + sample_time(self.packets * opus::FRAME as u64),
            duration: Some(packet_length),
            keyframe: true,
        })?;
        self.packets += 1;
        Ok(())
    }

    /// Pads the last packet and adds the packets that carry the samples the
    /// pre-skip still holds back inside the encoder.
    pub(super) fn finish(&mut self, mux: &mut dyn Muxer) -> Result<()> {
        let needed = self.frames + u64::from(self.encoder.pre_skip());
        while self.packets * (opus::FRAME as u64) < needed {
            self.pending.resize(opus::FRAME * 2, 0.0);
            self.encode_packet(mux)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_times_round_trip() {
        for frame in [0, 1, 959, 960, 48_000, 48_001, 10_000_000] {
            assert_eq!(frames_before(sample_time(frame)), frame);
        }
        assert_eq!(frames_before(Duration::from_millis(20)), 960);
        assert_eq!(frames_before(Duration::from_nanos(1)), 1);
    }
}
