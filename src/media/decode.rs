//! Reading media through libavformat, libavcodec, swscale and swresample.

use super::{Kind, MediaInfo};
use crate::raster::Image;
use anyhow::{Context as _, Result, bail, ensure};
use ffmpeg::{ChannelLayout, codec, format, frame, media, software};
use ffmpeg_next as ffmpeg;
use std::{collections::VecDeque, path::Path, time::Duration};

fn seconds(value: i64, base: ffmpeg::Rational) -> Duration {
    Duration::from_secs_f64((value as f64 * f64::from(base)).max(0.0))
}

fn next_packet(
    input: &mut format::context::Input,
    stream: usize,
) -> Result<Option<ffmpeg::Packet>> {
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(input) {
            Ok(()) if packet.stream() == stream => return Ok(Some(packet)),
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    super::init();
    let mut input = format::input(path).with_context(|| format!("Opening {}", path.display()))?;
    let stream = input
        .streams()
        .best(media::Type::Video)
        .context("No video stream")?;
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    let mut fps = f64::from(stream.avg_frame_rate());
    let mut duration = if stream.duration() > 0 {
        seconds(stream.duration(), stream.time_base())
    } else {
        Duration::from_secs_f64((input.duration() as f64 / 1_000_000.0).max(0.0))
    };
    let input_format = input.format();
    let name = input_format.name();
    let image = name.split(',').any(|n| {
        n.ends_with("_pipe")
            || matches!(
                n,
                "gif" | "webp" | "webp_anim" | "avif" | "jpegxl_anim" | "jxl" | "apng" | "image2"
            )
    }) || path.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        ["gif", "webp", "avif", "jxl", "apng", "png"]
            .iter()
            .any(|s| x.eq_ignore_ascii_case(s))
    });
    let mut frames = (stream.frames() > 0).then_some(stream.frames() as u64);
    let index = stream.index();
    let base = stream.time_base();
    let origin = if stream.start_time() == ffmpeg::ffi::AV_NOPTS_VALUE {
        0
    } else {
        stream.start_time()
    };
    if image && (frames.is_none() || duration.is_zero()) {
        let mut count = 0_u64;
        let mut end = Duration::ZERO;
        let mut frame = frame::Video::empty();
        let mut drain = |decoder: &mut ffmpeg::decoder::Video| -> Result<()> {
            loop {
                match decoder.receive_frame(&mut frame) {
                    Ok(()) => {
                        count += 1;
                        let at = frame.pts().or(frame.timestamp()).unwrap_or(0) - origin;
                        let ticks = unsafe { (*frame.as_ptr()).duration };
                        end = end.max(seconds(at + ticks.max(0), base));
                    }
                    Err(ffmpeg::Error::Eof) => break,
                    Err(e) if super::encode::again(e) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        };
        while let Some(packet) = next_packet(&mut input, index)? {
            decoder.set_packet_time_base(input.stream(index).unwrap().time_base());
            decoder.send_packet(&packet)?;
            drain(&mut decoder)?;
        }
        decoder.send_eof()?;
        drain(&mut decoder)?;
        frames = Some(count);
        duration = duration.max(end);
        if duration > Duration::ZERO && (!fps.is_finite() || fps <= 0.0) {
            fps = count as f64 / duration.as_secs_f64();
        }
    }
    Ok(MediaInfo {
        width: decoder.width(),
        height: decoder.height(),
        duration,
        fps: if fps.is_finite() { fps } else { 0.0 },
        frames,
        has_audio: input.streams().best(media::Type::Audio).is_some(),
        kind: if image { Kind::Animation } else { Kind::Video },
    })
}

pub struct DecodedFrame {
    pub image: Image,
    pub pts: Duration,
    pub duration: Duration,
}

pub struct Decoder {
    input: format::context::Input,
    path: std::path::PathBuf,
    decoder: ffmpeg::decoder::Video,
    stream: usize,
    base: ffmpeg::Rational,
    origin: i64,
    info: MediaInfo,
    size: (u32, u32),
    scaler: Option<software::scaling::Context>,
    eof: bool,
    pending: VecDeque<DecodedFrame>,
    next_pts: Duration,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let info = probe(path)?;
        let input = format::input(path)?;
        let stream = input
            .streams()
            .best(media::Type::Video)
            .context("No video stream")?;
        let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .video()?;
        decoder.set_packet_time_base(stream.time_base());
        Ok(Self {
            path: path.to_owned(),
            stream: stream.index(),
            base: stream.time_base(),
            origin: if stream.start_time() == ffmpeg::ffi::AV_NOPTS_VALUE {
                0
            } else {
                stream.start_time()
            },
            decoder,
            input,
            size: (info.width, info.height),
            info,
            scaler: None,
            eof: false,
            pending: VecDeque::new(),
            next_pts: Duration::ZERO,
        })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    pub fn set_output_size(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
        self.scaler = None;
        self.pending.clear();
    }

    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let timestamp = (to.as_secs_f64() / f64::from(self.base)) as i64 + self.origin;
        // Backward seeking deliberately lands before the target keyframe.
        let result = if self.info.kind == Kind::Animation {
            -1
        } else {
            unsafe {
                ffmpeg::ffi::av_seek_frame(
                    self.input.as_mut_ptr(),
                    self.stream as i32,
                    timestamp,
                    ffmpeg::ffi::AVSEEK_FLAG_BACKWARD,
                )
            }
        };
        if result < 0 {
            // Animated images may depend on the preceding canvas even when
            // their demuxer accepts seeking. Reopen and reconstruct that canvas.
            let size = self.size;
            *self = Self::open(&self.path)?;
            self.size = size;
        }
        self.decoder.flush();
        self.eof = false;
        self.pending.clear();
        self.next_pts = Duration::ZERO;
        let mut previous = None;
        while let Some(frame) = self.raw_next()? {
            if frame.pts > to {
                if let Some(previous) = previous.take() {
                    self.pending.push_back(previous);
                }
                self.pending.push_back(frame);
                return Ok(());
            }
            previous = Some(frame);
        }
        if let Some(frame) = previous {
            self.pending.push_back(frame);
        }
        Ok(())
    }

    fn raw_next(&mut self) -> Result<Option<DecodedFrame>> {
        loop {
            let mut av = frame::Video::empty();
            match self.decoder.receive_frame(&mut av) {
                Ok(()) => {
                    let pts = av
                        .timestamp()
                        .map(|p| seconds(p - self.origin, self.base))
                        .unwrap_or(self.next_pts);
                    let ticks = unsafe { (*av.as_ptr()).duration };
                    let duration = if ticks > 0 {
                        seconds(ticks, self.base)
                    } else {
                        Duration::from_secs_f64(1.0 / self.info.fps.max(1.0))
                    };
                    self.next_pts = pts + duration;
                    if self.scaler.as_ref().is_none_or(|s| {
                        s.input().format != av.format()
                            || s.input().width != av.width()
                            || s.input().height != av.height()
                    }) {
                        self.scaler = Some(software::scaling::Context::get(
                            av.format(),
                            av.width(),
                            av.height(),
                            format::Pixel::BGRA,
                            self.size.0,
                            self.size.1,
                            software::scaling::flag::Flags::AREA,
                        )?);
                    }
                    let mut bgra = frame::Video::empty();
                    self.scaler.as_mut().unwrap().run(&av, &mut bgra)?;
                    let mut image = Image::new(self.size.0, self.size.1);
                    let stride = image.stride();
                    for y in 0..self.size.1 as usize {
                        image.data[y * stride..(y + 1) * stride].copy_from_slice(
                            &bgra.data(0)[y * bgra.stride(0)..y * bgra.stride(0) + stride],
                        );
                    }
                    return Ok(Some(DecodedFrame {
                        image,
                        pts,
                        duration,
                    }));
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(e) if super::encode::again(e) => {}
                Err(e) => return Err(e.into()),
            }
            if self.eof {
                return Ok(None);
            }
            match next_packet(&mut self.input, self.stream)? {
                Some(packet) => self.decoder.send_packet(&packet)?,
                None => {
                    self.decoder.send_eof()?;
                    self.eof = true;
                }
            }
        }
    }

    pub fn next_frame(&mut self) -> Result<Option<DecodedFrame>> {
        let frame = if let Some(frame) = self.pending.pop_front() {
            Some(frame)
        } else {
            self.raw_next()?
        };
        let Some(mut frame) = frame else {
            return Ok(None);
        };
        if self.pending.is_empty()
            && let Some(next) = self.raw_next()?
        {
            self.pending.push_back(next);
        }
        if let Some(next) = self.pending.front()
            && next.pts > frame.pts
        {
            frame.duration = next.pts - frame.pts;
        }
        Ok(Some(frame))
    }
}

pub struct AudioDecoder {
    input: format::context::Input,
    decoder: ffmpeg::decoder::Audio,
    resampler: software::resampling::Context,
    stream: usize,
    base: ffmpeg::Rational,
    origin: i64,
    rate: u32,
    channels: u16,
    eof: bool,
    drained: bool,
    next_pts: Duration,
    skip_to: Option<Duration>,
}

impl AudioDecoder {
    pub fn open(path: &Path, rate: u32, channels: u16) -> Result<Option<Self>> {
        super::init();
        Self::from_input(format::input(path)?, rate, channels)
    }

    pub(crate) fn from_input(
        input: format::context::Input,
        rate: u32,
        channels: u16,
    ) -> Result<Option<Self>> {
        ensure!(
            rate > 0 && channels > 0,
            "Invalid audio output specification"
        );
        let Some(stream) = input.streams().best(media::Type::Audio) else {
            return Ok(None);
        };
        let decoder = codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        let layout = if decoder.channel_layout().is_empty() {
            ChannelLayout::default(decoder.channels() as i32)
        } else {
            decoder.channel_layout()
        };
        let resampler = software::resampling::Context::get(
            decoder.format(),
            layout,
            decoder.rate(),
            format::Sample::F32(format::sample::Type::Packed),
            ChannelLayout::default(channels as i32),
            rate,
        )?;
        Ok(Some(Self {
            stream: stream.index(),
            base: stream.time_base(),
            origin: if stream.start_time() == ffmpeg::ffi::AV_NOPTS_VALUE {
                0
            } else {
                stream.start_time()
            },
            input,
            decoder,
            resampler,
            rate,
            channels,
            eof: false,
            drained: false,
            next_pts: Duration::ZERO,
            skip_to: None,
        }))
    }

    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let timestamp = (to.as_secs_f64() / f64::from(self.base)) as i64 + self.origin;
        unsafe {
            super::encode::check(ffmpeg::ffi::av_seek_frame(
                self.input.as_mut_ptr(),
                self.stream as i32,
                timestamp,
                ffmpeg::ffi::AVSEEK_FLAG_BACKWARD,
            ))?;
        }
        self.decoder.flush();
        unsafe {
            ffmpeg::ffi::swr_close(self.resampler.as_mut_ptr());
            super::encode::check(ffmpeg::ffi::swr_init(self.resampler.as_mut_ptr()))?;
        }
        self.eof = false;
        self.drained = false;
        self.next_pts = to;
        self.skip_to = Some(to);
        Ok(())
    }

    pub fn next_chunk(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        loop {
            let mut decoded = frame::Audio::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let delay = self.resampler.delay().map_or(Duration::ZERO, |d| {
                        Duration::from_secs_f64(d.output.max(0) as f64 / self.rate as f64)
                    });
                    let pts = decoded
                        .timestamp()
                        .map(|p| seconds(p - self.origin, self.base))
                        .unwrap_or(self.next_pts)
                        .saturating_sub(delay);
                    let output = super::encode::resample(&mut self.resampler, &decoded)?;
                    let result = self.samples(&output, pts);
                    if result.0.is_empty() {
                        continue;
                    }
                    return Ok(Some(result));
                }
                Err(ffmpeg::Error::Eof) => {
                    if self.drained {
                        return Ok(None);
                    }
                    let Some(delay) = self.resampler.delay() else {
                        self.drained = true;
                        return Ok(None);
                    };
                    let mut output = frame::Audio::new(
                        format::Sample::F32(format::sample::Type::Packed),
                        delay.output.max(32) as usize,
                        ChannelLayout::default(self.channels as i32),
                    );
                    let delay = self.resampler.flush(&mut output)?;
                    self.drained = delay.is_none();
                    let result = self.samples(&output, self.next_pts);
                    if !result.0.is_empty() {
                        return Ok(Some(result));
                    }
                    return Ok(None);
                }
                Err(e) if super::encode::again(e) => {}
                Err(e) => return Err(e.into()),
            }
            if self.eof {
                return Ok(None);
            }
            match next_packet(&mut self.input, self.stream)? {
                Some(packet) => self.decoder.send_packet(&packet)?,
                None => {
                    self.decoder.send_eof()?;
                    self.eof = true;
                }
            }
        }
    }

    fn samples(&mut self, output: &frame::Audio, pts: Duration) -> (Vec<f32>, Duration) {
        let count = output.samples() * self.channels as usize;
        let mut samples = if count == 0 {
            Vec::new()
        } else {
            // Packed f32 has samples × channels initialized values in plane zero.
            unsafe {
                std::slice::from_raw_parts(output.data(0).as_ptr().cast::<f32>(), count).to_vec()
            }
        };
        self.next_pts = pts + Duration::from_secs_f64(output.samples() as f64 / self.rate as f64);
        if let Some(target) = self.skip_to {
            if self.next_pts <= target {
                return (Vec::new(), self.next_pts);
            }
            self.skip_to = None;
            let skip = ((target.saturating_sub(pts).as_secs_f64() * self.rate as f64).round()
                as usize)
                .min(output.samples())
                * self.channels as usize;
            samples.drain(..skip);
            return (samples, pts.max(target));
        }
        (samples, pts)
    }
}

pub fn thumbnail(path: &Path, max_side: u32) -> Result<Image> {
    ensure!(max_side > 0, "Thumbnail size must be positive");
    let mut decoder = Decoder::open(path)?;
    let info = decoder.info();
    let scale = (max_side as f64 / info.width.max(info.height) as f64).min(1.0);
    let size = (
        (info.width as f64 * scale).round() as u32,
        (info.height as f64 * scale).round() as u32,
    );
    let middle = info.duration / 2;
    decoder.set_output_size(size.0, size.1);
    if middle > Duration::ZERO {
        decoder.seek(middle)?;
    }
    match decoder.next_frame()? {
        Some(frame) => Ok(frame.image),
        None => bail!("Media has no decodable frames"),
    }
}
