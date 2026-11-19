//! Media fixtures made with quikscope's own engine, so media tests need no
//! external tools: H.264 + Opus videos in every container and animated
//! images, with known content ([`pattern`], [`index_of`]) and timing.
//!
//! Included as a module by several test crates; each uses part of it.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use quikscope::media::codec::{self, VideoEncoderConfig, opus};
use quikscope::media::container::{self, Codec, Container, Packet, Track, TrackKind};
use quikscope::media::{EncoderPreference, OutputFormat, PixelFormat, anim, convert};
use quikscope::raster::Image;

/// Side of each index bit's block in [`pattern`] (a macroblock).
const BLOCK: u32 = 16;

/// A frame of a moving test pattern (vertical colour bars scrolling with
/// `index`) whose top row of blocks encodes `index` (0–255) in binary, black
/// or white: no lossy coder turns one into the other, and no encoder can
/// mistake the change for noise. At least 128×16.
pub fn pattern(width: u32, height: u32, index: u32) -> Image {
    assert!(width >= 8 * BLOCK && height >= BLOCK && index < 256);
    let mut image = Image::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let bar = ((x + index * 4) / 40) % 6;
            let bgra = match bar {
                0 => [40, 40, 200, 255],
                1 => [40, 200, 40, 255],
                2 => [200, 40, 40, 255],
                3 => [40, 200, 200, 255],
                4 => [200, 40, 200, 255],
                _ => [200, 200, 40, 255],
            };
            let i = (y * width + x) as usize * 4;
            image.data[i..i + 4].copy_from_slice(&bgra);
        }
    }
    for bit in 0..8 {
        let level = if index >> (7 - bit) & 1 == 1 { 255 } else { 0 };
        fill(
            &mut image,
            bit * BLOCK,
            0,
            BLOCK,
            BLOCK,
            [level, level, level, 255],
        );
    }
    image
}

/// The index [`pattern`] drew into `image`.
pub fn index_of(image: &Image) -> u32 {
    (0..8).fold(0, |index, bit| {
        let (cx, cy) = (bit * BLOCK + BLOCK / 2, BLOCK / 2);
        let mut sum = 0;
        for y in cy - 2..cy + 2 {
            for x in cx - 2..cx + 2 {
                sum += u32::from(image.pixel(x, y)[1]);
            }
        }
        index << 1 | u32::from(sum > 128 * 16)
    })
}

/// Paints a `w`×`h` rectangle.
pub fn fill(image: &mut Image, x: u32, y: u32, w: u32, h: u32, bgra: [u8; 4]) {
    for row in y..(y + h).min(image.height) {
        for col in x..(x + w).min(image.width) {
            let i = (row * image.width + col) as usize * 4;
            image.data[i..i + 4].copy_from_slice(&bgra);
        }
    }
}

/// A sine on both channels.
#[derive(Clone, Copy, Debug)]
pub struct Tone {
    pub frequency: f32,
    pub amplitude: f32,
    /// Where it starts on the video's timeline.
    pub start: Duration,
    pub length: Duration,
}

/// A constant-rate clip of [`pattern`] frames.
#[derive(Clone, Copy, Debug)]
pub struct Clip {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u32,
    pub tone: Option<Tone>,
}

impl Clip {
    pub fn duration(&self) -> Duration {
        frame_time(self.frames, self.fps)
    }
}

/// Start of frame `index` at `fps`.
pub fn frame_time(index: u32, fps: u32) -> Duration {
    Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(fps))
}

/// Writes `clip` as H.264 (OpenH264, deterministic) plus Opus when it has a
/// tone, in `format`'s container (MP4 fragmented as recordings are).
pub fn video(path: &Path, format: OutputFormat, clip: &Clip) -> Result<()> {
    let container = match format {
        OutputFormat::Mp4 => Container::Mp4 { fragmented: true },
        OutputFormat::Mov => Container::Mov,
        OutputFormat::Mkv => Container::Mkv,
        _ => anyhow::bail!("{format} is not a video format"),
    };
    let config = VideoEncoderConfig::for_screen(clip.width, clip.height, clip.fps);
    let mut encoder = codec::h264_encoder(&config, EncoderPreference::Software)?;
    let mut opus = clip
        .tone
        .map(|_| opus::Encoder::new(opus::BITRATE))
        .transpose()?;
    let mut tracks = vec![Track {
        codec: Codec::H264,
        kind: TrackKind::Video {
            width: clip.width,
            height: clip.height,
            fps: f64::from(clip.fps),
        },
        codec_private: Vec::new(),
    }];
    if let Some(opus) = &opus {
        tracks.push(Track {
            codec: Codec::Opus,
            kind: TrackKind::Audio {
                rate: opus::RATE,
                channels: 2,
            },
            codec_private: opus.head(),
        });
    }
    let mut mux = container::create_muxer(path, container, &tracks)?;
    let mut audio = match (clip.tone, &mut opus) {
        (Some(tone), Some(encoder)) => tone_packets(tone, encoder)?,
        _ => VecDeque::new(),
    };
    for index in 0..clip.frames {
        let image = pattern(clip.width, clip.height, index % 256);
        let picture = convert::to_yuv420(
            &image.data,
            image.stride(),
            PixelFormat::Bgra,
            clip.width,
            clip.height,
            encoder.input_layout(),
        );
        let pts = frame_time(index, clip.fps);
        for frame in encoder.encode(&picture, pts, false)? {
            mux.write(video_packet(frame, clip))?;
        }
        // Audio up to the picture's time, so the muxer interleaves.
        while audio.front().is_some_and(|packet| packet.pts <= pts) {
            mux.write(audio.pop_front().unwrap())?;
        }
    }
    for frame in encoder.finish()? {
        mux.write(video_packet(frame, clip))?;
    }
    for packet in audio {
        mux.write(packet)?;
    }
    mux.finish()
}

fn video_packet(frame: codec::EncodedFrame, clip: &Clip) -> Packet {
    Packet {
        track: 0,
        data: frame.data,
        pts: frame.pts,
        duration: Some(frame_time(1, clip.fps)),
        keyframe: frame.keyframe,
    }
}

/// 20 ms Opus packets of `tone`, plus the packets that flush the pre-skip.
fn tone_packets(tone: Tone, encoder: &mut opus::Encoder) -> Result<VecDeque<Packet>> {
    let rate = opus::RATE as usize;
    let frames = (tone.length.as_secs_f64() * rate as f64).round() as usize;
    let samples: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let v = (i as f32 * tone.frequency * std::f32::consts::TAU / rate as f32).sin()
                * tone.amplitude;
            [v, v]
        })
        .collect();
    let packets = (frames + usize::from(encoder.pre_skip())).div_ceil(opus::FRAME);
    (0..packets)
        .map(|k| {
            let mut chunk = vec![0.0; opus::FRAME * 2];
            let from = (k * opus::FRAME * 2).min(samples.len());
            let to = ((k + 1) * opus::FRAME * 2).min(samples.len());
            chunk[..to - from].copy_from_slice(&samples[from..to]);
            Ok(Packet {
                track: 1,
                data: encoder.encode(&chunk)?,
                pts: tone.start + Duration::from_millis(20 * k as u64),
                duration: Some(Duration::from_millis(20)),
                keyframe: true,
            })
        })
        .collect()
}

/// Writes `frames` (BGRA, shown for their durations) as an animation.
pub fn animation(path: &Path, format: OutputFormat, frames: &[(Image, Duration)]) -> Result<()> {
    let (width, height) = (frames[0].0.width, frames[0].0.height);
    let mut encoder = anim::create_encoder(format, path, width, height)?;
    for (image, duration) in frames {
        encoder.push(image, *duration)?;
    }
    encoder.finish()
}

/// Checks `path` with `ffprobe` when it is installed: it must read without
/// errors and find `streams` (codec names) in order.
pub fn ffprobe(path: &Path, streams: &[&str]) {
    let Ok(out) = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
    else {
        return;
    };
    let found = String::from_utf8_lossy(&out.stdout);
    let found: Vec<_> = found.lines().map(|l| l.trim_end_matches(',')).collect();
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "ffprobe {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(found, streams, "ffprobe {}", path.display());
}
