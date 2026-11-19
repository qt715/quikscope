//! Pixel conversion between captured/decoded pictures: 4-byte RGB ↔ YUV
//! 4:2:0 (BT.709 limited range) and BGRA scaling.

use super::codec::{Yuv420, YuvLayout};
use super::frame::PixelFormat;
use crate::raster::Image;

/// Converts the top-left `width`×`height` pixels (both even, at most the
/// source size) of `pixels` (`stride` bytes per row) to 4:2:0. Callers crop
/// odd-sized captures by one column/row this way; nothing is ever resampled.
pub fn to_yuv420(
    pixels: &[u8],
    stride: usize,
    format: PixelFormat,
    width: u32,
    height: u32,
    layout: YuvLayout,
) -> Yuv420 {
    let _ = (pixels, stride, format, width, height, layout);
    todo!("VideoCodecs: yuv crate, SIMD")
}

/// Converts a decoded picture to a BGRA image of the same size.
pub fn to_bgra(picture: &Yuv420) -> Image {
    let _ = picture;
    todo!("VideoCodecs: yuv crate")
}

/// Scales `image` to `width`×`height` with an area-averaging filter
/// (downscaling) or bilinear (upscaling).
pub fn resize(image: &Image, width: u32, height: u32) -> Image {
    let _ = (image, width, height);
    todo!("VideoCodecs: fast_image_resize")
}
