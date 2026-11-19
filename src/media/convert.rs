//! Pixel conversion between captured/decoded pictures: 4-byte RGB ↔ YUV
//! 4:2:0 (BT.709 limited range) and BGRA scaling.

use fast_image_resize::images::{Image as ResizeImage, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use yuv::{
    BufferStoreMut, YuvBiPlanarImage, YuvBiPlanarImageMut, YuvConversionMode, YuvPlanarImage,
    YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
};

use super::codec::{Yuv420, YuvLayout};
use super::frame::PixelFormat;
use crate::raster::Image;

const RANGE: YuvRange = YuvRange::Limited;
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt709;
const MODE: YuvConversionMode = YuvConversionMode::Balanced;

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
    let mut picture = Yuv420 {
        width,
        height,
        layout,
        data: Vec::new(),
    };
    to_yuv420_into(pixels, stride, format, width, height, layout, &mut picture);
    picture
}

/// [`to_yuv420`] into `out`, reusing its buffer: a recorder converting every
/// frame avoids allocating a picture each time.
pub fn to_yuv420_into(
    pixels: &[u8],
    stride: usize,
    format: PixelFormat,
    width: u32,
    height: u32,
    layout: YuvLayout,
    out: &mut Yuv420,
) {
    assert!(
        width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
        "4:2:0 needs even, non-zero sizes, got {width}x{height}"
    );
    let (w, h) = (width as usize, height as usize);
    assert!(stride >= w * 4, "stride shorter than the cropped row");
    assert!(
        pixels.len() >= stride * (h - 1) + w * 4,
        "pixel buffer shorter than the cropped picture"
    );
    let stride = u32::try_from(stride).expect("row stride fits in u32");
    let bgr = matches!(format, PixelFormat::Bgra | PixelFormat::Bgrx);

    out.width = width;
    out.height = height;
    out.layout = layout;
    // Every byte is overwritten below; only the length matters.
    out.data.resize(w * h * 3 / 2, 0);
    let (y_plane, chroma) = out.data.split_at_mut(w * h);
    // Alpha is ignored either way, so the X formats share the A converters.
    let converted = match layout {
        YuvLayout::Nv12 => {
            let mut image = YuvBiPlanarImageMut {
                y_plane: BufferStoreMut::Borrowed(y_plane),
                y_stride: width,
                uv_plane: BufferStoreMut::Borrowed(chroma),
                uv_stride: width,
                width,
                height,
            };
            if bgr {
                yuv::bgra_to_yuv_nv12(&mut image, pixels, stride, RANGE, MATRIX, MODE)
            } else {
                yuv::rgba_to_yuv_nv12(&mut image, pixels, stride, RANGE, MATRIX, MODE)
            }
        }
        YuvLayout::I420 => {
            let (u_plane, v_plane) = chroma.split_at_mut(w * h / 4);
            let mut image = YuvPlanarImageMut {
                y_plane: BufferStoreMut::Borrowed(y_plane),
                y_stride: width,
                u_plane: BufferStoreMut::Borrowed(u_plane),
                u_stride: width / 2,
                v_plane: BufferStoreMut::Borrowed(v_plane),
                v_stride: width / 2,
                width,
                height,
            };
            if bgr {
                yuv::bgra_to_yuv420(&mut image, pixels, stride, RANGE, MATRIX, MODE)
            } else {
                yuv::rgba_to_yuv420(&mut image, pixels, stride, RANGE, MATRIX, MODE)
            }
        }
    };
    converted.expect("plane sizes checked above");
}

/// Converts a decoded picture to a BGRA image of the same size.
pub fn to_bgra(picture: &Yuv420) -> Image {
    let Yuv420 {
        width,
        height,
        layout,
        ref data,
    } = *picture;
    let (w, h) = (width as usize, height as usize);
    assert!(
        width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0,
        "4:2:0 needs even, non-zero sizes, got {width}x{height}"
    );
    assert_eq!(data.len(), w * h * 3 / 2, "picture data size");
    let (y_plane, chroma) = data.split_at(w * h);
    let mut bgra = vec![0; w * h * 4];
    let converted = match layout {
        YuvLayout::Nv12 => yuv::yuv_nv12_to_bgra(
            &YuvBiPlanarImage {
                y_plane,
                y_stride: width,
                uv_plane: chroma,
                uv_stride: width,
                width,
                height,
            },
            &mut bgra,
            width * 4,
            RANGE,
            MATRIX,
            MODE,
        ),
        YuvLayout::I420 => {
            let (u_plane, v_plane) = chroma.split_at(w * h / 4);
            yuv::yuv420_to_bgra(
                &YuvPlanarImage {
                    y_plane,
                    y_stride: width,
                    u_plane,
                    u_stride: width / 2,
                    v_plane,
                    v_stride: width / 2,
                    width,
                    height,
                },
                &mut bgra,
                width * 4,
                RANGE,
                MATRIX,
            )
        }
    };
    converted.expect("plane sizes checked above");
    Image {
        width,
        height,
        data: bgra,
    }
}

/// Scales `image` to `width`×`height` with an area-averaging filter
/// (downscaling) or bilinear (upscaling).
pub fn resize(image: &Image, width: u32, height: u32) -> Image {
    if (width, height) == (image.width, image.height) {
        return image.clone();
    }
    if width == 0 || height == 0 || image.width == 0 || image.height == 0 {
        return Image::new(width, height);
    }
    let src = ImageRef::new(image.width, image.height, &image.data, PixelType::U8x4)
        .expect("BGRA image buffer matches its size");
    let mut dst = ResizeImage::new(width, height, PixelType::U8x4);
    // A box kernel stretched over the scale factor averages every covered
    // source pixel; it would be nearest-neighbour when enlarging.
    let filter = if width <= image.width && height <= image.height {
        FilterType::Box
    } else {
        FilterType::Bilinear
    };
    Resizer::new()
        .resize(
            &src,
            &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)),
        )
        .expect("source and destination are both U8x4");
    Image {
        width,
        height,
        data: dst.into_vec(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    const COLOURS: [(&str, [u8; 3]); 8] = [
        ("red", [255, 0, 0]),
        ("green", [0, 255, 0]),
        ("blue", [0, 0, 255]),
        ("white", [255, 255, 255]),
        ("black", [0, 0, 0]),
        ("grey", [128, 128, 128]),
        ("dark grey", [37, 37, 37]),
        ("orange", [255, 140, 20]),
    ];

    /// A `w`×`h` picture of one colour with `pad` junk bytes after each row.
    fn solid(rgb: [u8; 3], format: PixelFormat, w: usize, h: usize, pad: usize) -> Vec<u8> {
        let px = match format {
            PixelFormat::Bgra | PixelFormat::Bgrx => [rgb[2], rgb[1], rgb[0], 255],
            PixelFormat::Rgba | PixelFormat::Rgbx => [rgb[0], rgb[1], rgb[2], 255],
        };
        let mut data = Vec::new();
        for _ in 0..h {
            for _ in 0..w {
                data.extend_from_slice(&px);
            }
            data.extend(std::iter::repeat_n(0x5a, pad));
        }
        data
    }

    fn assert_close(got: &[u8], want: [u8; 3], what: &str) {
        // BGRA output.
        let got = [got[2], got[1], got[0]];
        for c in 0..3 {
            assert!(
                got[c].abs_diff(want[c]) <= 2,
                "{what}: got rgb {got:?}, want {want:?}"
            );
        }
    }

    #[test]
    fn solid_colours_round_trip_in_every_format_and_layout() {
        for (name, rgb) in COLOURS {
            for format in [
                PixelFormat::Bgra,
                PixelFormat::Bgrx,
                PixelFormat::Rgba,
                PixelFormat::Rgbx,
            ] {
                for layout in [YuvLayout::Nv12, YuvLayout::I420] {
                    let src = solid(rgb, format, 6, 4, 8);
                    let yuv = to_yuv420(&src, 6 * 4 + 8, format, 6, 4, layout);
                    assert_eq!(yuv.data.len(), 6 * 4 * 3 / 2);
                    let back = to_bgra(&yuv);
                    assert_eq!((back.width, back.height), (6, 4));
                    for px in back.data.as_chunks::<4>().0 {
                        assert_close(px, rgb, &format!("{name} {format:?} {layout:?}"));
                        assert_eq!(px[3], 255, "opaque alpha");
                    }
                }
            }
        }
    }

    #[test]
    fn uses_bt709_limited_range() {
        // White/black map to the limited range ends; pure red has the BT.709
        // luma 0.2126*219+16 = 62.6 (BT.601 would give 81).
        for (rgb, luma) in [([255, 255, 255], 235), ([0, 0, 0], 16), ([255, 0, 0], 63)] {
            let src = solid(rgb, PixelFormat::Rgbx, 2, 2, 0);
            let yuv = to_yuv420(&src, 8, PixelFormat::Rgbx, 2, 2, YuvLayout::I420);
            assert!(
                yuv.data[0].abs_diff(luma) <= 1,
                "{rgb:?}: Y {}",
                yuv.data[0]
            );
        }
        let src = solid([255, 255, 255], PixelFormat::Bgrx, 2, 2, 0);
        let yuv = to_yuv420(&src, 8, PixelFormat::Bgrx, 2, 2, YuvLayout::Nv12);
        assert_eq!(&yuv.data[4..], &[128, 128], "neutral chroma");
    }

    #[test]
    fn chroma_planes_land_where_the_layout_says() {
        // Blue has Cb high, Cr low: NV12 interleaves them, I420 splits them.
        let src = solid([0, 0, 255], PixelFormat::Bgra, 4, 2, 0);
        let nv12 = to_yuv420(&src, 16, PixelFormat::Bgra, 4, 2, YuvLayout::Nv12);
        let i420 = to_yuv420(&src, 16, PixelFormat::Bgra, 4, 2, YuvLayout::I420);
        let (cb, cr) = (i420.data[8], i420.data[10]);
        assert!(cb > 200 && cr < 128, "Cb {cb} Cr {cr}");
        assert_eq!(&nv12.data[8..], &[cb, cr, cb, cr]);
        assert_eq!(&i420.data[8..], &[cb, cb, cr, cr]);
    }

    /// A reused picture comes out exactly like a fresh one, whatever size,
    /// layout and contents it had before.
    #[test]
    fn converting_into_a_used_picture_matches_a_fresh_one() {
        let src = solid([255, 140, 20], PixelFormat::Bgra, 6, 4, 0);
        let mut reused = to_yuv420(
            &solid([0, 0, 255], PixelFormat::Bgra, 8, 8, 0),
            32,
            PixelFormat::Bgra,
            8,
            8,
            YuvLayout::Nv12,
        );
        for layout in [YuvLayout::I420, YuvLayout::Nv12] {
            to_yuv420_into(&src, 24, PixelFormat::Bgra, 6, 4, layout, &mut reused);
            assert_eq!(reused, to_yuv420(&src, 24, PixelFormat::Bgra, 6, 4, layout));
        }
    }

    #[test]
    fn crops_odd_sources_to_the_top_left() {
        // 5x3 source: the last column and row are a different colour and
        // must not bleed into the 4x2 crop.
        let (w, h, stride) = (5, 3, 5 * 4 + 4);
        let mut src = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let px = if x == 4 || y == 2 {
                    [255, 0, 255, 255]
                } else {
                    [0, 200, 0, 255]
                };
                src[y * stride + x * 4..][..4].copy_from_slice(&px);
            }
        }
        // The buffer may end right after the last pixel of the last row.
        src.truncate(stride * (h - 1) + w * 4);
        for layout in [YuvLayout::Nv12, YuvLayout::I420] {
            let yuv = to_yuv420(&src, stride, PixelFormat::Bgra, 4, 2, layout);
            let back = to_bgra(&yuv);
            assert_eq!((back.width, back.height), (4, 2));
            for px in back.data.as_chunks::<4>().0 {
                assert_close(px, [0, 200, 0], "cropped green");
            }
        }
    }

    #[test]
    fn resize_averages_down_and_interpolates_up() {
        // 4x2 checker of black/white columns → 2x1 averages to grey.
        let mut image = Image::new(4, 2);
        for (i, px) in image.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = if i % 2 == 0 { 0 } else { 255 };
            *px = [v, v, v, 255];
        }
        let small = resize(&image, 2, 1);
        assert_eq!((small.width, small.height, small.data.len()), (2, 1, 8));
        for px in small.data.as_chunks::<4>().0 {
            assert!(px[0].abs_diff(128) <= 1, "{px:?}");
        }

        let mut ramp = Image::new(2, 1);
        ramp.data
            .copy_from_slice(&[0, 0, 0, 255, 200, 200, 200, 255]);
        let big = resize(&ramp, 8, 2);
        assert_eq!((big.width, big.height), (8, 2));
        let row: Vec<u8> = big
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .take(8)
            .map(|p| p[0])
            .collect();
        assert!(row.windows(2).all(|w| w[0] <= w[1]), "monotonic {row:?}");
        assert!(row[3] > 0 && row[3] < 200, "interpolated {row:?}");
        assert_eq!(resize(&ramp, 2, 1), ramp);
    }

    #[test]
    fn converts_1440p_quickly() {
        let (w, h) = (2560usize, 1440usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        for layout in [YuvLayout::Nv12, YuvLayout::I420] {
            let start = Instant::now();
            let yuv = to_yuv420(&src, w * 4, PixelFormat::Bgrx, w as u32, h as u32, layout);
            let encode = start.elapsed();
            let start = Instant::now();
            let back = to_bgra(&yuv);
            let decode = start.elapsed();
            assert_eq!(back.data.len(), src.len());
            println!("2560x1440 {layout:?}: to_yuv420 {encode:?}, to_bgra {decode:?}");
            if !cfg!(debug_assertions) {
                assert!(encode.as_millis() < 10 && decode.as_millis() < 10);
            }
        }
    }
}
