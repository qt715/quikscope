//! One captured video frame: CPU pixels plus the moment they were captured.

use std::time::Instant;

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
/// apart, in the storage the capture produced. A recording converts it once
/// on its encoder thread and drops it right after, which hands a
/// [`Pixels::Shared`] buffer back to its pool.
pub struct Frame {
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    pixels: Pixels,
    /// When the pixels were captured; the session derives timestamps from it.
    pub captured: Instant,
}

/// Describes the frame without its pixels (for test failures and logs).
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("format", &self.format)
            .field("captured", &self.captured)
            .finish_non_exhaustive()
    }
}

impl Frame {
    /// Takes ownership of (or borrows from a pool) `height` rows of pixels.
    pub fn from_pixels(
        width: u32,
        height: u32,
        stride: usize,
        format: PixelFormat,
        pixels: Pixels,
        captured: Instant,
    ) -> Frame {
        assert!(stride >= width as usize * 4, "stride shorter than a row");
        assert!(
            pixels.bytes().len()
                >= stride * (height as usize).saturating_sub(1) + width as usize * 4,
            "pixel buffer shorter than the frame"
        );
        Frame {
            width,
            height,
            stride,
            format,
            pixels,
            captured,
        }
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
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes from one row to the next.
    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The rows, `stride` bytes apart (the last row may end right after its
    /// pixels).
    pub fn pixels(&self) -> &[u8] {
        self.pixels.bytes()
    }

    /// The frame as a straight-alpha BGRA image (opaque formats get alpha
    /// 255).
    pub fn to_image(&self) -> Image {
        let row = self.width as usize * 4;
        let mut data = Vec::with_capacity(row * self.height as usize);
        let pixels = self.pixels();
        for y in 0..self.height as usize {
            data.extend_from_slice(&pixels[y * self.stride..y * self.stride + row]);
        }
        let swap = matches!(self.format, PixelFormat::Rgba | PixelFormat::Rgbx);
        let opaque = self.format.is_opaque();
        if swap || opaque {
            for pixel in data.as_chunks_mut::<4>().0 {
                if swap {
                    pixel.swap(0, 2);
                }
                if opaque {
                    pixel[3] = 255;
                }
            }
        }
        Image {
            width: self.width,
            height: self.height,
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_image_honours_stride_and_byte_order() {
        // 2×2 RGBX with 4 bytes of row padding; the X bytes are garbage.
        let rows = vec![
            1, 2, 3, 9, 4, 5, 6, 9, 0, 0, 0, 0, //
            7, 8, 9, 9, 10, 11, 12, 9,
        ];
        let frame = Frame::from_pixels(
            2,
            2,
            12,
            PixelFormat::Rgbx,
            Pixels::Shared(Box::new(rows)),
            Instant::now(),
        );
        assert_eq!(
            frame.to_image().data,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }
}
