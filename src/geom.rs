//! Integer and float rectangles shared by capture, the overlay and the editor.

use std::fmt;
use std::str::FromStr;

/// Integer rectangle. Used for logical compositor coordinates and pixel crops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl IRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            self.w as i64 * self.h as i64
        }
    }

    pub fn intersect(&self, other: &IRect) -> Option<IRect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        (r > x && b > y).then(|| IRect::new(x, y, r - x, b - y))
    }

    pub fn union(&self, other: &IRect) -> IRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        IRect::new(x, y, r - x, b - y)
    }

    /// Rectangle spanned by two corner points, in any order.
    pub fn from_corners(ax: i32, ay: i32, bx: i32, by: i32) -> IRect {
        IRect::new(ax.min(bx), ay.min(by), (ax - bx).abs(), (ay - by).abs())
    }
}

/// `X,Y WxH`, the format `slurp` prints and `grim -g` accepts, so existing
/// scripts can pass geometry through unchanged. `X,Y,WxH` (no space) is also
/// accepted because it survives unquoted shell words and the IPC line format.
impl FromStr for IRect {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (pos, size) = s
            .rsplit_once([' ', ','])
            .ok_or_else(|| format!("invalid geometry {s:?}, expected \"X,Y WxH\""))?;
        let (x, y) = pos
            .split_once(',')
            .ok_or_else(|| format!("invalid position in geometry {s:?}"))?;
        let (w, h) = size
            .split_once('x')
            .ok_or_else(|| format!("invalid size in geometry {s:?}"))?;
        let num = |v: &str| {
            v.trim()
                .parse::<i32>()
                .map_err(|_| format!("invalid number {v:?} in geometry {s:?}"))
        };
        let rect = IRect::new(num(x)?, num(y)?, num(w)?, num(h)?);
        if rect.is_empty() {
            return Err(format!("geometry {s:?} has an empty size"));
        }
        Ok(rect)
    }
}

impl fmt::Display for IRect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{}x{}", self.x, self.y, self.w, self.h)
    }
}

/// Point in image pixel space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

impl Pt {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn dist(self, other: Pt) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    pub fn offset(self, dx: f32, dy: f32) -> Pt {
        Pt::new(self.x + dx, self.y + dy)
    }
}

/// Float rectangle in image pixel space. Width/height are never negative.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn from_points(a: Pt, b: Pt) -> RectF {
        RectF::new(
            a.x.min(b.x),
            a.y.min(b.y),
            (a.x - b.x).abs(),
            (a.y - b.y).abs(),
        )
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> Pt {
        Pt::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, p: Pt) -> bool {
        p.x >= self.x && p.y >= self.y && p.x <= self.right() && p.y <= self.bottom()
    }

    pub fn inflate(&self, d: f32) -> RectF {
        RectF::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    pub fn translate(&self, dx: f32, dy: f32) -> RectF {
        RectF::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn union(&self, other: &RectF) -> RectF {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        RectF::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    pub fn intersect(&self, other: &RectF) -> Option<RectF> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        (r > x && b > y).then(|| RectF::new(x, y, r - x, b - y))
    }

    /// Pixel-aligned rectangle covering this one, clamped to `w`×`h`.
    pub fn to_pixel_rect(&self, w: u32, h: u32) -> Option<IRect> {
        let x0 = self.x.floor().max(0.0) as i32;
        let y0 = self.y.floor().max(0.0) as i32;
        let x1 = (self.right().ceil() as i32).min(w as i32);
        let y1 = (self.bottom().ceil() as i32).min(h as i32);
        (x1 > x0 && y1 > y0).then(|| IRect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

/// Distance from `p` to the segment `a`–`b`.
pub fn dist_to_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 <= f32::EPSILON {
        return p.dist(a);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0);
    p.dist(Pt::new(a.x + t * dx, a.y + t * dy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slurp_and_ipc_geometry() {
        assert_eq!("10,20 300x200".parse(), Ok(IRect::new(10, 20, 300, 200)));
        assert_eq!("10,20,300x200".parse(), Ok(IRect::new(10, 20, 300, 200)));
        assert_eq!("-1440,0 10x10".parse(), Ok(IRect::new(-1440, 0, 10, 10)));
        assert!("10,20 0x5".parse::<IRect>().is_err());
        assert!("10 20 30 40".parse::<IRect>().is_err());
        assert!("a,b cxd".parse::<IRect>().is_err());
    }

    #[test]
    fn display_round_trips_through_parse() {
        let r = IRect::new(-5, 7, 11, 13);
        assert_eq!(r.to_string().parse(), Ok(r));
    }

    #[test]
    fn intersect_and_union() {
        let a = IRect::new(0, 0, 10, 10);
        let b = IRect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Some(IRect::new(5, 5, 5, 5)));
        assert_eq!(
            a.intersect(&IRect::new(10, 0, 5, 5)),
            None,
            "touching edges"
        );
        assert_eq!(a.union(&b), IRect::new(0, 0, 15, 15));
        assert_eq!(IRect::default().union(&b), b);
    }

    #[test]
    fn corners_normalize() {
        assert_eq!(IRect::from_corners(10, 10, 2, 4), IRect::new(2, 4, 8, 6));
        let r = RectF::from_points(Pt::new(5.0, 1.0), Pt::new(1.0, 5.0));
        assert_eq!(r, RectF::new(1.0, 1.0, 4.0, 4.0));
    }

    #[test]
    fn pixel_rect_clamps_to_image() {
        let r = RectF::new(-3.5, 2.2, 10.0, 100.0);
        assert_eq!(r.to_pixel_rect(5, 50), Some(IRect::new(0, 2, 5, 48)));
        assert_eq!(RectF::new(10.0, 10.0, 2.0, 2.0).to_pixel_rect(5, 5), None);
    }

    #[test]
    fn segment_distance() {
        let (a, b) = (Pt::new(0.0, 0.0), Pt::new(10.0, 0.0));
        assert_eq!(dist_to_segment(Pt::new(5.0, 3.0), a, b), 3.0);
        assert_eq!(dist_to_segment(Pt::new(-4.0, 3.0), a, b), 5.0);
        assert_eq!(dist_to_segment(Pt::new(1.0, 1.0), a, a), 2f32.sqrt());
    }
}
