use std::time::{Duration, Instant};

use crate::geom::IRect;
use anyhow::{Context, Result, bail};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        ConfigureWindowAux, ConnectionExt as _, InputFocus, MapState, StackMode, Window,
    },
    rust_connection::RustConnection,
};

/// Override-redirect windows bypass WM focus policy. Set native focus only once
/// they are viewable, and return it without stealing a later focus change.
/// The normal X11 implicit button grab keeps drags alive across monitors.
pub struct SelectionFocus {
    conn: RustConnection,
    previous: Window,
    revert: InputFocus,
    windows: Vec<Window>,
}

impl SelectionFocus {
    pub fn acquire(outputs: Vec<(Window, IRect)>) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let windows: Vec<_> = outputs.iter().map(|(window, _)| *window).collect();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let ready = windows.iter().all(|window| {
                conn.get_window_attributes(*window)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|a| a.map_state == MapState::VIEWABLE)
            });
            if ready {
                break;
            }
            if Instant::now() >= deadline {
                bail!("X11 selection windows did not become viewable");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // GPUI's X11 creation path adds two pixels to x, and applies the UI's
        // DPI scale. Frozen overlays must cover the exact physical root rect.
        for (window, rect) in outputs {
            conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(rect.x)
                    .y(rect.y)
                    .width(rect.w as u32)
                    .height(rect.h as u32)
                    .stack_mode(StackMode::ABOVE),
            )?
            .check()?;
        }
        let previous = conn.get_input_focus()?.reply()?;
        let root = conn.setup().roots[screen].root;
        let pointer = conn.query_pointer(root)?.reply()?;
        let target = windows
            .iter()
            .copied()
            .find(|window| {
                let Ok(cookie) = conn.get_geometry(*window) else {
                    return false;
                };
                let Ok(g) = cookie.reply() else {
                    return false;
                };
                pointer.root_x >= g.x
                    && pointer.root_y >= g.y
                    && i32::from(pointer.root_x) < i32::from(g.x) + i32::from(g.width)
                    && i32::from(pointer.root_y) < i32::from(g.y) + i32::from(g.height)
            })
            .or_else(|| windows.first().copied())
            .context("no X11 selection windows")?;
        conn.set_input_focus(InputFocus::POINTER_ROOT, target, x11rb::CURRENT_TIME)?
            .check()?;
        let focused = conn.get_input_focus()?.reply()?;
        if focused.focus != target {
            bail!("X11 refused selection keyboard focus");
        }
        Ok(Self {
            conn,
            previous: previous.focus,
            revert: previous.revert_to,
            windows,
        })
    }
}

impl Drop for SelectionFocus {
    fn drop(&mut self) {
        if let Ok(cookie) = self.conn.get_input_focus()
            && let Ok(focus) = cookie.reply()
            && self.windows.contains(&focus.focus)
        {
            let _ = self
                .conn
                .set_input_focus(self.revert, self.previous, x11rb::CURRENT_TIME)
                .map(|cookie| cookie.check());
            let _ = self.conn.flush();
        }
    }
}
