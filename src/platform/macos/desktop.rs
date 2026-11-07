//! CoreGraphics coordinates already use top-left desktop points, matching Snapshot.
use crate::{
    geom::IRect,
    platform::{Desktop, WindowRect},
};
use anyhow::{Context as _, Result, anyhow};
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use core_graphics::{display::CGDisplay, geometry::CGRect, window};

pub struct MacDesktop;

pub(super) fn rect(bounds: CGRect) -> IRect {
    IRect::from_corners(
        bounds.origin.x.floor() as i32,
        bounds.origin.y.floor() as i32,
        (bounds.origin.x + bounds.size.width).ceil() as i32,
        (bounds.origin.y + bounds.size.height).ceil() as i32,
    )
}

pub(super) fn display_rect(id: u32) -> IRect {
    rect(CGDisplay::new(id).bounds())
}

pub(super) fn physical_size(logical: IRect, mode_width: u64, pixel_width: u64) -> (u32, u32) {
    // Mode dimensions describe the unrotated mode; desktop bounds describe its
    // current orientation. Apply only the mode's Retina factor to those bounds.
    let scale = pixel_width as f64 / mode_width as f64;
    (
        (logical.w as f64 * scale).round() as u32,
        (logical.h as f64 * scale).round() as u32,
    )
}

pub fn selection_outputs() -> Result<Vec<(String, IRect, u32)>> {
    CGDisplay::active_displays()
        .map_err(|e| anyhow!("enumerating macOS displays: {e}"))?
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let mode = display.display_mode().context("display disconnected")?;
            let logical = rect(display.bounds());
            let (width, _) = physical_size(logical, mode.width(), mode.pixel_width());
            Ok((id.to_string(), logical, width))
        })
        .collect()
}

fn windows() -> impl Iterator<Item = WindowRect> {
    let options =
        window::kCGWindowListOptionOnScreenOnly | window::kCGWindowListExcludeDesktopElements;
    // The window-list API specifies an array of dictionaries, in front-to-back order.
    let list = window::copy_window_info(options, window::kCGNullWindowID).map(|list| unsafe {
        CFArray::<CFDictionary<CFString, CFType>>::wrap_under_get_rule(list.as_concrete_TypeRef())
    });
    list.into_iter()
        .flat_map(|list| {
            // Borrow the framework's constant keys instead of allocating strings per window.
            let layer = unsafe { CFString::wrap_under_get_rule(window::kCGWindowLayer) };
            let alpha = unsafe { CFString::wrap_under_get_rule(window::kCGWindowAlpha) };
            let bounds_key = unsafe { CFString::wrap_under_get_rule(window::kCGWindowBounds) };
            (0..list.len()).filter_map(move |index| {
                let info = list.get(index)?;
                let number = |key: &CFString| info.find(key).and_then(|v| v.downcast::<CFNumber>());
                if number(&layer)?.to_i64()? != 0
                    || number(&alpha).and_then(|n| n.to_f64()).unwrap_or(1.0) <= 0.0
                {
                    return None;
                }
                let bounds = info.find(&bounds_key)?.downcast::<CFDictionary>()?;
                let bounds = rect(CGRect::from_dict_representation(&bounds)?);
                (bounds.w > 0 && bounds.h > 0).then_some(bounds)
            })
        })
        .enumerate()
        .map(|(index, rect)| WindowRect {
            rect,
            floating: true,
            focus_order: index as i64,
        })
}

impl Desktop for MacDesktop {
    fn focused_output(&self) -> Option<String> {
        // The frontmost normal window defines keyboard focus. With no window (for
        // example Finder's desktop), use the pointer's display rather than display 0.
        let point = self.active_window().map(|r| (r.x + r.w / 2, r.y + r.h / 2));
        let point = point.or_else(|| {
            use core_graphics::{
                event::CGEvent,
                event_source::{CGEventSource, CGEventSourceStateID},
            };
            let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
            let p = CGEvent::new(source).ok()?.location();
            Some((p.x as i32, p.y as i32))
        })?;
        CGDisplay::active_displays()
            .ok()?
            .into_iter()
            .find(|id| display_rect(*id).contains(point.0, point.1))
            .map(|id| id.to_string())
    }
    fn active_window(&self) -> Option<IRect> {
        windows().next().map(|w| w.rect)
    }
    fn visible_windows(&self) -> Vec<WindowRect> {
        windows().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::geometry::{CGPoint, CGSize};
    #[test]
    fn desktop_bounds_preserve_negative_origins_and_outward_rounding() {
        let cg = CGRect::new(
            &CGPoint::new(-1920.5, -20.25),
            &CGSize::new(1920.5, 1080.25),
        );
        assert_eq!(rect(cg), IRect::new(-1921, -21, 1921, 1081));
    }

    #[test]
    fn capture_size_respects_retina_scale_after_display_rotation() {
        assert_eq!(
            physical_size(IRect::new(-1080, 0, 1080, 1920), 1920, 3840),
            (2160, 3840)
        );
        assert_eq!(
            physical_size(IRect::new(0, 0, 1920, 1080), 1920, 1920),
            (1920, 1080)
        );
    }
}
