//! GIF: the gif crate for the container, quantette for palettes.
//!
//! Encoding quantizes every frame to its own palette (Wu's method with
//! Floyd–Steinberg dithering) and only codes the rectangle that changed
//! since the previous frame, with unchanged pixels inside it transparent,
//! which keeps screen recordings small. Delays are whole centiseconds,
//! rounded on the cumulative timeline so long animations keep their length.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ::gif::{ColorOutput, DecodeOptions, DisposalMethod, Encoder, Frame, Repeat};
use anyhow::{Context, Result, ensure};
use quantette::deps::palette::Srgb;
use quantette::{ImageRef, PaletteSize, Pipeline, QuantizeMethod};

use super::{
    AnimDecoder, AnimEncoder, AnimFrame, AnimInfo, TickClock, browser_delay, changed_rect, clear,
};
use crate::geom::IRect;
use crate::raster::Image;

/// Pixels this transparent or more become the GIF's transparent index.
const ALPHA_THRESHOLD: u8 = 128;
/// Browsers play shorter delays at 100 ms, so frames that would be shorter
/// are dropped and their time given to the frame before.
const MIN_DELAY_CS: u64 = 2;

pub struct GifDecoder {
    path: PathBuf,
    info: AnimInfo,
    decoder: ::gif::Decoder<BufReader<File>>,
    canvas: Image,
    /// What a "restore to previous" frame puts back: the canvas before it.
    restore: Option<(IRect, Image)>,
    /// Clears this rectangle before the next frame ("restore to background").
    clear: Option<IRect>,
    pts: Duration,
}

fn open(path: &Path, skip_pixels: bool) -> Result<::gif::Decoder<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut options = DecodeOptions::new();
    options.set_color_output(ColorOutput::RGBA);
    options.skip_frame_decoding(skip_pixels);
    options
        .read_info(BufReader::new(file))
        .with_context(|| format!("reading GIF {}", path.display()))
}

impl GifDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        // Frame count and length come from the frame headers alone.
        let mut probe = open(path, true)?;
        let (mut frames, mut duration) = (0u64, Duration::ZERO);
        while let Some(frame) = probe.read_next_frame()? {
            frames += 1;
            duration += frame_delay(frame.delay);
        }
        ensure!(frames > 0, "GIF {} has no frames", path.display());
        let decoder = open(path, false)?;
        let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
        Ok(GifDecoder {
            path: path.to_owned(),
            info: AnimInfo {
                width,
                height,
                frames,
                duration: if frames == 1 {
                    Duration::ZERO
                } else {
                    duration
                },
            },
            decoder,
            canvas: Image::new(width, height),
            restore: None,
            clear: None,
            pts: Duration::ZERO,
        })
    }
}

fn frame_delay(centiseconds: u16) -> Duration {
    browser_delay(Duration::from_millis(u64::from(centiseconds) * 10))
}

impl AnimDecoder for GifDecoder {
    fn info(&self) -> &AnimInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<AnimFrame>> {
        if let Some(rect) = self.clear.take() {
            clear(&mut self.canvas, rect);
        }
        if let Some((rect, saved)) = self.restore.take() {
            self.canvas.blit(&saved, rect.x, rect.y);
        }
        let Some(frame) = self.decoder.read_next_frame()? else {
            return Ok(None);
        };
        let rect = IRect::new(
            i32::from(frame.left),
            i32::from(frame.top),
            i32::from(frame.width),
            i32::from(frame.height),
        );
        match frame.dispose {
            DisposalMethod::Background => self.clear = Some(rect),
            DisposalMethod::Previous => self.restore = Some((rect, self.canvas.crop(rect))),
            DisposalMethod::Keep | DisposalMethod::Any => {}
        }
        // Transparent pixels leave the canvas as it is.
        let width = usize::from(frame.width).max(1);
        for (y, row) in frame.buffer.chunks_exact(width * 4).enumerate() {
            let cy = rect.y + y as i32;
            if cy >= self.canvas.height as i32 {
                break;
            }
            for (x, rgba) in row.as_chunks::<4>().0.iter().enumerate() {
                let cx = rect.x + x as i32;
                if cx >= self.canvas.width as i32 {
                    break;
                }
                if rgba[3] != 0 {
                    let i = (cy as usize * self.canvas.width as usize + cx as usize) * 4;
                    self.canvas.data[i..i + 4].copy_from_slice(&[rgba[2], rgba[1], rgba[0], 255]);
                }
            }
        }
        let duration = if self.info.frames == 1 {
            Duration::ZERO
        } else {
            frame_delay(frame.delay)
        };
        let pts = self.pts;
        self.pts += duration;
        Ok(Some(AnimFrame {
            image: self.canvas.clone(),
            pts,
            duration,
        }))
    }

    fn rewind(&mut self) -> Result<()> {
        self.decoder = open(&self.path, false)?;
        self.canvas = Image::new(self.info.width, self.info.height);
        self.restore = None;
        self.clear = None;
        self.pts = Duration::ZERO;
        Ok(())
    }
}

pub struct GifEncoder {
    encoder: Encoder<BufWriter<File>>,
    width: u32,
    height: u32,
    clock: TickClock,
    /// The source pixels the viewer shows after the last written frame.
    shown: Option<Image>,
    /// The latest frame, written once the next one fixes its delay.
    pending: Option<(Frame<'static>, u64)>,
}

impl GifEncoder {
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        let (w, h) = (
            u16::try_from(width).context("GIF is at most 65535 pixels wide")?,
            u16::try_from(height).context("GIF is at most 65535 pixels high")?,
        );
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut encoder = Encoder::new(BufWriter::new(file), w, h, &[])?;
        encoder.set_repeat(Repeat::Infinite)?;
        Ok(GifEncoder {
            encoder,
            width,
            height,
            clock: TickClock::new(100),
            shown: None,
            pending: None,
        })
    }

    /// Writes the pending frame, shown until centisecond `until`.
    fn flush_pending(&mut self, until: u64, next_clears: bool) -> Result<()> {
        if let Some((mut frame, start)) = self.pending.take() {
            frame.delay =
                (until.saturating_sub(start)).clamp(MIN_DELAY_CS, u64::from(u16::MAX)) as u16;
            // A frame with transparency starts from a cleared canvas.
            frame.dispose = if next_clears {
                DisposalMethod::Background
            } else {
                DisposalMethod::Keep
            };
            self.encoder.write_frame(&frame)?;
        }
        Ok(())
    }
}

impl AnimEncoder for GifEncoder {
    fn push(&mut self, image: &Image, duration: Duration) -> Result<()> {
        ensure!(
            (image.width, image.height) == (self.width, self.height),
            "{}x{} frame for a {}x{} GIF",
            image.width,
            image.height,
            self.width,
            self.height
        );
        let (start, _) = self.clock.advance(duration);
        if let Some((_, pending_start)) = &self.pending
            && start - pending_start < MIN_DELAY_CS
        {
            return Ok(());
        }
        let has_alpha = image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[3] < ALPHA_THRESHOLD);
        // "Restore to background" clears only the frame's own rectangle, so
        // the frame before one with transparency must cover the canvas.
        if has_alpha
            && let (Some((pending, _)), Some(shown)) = (&mut self.pending, &self.shown)
            && (u32::from(pending.width), u32::from(pending.height)) != (self.width, self.height)
        {
            *pending = quantize(shown, shown.bounds(), None);
        }
        let frame = match &self.shown {
            // Unchanged pixels may only stay on screen if nothing is cleared.
            Some(shown) if !has_alpha => match changed_rect(shown, image) {
                Some(rect) => quantize(image, rect, Some(shown)),
                None => unchanged_frame(),
            },
            _ => quantize(image, image.bounds(), None),
        };
        self.flush_pending(start, has_alpha)?;
        self.pending = Some((frame, start));
        self.shown = Some(image.clone());
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        let end = self.clock.now();
        self.flush_pending(end, false)?;
        let mut out = self.encoder.into_inner()?;
        out.flush()?;
        Ok(())
    }
}

/// One transparent pixel: carries a delay without changing the canvas.
fn unchanged_frame() -> Frame<'static> {
    Frame {
        width: 1,
        height: 1,
        transparent: Some(0),
        palette: Some(vec![0; 3]),
        buffer: Cow::Owned(vec![0]),
        ..Frame::default()
    }
}

/// Quantizes `rect` of `image` to a frame of its own. Pixels equal to
/// `shown` (what the viewer already sees) and transparent pixels become the
/// transparent index.
fn quantize(image: &Image, rect: IRect, shown: Option<&Image>) -> Frame<'static> {
    let (w, h) = (rect.w as usize, rect.h as usize);
    let mut colors = Vec::with_capacity(w * h);
    let mut transparent = Vec::with_capacity(w * h);
    for y in rect.y..rect.bottom() {
        let row = &image.row(y as u32)[rect.x as usize * 4..rect.right() as usize * 4];
        let shown_row =
            shown.map(|s| &s.row(y as u32)[rect.x as usize * 4..rect.right() as usize * 4]);
        for (x, px) in row.as_chunks::<4>().0.iter().enumerate() {
            colors.push(Srgb::new(px[2], px[1], px[0]));
            let same = shown_row.is_some_and(|s| s[x * 4..x * 4 + 4] == *px);
            transparent.push(px[3] < ALPHA_THRESHOLD || same);
        }
    }
    let any_transparent = transparent.contains(&true);
    let size = if any_transparent { 255 } else { 256 };
    let pixels = ImageRef::new(rect.w as u32, rect.h as u32, colors.as_slice())
        .expect("rectangle inside the image");
    let indexed = Pipeline::new()
        .palette_size(PaletteSize::from_u16_clamped(size))
        .quantize_method(QuantizeMethod::Wu)
        .parallel(true)
        .input_image(pixels)
        .output_srgb8_indexed_image();
    let (palette, mut indices) = indexed.into_parts();
    let mut rgb: Vec<u8> = palette
        .iter()
        .flat_map(|c| [c.red, c.green, c.blue])
        .collect();
    let key = any_transparent.then(|| {
        // The entry after the colors, or the only entry of an empty palette.
        let key = palette.len() as u8;
        rgb.extend_from_slice(&[0; 3]);
        for (index, clear) in indices.iter_mut().zip(&transparent) {
            if *clear {
                *index = key;
            }
        }
        key
    });
    Frame {
        left: rect.x as u16,
        top: rect.y as u16,
        width: rect.w as u16,
        height: rect.h as u16,
        transparent: key,
        palette: Some(rgb),
        buffer: Cow::Owned(indices),
        ..Frame::default()
    }
}
