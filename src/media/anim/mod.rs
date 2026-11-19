//! Animated images without FFmpeg: GIF, WebP, AVIF and JPEG XL in both
//! directions, APNG/PNG for reading, and the lossless recording master.
//!
//! Frames keep their own durations end to end: decoders report each frame's
//! display time, encoders take one per frame, so variable-delay animations
//! survive a decode/annotate/encode round trip unchanged (quantized only to
//! the destination format's precision, e.g. 10 ms for GIF).

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use super::OutputFormat;
use crate::geom::IRect;
use crate::raster::Image;

pub mod avif;
pub mod gif;
pub mod jxl;
pub mod master;
pub mod png;
pub mod webp;

#[cfg(test)]
mod tests;

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
    let mut head = Vec::with_capacity(256);
    std::fs::File::open(path)
        .and_then(|file| file.take(256).read_to_end(&mut head))
        .with_context(|| format!("reading {}", path.display()))?;
    let decoder: Box<dyn AnimDecoder> = match sniff(&head) {
        Some(Kind::Gif) => Box::new(gif::GifDecoder::open(path)?),
        Some(Kind::Webp) => Box::new(webp::WebpDecoder::open(path)?),
        Some(Kind::Avif) => Box::new(avif::AvifDecoder::open(path)?),
        Some(Kind::Jxl) => Box::new(jxl::JxlDecoder::open(path)?),
        Some(Kind::Png) => Box::new(png::PngDecoder::open(path)?),
        Some(Kind::Master) => Box::new(master::MasterReader::open(path)?),
        None => return Ok(None),
    };
    Ok(Some(decoder))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Gif,
    Webp,
    Avif,
    Jxl,
    Png,
    Master,
}

/// Recognises a file by its first bytes (at least 256 when the file has
/// them, so an ISOBMFF `ftyp` box with many brands fits).
fn sniff(head: &[u8]) -> Option<Kind> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JXL_CONTAINER: &[u8] = b"\0\0\0\x0cJXL \r\n\x87\n";
    if head.starts_with(b"GIF8") {
        Some(Kind::Gif)
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        Some(Kind::Webp)
    } else if head.starts_with(PNG) {
        Some(Kind::Png)
    } else if head.starts_with(&[0xff, 0x0a]) || head.starts_with(JXL_CONTAINER) {
        Some(Kind::Jxl)
    } else if master::has_magic(head) {
        Some(Kind::Master)
    } else if is_avif(head) {
        Some(Kind::Avif)
    } else {
        None
    }
}

/// An ISOBMFF `ftyp` box naming `avif` or `avis` as major or compatible
/// brand.
fn is_avif(head: &[u8]) -> bool {
    if head.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let size = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
    let Some(body) = head.get(8..size.min(head.len())) else {
        return false;
    };
    // Major brand, minor version (skipped), compatible brands.
    let major = body.get(..4).into_iter();
    let compatible = body.get(8..).unwrap_or_default().chunks_exact(4);
    major
        .chain(compatible)
        .any(|brand| brand == b"avif" || brand == b"avis")
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
    check_size(format, width, height)?;
    Ok(match format {
        OutputFormat::Gif => Box::new(gif::GifEncoder::create(path, width, height)?),
        OutputFormat::Webp => Box::new(webp::WebpEncoder::create(path, width, height)?),
        OutputFormat::Avif => Box::new(avif::AvifEncoder::create(path, width, height)?),
        OutputFormat::Jxl => Box::new(jxl::JxlEncoder::create(path, width, height)?),
        OutputFormat::Mp4 | OutputFormat::Mkv | OutputFormat::Mov => {
            bail!("{format:?} is not an animation format")
        }
    })
}

/// Whether [`create_encoder`] takes a `width`×`height` animation in
/// `format`, without starting an encoder: a recording checks this before it
/// starts, so a size the final format cannot hold fails at once instead of
/// after the whole recording.
pub fn check_size(format: OutputFormat, width: u32, height: u32) -> Result<()> {
    let (min, max) = match format {
        // Sizes are 16-bit fields.
        OutputFormat::Gif => (1, u32::from(u16::MAX)),
        // libwebp's limit.
        OutputFormat::Webp => (1, 16383),
        // rav1e refuses video (as opposed to still) pictures below 16.
        OutputFormat::Avif => (16, u32::from(u16::MAX)),
        // The encoder's group layout needs 2 pixels each way; 2^30 is the
        // codestream's limit.
        OutputFormat::Jxl => (2, 1 << 30),
        OutputFormat::Mp4 | OutputFormat::Mkv | OutputFormat::Mov => {
            bail!("{format:?} is not an animation format")
        }
    };
    ensure!(
        (min..=max).contains(&width) && (min..=max).contains(&height),
        "{} animations are {min} to {max} pixels each way, not {width}x{height}",
        format.label()
    );
    Ok(())
}

/// Converts encoder input durations to a format's tick rate without letting
/// rounding errors add up: a frame covers the ticks between its rounded
/// start and its rounded end on the shared timeline.
struct TickClock {
    ticks_per_second: u64,
    elapsed: Duration,
}

impl TickClock {
    fn new(ticks_per_second: u64) -> TickClock {
        TickClock {
            ticks_per_second,
            elapsed: Duration::ZERO,
        }
    }

    /// Rounded tick at which the next frame starts.
    fn now(&self) -> u64 {
        self.ticks(self.elapsed)
    }

    /// Moves past a frame shown for `duration`; returns its start and end
    /// tick.
    fn advance(&mut self, duration: Duration) -> (u64, u64) {
        let start = self.now();
        self.elapsed += duration;
        (start, self.now())
    }

    fn ticks(&self, at: Duration) -> u64 {
        let half = 500_000_000;
        ((at.as_nanos() * u128::from(self.ticks_per_second) + half) / 1_000_000_000) as u64
    }
}

/// Display time of a GIF or APNG frame as browsers show it: anything up to
/// 10 ms (often 0, meaning "as fast as possible") plays at 100 ms.
fn browser_delay(delay: Duration) -> Duration {
    if delay <= Duration::from_millis(10) {
        Duration::from_millis(100)
    } else {
        delay
    }
}

/// Converts 8-bit gray, gray+alpha, RGB or RGBA samples to BGRA.
fn to_bgra(src: &[u8], channels: usize, dst: &mut [u8]) {
    let pixels = dst.chunks_exact_mut(4).zip(src.chunks_exact(channels));
    match channels {
        1 => pixels.for_each(|(d, s)| d.copy_from_slice(&[s[0], s[0], s[0], 255])),
        2 => pixels.for_each(|(d, s)| d.copy_from_slice(&[s[0], s[0], s[0], s[1]])),
        3 => pixels.for_each(|(d, s)| d.copy_from_slice(&[s[2], s[1], s[0], 255])),
        4 => pixels.for_each(|(d, s)| d.copy_from_slice(&[s[2], s[1], s[0], s[3]])),
        _ => unreachable!("{channels} channels"),
    }
}

/// Makes the part of `rect` inside `image` transparent black (what GIF and
/// APNG "dispose to background" mean in browsers).
fn clear(image: &mut Image, rect: IRect) {
    let Some(r) = rect.intersect(&image.bounds()) else {
        return;
    };
    let stride = image.stride();
    for y in r.y..r.bottom() {
        let start = y as usize * stride + r.x as usize * 4;
        image.data[start..start + r.w as usize * 4].fill(0);
    }
}

/// Unchanged rows that still join two changed regions into one band:
/// coding a few unchanged rows is cheaper than another frame header.
const BAND_GAP: i32 = 32;

/// Row bands, each narrowed to its changed columns, that together cover
/// every pixel differing between two same-sized images (top to bottom;
/// none when the images are equal).
fn changed_bands(a: &Image, b: &Image) -> Vec<IRect> {
    debug_assert_eq!((a.width, a.height), (b.width, b.height));
    let mut bands: Vec<IRect> = Vec::new();
    for y in 0..a.height {
        let (ra, rb) = (a.row(y), b.row(y));
        if ra == rb {
            continue;
        }
        let differs = |x: &u32| {
            let i = *x as usize * 4;
            ra[i..i + 4] != rb[i..i + 4]
        };
        let left = (0..a.width).find(differs).unwrap_or(0);
        let right = (left..a.width)
            .rev()
            .find(differs)
            .map_or(a.width, |x| x + 1);
        let row = IRect::new(left as i32, y as i32, (right - left) as i32, 1);
        match bands.last_mut() {
            Some(band) if row.y - band.bottom() < BAND_GAP => *band = band.union(&row),
            _ => bands.push(row),
        }
    }
    bands
}

/// Bounding box of the pixels that differ between two same-sized images,
/// `None` when they are equal.
fn changed_rect(a: &Image, b: &Image) -> Option<IRect> {
    changed_bands(a, b).into_iter().reduce(|a, b| a.union(&b))
}
