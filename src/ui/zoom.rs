//! The zoom levels the editor stops at: 10% steps up to 100%, then 25%
//! steps. Levels are device pixels per image pixel, so 100% is "actual
//! pixels" on any display scale.

/// Highest level (3200%).
pub const MAX: f32 = 32.0;
/// Two levels this close are the same level (float noise from transitions).
const EPS: f32 = 1e-3;

fn levels() -> impl Iterator<Item = f32> {
    (1..10).map(|i| i as f32 / 10.0).chain(
        (0..)
            .map(|i| 1.0 + i as f32 * 0.25)
            .take_while(|&z| z <= MAX),
    )
}

/// The level after `scale` towards bigger (`up`) or smaller images, or
/// `None` at the end of the range. From between two levels this is the
/// nearest level in that direction.
pub fn step(scale: f32, up: bool) -> Option<f32> {
    if up {
        levels().find(|&l| l > scale + EPS)
    } else {
        levels().take_while(|&l| l < scale - EPS).last()
    }
}

/// The largest level not above `scale`, so fitting never overflows; a scale
/// below the smallest level (huge images) is kept as is.
pub fn snap_down(scale: f32) -> f32 {
    levels()
        .take_while(|&l| l <= scale + EPS)
        .last()
        .unwrap_or(scale)
}

/// Wheel or touchpad scrolling accumulated into whole zoom steps: one step
/// per `NOTCH` pixels, so a mouse wheel moves one level per notch and a
/// touchpad needs a deliberate swipe.
#[derive(Debug, Default)]
pub struct Wheel {
    pending: f32,
}

impl Wheel {
    pub const NOTCH: f32 = 40.0;

    /// Adds a scroll delta (positive zooms in) and returns the whole steps
    /// it completes, signed.
    pub fn feed(&mut self, delta: f32) -> i32 {
        // Turning around drops what was left over from the other direction.
        if delta * self.pending < 0.0 {
            self.pending = 0.0;
        }
        self.pending += delta;
        let steps = (self.pending / Self::NOTCH).trunc();
        self.pending -= steps * Self::NOTCH;
        steps as i32
    }
}

/// `scale` moved by `steps` levels (negative: smaller), stopping at the
/// ends of the range.
pub fn step_by(mut scale: f32, steps: i32) -> f32 {
    for _ in 0..steps.unsigned_abs() {
        match step(scale, steps > 0) {
            Some(next) => scale = next,
            None => break,
        }
    }
    scale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(z: Option<f32>) -> Option<u32> {
        z.map(|z| (z * 100.0).round() as u32)
    }

    #[test]
    fn tenths_below_actual_size_quarters_above() {
        assert_eq!(pct(step(0.1, true)), Some(20));
        assert_eq!(pct(step(0.9, true)), Some(100));
        assert_eq!(pct(step(1.0, true)), Some(125));
        assert_eq!(pct(step(1.25, false)), Some(100));
        assert_eq!(pct(step(1.0, false)), Some(90));
        assert_eq!(pct(step(0.1, false)), None);
        assert_eq!(pct(step(MAX, true)), None);
    }

    #[test]
    fn off_level_scales_step_to_the_nearest_level_in_that_direction() {
        // The 118% from continuous zooming goes to 125% or 100%, never 143%.
        assert_eq!(pct(step(1.18, true)), Some(125));
        assert_eq!(pct(step(1.18, false)), Some(100));
        assert_eq!(pct(step(0.58, true)), Some(60));
        assert_eq!(pct(step(0.58, false)), Some(50));
        // Float noise on a level counts as that level.
        assert_eq!(pct(step(0.3000004, true)), Some(40));
        assert_eq!(pct(step(0.04, true)), Some(10));
    }

    #[test]
    fn fitting_snaps_down_to_a_level() {
        assert_eq!(snap_down(0.58), 0.5);
        assert_eq!(snap_down(1.0), 1.0);
        assert_eq!(snap_down(1.4), 1.25);
        assert_eq!(snap_down(0.04), 0.04, "below 10% the fit is kept");
    }

    #[test]
    fn wheel_steps_once_per_notch_and_resets_on_reversal() {
        let mut w = Wheel::default();
        assert_eq!(w.feed(40.0), 1);
        assert_eq!(w.feed(100.0), 2);
        assert_eq!(w.feed(15.0), 0, "20 + 15 is under a notch");
        assert_eq!(w.feed(-30.0), 0, "reversing drops the leftover");
        assert_eq!(w.feed(-10.0), -1);
        assert_eq!(step_by(1.0, 3), 1.75);
        assert_eq!(step_by(1.0, -2), 0.8);
        assert_eq!(step_by(0.2, -5), 0.1, "stops at the smallest level");
    }
}
