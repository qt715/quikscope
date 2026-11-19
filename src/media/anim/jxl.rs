//! JPEG XL: jxl-oxide for reading, a vendored zune-jpegxl (lossless modular,
//! patched for animation) for writing.
//!
//! Frames are lossless and timed in milliseconds. After the first frame
//! only the bands of rows that changed are coded, each replacing that part
//! of the previous frame, so mostly static screen recordings stay small.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use jxl_oxide::JxlImage;
use zune_core::bit_depth::BitDepth;
use zune_core::colorspace::ColorSpace;
use zune_core::options::EncoderOptions;

use super::{AnimDecoder, AnimEncoder, AnimFrame, AnimInfo, TickClock, changed_bands, to_bgra};
use crate::geom::IRect;
use crate::raster::Image;

// Upstream style and lints are kept to ease updates; see its NOTICE.
#[allow(dead_code, clippy::all)]
#[rustfmt::skip]
#[path = "../../../vendor/zune-jpegxl/src/lib.rs"]
mod zune_jpegxl;

use zune_jpegxl::{JxlAnimation, JxlFrame, JxlSimpleEncoder};

pub struct JxlDecoder {
    image: JxlImage,
    info: AnimInfo,
    /// Display time of each keyframe.
    durations: Vec<Duration>,
    next: usize,
    pts: Duration,
}

impl JxlDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let image = JxlImage::builder()
            .open(path)
            .map_err(|e| anyhow!("reading JPEG XL {}: {e}", path.display()))?;
        ensure!(
            !image.pixel_format().has_black(),
            "CMYK JPEG XL {} is not supported",
            path.display()
        );
        let frames = image.num_loaded_keyframes();
        ensure!(frames > 0, "JPEG XL {} has no frames", path.display());
        let durations: Vec<Duration> = match &image.image_header().metadata.animation {
            Some(animation) if frames > 1 => (0..frames)
                .map(|i| {
                    let ticks = image.frame_header(i).map_or(0, |h| h.duration);
                    let nanos =
                        u128::from(ticks) * u128::from(animation.tps_denominator) * 1_000_000_000
                            / u128::from(animation.tps_numerator.max(1));
                    Duration::from_nanos(nanos as u64)
                })
                .collect(),
            _ => vec![Duration::ZERO; frames],
        };
        let info = AnimInfo {
            width: image.width(),
            height: image.height(),
            frames: frames as u64,
            duration: durations.iter().sum(),
        };
        Ok(JxlDecoder {
            image,
            info,
            durations,
            next: 0,
            pts: Duration::ZERO,
        })
    }
}

impl AnimDecoder for JxlDecoder {
    fn info(&self) -> &AnimInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<AnimFrame>> {
        let i = self.next;
        if i == self.durations.len() {
            return Ok(None);
        }
        let render = self
            .image
            .render_frame(i)
            .map_err(|e| anyhow!("decoding JPEG XL frame {i}: {e}"))?;
        let mut stream = render.stream();
        let (width, height) = (stream.width(), stream.height());
        ensure!(
            (width, height) == (self.info.width, self.info.height),
            "JPEG XL frame {i} is {width}x{height}"
        );
        let channels = stream.channels() as usize;
        ensure!(channels <= 4, "JPEG XL frame with {channels} channels");
        let mut samples = vec![0u8; width as usize * height as usize * channels];
        stream.write_to_buffer(&mut samples);
        let mut image = Image::new(width, height);
        to_bgra(&samples, channels, &mut image.data);
        self.next += 1;
        let duration = self.durations[i];
        let pts = self.pts;
        self.pts += duration;
        Ok(Some(AnimFrame {
            image,
            pts,
            duration,
        }))
    }

    fn rewind(&mut self) -> Result<()> {
        self.next = 0;
        self.pts = Duration::ZERO;
        Ok(())
    }
}

/// Milliseconds as ticks, playing forever.
const ANIMATION: JxlAnimation = JxlAnimation {
    tps_numerator: 1000,
    tps_denominator: 1,
    num_loops: 0,
};

/// One coded frame: an animation frame is one or more of these, all but
/// the last without duration (the decoder composites them into one).
struct Pending {
    /// RGBA pixels of `rect`.
    rgba: Vec<u8>,
    rect: IRect,
    ticks: u32,
}

pub struct JxlEncoder {
    out: BufWriter<File>,
    width: u32,
    height: u32,
    clock: TickClock,
    previous: Option<Image>,
    /// The latest animation frame; the codestream marks its last frame, so
    /// each is written once the next arrives (or at finish).
    pending: Vec<Pending>,
    written: u64,
    threads: u8,
    buf: Vec<u8>,
}

impl JxlEncoder {
    /// The size is within [`super::check_size`] (as `create_encoder` checks).
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8) as u8);
        Ok(JxlEncoder {
            out: BufWriter::new(file),
            width,
            height,
            clock: TickClock::new(1000),
            previous: None,
            pending: Vec::new(),
            written: 0,
            threads,
            buf: Vec::new(),
        })
    }

    fn write(&mut self, frame: Pending, is_last: bool) -> Result<()> {
        let full = IRect::new(0, 0, self.width as i32, self.height as i32);
        let options = EncoderOptions::new(
            frame.rect.w as usize,
            frame.rect.h as usize,
            ColorSpace::RGBA,
            BitDepth::Eight,
        )
        .set_num_threads(self.threads);
        let header = JxlFrame {
            canvas: (self.written == 0).then_some((self.width as usize, self.height as usize)),
            animation: ANIMATION,
            duration: frame.ticks,
            is_last,
            origin: (frame.rect != full).then_some((frame.rect.x as u32, frame.rect.y as u32)),
        };
        self.buf.clear();
        JxlSimpleEncoder::new(&frame.rgba, options)
            .encode_frame(&header, &mut self.buf)
            .map_err(|e| anyhow!("encoding JPEG XL frame: {e:?}"))?;
        self.out.write_all(&self.buf)?;
        self.written += 1;
        Ok(())
    }
}

impl AnimEncoder for JxlEncoder {
    fn push(&mut self, image: &Image, duration: Duration) -> Result<()> {
        ensure!(
            (image.width, image.height) == (self.width, self.height),
            "{}x{} frame for a {}x{} JPEG XL",
            image.width,
            image.height,
            self.width,
            self.height
        );
        let (start, end) = self.clock.advance(duration);
        let ticks = u32::try_from(end - start).context("JPEG XL frame too long")?;
        let rects = match &self.previous {
            None => vec![image.bounds()],
            Some(previous) => {
                let bands = changed_bands(previous, image);
                if bands.is_empty() {
                    // Nothing changed, but the frame still carries its time.
                    vec![IRect::new(0, 0, 2, 2)]
                } else {
                    bands
                        .into_iter()
                        .map(|band| at_least_2x2(band, self.width, self.height))
                        .collect()
                }
            }
        };
        for pending in std::mem::take(&mut self.pending) {
            self.write(pending, false)?;
        }
        let last = rects.len() - 1;
        for (i, rect) in rects.into_iter().enumerate() {
            let mut rgba = image.crop(rect).data;
            for px in rgba.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
            let ticks = if i == last { ticks } else { 0 };
            self.pending.push(Pending { rgba, rect, ticks });
        }
        self.previous = Some(image.clone());
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        let Some(last) = self.pending.pop() else {
            bail!("JPEG XL animation without frames");
        };
        for pending in std::mem::take(&mut self.pending) {
            self.write(pending, false)?;
        }
        self.write(last, true)?;
        self.out.flush()?;
        Ok(())
    }
}

/// Grows `rect` to at least 2x2 inside a canvas of at least 2x2.
fn at_least_2x2(rect: IRect, width: u32, height: u32) -> IRect {
    let grow = |start: i32, len: i32, limit: u32| {
        let len = len.max(2);
        (start.min(limit as i32 - len), len)
    };
    let (x, w) = grow(rect.x, rect.w, width);
    let (y, h) = grow(rect.y, rect.h, height);
    IRect::new(x, y, w, h)
}
