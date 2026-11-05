//! Hyprland IPC: focused monitor, active window and visible windows, used to
//! resolve `output`/`window` captures and window snapping in region mode.
//! Other compositors simply return `None` here.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::geom::IRect;

#[derive(Debug, Deserialize)]
struct WorkspaceRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Monitor {
    name: String,
    focused: bool,
    active_workspace: WorkspaceRef,
    special_workspace: Option<WorkspaceRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Client {
    at: [i32; 2],
    size: [i32; 2],
    workspace: WorkspaceRef,
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    focus_history_id: i64,
}

fn yes() -> bool {
    true
}

/// A window that can be snapped to in region mode.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowRect {
    pub rect: IRect,
    pub floating: bool,
    /// 0 = most recently focused.
    pub focus_order: i64,
}

fn socket() -> Option<PathBuf> {
    let base = crate::config::runtime_dir().join("hypr");
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        return Some(base.join(sig).join(".socket.sock"));
    }
    // Daemons started outside the compositor's environment (systemd units
    // without imported variables) still find a lone running instance.
    let mut instances = std::fs::read_dir(&base)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join(".socket.sock"))
        .filter(|p| p.exists());
    let first = instances.next()?;
    instances.next().is_none().then_some(first)
}

fn query(command: &str) -> Result<Vec<u8>> {
    let path = socket().context("Hyprland is not running")?;
    let mut s =
        UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    s.write_all(command.as_bytes())?;
    let mut out = Vec::with_capacity(16 * 1024);
    s.read_to_end(&mut out)?;
    Ok(out)
}

pub fn available() -> bool {
    socket().is_some()
}

pub fn focused_output() -> Option<String> {
    let monitors: Vec<Monitor> = serde_json::from_slice(&query("j/monitors").ok()?).ok()?;
    monitors.into_iter().find(|m| m.focused).map(|m| m.name)
}

pub fn active_window() -> Option<IRect> {
    #[derive(Deserialize)]
    struct Active {
        at: [i32; 2],
        size: [i32; 2],
    }
    let w: Active = serde_json::from_slice(&query("j/activewindow").ok()?).ok()?;
    let rect = IRect::new(w.at[0], w.at[1], w.size[0], w.size[1]);
    (!rect.is_empty()).then_some(rect)
}

/// Windows on currently visible workspaces, topmost candidates first.
pub fn visible_windows() -> Vec<WindowRect> {
    let (Ok(monitors), Ok(clients)) = (query("j/monitors"), query("j/clients")) else {
        return Vec::new();
    };
    parse_visible_windows(&monitors, &clients).unwrap_or_default()
}

fn parse_visible_windows(monitors: &[u8], clients: &[u8]) -> Result<Vec<WindowRect>> {
    let monitors: Vec<Monitor> = serde_json::from_slice(monitors)?;
    let clients: Vec<Client> = serde_json::from_slice(clients)?;
    let visible: Vec<i64> = monitors
        .iter()
        .flat_map(|m| {
            [
                Some(m.active_workspace.id),
                m.special_workspace
                    .as_ref()
                    .map(|w| w.id)
                    .filter(|id| *id != 0),
            ]
        })
        .flatten()
        .collect();
    let mut windows: Vec<WindowRect> = clients
        .into_iter()
        .filter(|c| c.mapped && !c.hidden && visible.contains(&c.workspace.id))
        .map(|c| WindowRect {
            rect: IRect::new(c.at[0], c.at[1], c.size[0], c.size[1]),
            floating: c.floating,
            focus_order: c.focus_history_id,
        })
        .filter(|w| !w.rect.is_empty())
        .collect();
    // Floating windows sit above tiled ones; among each group, the most
    // recently focused is the best guess for what is on top.
    windows.sort_by_key(|w| (!w.floating, w.focus_order));
    Ok(windows)
}

/// The topmost window under a logical point.
pub fn window_at(windows: &[WindowRect], x: i32, y: i32) -> Option<IRect> {
    windows
        .iter()
        .find(|w| w.rect.contains(x, y))
        .map(|w| w.rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITORS: &str = r#"[
        {"name":"DP-1","focused":true,"activeWorkspace":{"id":2,"name":"2"},
         "specialWorkspace":{"id":0,"name":""}},
        {"name":"DP-2","focused":false,"activeWorkspace":{"id":11,"name":"11"},
         "specialWorkspace":{"id":-98,"name":"special:chat"}}
    ]"#;

    const CLIENTS: &str = r#"[
        {"at":[0,0],"size":[1280,1440],"workspace":{"id":2},"floating":false,"focusHistoryID":1,"mapped":true,"hidden":false},
        {"at":[100,100],"size":[400,300],"workspace":{"id":2},"floating":true,"focusHistoryID":3,"mapped":true,"hidden":false},
        {"at":[0,0],"size":[500,500],"workspace":{"id":5},"floating":false,"focusHistoryID":0,"mapped":true,"hidden":false},
        {"at":[2600,40],"size":[800,600],"workspace":{"id":-98},"floating":true,"focusHistoryID":2,"mapped":true,"hidden":false},
        {"at":[2600,40],"size":[10,10],"workspace":{"id":11},"floating":false,"focusHistoryID":4,"mapped":true,"hidden":true}
    ]"#;

    #[test]
    fn keeps_only_windows_on_visible_workspaces() {
        let w = parse_visible_windows(MONITORS.as_bytes(), CLIENTS.as_bytes()).unwrap();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w.iter().all(|w| w.rect != IRect::new(0, 0, 500, 500)));
    }

    #[test]
    fn floating_windows_win_hit_tests() {
        let w = parse_visible_windows(MONITORS.as_bytes(), CLIENTS.as_bytes()).unwrap();
        assert_eq!(
            window_at(&w, 150, 150),
            Some(IRect::new(100, 100, 400, 300))
        );
        assert_eq!(window_at(&w, 1000, 150), Some(IRect::new(0, 0, 1280, 1440)));
        assert_eq!(
            window_at(&w, 2700, 50),
            Some(IRect::new(2600, 40, 800, 600))
        );
        assert_eq!(window_at(&w, 4000, 1000), None);
    }
}
