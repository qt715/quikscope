//! PNG and APNG reading with the png crate, which hands out raw sub-frames:
//! compositing (blend and dispose operations) happens here.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ::png::{BlendOp, DisposeOp, Transformations};
use anyhow::{Context, Result, ensure};

use super::{AnimDecoder, AnimFrame, AnimInfo, browser_delay, clear, to_bgra};
use crate::geom::IRect;
use crate::raster::Image;

pub struct PngDecoder {
    path: PathBuf,
    info: AnimInfo,
    reader: ::png::Reader<BufReader<File>>,
    /// Whether the IDAT image is shown as the first frame; when it is not
    /// it is only a fallback for viewers without APNG support.
    default_is_frame: bool,
    animated: bool,
    canvas: Image,
    buf: Vec<u8>,
    sub: Vec<u8>,
    restore: Option<(IRect, Image)>,
    clear: Option<IRect>,
    next: u64,
    pts: Duration,
}

fn open(path: &Path) -> Result<::png::Reader<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = ::png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(Transformations::normalize_to_color8());
    decoder
        .read_info()
        .with_context(|| format!("reading PNG {}", path.display()))
}

impl PngDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let reader = open(path)?;
        let png_info = reader.info();
        let (width, height) = png_info.size();
        let animated = png_info.animation_control.is_some();
        let default_is_frame = !animated || png_info.frame_control.is_some();
        let (frames, duration) = if animated {
            let delays = frame_delays(path)?;
            ensure!(!delays.is_empty(), "APNG {} has no frames", path.display());
            (delays.len() as u64, delays.iter().sum())
        } else {
            (1, Duration::ZERO)
        };
        let len = reader.output_buffer_size().context("PNG image too large")?;
        Ok(PngDecoder {
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
            reader,
            default_is_frame,
            animated,
            canvas: Image::new(width, height),
            buf: vec![0; len],
            sub: Vec::new(),
            restore: None,
            clear: None,
            next: 0,
            pts: Duration::ZERO,
        })
    }
}

/// Display times of the animation frames, read from the `fcTL` chunks
/// without decompressing any image data.
fn frame_delays(path: &Path) -> Result<Vec<Duration>> {
    let mut file = BufReader::new(File::open(path)?);
    file.seek(SeekFrom::Start(8))?;
    let mut delays = Vec::new();
    let mut header = [0; 8];
    while file.read_exact(&mut header).is_ok() {
        let len = u32::from_be_bytes(header[..4].try_into().unwrap());
        match &header[4..] {
            b"fcTL" => {
                let mut fctl = [0; 26];
                ensure!(len == 26, "fcTL chunk of {len} bytes");
                file.read_exact(&mut fctl)?;
                let num = u16::from_be_bytes([fctl[20], fctl[21]]);
                let den = u16::from_be_bytes([fctl[22], fctl[23]]);
                delays.push(apng_delay(num, den));
                file.seek_relative(4)?;
            }
            b"IEND" => break,
            _ => file.seek_relative(i64::from(len) + 4)?,
        }
    }
    Ok(delays)
}

fn apng_delay(num: u16, den: u16) -> Duration {
    let den = if den == 0 { 100 } else { den };
    browser_delay(Duration::from_nanos(
        u64::from(num) * 1_000_000_000 / u64::from(den),
    ))
}

impl AnimDecoder for PngDecoder {
    fn info(&self) -> &AnimInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<AnimFrame>> {
        if self.next == self.info.frames {
            return Ok(None);
        }
        if let Some(rect) = self.clear.take() {
            clear(&mut self.canvas, rect);
        }
        if let Some((rect, saved)) = self.restore.take() {
            self.canvas.blit(&saved, rect.x, rect.y);
        }
        if self.next == 0 && !self.default_is_frame {
            // Skips the IDAT fallback image.
            self.reader.next_frame_info()?;
        }
        let out = self.reader.next_frame(&mut self.buf)?;
        let channels = out.color_type.samples();
        let (w, h) = (out.width as usize, out.height as usize);
        self.sub.resize(w * h * 4, 0);
        for (src, dst) in self
            .buf
            .chunks(out.line_size)
            .take(h)
            .zip(self.sub.chunks_exact_mut(w * 4))
        {
            to_bgra(&src[..w * channels], channels, dst);
        }

        let (rect, blend, dispose, duration) = match self.reader.info().frame_control {
            Some(fc) if self.animated => (
                IRect::new(fc.x_offset as i32, fc.y_offset as i32, w as i32, h as i32),
                fc.blend_op,
                fc.dispose_op,
                apng_delay(fc.delay_num, fc.delay_den),
            ),
            _ => (
                self.canvas.bounds(),
                BlendOp::Source,
                DisposeOp::None,
                Duration::ZERO,
            ),
        };
        match dispose {
            // On the first frame "previous" means the cleared canvas.
            DisposeOp::Previous if self.next > 0 => {
                self.restore = Some((rect, self.canvas.crop(rect)));
            }
            DisposeOp::Background | DisposeOp::Previous => self.clear = Some(rect),
            DisposeOp::None => {}
        }
        let sub = Image {
            width: w as u32,
            height: h as u32,
            data: std::mem::take(&mut self.sub),
        };
        match blend {
            BlendOp::Source => self.canvas.blit(&sub, rect.x, rect.y),
            BlendOp::Over => blend_over(&mut self.canvas, &sub, rect.x, rect.y),
        }
        self.sub = sub.data;

        let duration = if self.info.frames == 1 {
            Duration::ZERO
        } else {
            duration
        };
        self.next += 1;
        let pts = self.pts;
        self.pts += duration;
        Ok(Some(AnimFrame {
            image: self.canvas.clone(),
            pts,
            duration,
        }))
    }

    fn rewind(&mut self) -> Result<()> {
        self.reader = open(&self.path)?;
        self.canvas = Image::new(self.info.width, self.info.height);
        self.restore = None;
        self.clear = None;
        self.next = 0;
        self.pts = Duration::ZERO;
        Ok(())
    }
}

/// Draws straight-alpha `src` over `dst` with its top-left at (`x`, `y`).
fn blend_over(dst: &mut Image, src: &Image, x: i32, y: i32) {
    let Some(r) = IRect::new(x, y, src.width as i32, src.height as i32).intersect(&dst.bounds())
    else {
        return;
    };
    let stride = dst.stride();
    for row in r.y..r.bottom() {
        let s = src.row((row - y) as u32);
        let d = &mut dst.data[row as usize * stride..][..stride];
        for col in r.x..r.right() {
            let sp = &s[(col - x) as usize * 4..][..4];
            let dp = &mut d[col as usize * 4..][..4];
            over(dp, sp);
        }
    }
}

fn over(dst: &mut [u8], src: &[u8]) {
    let sa = u32::from(src[3]);
    if sa == 255 {
        dst.copy_from_slice(src);
        return;
    }
    if sa == 0 {
        return;
    }
    let da = u32::from(dst[3]);
    // Alpha and color weights scaled by 255 * 255.
    let src_w = sa * 255;
    let dst_w = da * (255 - sa);
    let out = src_w + dst_w;
    for c in 0..3 {
        dst[c] = ((u32::from(src[c]) * src_w + u32::from(dst[c]) * dst_w + out / 2) / out) as u8;
    }
    dst[3] = ((out + 127) / 255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_blends_straight_alpha() {
        let mut dst = [0, 0, 255, 255];
        over(&mut dst, &[255, 0, 0, 128]);
        assert_eq!(dst, [128, 0, 127, 255]);

        // Onto transparent, the source color stays as it is.
        let mut dst = [0, 0, 0, 0];
        over(&mut dst, &[10, 20, 30, 40]);
        assert_eq!(dst, [10, 20, 30, 40]);
    }
}
