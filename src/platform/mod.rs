//! Everything that differs between desktops: grabbing screen pixels, knowing
//! where windows are, owning the clipboard, notifications, and how the
//! selection overlay is put on screen. The daemon only talks to the traits
//! here; [`connect`] picks the backends at start-up.

use std::sync::Arc;

use anyhow::Result;
use gpui::{App, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions};
#[cfg(not(windows))]
use gpui::{Bounds, DisplayId, point, px, size};

use crate::capture::Snapshot;
use crate::clipboard::PendingPng;
use crate::geom::IRect;
use crate::ipc::Request;
use crate::raster::Image;

#[cfg(target_os = "linux")]
pub mod freedesktop;
#[cfg(target_os = "linux")]
pub mod hyprland;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod wlroots;
#[cfg(target_os = "linux")]
pub mod x11;

/// Grabs screen pixels. Blocking: the daemon calls it on a background
/// thread.
pub trait Capture: Send + Sync {
    /// Captures the named outputs (all outputs when `None`).
    fn capture(&self, outputs: Option<&[String]>, cursor: bool) -> Result<Snapshot>;
}

/// What the window manager knows: used for window snapping in region mode
/// and for the `output` and `window` modes. Every query may answer "don't
/// know"; callers fall back sensibly.
pub trait Desktop: Send + Sync {
    /// Name of the output with keyboard focus (as in [`Snapshot`]).
    fn focused_output(&self) -> Option<String> {
        None
    }
    /// Logical rectangle of the focused window.
    fn active_window(&self) -> Option<IRect> {
        None
    }
    /// Windows that can be snapped to, topmost first.
    fn visible_windows(&self) -> Vec<WindowRect> {
        Vec::new()
    }
}

/// A desktop the window manager tells nothing about.
pub struct NoDesktop;

impl Desktop for NoDesktop {}

/// Clipboard ownership for the daemon, which usually has no focused window.
pub trait Clipboard: Send + Sync {
    /// Offers `image` as PNG (encoded in the background) and takes the
    /// selection.
    fn set_image(&self, image: Arc<Image>) -> Arc<PendingPng>;
    /// Offers UTF-8 text and takes the selection.
    fn set_text(&self, text: &str);
}

/// Desktop notifications. Clicks on a capture notification come back as
/// [`Event::OpenShot`].
pub trait Notifier: Send + Sync {
    /// "Screenshot captured" with a thumbnail; clicking it opens the editor.
    fn shot_captured(&self, shot: u64, image: &Image, body: &str, timeout_ms: i32) -> Result<()>;
    /// Plain notification without actions (errors, saves, toggles).
    fn message(&self, summary: &str, body: &str) -> Result<()>;
}

/// Something the desktop asks the daemon to do: a notification click, or a
/// request from a tray menu or global hotkey (platforms without
/// compositor-level keybinds).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    OpenShot(u64),
    Request(Request),
}

/// Delivers [`Event`]s to the daemon from any thread.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// A window that can be snapped to in region mode.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowRect {
    pub rect: IRect,
    pub floating: bool,
    /// 0 = most recently focused.
    pub focus_order: i64,
}

/// The topmost window under a logical point.
pub fn window_at(windows: &[WindowRect], x: i32, y: i32) -> Option<IRect> {
    windows
        .iter()
        .find(|w| w.rect.contains(x, y))
        .map(|w| w.rect)
}

/// How the per-output selection overlays become windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surfaces {
    /// `wlr-layer-shell` overlay surfaces: above everything, no animation,
    /// exclusive keyboard.
    #[cfg(target_os = "linux")]
    LayerShell,
    /// Override-redirect popups, one per X11 monitor.
    #[cfg(target_os = "linux")]
    X11,
    #[cfg(windows)]
    Windows,
}

/// The backends for this session.
pub struct Platform {
    /// Short backend name for logs and `quickscope status`.
    pub name: &'static str,
    pub capture: Option<Arc<dyn Capture>>,
    pub desktop: Arc<dyn Desktop>,
    pub clipboard: Option<Arc<dyn Clipboard>>,
    pub notifier: Option<Arc<dyn Notifier>>,
    pub surfaces: Surfaces,
}

/// Connects the backends for the current session. Missing pieces are
/// logged and left `None`; the daemon reports them when a request needs
/// them. `events` may be called from any thread.
pub fn connect(events: EventSink) -> Platform {
    #[cfg(target_os = "linux")]
    {
        let notifier = freedesktop::Notifications::spawn(events.clone())
            .map(|n| Arc::new(n) as Arc<dyn Notifier>)
            .map_err(|e| log::warn!("notifications unavailable: {e:#}"))
            .ok();
        let backend = std::env::var("QUICKSCOPE_BACKEND").ok();
        if backend.as_deref() == Some("x11")
            || (backend.is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
                && std::env::var_os("DISPLAY").is_some())
        {
            return Platform {
                name: "x11",
                capture: x11::ScreenCapture::spawn()
                    .map(|c| Arc::new(c) as Arc<dyn Capture>)
                    .map_err(|e| log::error!("X11 capture unavailable: {e:#}"))
                    .ok(),
                desktop: x11::Ewmh::connect()
                    .map(|d| Arc::new(d) as Arc<dyn Desktop>)
                    .unwrap_or_else(|e| {
                        log::warn!("X11 desktop unavailable: {e:#}");
                        Arc::new(NoDesktop)
                    }),
                clipboard: x11::SelectionOwner::spawn()
                    .map(|c| Arc::new(c) as Arc<dyn Clipboard>)
                    .map_err(|e| log::warn!("X11 clipboard unavailable: {e:#}"))
                    .ok(),
                notifier,
                surfaces: Surfaces::X11,
            };
        }
        let desktop: Arc<dyn Desktop> = if hyprland::available() {
            Arc::new(hyprland::Hyprland)
        } else {
            Arc::new(NoDesktop)
        };
        let capture = wlroots::Screencopy::spawn()
            .map(|c| Arc::new(c) as Arc<dyn Capture>)
            .map_err(|e| log::error!("screen capture unavailable: {e:#}"))
            .ok();
        let clipboard = wlroots::DataControl::spawn()
            .map(|c| Arc::new(c) as Arc<dyn Clipboard>)
            .map_err(|e| log::warn!("clipboard unavailable: {e:#}"))
            .ok();
        Platform {
            name: "wlroots",
            capture,
            desktop,
            clipboard,
            notifier,
            surfaces: Surfaces::LayerShell,
        }
    }
    #[cfg(windows)]
    {
        windows::connect(events)
    }
}

impl Platform {
    /// Window options for the selection overlay covering `logical` (global
    /// logical coordinates) on the output called `name`.
    pub fn overlay_options(&self, name: &str, logical: IRect, _cx: &App) -> WindowOptions {
        #[cfg(not(windows))]
        let display_id = display_for(name, logical, _cx);
        #[cfg(not(windows))]
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(logical.w as f32), px(logical.h as f32)),
        };
        match self.surfaces {
            #[cfg(target_os = "linux")]
            Surfaces::X11 => WindowOptions {
                titlebar: None,
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(logical.x as f32), px(logical.y as f32)),
                    ..bounds
                })),
                display_id,
                app_id: Some("quickscope".into()),
                window_background: WindowBackgroundAppearance::Opaque,
                kind: WindowKind::PopUp,
                ..Default::default()
            },
            #[cfg(target_os = "linux")]
            Surfaces::LayerShell => {
                use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
                WindowOptions {
                    titlebar: None,
                    focus: true,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    display_id,
                    app_id: Some("quickscope".into()),
                    window_background: WindowBackgroundAppearance::Opaque,
                    kind: WindowKind::LayerShell(LayerShellOptions {
                        namespace: "quickscope-selection".into(),
                        layer: Layer::Overlay,
                        anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                        exclusive_zone: Some(px(-1.)),
                        keyboard_interactivity: KeyboardInteractivity::Exclusive,
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            }
            #[cfg(windows)]
            Surfaces::Windows => {
                let (display_id, bounds) = windows::overlay_geometry(name, logical);
                WindowOptions {
                    titlebar: None,
                    focus: true,
                    show: false,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    display_id,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Opaque,
                    ..Default::default()
                }
            }
        }
    }

    /// An invisible window opened (and closed again) at start-up so the GPU
    /// device, swapchain format and shader pipelines exist before the first
    /// capture needs a window. `None` where that would flash on screen.
    pub fn prewarm_options(&self) -> Option<WindowOptions> {
        match self.surfaces {
            #[cfg(target_os = "linux")]
            Surfaces::X11 => None,
            #[cfg(target_os = "linux")]
            Surfaces::LayerShell => {
                use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
                Some(WindowOptions {
                    titlebar: None,
                    focus: false,
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(1.), px(1.)),
                    })),
                    window_background: WindowBackgroundAppearance::Transparent,
                    kind: WindowKind::LayerShell(LayerShellOptions {
                        namespace: "quickscope-prewarm".into(),
                        layer: Layer::Background,
                        anchor: Anchor::TOP | Anchor::LEFT,
                        keyboard_interactivity: KeyboardInteractivity::None,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            #[cfg(windows)]
            Surfaces::Windows => None,
        }
    }
}

/// GPUI's display for an output: Wayland displays are named by a v5 UUID
/// of the output name; elsewhere the display whose bounds start at the
/// output's logical origin.
#[cfg(not(windows))]
fn display_for(name: &str, logical: IRect, cx: &App) -> Option<DisplayId> {
    let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes());
    let displays = cx.displays();
    displays
        .iter()
        .find(|d| d.uuid().ok() == Some(uuid))
        .or_else(|| {
            displays.iter().find(|d| {
                let o = d.bounds().origin;
                (f32::from(o.x) as i32, f32::from(o.y) as i32) == (logical.x, logical.y)
            })
        })
        .map(|d| d.id())
}
