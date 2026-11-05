//! Eased transitions between editor views (zoom + pan).

use std::time::{Duration, Instant};

use gpui::{Pixels, Point, point, px};

/// CSS-style `cubic-bezier(x1, y1, x2, y2)` timing function.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    /// Fast start, long gentle settle (easeOutQuint-like): the zoom reacts
    /// immediately and lands softly.
    pub const EASE_OUT: CubicBezier = CubicBezier {
        x1: 0.22,
        y1: 1.0,
        x2: 0.36,
        y2: 1.0,
    };

    fn sample(a1: f32, a2: f32, t: f32) -> f32 {
        let u = 1.0 - t;
        3.0 * u * u * t * a1 + 3.0 * u * t * t * a2 + t * t * t
    }

    fn slope(a1: f32, a2: f32, t: f32) -> f32 {
        let u = 1.0 - t;
        3.0 * u * u * a1 + 6.0 * u * t * (a2 - a1) + 3.0 * t * t * (1.0 - a2)
    }

    /// Eased progress for linear progress `x` in [0, 1].
    pub fn ease(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        if x == 0.0 || x == 1.0 {
            return x;
        }
        // Newton's method on x(t) = x, falling back to bisection where the
        // slope is too flat to trust.
        let mut t = x;
        for _ in 0..8 {
            let err = Self::sample(self.x1, self.x2, t) - x;
            if err.abs() < 1e-5 {
                return Self::sample(self.y1, self.y2, t);
            }
            let d = Self::slope(self.x1, self.x2, t);
            if d.abs() < 1e-6 {
                break;
            }
            t = (t - err / d).clamp(0.0, 1.0);
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        t = x;
        for _ in 0..32 {
            let v = Self::sample(self.x1, self.x2, t);
            if (v - x).abs() < 1e-5 {
                break;
            }
            if v < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) / 2.0;
        }
        Self::sample(self.y1, self.y2, t)
    }
}

/// What the editor shows: window pixels per image pixel, and the canvas
/// position of image pixel (0, 0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: f32,
    pub pan: Point<Pixels>,
}

impl View {
    /// The view `e` (eased, 0–1) of the way from `from` to `to`.
    ///
    /// Zoom moves geometrically, so each frame scales by the same factor and
    /// a 4× zoom does not rush through its first half. Pan follows the one
    /// canvas point both views map to the same image point, so the spot
    /// under the cursor (or the centre) stays put for the whole transition
    /// instead of drifting and coming back.
    pub fn lerp(from: View, to: View, e: f32) -> View {
        let (z0, z1) = (from.zoom, to.zoom);
        if (z1 / z0 - 1.0).abs() < 1e-4 {
            return View {
                zoom: z1,
                pan: from.pan + (to.pan - from.pan) * e,
            };
        }
        let zoom = z0 * (z1 / z0).powf(e);
        let k = zoom / z0;
        let axis = |p0: Pixels, p1: Pixels| {
            let (p0, p1) = (f32::from(p0), f32::from(p1));
            let fixed = (p0 * z1 - p1 * z0) / (z1 - z0);
            px(fixed - (fixed - p0) * k)
        };
        View {
            zoom,
            pan: point(axis(from.pan.x, to.pan.x), axis(from.pan.y, to.pan.y)),
        }
    }
}

/// A view transition in progress.
#[derive(Clone, Copy, Debug)]
pub struct Transition {
    pub from: View,
    pub start: Instant,
    pub duration: Duration,
}

impl Transition {
    /// Eased progress at `now`; `None` once finished.
    pub fn progress(&self, now: Instant) -> Option<f32> {
        let t = now.saturating_duration_since(self.start).as_secs_f32()
            / self.duration.as_secs_f32().max(1e-3);
        (t < 1.0).then(|| CubicBezier::EASE_OUT.ease(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_hits_endpoints_and_is_monotonic() {
        let e = CubicBezier::EASE_OUT;
        assert_eq!(e.ease(0.0), 0.0);
        assert_eq!(e.ease(1.0), 1.0);
        let mut last = 0.0;
        for i in 1..100 {
            let v = e.ease(i as f32 / 100.0);
            assert!(v >= last, "monotonic at {i}");
            last = v;
        }
        assert!(e.ease(0.25) > 0.6, "ease-out front-loads the motion");
    }

    #[test]
    fn linear_bezier_is_identity() {
        let lin = CubicBezier {
            x1: 0.0,
            y1: 0.0,
            x2: 1.0,
            y2: 1.0,
        };
        for x in [0.1, 0.33, 0.5, 0.9] {
            assert!((lin.ease(x) - x).abs() < 1e-3);
        }
    }

    fn image_at(v: View, s: f32) -> f32 {
        (s - f32::from(v.pan.x)) / v.zoom
    }

    #[test]
    fn zoom_transition_keeps_the_anchor_fixed() {
        // Zoom 1 → 4 around canvas x = 300.
        let from = View {
            zoom: 1.0,
            pan: point(px(100.), px(50.)),
        };
        let anchor_img = image_at(from, 300.0);
        let to = View {
            zoom: 4.0,
            pan: point(px(300. - anchor_img * 4.0), px(50.)),
        };
        assert_eq!(View::lerp(from, to, 0.0), from);
        let end = View::lerp(from, to, 1.0);
        assert!((end.zoom - 4.0).abs() < 1e-4);
        assert!((f32::from(end.pan.x) - f32::from(to.pan.x)).abs() < 1e-2);
        for e in [0.1, 0.5, 0.9] {
            let v = View::lerp(from, to, e);
            assert!(
                (image_at(v, 300.0) - anchor_img).abs() < 1e-2,
                "anchor drift at {e}"
            );
        }
        // Geometric: halfway is 2x, not 2.5x.
        assert!((View::lerp(from, to, 0.5).zoom - 2.0).abs() < 1e-3);
    }

    #[test]
    fn pure_pan_interpolates_linearly() {
        let from = View {
            zoom: 2.0,
            pan: point(px(0.), px(0.)),
        };
        let to = View {
            zoom: 2.0,
            pan: point(px(100.), px(-40.)),
        };
        assert_eq!(View::lerp(from, to, 0.5).pan, point(px(50.), px(-20.)));
    }

    #[test]
    fn transition_finishes_after_its_duration() {
        let start = Instant::now();
        let t = Transition {
            from: View {
                zoom: 1.0,
                pan: point(px(0.), px(0.)),
            },
            start,
            duration: Duration::from_millis(200),
        };
        assert_eq!(t.progress(start), Some(0.0));
        assert!(t.progress(start + Duration::from_millis(100)).unwrap() > 0.5);
        assert_eq!(t.progress(start + Duration::from_millis(200)), None);
    }
}
