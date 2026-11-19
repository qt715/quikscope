//! One captured video frame: CPU pixels plus the moment they were captured.

use std::time::Instant;

use ffmpeg_next as ffmpeg;

use crate::raster::Image;

/// Byte order of a 4-byte pixel. The `x` variants carry no alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

impl PixelFormat {
    pub fn is_opaque(self) -> bool {
        matches!(self, PixelFormat::Bgrx | PixelFormat::Rgbx)
    }
}

/// Pixel storage of a [`Frame`]. `Shared` lets a capture backend lend a
/// buffer from its own pool without copying; dropping the box returns it.
pub enum Pixels {
    Owned(Vec<u8>),
    Shared(Box<dyn AsRef<[u8]> + Send>),
}

impl Pixels {
    fn bytes(&self) -> &[u8] {
        match self {
            Pixels::Owned(v) => v,
            Pixels::Shared(b) => (**b).as_ref(),
        }
    }
}

/// A captured frame: `height` rows of `width` 4-byte pixels, `stride` bytes
/// apart.
pub struct Frame {
    // Transitional: the FFmpeg engine still consumes AVFrames. The FFmpeg-free
    // pipeline replaces this with the `Pixels` it was built from.
    pub(crate) av: ffmpeg::frame::Video,
    /// When the pixels were captured; the session derives timestamps from it.
    pub captured: Instant,
}

// SAFETY: an AVFrame is reference-counted data with no thread affinity; the
// session moves each frame to its encoder thread exactly once.
unsafe impl Send for Frame {}

impl Frame {
    /// Wraps a frame built by a platform source (hardware frames included).
    pub fn from_av(av: ffmpeg::frame::Video, captured: Instant) -> Frame {
        Frame { av, captured }
    }

    /// Takes ownership of (or borrows from a pool) `height` rows of pixels.
    pub fn from_pixels(
        width: u32,
        height: u32,
        stride: usize,
        format: PixelFormat,
        pixels: Pixels,
        captured: Instant,
    ) -> Frame {
        let data = pixels.bytes();
        assert!(stride >= width as usize * 4, "stride shorter than a row");
        assert!(
            data.len() >= stride * (height as usize).saturating_sub(1) + width as usize * 4,
            "pixel buffer shorter than the frame"
        );
        let pixel = match format {
            PixelFormat::Bgra => ffmpeg::format::Pixel::BGRA,
            PixelFormat::Bgrx => ffmpeg::format::Pixel::BGRZ,
            PixelFormat::Rgba => ffmpeg::format::Pixel::RGBA,
            PixelFormat::Rgbx => ffmpeg::format::Pixel::RGBZ,
        };
        let mut av = ffmpeg::frame::Video::new(pixel, width, height);
        let dst_stride = av.stride(0);
        let row = width as usize * 4;
        let dst = av.data_mut(0);
        for y in 0..height as usize {
            dst[y * dst_stride..y * dst_stride + row]
                .copy_from_slice(&data[y * stride..y * stride + row]);
        }
        Frame { av, captured }
    }

    /// Copies BGRA rows (`stride` bytes apart) into a new frame. `opaque`
    /// frames are tagged [`PixelFormat::Bgrx`] so encoders ignore alpha.
    pub fn from_bgra(
        width: u32,
        height: u32,
        stride: usize,
        data: &[u8],
        opaque: bool,
        captured: Instant,
    ) -> Frame {
        let rows = stride * (height as usize).saturating_sub(1) + width as usize * 4;
        Frame::from_pixels(
            width,
            height,
            stride,
            if opaque {
                PixelFormat::Bgrx
            } else {
                PixelFormat::Bgra
            },
            Pixels::Owned(data[..rows].to_vec()),
            captured,
        )
    }

    pub fn from_image(image: &Image, captured: Instant) -> Frame {
        Frame::from_bgra(
            image.width,
            image.height,
            image.stride(),
            &image.data,
            image.is_opaque(),
            captured,
        )
    }

    pub fn width(&self) -> u32 {
        self.av.width()
    }

    pub fn height(&self) -> u32 {
        self.av.height()
    }

    pub fn av(&self) -> &ffmpeg::frame::Video {
        &self.av
    }

    pub fn into_av(self) -> ffmpeg::frame::Video {
        self.av
    }
}
