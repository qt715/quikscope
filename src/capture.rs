//! Captured screens: per-output frames (from whichever platform backend)
//! and composition of arbitrary logical rectangles out of them.

use std::sync::Arc;

use crate::geom::IRect;
use crate::raster::Image;

/// One output's pixels, already rotated into the orientation the user sees.
#[derive(Clone, Debug)]
pub struct OutputFrame {
    /// Connector name, e.g. `DP-1`.
    pub name: String,
    /// Position and size in the compositor's logical layout.
    pub logical: IRect,
    /// Physical pixels (`logical` × output scale). Shared, so whole-output
    /// captures and overlays never copy the frame.
    pub image: Arc<Image>,
}

impl OutputFrame {
    /// Physical pixels per logical pixel.
    pub fn scale(&self) -> f64 {
        if self.logical.w <= 0 {
            1.0
        } else {
            self.image.width as f64 / self.logical.w as f64
        }
    }
}

/// A frozen copy of some or all outputs, taken at one moment.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub outputs: Vec<OutputFrame>,
}

/// A composed capture ready to become a shot.
#[derive(Debug)]
pub struct Composed {
    pub image: Arc<Image>,
    /// Physical pixels per logical pixel of the result.
    pub scale: f32,
}

impl Snapshot {
    pub fn bounds(&self) -> IRect {
        self.outputs
            .iter()
            .fold(IRect::default(), |acc, o| acc.union(&o.logical))
    }

    pub fn output(&self, name: &str) -> Option<&OutputFrame> {
        self.outputs.iter().find(|o| o.name == name)
    }

    /// Composes the logical rectangle `region` from every output it touches,
    /// at the highest scale among them, so no output loses detail. Parts of
    /// `region` outside every output stay transparent.
    pub fn compose(&self, region: IRect) -> Option<Composed> {
        let touching: Vec<(&OutputFrame, IRect)> = self
            .outputs
            .iter()
            .filter_map(|o| o.logical.intersect(&region).map(|i| (o, i)))
            .collect();
        let scale = touching
            .iter()
            .map(|(o, _)| o.scale())
            .fold(0.0f64, f64::max);
        if touching.is_empty() || scale <= 0.0 {
            return None;
        }

        // A capture of exactly one whole output needs no copy.
        if let [(o, inter)] = touching.as_slice()
            && *inter == o.logical
            && region == o.logical
        {
            return Some(Composed {
                image: o.image.clone(),
                scale: scale as f32,
            });
        }

        let px = |v: i32, s: f64| (v as f64 * s).round() as i32;
        let mut out = Image::new(
            px(region.w, scale).max(1) as u32,
            px(region.h, scale).max(1) as u32,
        );
        for (o, inter) in touching {
            let os = o.scale();
            let src = IRect::from_corners(
                px(inter.x - o.logical.x, os),
                px(inter.y - o.logical.y, os),
                px(inter.right() - o.logical.x, os),
                px(inter.bottom() - o.logical.y, os),
            );
            let dst = IRect::from_corners(
                px(inter.x - region.x, scale),
                px(inter.y - region.y, scale),
                px(inter.right() - region.x, scale),
                px(inter.bottom() - region.y, scale),
            );
            let mut part = o.image.crop(src);
            if part.width as i32 != dst.w || part.height as i32 != dst.h {
                part = part.resize(dst.w.max(1) as u32, dst.h.max(1) as u32);
            }
            out.blit(&part, dst.x, dst.y);
        }
        Some(Composed {
            image: Arc::new(out),
            scale: scale as f32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str, logical: IRect, scale: u32, color: u8) -> OutputFrame {
        OutputFrame {
            name: name.into(),
            logical,
            image: Arc::new(Image::filled(
                logical.w as u32 * scale,
                logical.h as u32 * scale,
                [color, color, color, 255],
            )),
        }
    }

    #[test]
    fn whole_output_is_shared_not_copied() {
        let s = Snapshot {
            outputs: vec![frame("A", IRect::new(0, 0, 40, 20), 1, 10)],
        };
        let c = s.compose(IRect::new(0, 0, 40, 20)).unwrap();
        assert_eq!((c.image.width, c.image.height, c.scale), (40, 20, 1.0));
        assert!(Arc::ptr_eq(&c.image, &s.outputs[0].image));
    }

    #[test]
    fn region_spanning_two_outputs() {
        let s = Snapshot {
            outputs: vec![
                frame("A", IRect::new(0, 0, 40, 20), 1, 10),
                frame("B", IRect::new(40, 0, 40, 20), 1, 200),
            ],
        };
        assert_eq!(s.bounds(), IRect::new(0, 0, 80, 20));
        let c = s.compose(IRect::new(30, 5, 20, 10)).unwrap();
        assert_eq!((c.image.width, c.image.height), (20, 10));
        assert_eq!(c.image.pixel(9, 0)[0], 10);
        assert_eq!(c.image.pixel(10, 0)[0], 200);
    }

    #[test]
    fn mixed_scales_compose_at_the_highest_scale() {
        let s = Snapshot {
            outputs: vec![
                frame("lo", IRect::new(0, 0, 10, 10), 1, 50),
                frame("hi", IRect::new(10, 0, 10, 10), 2, 150),
            ],
        };
        let c = s.compose(IRect::new(0, 0, 20, 10)).unwrap();
        assert_eq!(c.scale, 2.0);
        assert_eq!((c.image.width, c.image.height), (40, 20));
        assert_eq!(c.image.pixel(5, 5)[0], 50);
        assert_eq!(c.image.pixel(35, 15)[0], 150);
    }

    #[test]
    fn gaps_between_outputs_stay_transparent() {
        let s = Snapshot {
            outputs: vec![
                frame("tall", IRect::new(0, 0, 10, 20), 1, 99),
                frame("short", IRect::new(10, 0, 10, 10), 1, 99),
            ],
        };
        let c = s.compose(s.bounds()).unwrap();
        assert_eq!(c.image.pixel(15, 15), [0, 0, 0, 0]);
        assert_eq!(c.image.pixel(15, 5)[3], 255);
    }

    #[test]
    fn region_outside_every_output() {
        let s = Snapshot {
            outputs: vec![frame("A", IRect::new(0, 0, 10, 10), 1, 1)],
        };
        assert!(s.compose(IRect::new(50, 50, 5, 5)).is_none());
    }
}
