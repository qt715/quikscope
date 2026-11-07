//! Owned BGRA8 pixel buffers and the pixel operations the app needs.
//!
//! BGRA is the byte order of `wl_shm` `ARGB8888`/`XRGB8888` on little-endian
//! machines and the order GPUI's `RenderImage` expects, so a capture travels
//! from the compositor to the GPU without channel swizzling.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};

use crate::geom::IRect;

/// BGRA8 image, rows packed with no padding (`stride == width * 4`).
/// Alpha is straight (not premultiplied); captures are always opaque.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

/// Wayland output transform (the `wl_output.transform` enum).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transform {
    #[default]
    Normal,
    Rot90,
    Rot180,
    Rot270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    pub fn from_wl(value: u32) -> Transform {
        match value {
            1 => Transform::Rot90,
            2 => Transform::Rot180,
            3 => Transform::Rot270,
            4 => Transform::Flipped,
            5 => Transform::Flipped90,
            6 => Transform::Flipped180,
            7 => Transform::Flipped270,
            _ => Transform::Normal,
        }
    }

    /// The transform that undoes this one. Flipped variants are involutions.
    pub fn invert(self) -> Transform {
        match self {
            Transform::Rot90 => Transform::Rot270,
            Transform::Rot270 => Transform::Rot90,
            other => other,
        }
    }

    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Transform::Rot90 | Transform::Rot270 | Transform::Flipped90 | Transform::Flipped270
        )
    }
}

/// PNG compression presets. `Fast` is for latency-sensitive paths (clipboard).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    Fast,
    Balanced,
}

const RAW_MAGIC: &[u8; 8] = b"QSCOPE01";

impl Image {
    pub fn new(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            data: vec![0; width as usize * height as usize * 4],
        }
    }

    pub fn filled(width: u32, height: u32, bgra: [u8; 4]) -> Image {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width as usize * height as usize {
            data.extend_from_slice(&bgra);
        }
        Image {
            width,
            height,
            data,
        }
    }

    pub fn from_bgra(width: u32, height: u32, data: Vec<u8>) -> Result<Image> {
        ensure!(
            data.len() == width as usize * height as usize * 4,
            "BGRA buffer of {} bytes does not match {width}x{height}",
            data.len()
        );
        Ok(Image {
            width,
            height,
            data,
        })
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width as i32, self.height as i32)
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }

    pub fn row(&self, y: u32) -> &[u8] {
        let s = self.stride();
        &self.data[y as usize * s..(y as usize + 1) * s]
    }

    /// Copies the part of `rect` that lies inside the image.
    pub fn crop(&self, rect: IRect) -> Image {
        let Some(r) = rect.intersect(&self.bounds()) else {
            return Image::new(0, 0);
        };
        let mut out = Vec::with_capacity(r.w as usize * r.h as usize * 4);
        let s = self.stride();
        for y in r.y..r.bottom() {
            let start = y as usize * s + r.x as usize * 4;
            out.extend_from_slice(&self.data[start..start + r.w as usize * 4]);
        }
        Image {
            width: r.w as u32,
            height: r.h as u32,
            data: out,
        }
    }

    /// Copies `src` with its top-left corner at (`dx`, `dy`), clipping both sides.
    pub fn blit(&mut self, src: &Image, dx: i32, dy: i32) {
        let dst = IRect::new(dx, dy, src.width as i32, src.height as i32);
        let Some(r) = dst.intersect(&self.bounds()) else {
            return;
        };
        let (ds, ss) = (self.stride(), src.stride());
        let n = r.w as usize * 4;
        for y in r.y..r.bottom() {
            let sy = (y - dy) as usize;
            let sx = (r.x - dx) as usize;
            let d = y as usize * ds + r.x as usize * 4;
            let s = sy * ss + sx * 4;
            self.data[d..d + n].copy_from_slice(&src.data[s..s + n]);
        }
    }

    /// Bilinear resample to `w`×`h`. Used to compose outputs with different
    /// scales into one image; same-size calls return a clone.
    pub fn resize(&self, w: u32, h: u32) -> Image {
        if w == self.width && h == self.height {
            return self.clone();
        }
        if w == 0 || h == 0 || self.width == 0 || self.height == 0 {
            return Image::new(w, h);
        }
        let mut out = Image::new(w, h);
        let sx = self.width as f32 / w as f32;
        let sy = self.height as f32 / h as f32;
        let (mw, mh) = (self.width - 1, self.height - 1);
        for y in 0..h {
            let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
            let y0 = (fy as u32).min(mh);
            let y1 = (y0 + 1).min(mh);
            let ty = fy - y0 as f32;
            for x in 0..w {
                let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
                let x0 = (fx as u32).min(mw);
                let x1 = (x0 + 1).min(mw);
                let tx = fx - x0 as f32;
                let (a, b, c, d) = (
                    self.pixel(x0, y0),
                    self.pixel(x1, y0),
                    self.pixel(x0, y1),
                    self.pixel(x1, y1),
                );
                let o = (y as usize * w as usize + x as usize) * 4;
                for ch in 0..4 {
                    let top = a[ch] as f32 + (b[ch] as f32 - a[ch] as f32) * tx;
                    let bottom = c[ch] as f32 + (d[ch] as f32 - c[ch] as f32) * tx;
                    out.data[o + ch] = (top + (bottom - top) * ty).round() as u8;
                }
            }
        }
        out
    }

    /// Box-filtered downscale so the longest side is at most `max_side`.
    /// Cheap enough for notification thumbnails of 8K captures.
    pub fn thumbnail(&self, max_side: u32) -> Image {
        let longest = self.width.max(self.height);
        if longest <= max_side || longest == 0 {
            return self.clone();
        }
        let factor = longest.div_ceil(max_side).max(1);
        let w = (self.width / factor).max(1);
        let h = (self.height / factor).max(1);
        let mut out = Image::new(w, h);
        let n = factor * factor;
        for ty in 0..h {
            for tx in 0..w {
                let mut acc = [0u32; 4];
                for y in ty * factor..(ty + 1) * factor {
                    let row = self.row(y);
                    for x in tx * factor..(tx + 1) * factor {
                        let i = x as usize * 4;
                        for ch in 0..4 {
                            acc[ch] += row[i + ch] as u32;
                        }
                    }
                }
                let o = (ty as usize * w as usize + tx as usize) * 4;
                for (dst, sum) in out.data[o..o + 4].iter_mut().zip(acc) {
                    *dst = (sum / n) as u8;
                }
            }
        }
        out
    }

    /// Mosaic of `block`-sized averages over the whole image. Unlike blur,
    /// mosaic of a large enough block cannot be reversed to recover text.
    pub fn pixelate(&self, block: u32) -> Image {
        let block = block.max(1);
        let mut out = self.clone();
        let mut by = 0;
        while by < self.height {
            let bh = block.min(self.height - by);
            let mut bx = 0;
            while bx < self.width {
                let bw = block.min(self.width - bx);
                let mut acc = [0u32; 4];
                for y in by..by + bh {
                    let row = self.row(y);
                    for x in bx..bx + bw {
                        let i = x as usize * 4;
                        for ch in 0..4 {
                            acc[ch] += row[i + ch] as u32;
                        }
                    }
                }
                let n = bw * bh;
                let avg = acc.map(|v| (v / n) as u8);
                for y in by..by + bh {
                    let start = (y as usize * self.width as usize + bx as usize) * 4;
                    for px in out.data[start..start + bw as usize * 4]
                        .as_chunks_mut::<4>()
                        .0
                    {
                        px.copy_from_slice(&avg);
                    }
                }
                bx += block;
            }
            by += block;
        }
        out
    }

    /// Applies `t` as defined by `wl_output.transform`: an optional horizontal
    /// flip followed by a counter-clockwise rotation.
    pub fn transformed(&self, t: Transform) -> Image {
        if t == Transform::Normal {
            return self.clone();
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (ow, oh) = if t.swaps_axes() { (h, w) } else { (w, h) };
        let mut out = vec![0u8; self.data.len()];
        let src = as_u32(&self.data);
        let dst = as_u32_mut(&mut out);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = match t {
                    Transform::Flipped
                    | Transform::Flipped90
                    | Transform::Flipped180
                    | Transform::Flipped270 => (w - 1 - x, y),
                    _ => (x, y),
                };
                // Counter-clockwise rotation of the (possibly flipped) point.
                let (nx, ny) = match t {
                    Transform::Normal | Transform::Flipped => (fx, fy),
                    Transform::Rot90 | Transform::Flipped90 => (fy, w - 1 - fx),
                    Transform::Rot180 | Transform::Flipped180 => (w - 1 - fx, h - 1 - fy),
                    Transform::Rot270 | Transform::Flipped270 => (h - 1 - fy, fx),
                };
                dst[ny * ow + nx] = src[y * w + x];
            }
        }
        debug_assert_eq!(ow * oh * 4, out.len());
        Image {
            width: ow as u32,
            height: oh as u32,
            data: out,
        }
    }

    pub fn flip_vertical(&mut self) {
        let s = self.stride();
        let h = self.height as usize;
        for y in 0..h / 2 {
            let (top, bottom) = self.data.split_at_mut((h - 1 - y) * s);
            top[y * s..(y + 1) * s].swap_with_slice(&mut bottom[..s]);
        }
    }

    pub fn is_opaque(&self) -> bool {
        self.data.as_chunks::<4>().0.iter().all(|px| px[3] == 255)
    }

    /// Straight-alpha RGBA copy (for D-Bus image hints and `image` interop).
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = self.data.clone();
        for px in out.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        out
    }

    pub fn from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Result<Image> {
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Image::from_bgra(width, height, rgba)
    }

    /// Encodes as PNG. Opaque images are written as RGB, which is a quarter
    /// smaller and faster to compress than RGBA.
    pub fn encode_png(&self, compression: Compression) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.data.len() / 4);
        self.write_png(&mut out, compression)?;
        Ok(out)
    }

    pub fn write_png(&self, w: impl Write, compression: Compression) -> Result<()> {
        let opaque = self.is_opaque();
        let mut enc = png::Encoder::new(w, self.width, self.height);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_color(if opaque {
            png::ColorType::Rgb
        } else {
            png::ColorType::Rgba
        });
        match compression {
            Compression::Fast => {
                enc.set_compression(png::Compression::Fastest);
                enc.set_filter(png::Filter::Up);
            }
            Compression::Balanced => {
                enc.set_compression(png::Compression::Fast);
                enc.set_filter(png::Filter::Adaptive);
            }
        }
        let mut writer = enc.write_header()?;
        let channels = if opaque { 3 } else { 4 };
        let mut stream = writer.stream_writer()?;
        let mut row = vec![0u8; self.width as usize * channels];
        for y in 0..self.height {
            for (dst, src) in row
                .chunks_exact_mut(channels)
                .zip(self.row(y).as_chunks::<4>().0)
            {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                if channels == 4 {
                    dst[3] = src[3];
                }
            }
            stream.write_all(&row)?;
        }
        stream.finish()?;
        Ok(())
    }

    /// Writes the image to `path` atomically (temp file + rename) so a crash
    /// mid-write never leaves a truncated screenshot behind.
    pub fn save_png(&self, path: &Path, compression: Compression) -> Result<()> {
        let dir = path.parent().context("save path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        write_atomic(path, |file| {
            let mut w = BufWriter::with_capacity(1 << 20, file);
            self.write_png(&mut w, compression)?;
            w.flush()?;
            Ok(())
        })
        .with_context(|| format!("writing {}", path.display()))
    }

    /// Decodes PNG/JPEG/WebP into BGRA.
    pub fn decode(bytes: &[u8]) -> Result<Image> {
        let img = image::load_from_memory(bytes).context("decoding image")?;
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        Image::from_rgba(w, h, rgba.into_raw())
    }

    pub fn load(path: &Path) -> Result<Image> {
        if path.extension().is_some_and(|e| e == "raw") {
            return Image::read_raw(path).map(|(img, _)| img);
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Image::decode(&bytes).with_context(|| format!("decoding {}", path.display()))
    }

    /// Uncompressed cache format for "open last": a header plus the BGRA
    /// bytes, so reloading after a daemon restart is a single read with no
    /// decode step.
    pub fn write_raw(&self, path: &Path, scale: f32) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_atomic(path, |mut f| {
            let mut header = Vec::with_capacity(20);
            header.extend_from_slice(RAW_MAGIC);
            header.extend_from_slice(&self.width.to_le_bytes());
            header.extend_from_slice(&self.height.to_le_bytes());
            header.extend_from_slice(&scale.to_le_bytes());
            f.write_all(&header)?;
            f.write_all(&self.data)?;
            Ok(())
        })
    }

    pub fn read_raw(path: &Path) -> Result<(Image, f32)> {
        let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut header = [0u8; 20];
        f.read_exact(&mut header)?;
        if &header[..8] != RAW_MAGIC {
            bail!("{} is not a quikscope cache file", path.display());
        }
        let w = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let h = u32::from_le_bytes(header[12..16].try_into().unwrap());
        let scale = f32::from_le_bytes(header[16..20].try_into().unwrap());
        let len = w as usize * h as usize * 4;
        let mut data = Vec::with_capacity(len);
        f.take(len as u64 + 1).read_to_end(&mut data)?;
        ensure!(
            data.len() == len,
            "{} is truncated or has trailing data",
            path.display()
        );
        Ok((Image::from_bgra(w, h, data)?, scale))
    }
}

/// Writes `path` through a private temp file and a rename, so readers see
/// either the old or the complete new file. The temp file is removed when
/// `write` fails.
fn write_atomic(path: &Path, write: impl FnOnce(File) -> Result<()>) -> Result<()> {
    let tmp = temp_path(path);
    let result = File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))
        .and_then(write)
        .and_then(|()| std::fs::rename(&tmp, path).map_err(Into::into));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A temp-file name next to `path` that no other writer (thread or
/// process) uses, so concurrent atomic writes never share a partial file.
fn temp_path(path: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.part", std::process::id()));
    path.with_file_name(name)
}

fn as_u32(bytes: &[u8]) -> &[u32] {
    // SAFETY: u32 has no invalid bit patterns; `align_to` only yields the
    // aligned middle part, and Vec<u8> allocations from the global allocator
    // are at least 4-aligned for these sizes. The assert guards the rest.
    let (pre, mid, post) = unsafe { bytes.align_to::<u32>() };
    assert!(pre.is_empty() && post.is_empty(), "unaligned pixel buffer");
    mid
}

fn as_u32_mut(bytes: &mut [u8]) -> &mut [u32] {
    // SAFETY: see `as_u32`.
    let (pre, mid, post) = unsafe { bytes.align_to_mut::<u32>() };
    assert!(pre.is_empty() && post.is_empty(), "unaligned pixel buffer");
    mid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(w: u32, h: u32) -> Image {
        let mut img = Image::new(w, h);
        for (i, px) in img.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [i as u8, (i >> 8) as u8, 7, 255];
        }
        img
    }

    #[test]
    fn crop_clips_to_bounds() {
        let img = numbered(4, 3);
        let c = img.crop(IRect::new(2, 1, 10, 10));
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(c.pixel(0, 0), img.pixel(2, 1));
        assert_eq!(c.pixel(1, 1), img.pixel(3, 2));
        assert_eq!(img.crop(IRect::new(9, 9, 2, 2)).width, 0);
    }

    #[test]
    fn blit_clips_negative_offsets() {
        let mut dst = Image::new(3, 3);
        let src = numbered(2, 2);
        dst.blit(&src, -1, 2);
        assert_eq!(dst.pixel(0, 2), src.pixel(1, 0));
        assert_eq!(dst.pixel(1, 2), [0, 0, 0, 0]);
    }

    #[test]
    fn every_transform_is_undone_by_its_inverse() {
        let img = numbered(5, 3);
        for v in 0..8 {
            let t = Transform::from_wl(v);
            let back = img.transformed(t).transformed(t.invert());
            assert_eq!(back, img, "{t:?}");
        }
    }

    #[test]
    fn rot90_is_counter_clockwise() {
        // 2x1 image [A B] rotated 90° CCW becomes a column with B on top.
        let mut img = Image::new(2, 1);
        img.data[0] = 1;
        img.data[4] = 2;
        let r = img.transformed(Transform::Rot90);
        assert_eq!((r.width, r.height), (1, 2));
        assert_eq!(r.pixel(0, 0)[0], 2);
        assert_eq!(r.pixel(0, 1)[0], 1);
    }

    #[test]
    fn flip_vertical_swaps_rows() {
        let mut img = numbered(2, 3);
        let orig = img.clone();
        img.flip_vertical();
        assert_eq!(img.pixel(1, 0), orig.pixel(1, 2));
        assert_eq!(img.pixel(1, 1), orig.pixel(1, 1));
        img.flip_vertical();
        assert_eq!(img, orig);
    }

    #[test]
    fn resize_preserves_flat_color_and_size() {
        let img = Image::filled(3, 3, [10, 20, 30, 255]);
        let r = img.resize(7, 5);
        assert_eq!((r.width, r.height), (7, 5));
        assert!(
            r.data
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [10, 20, 30, 255])
        );
    }

    #[test]
    fn pixelate_averages_blocks() {
        let mut img = Image::filled(4, 2, [0, 0, 0, 255]);
        img.data[0] = 200; // pixel (0,0) blue channel
        let p = img.pixelate(2);
        assert_eq!(p.pixel(0, 0)[0], 50);
        assert_eq!(p.pixel(1, 1)[0], 50);
        assert_eq!(p.pixel(2, 0)[0], 0, "neighbouring block untouched");
    }

    #[test]
    fn thumbnail_bounds_longest_side() {
        let t = numbered(1000, 300).thumbnail(256);
        assert!(t.width <= 256 && t.height <= 256);
        assert_eq!(t.width, 250);
    }

    #[test]
    fn png_round_trip_rgb_and_rgba() {
        let opaque = numbered(17, 9);
        for c in [Compression::Fast, Compression::Balanced] {
            let png = opaque.encode_png(c).unwrap();
            assert_eq!(Image::decode(&png).unwrap(), opaque);
        }
        let mut translucent = numbered(4, 4);
        translucent.data[3] = 128;
        let png = translucent.encode_png(Compression::Fast).unwrap();
        assert_eq!(Image::decode(&png).unwrap(), translucent);
    }

    #[test]
    fn raw_cache_round_trip_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last.raw");
        let img = numbered(31, 7);
        img.write_raw(&path, 1.5).unwrap();
        let (back, scale) = Image::read_raw(&path).unwrap();
        assert_eq!(back, img);
        assert_eq!(scale, 1.5);

        std::fs::write(&path, b"not a cache file at all").unwrap();
        assert!(Image::read_raw(&path).is_err());
        let mut truncated = RAW_MAGIC.to_vec();
        truncated.extend_from_slice(&[4, 0, 0, 0, 4, 0, 0, 0, 0, 0, 128, 63, 1, 2]);
        std::fs::write(&path, truncated).unwrap();
        assert!(Image::read_raw(&path).is_err());
    }

    #[test]
    fn concurrent_raw_writes_never_mix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last.raw");
        let images: Vec<Image> = (1..=8u8)
            .map(|i| Image::filled(300 + i as u32, 200, [i, i, i, 255]))
            .collect();
        std::thread::scope(|s| {
            for img in &images {
                let path = &path;
                s.spawn(move || {
                    for _ in 0..5 {
                        img.write_raw(path, 1.0).unwrap();
                    }
                });
            }
        });
        let (back, _) = Image::read_raw(&path).unwrap();
        assert!(images.contains(&back), "file is one complete write");
        let entries = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(entries, 1, "no temp files left");
    }
}
