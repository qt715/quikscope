//! EWMH desktop queries on a persistent, background-thread-safe X11 connection.
//! Missing WM support and windows disappearing between requests are unknown
//! answers, not backend errors. X11 root pixels are the desktop coordinates.

use std::sync::Mutex;

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, GetPropertyReply, MapState, Window};
use x11rb::rust_connection::RustConnection;

use crate::geom::IRect;
use crate::platform::{Desktop, WindowRect};

use super::monitors;

/// EWMH window knowledge. Construction needs only an X server, not an EWMH WM.
/// Every query rechecks the WM's supporting-window handshake, allowing a WM to
/// start, exit, or restart without reconnecting the backend.
pub struct Ewmh {
    conn: Mutex<RustConnection>,
    screen: usize,
    root: Window,
    atoms: Atoms,
}

struct Atoms {
    supporting: Atom,
    active: Atom,
    stacking: Atom,
    frame_extents: Atom,
    current_desktop: Atom,
    desktop: Atom,
    state: Atom,
    hidden: Atom,
    sticky: Atom,
    wm_state: Atom,
}

impl Ewmh {
    pub fn connect() -> Result<Self> {
        let (conn, screen) = RustConnection::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let intern =
            |name: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let atoms = Atoms {
            supporting: intern(b"_NET_SUPPORTING_WM_CHECK")?,
            active: intern(b"_NET_ACTIVE_WINDOW")?,
            stacking: intern(b"_NET_CLIENT_LIST_STACKING")?,
            frame_extents: intern(b"_NET_FRAME_EXTENTS")?,
            current_desktop: intern(b"_NET_CURRENT_DESKTOP")?,
            desktop: intern(b"_NET_WM_DESKTOP")?,
            state: intern(b"_NET_WM_STATE")?,
            hidden: intern(b"_NET_WM_STATE_HIDDEN")?,
            sticky: intern(b"_NET_WM_STATE_STICKY")?,
            wm_state: intern(b"WM_STATE")?,
        };
        Ok(Self {
            conn: Mutex::new(conn),
            screen,
            root,
            atoms,
        })
    }

    fn supported(&self, conn: &RustConnection) -> bool {
        let Some(wm) = scalar(
            conn,
            self.root,
            self.atoms.supporting,
            AtomEnum::WINDOW.into(),
        )
        .filter(|wm| *wm != 0 && *wm != self.root) else {
            return false;
        };
        scalar(conn, wm, self.atoms.supporting, AtomEnum::WINDOW.into()) == Some(wm)
    }

    fn active_rect(&self, conn: &RustConnection) -> Option<IRect> {
        if !self.supported(conn) {
            return None;
        }
        let window = scalar(conn, self.root, self.atoms.active, AtomEnum::WINDOW.into())?;
        (window != 0).then_some(())?;
        self.frame_rect(conn, window)
    }

    fn frame_rect(&self, conn: &RustConnection, window: Window) -> Option<IRect> {
        let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
        if geometry.root != self.root || geometry.width == 0 || geometry.height == 0 {
            return None;
        }
        // Geometry x/y are parent-relative, including with reparenting WMs.
        // Translate the client's inner origin, then expand by EWMH decorations.
        let origin = conn
            .translate_coordinates(window, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        if !origin.same_screen {
            return None;
        }
        let extents = property(
            conn,
            window,
            self.atoms.frame_extents,
            AtomEnum::CARDINAL.into(),
        );
        let mut sides = [0; 4];
        if let Some(extents) = extents.filter(|values| values.value_len == 4) {
            for (side, value) in sides.iter_mut().zip(extents.value32()?) {
                *side = i32::try_from(value).ok()?;
            }
        }
        let [left, right, top, bottom] = sides;
        let x = i32::from(origin.dst_x).checked_sub(left)?;
        let y = i32::from(origin.dst_y).checked_sub(top)?;
        let w = i32::from(geometry.width)
            .checked_add(left)?
            .checked_add(right)?;
        let h = i32::from(geometry.height)
            .checked_add(top)?
            .checked_add(bottom)?;
        // IRect's containment helpers add the dimensions without checking.
        x.checked_add(w)?;
        y.checked_add(h)?;
        Some(IRect::new(x, y, w, h))
    }

    fn visible(&self, conn: &RustConnection, window: Window, current: Option<u32>) -> bool {
        let Ok(cookie) = conn.get_window_attributes(window) else {
            return false;
        };
        let Ok(attributes) = cookie.reply() else {
            return false;
        };
        if attributes.map_state != MapState::VIEWABLE {
            return false;
        }
        let state = property(conn, window, self.atoms.state, AtomEnum::ATOM.into());
        let has_state = |atom| {
            state
                .as_ref()
                .and_then(|reply| reply.value32())
                .is_some_and(|mut values| values.any(|value| value == atom))
        };
        // ICCCM IconicState is 3. Some WMs expose this before updating HIDDEN.
        let iconic = property(conn, window, self.atoms.wm_state, self.atoms.wm_state)
            .and_then(|reply| reply.value32()?.next())
            == Some(3);
        visible_on_desktop(
            true,
            has_state(self.atoms.hidden),
            iconic,
            scalar(conn, window, self.atoms.desktop, AtomEnum::CARDINAL.into()),
            current,
            has_state(self.atoms.sticky),
        )
    }
}

impl Desktop for Ewmh {
    fn focused_output(&self) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        if !self.supported(&conn) {
            return None;
        }
        let outputs = monitors(&conn, self.screen).ok()?;
        if let Some(rect) = self.active_rect(&conn) {
            let x = rect.x + rect.w / 2;
            let y = rect.y + rect.h / 2;
            if let Some(output) = outputs.iter().find(|output| output.rect.contains(x, y)) {
                return Some(output.name.clone());
            }
        }
        let pointer = conn.query_pointer(self.root).ok()?.reply().ok()?;
        if !pointer.same_screen {
            return None;
        }
        outputs
            .into_iter()
            .find(|output| {
                output
                    .rect
                    .contains(i32::from(pointer.root_x), i32::from(pointer.root_y))
            })
            .map(|output| output.name)
    }

    fn active_window(&self) -> Option<IRect> {
        let conn = self.conn.lock().ok()?;
        self.active_rect(&conn)
    }

    fn visible_windows(&self) -> Vec<WindowRect> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        if !self.supported(&conn) {
            return Vec::new();
        }
        let Some(clients) = property(
            &conn,
            self.root,
            self.atoms.stacking,
            AtomEnum::WINDOW.into(),
        ) else {
            return Vec::new();
        };
        let current = scalar(
            &conn,
            self.root,
            self.atoms.current_desktop,
            AtomEnum::CARDINAL.into(),
        );
        // RustConnection uses native byte order. Decode directly from the
        // reply so reversing the stack needs no intermediate window-ID vector.
        let clients = clients
            .value
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        topmost_windows(clients, |window| {
            if !self.visible(&conn, window, current) {
                return None;
            }
            self.frame_rect(&conn, window)
        })
    }
}

fn property(
    conn: &RustConnection,
    window: Window,
    name: Atom,
    kind: Atom,
) -> Option<GetPropertyReply> {
    let reply = conn
        .get_property(false, window, name, kind, 0, u32::MAX)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ != kind || reply.format != 32 || reply.bytes_after != 0 {
        return None;
    }
    Some(reply)
}

fn scalar(conn: &RustConnection, window: Window, name: Atom, kind: Atom) -> Option<u32> {
    let reply = property(conn, window, name, kind)?;
    if reply.value_len != 1 {
        return None;
    }
    reply.value32()?.next()
}

fn visible_on_desktop(
    mapped: bool,
    hidden: bool,
    iconic: bool,
    desktop: Option<u32>,
    current: Option<u32>,
    sticky: bool,
) -> bool {
    mapped
        && !hidden
        && !iconic
        && (sticky
            || desktop == Some(u32::MAX)
            || desktop.is_none()
            || current.is_none()
            || desktop == current)
}

fn topmost_windows(
    clients: impl DoubleEndedIterator<Item = Window>,
    query: impl FnMut(Window) -> Option<IRect>,
) -> Vec<WindowRect> {
    // EWMH defines bottom-to-top ordering. Preserve that order exactly after
    // reversing; focus history and tiled/floating status are not in EWMH.
    clients
        .rev()
        .filter_map(query)
        .enumerate()
        .map(|(order, rect)| WindowRect {
            rect,
            floating: false,
            focus_order: order as i64,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_excludes_unmapped_hidden_and_minimized_windows() {
        for (mapped, hidden, iconic) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert!(!visible_on_desktop(
                mapped,
                hidden,
                iconic,
                Some(1),
                Some(1),
                false
            ));
        }
        assert!(visible_on_desktop(
            true,
            false,
            false,
            Some(1),
            Some(1),
            false
        ));
    }

    #[test]
    fn visibility_handles_other_desktops_sticky_and_unknown_properties() {
        assert!(!visible_on_desktop(
            true,
            false,
            false,
            Some(2),
            Some(1),
            false
        ));
        assert!(visible_on_desktop(
            true,
            false,
            false,
            Some(2),
            Some(1),
            true
        ));
        assert!(visible_on_desktop(
            true,
            false,
            false,
            Some(u32::MAX),
            Some(1),
            false
        ));
        assert!(visible_on_desktop(true, false, false, None, Some(1), false));
        assert!(visible_on_desktop(true, false, false, Some(2), None, false));
        assert!(!visible_on_desktop(
            true,
            true,
            false,
            Some(u32::MAX),
            Some(1),
            true
        ));
    }

    #[test]
    fn stacking_is_topmost_first_and_skips_unavailable_windows() {
        let windows = topmost_windows([10, 20, 30, 40].into_iter(), |id| {
            (id != 30).then(|| IRect::new(id as i32, 0, 100, 100))
        });
        assert_eq!(
            windows
                .iter()
                .map(|window| window.rect.x)
                .collect::<Vec<_>>(),
            [40, 20, 10]
        );
        assert_eq!(
            windows
                .iter()
                .map(|window| window.focus_order)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            crate::platform::window_at(&windows, 50, 50),
            Some(windows[0].rect)
        );
        assert!(topmost_windows([].into_iter(), |_| unreachable!()).is_empty());
        assert!(topmost_windows([10].into_iter(), |_| None).is_empty());
    }
}
