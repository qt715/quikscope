//! Pixel math of Windows desktop capture, free of Win32 types so its tests
//! run on every host: DXGI output rotation, mapping desktop regions into the
//! duplication texture, and composing DXGI pointer shapes.
//!
//! Desktop Duplication hands out textures in the display's scan-out
//! orientation; `Rotation` (DXGI_MODE_ROTATION) says how that texture is
//! turned to give the desktop the user sees.

use crate::{geom::IRect, raster::Image};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Rotation {
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Rotate90 | Rotation::Rotate270)
    }

    /// Desktop size of a texture of `texture` size (and vice versa).
    pub fn turn(self, (width, height): (u32, u32)) -> (u32, u32) {
        if self.swaps_axes() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// The texture rectangle holding `region` of a desktop of size
    /// `desktop`; both in desktop orientation relative to the output's
    /// top-left. [`rotate_into`] turns that rectangle back into `region`.
    pub fn texture_rect(self, region: IRect, desktop: (u32, u32)) -> IRect {
        let (w, h) = (desktop.0 as i32, desktop.1 as i32);
        let IRect { x, y, w: rw, h: rh } = region;
        match self {
            Rotation::Identity => region,
            Rotation::Rotate90 => IRect::new(y, w - x - rw, rh, rw),
            Rotation::Rotate180 => IRect::new(w - x - rw, h - y - rh, rw, rh),
            Rotation::Rotate270 => IRect::new(h - y - rh, x, rh, rw),
        }
    }
}

/// Turns `width`×`height` texture pixels (`pitch` bytes per row) into
/// desktop orientation in `dst`, packed rows of the turned width, opaque.
pub fn rotate_into(
    src: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    rotation: Rotation,
    dst: &mut [u8],
) {
    let (w, h) = (width as usize, height as usize);
    assert!(dst.len() >= w * h * 4, "destination shorter than the frame");
    if rotation == Rotation::Identity {
        for (source, target) in src.chunks(pitch).zip(dst.chunks_exact_mut(w * 4)).take(h) {
            target.copy_from_slice(&source[..w * 4]);
            for alpha in target.iter_mut().skip(3).step_by(4) {
                *alpha = 255;
            }
        }
        return;
    }
    let out_width = if rotation.swaps_axes() { h } else { w };
    for y in 0..h {
        let row = &src[y * pitch..y * pitch + w * 4];
        for (x, pixel) in row.chunks_exact(4).enumerate() {
            let (dx, dy) = match rotation {
                Rotation::Rotate90 => (h - 1 - y, x),
                Rotation::Rotate180 => (w - 1 - x, h - 1 - y),
                Rotation::Rotate270 => (y, w - 1 - x),
                Rotation::Identity => unreachable!(),
            };
            let at = (dy * out_width + dx) * 4;
            dst[at..at + 4].copy_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
}

/// A whole texture as an opaque desktop-oriented image.
pub fn to_image(src: &[u8], pitch: usize, width: u32, height: u32, rotation: Rotation) -> Image {
    let (w, h) = rotation.turn((width, height));
    let mut image = Image::new(w, h);
    rotate_into(src, pitch, width, height, rotation, &mut image.data);
    image
}

/// DXGI_OUTDUPL_POINTER_SHAPE_TYPE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    /// 1 bpp AND mask rows followed by as many 1 bpp XOR mask rows.
    Monochrome,
    /// 32 bpp BGRA with straight alpha.
    Color,
    /// 32 bpp BGR plus a mask byte: 0 replaces, 0xFF XORs the screen.
    MaskedColor,
}

/// A pointer shape as DXGI_OUTDUPL_POINTER_SHAPE_INFO describes it.
#[derive(Clone, Debug)]
pub struct PointerShape {
    pub kind: PointerKind,
    pub width: u32,
    /// Bitmap rows; twice the pointer height for [`PointerKind::Monochrome`].
    pub height: u32,
    pub pitch: usize,
    pub data: Vec<u8>,
}

/// Draws `shape` with its top-left corner at (`x`, `y`) onto `width`×`height`
/// BGRX pixels `stride` bytes apart, clipped to the frame.
pub fn draw_pointer(
    frame: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    shape: &PointerShape,
    x: i32,
    y: i32,
) {
    let rows = match shape.kind {
        PointerKind::Monochrome => shape.height / 2,
        PointerKind::Color | PointerKind::MaskedColor => shape.height,
    } as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + shape.width as i32).min(width as i32);
    let y1 = (y + rows).min(height as i32);
    for fy in y0..y1 {
        let sy = (fy - y) as usize;
        for fx in x0..x1 {
            let sx = (fx - x) as usize;
            let at = fy as usize * stride + fx as usize * 4;
            let screen = &mut frame[at..at + 3];
            match shape.kind {
                PointerKind::Monochrome => {
                    let bit = |row: usize| {
                        shape.data[row * shape.pitch + sx / 8] & (0x80 >> (sx % 8)) != 0
                    };
                    let and = bit(sy);
                    let xor = bit(sy + rows as usize);
                    for channel in screen {
                        *channel = if and { *channel } else { 0 } ^ if xor { 255 } else { 0 };
                    }
                }
                PointerKind::Color => {
                    let p = &shape.data[sy * shape.pitch + sx * 4..][..4];
                    let alpha = u32::from(p[3]);
                    for (channel, &source) in screen.iter_mut().zip(p) {
                        *channel = ((u32::from(source) * alpha
                            + u32::from(*channel) * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                }
                PointerKind::MaskedColor => {
                    let p = &shape.data[sy * shape.pitch + sx * 4..][..4];
                    for (channel, &source) in screen.iter_mut().zip(p) {
                        *channel = if p[3] == 0 { source } else { *channel ^ source };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROTATIONS: [Rotation; 4] = [
        Rotation::Identity,
        Rotation::Rotate90,
        Rotation::Rotate180,
        Rotation::Rotate270,
    ];

    fn blues(image: &Image) -> Vec<u8> {
        image.data.chunks_exact(4).map(|p| p[0]).collect()
    }

    #[test]
    fn pitched_bgra_stays_bgra_and_becomes_opaque() {
        let image = to_image(
            &[
                1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, 7, 8, 9, 0, 10, 11, 12, 0,
            ],
            12,
            2,
            2,
            Rotation::Identity,
        );
        assert_eq!(
            image.data,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }

    /// DXGI's convention, verified on rotated monitors: the desktop is the
    /// scan-out texture turned clockwise by the rotation.
    #[test]
    fn all_rotations_preserve_pixel_positions() {
        let bytes = [
            1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0,
        ];
        for (rotation, dimensions, expected) in [
            (Rotation::Identity, (2, 3), vec![1, 2, 3, 4, 5, 6]),
            (Rotation::Rotate90, (3, 2), vec![5, 3, 1, 6, 4, 2]),
            (Rotation::Rotate180, (2, 3), vec![6, 5, 4, 3, 2, 1]),
            (Rotation::Rotate270, (3, 2), vec![2, 4, 6, 1, 3, 5]),
        ] {
            let image = to_image(&bytes, 8, 2, 3, rotation);
            assert_eq!((image.width, image.height), dimensions);
            assert_eq!(blues(&image), expected);
        }
    }

    /// A desktop region read through its texture rectangle must equal the
    /// same region cut from the whole rotated desktop, for every rotation
    /// and a region touching no edge and no axis of symmetry.
    #[test]
    fn regions_map_into_rotated_textures() {
        let texture_size = (7u32, 5u32);
        let pitch = texture_size.0 as usize * 4 + 8;
        let mut texture = vec![0u8; pitch * texture_size.1 as usize];
        for y in 0..texture_size.1 as usize {
            for x in 0..texture_size.0 as usize {
                texture[y * pitch + x * 4] = (y * 16 + x) as u8;
            }
        }
        for rotation in ROTATIONS {
            let desktop = to_image(&texture, pitch, texture_size.0, texture_size.1, rotation);
            assert_eq!((desktop.width, desktop.height), rotation.turn(texture_size));
            for region in [
                IRect::new(1, 2, 3, 1),
                IRect::new(0, 0, desktop.width as i32, desktop.height as i32),
                IRect::new(desktop.width as i32 - 2, desktop.height as i32 - 1, 2, 1),
            ] {
                let rect = rotation.texture_rect(region, (desktop.width, desktop.height));
                assert!(
                    rect.x >= 0
                        && rect.y >= 0
                        && rect.right() <= texture_size.0 as i32
                        && rect.bottom() <= texture_size.1 as i32,
                    "{rotation:?} {region:?} -> {rect:?} leaves the texture"
                );
                let mut out = vec![0u8; region.w as usize * region.h as usize * 4];
                rotate_into(
                    &texture[rect.y as usize * pitch + rect.x as usize * 4..],
                    pitch,
                    rect.w as u32,
                    rect.h as u32,
                    rotation,
                    &mut out,
                );
                assert_eq!(out, desktop.crop(region).data, "{rotation:?} {region:?}");
            }
        }
    }

    fn frame(width: u32, height: u32, bgr: [u8; 3]) -> Vec<u8> {
        [bgr[0], bgr[1], bgr[2], 255].repeat(width as usize * height as usize)
    }

    #[test]
    fn color_pointer_blends_straight_alpha_and_clips() {
        let mut pixels = frame(3, 2, [100, 100, 100]);
        let shape = PointerShape {
            kind: PointerKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            data: vec![
                0, 0, 0, 0, /**/ 200, 0, 0, 255, //
                255, 255, 255, 128, /**/ 10, 20, 30, 255,
            ],
        };
        // Top-left pointer pixel lies off the frame's left edge.
        draw_pointer(&mut pixels, 3, 2, 12, &shape, -1, 0);
        assert_eq!(&pixels[0..4], &[200, 0, 0, 255]);
        assert_eq!(&pixels[4..8], &[100, 100, 100, 255]);
        assert_eq!(&pixels[12..16], &[10, 20, 30, 255]);
        assert_eq!(&pixels[16..20], &[100, 100, 100, 255]);
        let mut pixels = frame(1, 1, [100, 100, 100]);
        draw_pointer(&mut pixels, 1, 1, 4, &shape, 0, -1);
        assert_eq!(&pixels, &[178, 178, 178, 255]);
    }

    #[test]
    fn monochrome_pointer_applies_and_then_xor() {
        // One row, four pixels: (AND, XOR) = (0,0) black, (0,1) white,
        // (1,0) screen, (1,1) inverted screen.
        let shape = PointerShape {
            kind: PointerKind::Monochrome,
            width: 4,
            height: 2,
            pitch: 2,
            data: vec![0b0011_0000, 0, 0b0101_0000, 0],
        };
        let mut pixels = frame(4, 1, [10, 20, 30]);
        draw_pointer(&mut pixels, 4, 1, 16, &shape, 0, 0);
        assert_eq!(
            pixels,
            [
                0, 0, 0, 255, 255, 255, 255, 255, 10, 20, 30, 255, 245, 235, 225, 255
            ]
        );
    }

    #[test]
    fn masked_color_pointer_replaces_or_xors() {
        let shape = PointerShape {
            kind: PointerKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![1, 2, 3, 0, 0xff, 0x0f, 0, 0xff],
        };
        let mut pixels = frame(2, 1, [0x0f, 0xf0, 0x55]);
        draw_pointer(&mut pixels, 2, 1, 8, &shape, 0, 0);
        assert_eq!(pixels, [1, 2, 3, 255, 0xf0, 0xff, 0x55, 255]);
    }
}
