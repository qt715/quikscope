//! Reading media: H.264 video with Opus/AAC audio from MP4/MOV/Matroska
//! ([`super::container`] and [`super::codec`]), animated images and
//! recording masters ([`super::anim`]).
//!
//! Every timestamp is relative to the file's presentation start, which all
//! tracks share (the demuxers apply edit lists and track start offsets), so
//! audio that starts after the picture keeps its offset through playback,
//! seeking and export.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

use super::anim::{self, AnimDecoder, master::MasterReader};
use super::codec::{self, DecodedPicture, VideoDecoder, resample::Resampler};
use super::container::{self, Codec, Demuxer, Track, TrackKind};
use super::{AudioSpec, EncoderPreference, Kind, MediaInfo, convert};
use crate::raster::Image;

/// Set when the user chose software coding; see [`set_decoder_preference`].
static SOFTWARE_DECODING: AtomicBool = AtomicBool::new(false);

/// Which H.264 decoders every [`Decoder`] opened from now on may use
/// (thumbnails, playback, exports). Follows the encoder setting, so
/// choosing "Software" to avoid a broken GPU driver keeps it out of decoding
/// too. Defaults to [`EncoderPreference::Auto`].
pub fn set_decoder_preference(preference: EncoderPreference) {
    SOFTWARE_DECODING.store(preference == EncoderPreference::Software, Ordering::Relaxed);
}

fn decoder_preference() -> EncoderPreference {
    if SOFTWARE_DECODING.load(Ordering::Relaxed) {
        EncoderPreference::Software
    } else {
        EncoderPreference::Auto
    }
}

/// What a file turned out to be.
enum Opened {
    Video {
        demuxer: Box<dyn Demuxer>,
        track: usize,
    },
    Animation(Box<dyn AnimDecoder>),
    /// Kept as its own type for its keyframe-index seek.
    Master(MasterReader),
}

/// Opens `path` by content: an MP4/MOV/Matroska file with an H.264 track,
/// else an animated image or master (AVIF is ISOBMFF too, so a container
/// without H.264 still gets that chance).
fn open(path: &Path) -> Result<(Opened, MediaInfo)> {
    let container_error = match container::open_demuxer(path) {
        Ok(Some(demuxer)) => {
            if let Some(track) = demuxer.tracks().iter().position(|t| t.codec == Codec::H264) {
                let info = video_info(&*demuxer, track)?;
                return Ok((Opened::Video { demuxer, track }, info));
            }
            None
        }
        Ok(None) => None,
        Err(e) => Some(e),
    };
    if anim::master::is_master(path) {
        let master = MasterReader::open(path)?;
        let mut info = animation_info(master.info());
        // Recorded frames are only the changed ones: the capture rate, not
        // their average, is what an export needs.
        if let Some(interval) = master.nominal_interval() {
            info.fps = 1.0 / interval.as_secs_f64();
        }
        return Ok((Opened::Master(master), info));
    }
    match anim::open_decoder(path)? {
        Some(decoder) => {
            let info = animation_info(decoder.info());
            Ok((Opened::Animation(decoder), info))
        }
        None => Err(container_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} is not a video or animation quikscope can read",
                path.display()
            )
        })),
    }
}

fn video_info(demuxer: &dyn Demuxer, track: usize) -> Result<MediaInfo> {
    let TrackKind::Video { width, height, fps } = demuxer.tracks()[track].kind else {
        bail!("H.264 track without picture size");
    };
    // The decoders crop odd sizes to even (4:2:0 pictures).
    let (width, height) = (width & !1, height & !1);
    ensure!(width > 0 && height > 0, "empty {width}x{height} video");
    Ok(MediaInfo {
        width,
        height,
        duration: demuxer.duration(),
        fps: if fps.is_finite() && fps > 0.0 {
            fps
        } else {
            0.0
        },
        frames: None,
        has_audio: audio_track(demuxer.tracks()).is_some(),
        kind: Kind::Video,
    })
}

fn animation_info(info: &anim::AnimInfo) -> MediaInfo {
    let seconds = info.duration.as_secs_f64();
    MediaInfo {
        width: info.width,
        height: info.height,
        duration: info.duration,
        fps: if seconds > 0.0 {
            info.frames as f64 / seconds
        } else {
            0.0
        },
        frames: Some(info.frames),
        has_audio: false,
        kind: Kind::Animation,
    }
}

/// The first audio track in a codec we decode.
fn audio_track(tracks: &[Track]) -> Option<usize> {
    tracks.iter().position(|t| {
        matches!(t.kind, TrackKind::Audio { .. }) && matches!(t.codec, Codec::Opus | Codec::Aac)
    })
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    open(path)
        .map(|(_, info)| info)
        .with_context(|| format!("Opening {}", path.display()))
}

pub struct DecodedFrame {
    pub image: Image,
    pub pts: Duration,
    /// How long the frame shows: up to the next frame, for the last one up to
    /// the end of its track.
    pub duration: Duration,
}

/// A frame decoded ahead of being returned. Video pictures stay YUV until
/// returned, so frames a seek skips past are never converted.
struct Pending {
    picture: Picture,
    pts: Duration,
    /// Known for animation frames; video frames run to the next one.
    duration: Option<Duration>,
}

enum Picture {
    Yuv(codec::Yuv420),
    Bgra(Image),
}

struct VideoStream {
    demuxer: Box<dyn Demuxer>,
    track: usize,
    decoder: Box<dyn VideoDecoder>,
    eof: bool,
    /// End of the latest video packet read, for the last frame's duration.
    end: Duration,
    /// Nominal frame interval, for packets without a duration.
    interval: Duration,
}

impl VideoStream {
    /// Pictures from the next packets that produce any; `None` once the
    /// stream is drained.
    fn decode_more(&mut self) -> Result<Option<Vec<DecodedPicture>>> {
        if self.eof {
            return Ok(None);
        }
        loop {
            match self.demuxer.next_packet()? {
                Some(packet) if packet.track == self.track => {
                    let length = packet.duration.unwrap_or(self.interval);
                    self.end = self.end.max(packet.pts + length);
                    let pictures = self.decoder.decode(&packet.data, packet.pts)?;
                    if !pictures.is_empty() {
                        return Ok(Some(pictures));
                    }
                }
                Some(_) => {}
                None => {
                    self.eof = true;
                    return Ok(Some(self.decoder.flush()?));
                }
            }
        }
    }
}

enum Source {
    Video(VideoStream),
    Animation {
        decoder: Box<dyn AnimDecoder>,
        /// End of the latest frame taken from `decoder`.
        decoded: Duration,
    },
    Master(MasterReader),
}

/// Decodes the frames of a video or animation in presentation order, as BGRA
/// at [`set_output_size`](Self::set_output_size) (the file's size unless
/// set).
pub struct Decoder {
    source: Source,
    info: MediaInfo,
    size: (u32, u32),
    pending: VecDeque<Pending>,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let (opened, info) = open(path).with_context(|| format!("Opening {}", path.display()))?;
        let source = match opened {
            Opened::Video { demuxer, track } => Source::Video(VideoStream {
                demuxer,
                track,
                decoder: codec::h264_decoder(decoder_preference())?,
                eof: false,
                end: Duration::ZERO,
                interval: Duration::from_secs_f64(
                    1.0 / if info.fps > 0.0 { info.fps } else { 30.0 },
                ),
            }),
            Opened::Animation(decoder) => Source::Animation {
                decoder,
                decoded: Duration::ZERO,
            },
            Opened::Master(master) => Source::Master(master),
        };
        Ok(Self {
            source,
            size: (info.width, info.height),
            info,
            pending: VecDeque::new(),
        })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    pub fn set_output_size(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
    }

    /// Positions the decoder so that [`next_frame`](Self::next_frame)
    /// returns the frame showing at `to` (the first frame before the file
    /// starts, the last one past its end), then the frames after it.
    pub fn seek(&mut self, to: Duration) -> Result<()> {
        match &mut self.source {
            Source::Video(video) => {
                self.pending.clear();
                video.demuxer.seek(to)?;
                // Pictures still in the decoder belong to the old position.
                video.decoder.flush()?;
                video.eof = false;
                let mut previous = None;
                while let Some(pictures) = video.decode_more()? {
                    let mut pictures = pictures.into_iter();
                    for picture in pictures.by_ref() {
                        if picture.pts <= to {
                            previous = Some(picture);
                            continue;
                        }
                        self.pending.extend(previous.take().map(video_pending));
                        self.pending.push_back(video_pending(picture));
                        self.pending.extend(pictures.map(video_pending));
                        return Ok(());
                    }
                }
                self.pending.extend(previous.map(video_pending));
            }
            Source::Animation { decoder, decoded } => {
                // Frames may build on earlier ones, so an animation decodes
                // forward only: a target among the frames decoded ahead is
                // used as is, one further on is decoded towards, and only an
                // earlier one (or one past the end, which shows the last
                // frame) starts over from the first frame.
                let showing = self
                    .pending
                    .iter()
                    .position(|p| p.pts + p.duration.unwrap_or_default() > to);
                match showing {
                    Some(i) if self.pending[i].pts <= to => {
                        self.pending.drain(..i);
                        return Ok(());
                    }
                    None if to >= *decoded && to < self.info.duration => self.pending.clear(),
                    _ => {
                        self.pending.clear();
                        decoder.rewind()?;
                        *decoded = Duration::ZERO;
                    }
                }
                let mut last = None;
                while let Some(frame) = decoder.next_frame()? {
                    *decoded = frame.pts + frame.duration;
                    last = Some(frame);
                    if *decoded > to {
                        break;
                    }
                }
                self.pending.extend(last.map(anim_pending));
            }
            Source::Master(master) => {
                self.pending.clear();
                master.seek(to)?;
            }
        }
        Ok(())
    }

    /// Decodes until `count` frames are pending or the stream ends.
    fn fill(&mut self, count: usize) -> Result<()> {
        while self.pending.len() < count {
            match &mut self.source {
                Source::Video(video) => match video.decode_more()? {
                    Some(pictures) => self.pending.extend(pictures.into_iter().map(video_pending)),
                    None => break,
                },
                Source::Animation { decoder, decoded } => match decoder.next_frame()? {
                    Some(frame) => {
                        *decoded = frame.pts + frame.duration;
                        self.pending.push_back(anim_pending(frame));
                    }
                    None => break,
                },
                Source::Master(master) => match master.next_frame()? {
                    Some(frame) => self.pending.push_back(anim_pending(frame)),
                    None => break,
                },
            }
        }
        Ok(())
    }

    pub fn next_frame(&mut self) -> Result<Option<DecodedFrame>> {
        // One frame of lookahead gives a video frame its duration.
        self.fill(2)?;
        let Some(frame) = self.pending.pop_front() else {
            return Ok(None);
        };
        let duration = match (frame.duration, self.pending.front(), &self.source) {
            (Some(duration), ..) => duration,
            (None, Some(next), _) if next.pts > frame.pts => next.pts - frame.pts,
            (None, _, Source::Video(video)) => Some(video.end.saturating_sub(frame.pts))
                .filter(|d| !d.is_zero())
                .unwrap_or(video.interval),
            (None, ..) => Duration::ZERO,
        };
        let image = match frame.picture {
            Picture::Yuv(picture) => convert::to_bgra(&picture),
            Picture::Bgra(image) => image,
        };
        let image = if (image.width, image.height) == self.size {
            image
        } else {
            convert::resize(&image, self.size.0, self.size.1)
        };
        Ok(Some(DecodedFrame {
            image,
            pts: frame.pts,
            duration,
        }))
    }
}

fn video_pending(picture: DecodedPicture) -> Pending {
    Pending {
        pts: picture.pts,
        picture: Picture::Yuv(picture.picture),
        duration: None,
    }
}

fn anim_pending(frame: anim::AnimFrame) -> Pending {
    Pending {
        picture: Picture::Bgra(frame.image),
        pts: frame.pts,
        duration: Some(frame.duration),
    }
}

/// Packet-time disagreement beyond which the audio timeline is repaired: a
/// gap is filled with silence, an overlap dropped. Far above timestamp
/// rounding and the Opus pre-skip (which packet times include).
const MAX_JITTER: Duration = Duration::from_millis(40);

/// Decodes a file's audio track to interleaved f32 at a requested rate and
/// channel count. Output is contiguous: each chunk starts where the previous
/// ended. Gaps between packets come out as silence and overlaps are dropped,
/// so callers place samples by count from the first chunk's time.
pub struct AudioDecoder {
    demuxer: Box<dyn Demuxer>,
    track: usize,
    decoder: Box<dyn codec::AudioDecoder>,
    input: AudioSpec,
    output: AudioSpec,
    resampler: Resampler,
    /// Media time of the first sample since opening or seeking.
    anchor: Option<Duration>,
    /// Where the next packet should start, on the packet timeline.
    expect: Option<Duration>,
    /// Resampler warm-up frames still to discard.
    warmup: usize,
    /// Output frames returned since `anchor`.
    emitted: u64,
    /// After a seek: output before this time is dropped.
    skip_to: Option<Duration>,
    eof: bool,
}

impl AudioDecoder {
    /// `Ok(None)` when the file has no audio track quikscope can decode.
    pub fn open(path: &Path, rate: u32, channels: u16) -> Result<Option<Self>> {
        ensure!(
            rate > 0 && channels > 0,
            "Invalid audio output specification"
        );
        let Some(demuxer) =
            container::open_demuxer(path).with_context(|| format!("Opening {}", path.display()))?
        else {
            return Ok(None);
        };
        let Some(track) = audio_track(demuxer.tracks()) else {
            return Ok(None);
        };
        let Track {
            codec,
            kind,
            codec_private,
        } = &demuxer.tracks()[track];
        let TrackKind::Audio {
            rate: track_rate,
            channels: track_channels,
        } = *kind
        else {
            unreachable!("audio_track only finds audio tracks");
        };
        let decoder = codec::audio_decoder(
            *codec,
            codec_private,
            AudioSpec {
                rate: track_rate,
                channels: track_channels,
            },
        )?;
        let input = decoder.spec();
        let output = AudioSpec { rate, channels };
        let resampler = Resampler::new(input, output)?;
        Ok(Some(Self {
            demuxer,
            track,
            decoder,
            input,
            output,
            warmup: resampler.delay(),
            resampler,
            anchor: None,
            expect: None,
            emitted: 0,
            skip_to: None,
            eof: false,
        }))
    }

    /// Continues from `to`: the next chunk starts at `to` (or at the track's
    /// first sample when that is later).
    pub fn seek(&mut self, to: Duration) -> Result<()> {
        self.demuxer.seek(to)?;
        self.decoder.reset();
        self.resampler = Resampler::new(self.input, self.output)?;
        self.warmup = self.resampler.delay();
        self.anchor = None;
        self.expect = None;
        self.emitted = 0;
        self.skip_to = Some(to);
        self.eof = false;
        Ok(())
    }

    /// The next samples and the media time of the first; `Ok(None)` at the
    /// end of the track.
    pub fn next_chunk(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        while !self.eof {
            let output = match self.demuxer.next_packet()? {
                Some(packet) if packet.track == self.track => {
                    let decoded = self.decoder.decode(&packet.data)?;
                    let frames = decoded.len() / usize::from(self.input.channels);
                    let length = packet.duration.unwrap_or_else(|| {
                        Duration::from_secs_f64(frames as f64 / f64::from(self.input.rate))
                    });
                    let input = self.place(decoded, packet.pts, length);
                    self.resampler.process(&input)?
                }
                Some(_) => continue,
                None => {
                    self.eof = true;
                    self.resampler.flush()?
                }
            };
            if let Some(chunk) = self.emit(output) {
                return Ok(Some(chunk));
            }
        }
        Ok(None)
    }

    /// Lines up one packet's samples with the timeline so far: silence for a
    /// gap before it, its overlapping start dropped.
    fn place(&mut self, mut samples: Vec<f32>, pts: Duration, length: Duration) -> Vec<f32> {
        let channels = usize::from(self.input.channels);
        let frames_in = |d: Duration| {
            (d.as_secs_f64() * f64::from(self.input.rate)).round() as usize * channels
        };
        match self.expect {
            None => self.anchor = Some(pts),
            Some(expect) if pts > expect + MAX_JITTER => {
                let mut padded = vec![0.0; frames_in(pts - expect)];
                padded.append(&mut samples);
                samples = padded;
            }
            Some(expect) if pts + MAX_JITTER < expect => {
                let overlap = frames_in(expect - pts).min(samples.len());
                samples.drain(..overlap);
            }
            Some(_) => {}
        }
        self.expect = Some(pts + length);
        samples
    }

    /// Turns resampler output into a chunk, dropping warm-up and anything
    /// before a seek target.
    fn emit(&mut self, mut samples: Vec<f32>) -> Option<(Vec<f32>, Duration)> {
        let channels = usize::from(self.output.channels);
        let warmup = self.warmup.min(samples.len() / channels);
        self.warmup -= warmup;
        samples.drain(..warmup * channels);
        let anchor = self.anchor?;
        let rate = u64::from(self.output.rate);
        let time = |frames: u64| anchor + Duration::from_nanos(frames * 1_000_000_000 / rate);
        let mut pts = time(self.emitted);
        self.emitted += (samples.len() / channels) as u64;
        if let Some(target) = self.skip_to {
            if time(self.emitted) <= target {
                return None;
            }
            self.skip_to = None;
            if target > pts {
                let skip = ((target - pts).as_secs_f64() * rate as f64).round() as usize;
                let skip = (skip * channels).min(samples.len());
                samples.drain(..skip);
                pts = target;
            }
        }
        (!samples.is_empty()).then_some((samples, pts))
    }
}

pub fn thumbnail(path: &Path, max_side: u32) -> Result<Image> {
    ensure!(max_side > 0, "Thumbnail size must be positive");
    let mut decoder = Decoder::open(path)?;
    let info = decoder.info();
    let scale = (max_side as f64 / info.width.max(info.height) as f64).min(1.0);
    let size = (
        ((info.width as f64 * scale).round() as u32).max(1),
        ((info.height as f64 * scale).round() as u32).max(1),
    );
    let middle = info.duration / 2;
    decoder.set_output_size(size.0, size.1);
    if middle > Duration::ZERO {
        decoder.seek(middle)?;
    }
    decoder
        .next_frame()?
        .map(|frame| frame.image)
        .context("Media has no decodable frames")
}
