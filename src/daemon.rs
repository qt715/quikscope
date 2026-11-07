//! The long-running daemon: owns the GPUI application (so editor windows
//! open without GPU start-up cost), the capture/clipboard/notification
//! threads, recent shots, and the IPC socket.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read as _, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, AppContext as _, AsyncApp, BorrowAppContext as _, DisplayId, Entity, Global, QuitMode,
    WindowBounds, WindowHandle, WindowOptions, px, size,
};

use crate::cache::LastCapture;
use crate::capture::{Composed, Snapshot};
use crate::config::{self, Config};
use crate::document::Document;
use crate::geom::IRect;
use crate::ipc::{CaptureOptions, Mode, Request, Response, Switch};
use crate::platform::{self, Capture, Clipboard, Event, Notifier, Platform};
use crate::raster::{Compression, Image};
use crate::ui::editor::{Editor, EditorServices, LastTool};
use crate::ui::overlay::{Overlay, Selection};
use crate::ui::settings::{OpenSettings, Settings};
use crate::ui::{self, Assets, Shot};

/// Shots kept in memory (newest last). Older ones remain reachable through
/// the on-disk cache only for the most recent capture.
const KEEP_SHOTS: usize = 8;

enum Msg {
    /// From the IPC socket; the response goes back to the client.
    Request(Request, oneshot::Sender<Response>),
    /// From the desktop: notification clicks, tray menus, global hotkeys.
    Desktop(Event),
}

struct Daemon {
    config: Arc<Config>,
    platform: Arc<Platform>,
    /// Sole writer of the `open-last` cache file.
    cache: LastCapture,
    shots: VecDeque<Arc<Shot>>,
    docs: HashMap<u64, Entity<Document>>,
    editors: HashMap<u64, WindowHandle<Editor>>,
    settings: Option<WindowHandle<Settings>>,
    next_id: u64,
    selecting: bool,
    /// Capture notifications; starts from the config, changed by
    /// `quikscope notifications …` (which also persists it).
    notify: bool,
}

impl Global for Daemon {}

/// Binds the socket, refusing to replace a live daemon.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!(
                "a quikscope daemon is already listening on {}",
                socket.display()
            );
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub fn run(socket: PathBuf) -> Result<()> {
    #[cfg(windows)]
    platform::windows::initialize()?;
    let started = Instant::now();
    let welcome = matches!(
        std::fs::metadata(config::config_path()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    let config = Arc::new(Config::load().unwrap_or_else(|error| {
        log::error!("configuration could not be loaded; using defaults: {error:#}");
        Config::default()
    }));
    // Index system fonts off the main thread before the font menu needs them.
    crate::fonts::warm();
    // The primary OCR model loads in the background and stays loaded, so
    // text extraction starts reading immediately.
    crate::ocr::warm(&config.ocr.languages);
    let listener = bind(&socket)?;
    let (tx, rx) = mpsc::unbounded::<Msg>();
    spawn_listener(listener, tx.clone());

    // Platform backends connect concurrently with GPUI start-up.
    let services = std::thread::spawn({
        let tx = tx.clone();
        move || {
            platform::connect(Arc::new(move |event| {
                let _ = tx.unbounded_send(Msg::Desktop(event));
            }))
        }
    });

    let socket_for_exit = socket.clone();
    gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            ui::init(cx);
            let platform = Arc::new(services.join().expect("platform start-up panicked"));
            log::info!("platform backend: {}", platform.name);
            cx.set_global(EditorServices {
                config: config.clone(),
                clipboard: platform.clipboard.clone(),
            });
            let prewarm = platform.prewarm_options();
            cx.set_global(Daemon {
                notify: config.notify,
                config,
                platform,
                cache: LastCapture::spawn(config::last_capture_path()),
                shots: VecDeque::new(),
                docs: HashMap::new(),
                editors: HashMap::new(),
                settings: None,
                next_id: 1,
                selecting: false,
            });
            if let Some(options) = prewarm {
                prewarm_gpu(options, cx);
            }
            cx.on_action::<OpenSettings>(|_, cx| {
                if let Err(error) = open_settings(false, cx) {
                    log::error!("opening settings: {error:#}");
                }
            });
            if welcome && let Err(error) = open_settings(true, cx) {
                log::error!("opening welcome settings: {error:#}");
            }
            log::info!(
                "daemon ready on {} in {:.0}ms",
                socket.display(),
                started.elapsed().as_secs_f64() * 1e3
            );
            cx.spawn(async move |cx| {
                let mut rx = rx;
                while let Some(msg) = rx.next().await {
                    match msg {
                        Msg::Request(req, reply) => {
                            cx.spawn(async move |cx| {
                                let quit = req == Request::Quit;
                                let response = handle(req, cx).await;
                                let _ = reply.send(response);
                                if quit {
                                    // Give the IPC thread a moment to flush the reply.
                                    cx.background_executor()
                                        .timer(Duration::from_millis(20))
                                        .await;
                                    cx.update(|cx| cx.quit());
                                }
                            })
                            .detach();
                        }
                        Msg::Desktop(Event::OpenShot(id)) => {
                            cx.update(|cx| {
                                if let Err(e) = open_editor(id, EditorTool::Default, cx) {
                                    log::error!("opening editor: {e:#}");
                                }
                            });
                        }
                        Msg::Desktop(Event::Request(req)) => {
                            cx.spawn(async move |cx| {
                                let quit = req == Request::Quit;
                                log::debug!("desktop request: {req}");
                                if let Response::Error(e) = handle(req, cx).await {
                                    notify_error(&e, cx);
                                }
                                if quit {
                                    cx.update(|cx| cx.quit());
                                }
                            })
                            .detach();
                        }
                    }
                }
            })
            .detach();
        });
    let _ = std::fs::remove_file(socket_for_exit);
    Ok(())
}

/// Opens and immediately closes an invisible window so the GPU device,
/// swapchain format and shader pipelines exist before the first capture
/// needs a window.
fn prewarm_gpu(options: WindowOptions, cx: &mut App) {
    struct Blank;
    impl gpui::Render for Blank {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let started = Instant::now();
    let opened = cx.open_window(options, |_, cx| cx.new(|_| Blank));
    match opened {
        Ok(handle) => {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let _ = handle.update(cx, |_, window, _| window.remove_window());
                log::debug!(
                    "GPU prewarmed in {:.0}ms",
                    started.elapsed().as_secs_f64() * 1e3
                );
            })
            .detach();
        }
        Err(e) => log::debug!("GPU prewarm skipped: {e:#}"),
    }
}

fn spawn_listener(listener: UnixListener, tx: mpsc::UnboundedSender<Msg>) {
    std::thread::Builder::new()
        .name("quikscope-ipc".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let tx = tx.clone();
                // One thread per connection: an interactive selection must not
                // block `open-last` or `ping` arriving meanwhile.
                std::thread::spawn(move || {
                    if let Err(e) = serve(stream, tx) {
                        log::debug!("ipc connection: {e:#}");
                    }
                });
            }
        })
        .expect("spawning the IPC thread");
}

fn serve(stream: UnixStream, tx: mpsc::UnboundedSender<Msg>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let line = read_request_line(&stream)?;
    if line.is_empty() {
        // Connected and closed without a request (a liveness probe).
        return Ok(());
    }
    let response = match parse_request_line(&line) {
        Err(e) => Response::Error(e),
        Ok(req) => {
            log::debug!("request: {req}");
            let (reply, rx) = oneshot::channel();
            tx.unbounded_send(Msg::Request(req, reply))
                .map_err(|_| anyhow!("daemon is shutting down"))?;
            futures::executor::block_on(rx).unwrap_or(Response::Error("request dropped".into()))
        }
    };
    let mut stream = stream;
    writeln!(stream, "{response}")?;
    Ok(())
}

const IPC_LINE_LIMIT: u64 = 64 * 1024;

fn read_request_line(reader: impl std::io::Read) -> std::io::Result<String> {
    let mut line = String::new();
    BufReader::new(reader)
        .take(IPC_LINE_LIMIT + 1)
        .read_line(&mut line)?;
    Ok(line)
}

fn parse_request_line(line: &str) -> Result<Request, String> {
    if line.len() as u64 > IPC_LINE_LIMIT {
        return Err("request exceeds 64 KiB".into());
    }
    Request::parse(line)
}

#[cfg(test)]
mod ipc_limit_tests {
    use super::*;

    #[test]
    fn hardening_reading_requests_is_bounded_without_a_newline() {
        let input = std::io::repeat(b'a');
        let line = read_request_line(input).unwrap();
        assert_eq!(line.len() as u64, IPC_LINE_LIMIT + 1);
        assert!(parse_request_line(&line).is_err());
    }

    #[test]
    fn hardening_oversized_request_is_rejected_before_parsing() {
        let prefix = "open ";
        let mut line = format!(
            "{prefix}{}",
            "a".repeat(IPC_LINE_LIMIT as usize - prefix.len())
        );
        assert!(parse_request_line(&line).is_ok());
        line.push('a');
        assert_eq!(
            parse_request_line(&line),
            Err("request exceeds 64 KiB".into())
        );
    }
}

async fn handle(req: Request, cx: &mut AsyncApp) -> Response {
    let result = match req {
        Request::Ping => return Response::Pong,
        Request::Quit => {
            // Finish the cache write so `open-last` after a restart sees
            // the newest capture.
            let cache = daemon(cx, |d, _| d.cache.clone());
            cx.background_spawn(async move { cache.flush() }).await;
            return Response::Pong;
        }
        Request::Capture(mode, opts) => capture(mode, opts, cx).await,
        Request::OpenLast => open_last(cx).await,
        Request::Notifications(switch) => set_notifications(switch, cx).await,
        Request::Open(path) => open_path(path, cx).await,
        Request::Settings => cx
            .update(|cx| open_settings(false, cx))
            .map(|_| Some(Response::Pong)),
    };
    match result {
        Ok(Some(r)) => r,
        Ok(None) => Response::Cancelled,
        Err(e) => {
            log::error!("{e:#}");
            Response::Error(format!("{e:#}"))
        }
    }
}

fn daemon<R>(cx: &mut AsyncApp, f: impl FnOnce(&mut Daemon, &mut App) -> R) -> R {
    cx.update(|cx| cx.update_global::<Daemon, R>(f))
}

async fn snapshot(
    capture: &Arc<dyn Capture>,
    outputs: Option<Vec<String>>,
    cursor: bool,
    cx: &mut AsyncApp,
) -> Result<Snapshot> {
    let capture = capture.clone();
    cx.background_spawn(async move { capture.capture(outputs.as_deref(), cursor) })
        .await
}

/// Errors from requests nobody waits on (tray menus, hotkeys) would
/// otherwise vanish into the log.
fn notify_error(message: &str, cx: &mut AsyncApp) {
    let Some(n) = daemon(cx, |d, _| d.platform.notifier.clone()) else {
        return;
    };
    let message = message.to_string();
    cx.background_spawn(async move {
        if let Err(e) = n.message("Screenshot failed", &message) {
            log::warn!("notification failed: {e:#}");
        }
    })
    .detach();
}

async fn capture(mode: Mode, opts: CaptureOptions, cx: &mut AsyncApp) -> Result<Option<Response>> {
    let started = Instant::now();
    let (config, platform) = daemon(cx, |d, _| (d.config.clone(), d.platform.clone()));
    let capturer = platform
        .capture
        .clone()
        .context("screen capture is unavailable (see the daemon log)")?;
    let desktop = platform.desktop.clone();
    let cursor = opts.cursor.unwrap_or(config.cursor);

    let composed: Composed = if let Some(rect) = opts.geometry {
        let snap = snapshot(&capturer, None, cursor, cx).await?;
        snap.compose(rect)
            .with_context(|| format!("geometry {rect} lies outside every output"))?
    } else {
        match mode {
            Mode::Screen => {
                let snap = snapshot(&capturer, None, cursor, cx).await?;
                snap.compose(snap.bounds())
                    .context("no outputs to capture")?
            }
            Mode::Output => {
                let name = match opts.output.clone() {
                    Some(n) => Some(n),
                    None => {
                        cx.background_spawn(async move { desktop.focused_output() })
                            .await
                    }
                };
                let snap = snapshot(&capturer, name.clone().map(|n| vec![n]), cursor, cx).await?;
                let frame = match &name {
                    Some(n) => snap.output(n),
                    None => snap.outputs.first(),
                }
                .context("no outputs to capture")?;
                Composed {
                    scale: frame.scale() as f32,
                    image: frame.image.clone(),
                }
            }
            Mode::Window => {
                let rect = cx
                    .background_spawn(async move { desktop.active_window() })
                    .await
                    .with_context(|| {
                        format!(
                            "window capture needs a focused window the {} backend can locate",
                            platform.name
                        )
                    })?;
                let snap = snapshot(&capturer, None, cursor, cx).await?;
                snap.compose(rect)
                    .context("the active window is not on any output")?
            }
            Mode::Region => {
                let live = if opts.freeze.unwrap_or(config.freeze) {
                    false
                } else {
                    let platform = platform.clone();
                    match cx
                        .background_spawn(async move { platform.live_selection_unavailable() })
                        .await
                    {
                        Some(reason) => {
                            log::warn!(
                                "live region selection unavailable ({reason}); falling back to frozen mode"
                            );
                            false
                        }
                        None => true,
                    }
                };
                if live {
                    cx.update(claim_selection)?;
                    let selected = select_region(None, &platform, None, cx).await;
                    let rect = match selected {
                        Ok(Some(rect)) => rect,
                        result => {
                            daemon(cx, |d, _| d.selecting = false);
                            result?;
                            return Ok(None);
                        }
                    };
                    // Keep the claim through capture, including portal permission:
                    // a second selector must not obscure the desktop or its dialog.
                    let snap = snapshot(&capturer, None, cursor, cx).await;
                    daemon(cx, |d, _| d.selecting = false);
                    snap?
                        .compose(rect)
                        .context("the selection lies outside every output")?
                } else {
                    // The capture runs while the overlays are being created.
                    let snap = {
                        let capturer = capturer.clone();
                        cx.background_spawn(async move { capturer.capture(None, cursor) })
                    };
                    let early = cx.update(|cx| begin_selection(&platform, cx))?;
                    log::debug!(
                        "selection claimed {:.1}ms after the request",
                        started.elapsed().as_secs_f64() * 1e3
                    );
                    let snap = match snap.await {
                        Ok(snap) => snap,
                        Err(e) => {
                            let windows = early.map(|e| e.windows.into_iter().map(|(_, w)| w));
                            end_selection(windows.into_iter().flatten(), cx).await;
                            return Err(e);
                        }
                    };
                    let Some(rect) = select_region(Some(&snap), &platform, early, cx).await? else {
                        return Ok(None);
                    };
                    cx.background_spawn(async move { snap.compose(rect) })
                        .await
                        .context("the selection lies outside every output")?
                }
            }
        }
    };
    log::debug!(
        "captured {}x{} in {:.1}ms",
        composed.image.width,
        composed.image.height,
        started.elapsed().as_secs_f64() * 1e3
    );
    let shot = add_shot(composed.image, composed.scale, None, cx);
    finish_capture(shot, opts, config, cx).await.map(Some)
}

/// Selection overlays opened while the capture is still in flight.
struct EarlyOverlays {
    selection: Entity<Selection>,
    done: oneshot::Receiver<Option<IRect>>,
    windows: Vec<(DisplayId, WindowHandle<Overlay>)>,
}

fn claim_selection(cx: &mut App) -> Result<()> {
    if cx.update_global::<Daemon, _>(|d, _| std::mem::replace(&mut d.selecting, true)) {
        bail!("a selection is already in progress");
    }
    Ok(())
}

/// Claims the (single) interactive selection and, where the platform allows
/// it, opens a transparent overlay per display right away: creating the
/// windows then overlaps with the compositor producing the frame instead of
/// following it.
fn begin_selection(platform: &Platform, cx: &mut App) -> Result<Option<EarlyOverlays>> {
    claim_selection(cx)?;
    let displays = cx.displays();
    if displays
        .first()
        .and_then(|d| platform.early_overlay_options(d.id(), d.bounds().size))
        .is_none()
    {
        return Ok(None);
    }
    let (selection, done) = Selection::new(Vec::new());
    let selection = cx.new(|_| selection);
    let mut windows = Vec::with_capacity(displays.len());
    for display in displays {
        let id = display.id();
        let Some(options) = platform.early_overlay_options(id, display.bounds().size) else {
            continue;
        };
        let selection = selection.clone();
        match cx.open_window(options, |window, cx| {
            cx.new(|cx| Overlay::pending(selection, window, cx))
        }) {
            Ok(window) => windows.push((id, window)),
            Err(e) => {
                // The regular path opens its own overlays after the capture.
                log::debug!("opening an early selection overlay: {e:#}");
                for (_, window) in windows {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                }
                return Ok(None);
            }
        }
    }
    Ok(Some(EarlyOverlays {
        selection,
        done,
        windows,
    }))
}

/// Closes selection overlays and releases the selection claim.
async fn end_selection(
    overlays: impl IntoIterator<Item = WindowHandle<Overlay>>,
    cx: &mut AsyncApp,
) {
    end_selection_windows(overlays, cx).await;
    daemon(cx, |d, _| d.selecting = false);
}

/// Shows live or frozen overlays and waits for global logical coordinates.
/// Live selection uses output metadata only, and synchronizes surface teardown
/// before returning so the caller can safely ask for fresh desktop pixels.
async fn select_region(
    snap: Option<&Snapshot>,
    platform: &Arc<Platform>,
    early: Option<EarlyOverlays>,
    cx: &mut AsyncApp,
) -> Result<Option<IRect>> {
    let started = Instant::now();
    // The window list and each output's GPU tiles are prepared in parallel;
    // neither depends on the other.
    let desktop = platform.desktop.clone();
    let windows = cx.background_spawn(async move { desktop.visible_windows() });
    let outputs = if let Some(snap) = snap {
        let tile_tasks: Vec<_> = snap
            .outputs
            .iter()
            .map(|o| {
                let (name, logical, image) = (o.name.clone(), o.logical, o.image.clone());
                cx.background_spawn(async move {
                    let tiles = ui::make_tiles(&image);
                    (name, logical, image.width, tiles)
                })
            })
            .collect();
        let mut outputs = Vec::with_capacity(tile_tasks.len());
        for task in tile_tasks {
            outputs.push(task.await);
        }
        outputs
    } else {
        let platform = platform.clone();
        cx.background_spawn(async move { platform.selection_outputs() })
            .await?
            .into_iter()
            .map(|(name, logical, width)| (name, logical, width, Arc::from([])))
            .collect()
    };
    let windows = windows.await;

    if let Some(early) = early {
        match cx.update(|cx| adopt_early(early, &outputs, windows.clone(), cx)) {
            Ok((overlays, done)) => {
                log::debug!(
                    "selection overlays opened {:.1}ms after the snapshot (early)",
                    started.elapsed().as_secs_f64() * 1e3
                );
                let rect = done.await.ok().flatten();
                end_selection(overlays, cx).await;
                return Ok(rect.filter(|r| !r.is_empty()));
            }
            Err(unused) => end_selection_windows(unused, cx).await,
        }
    }

    let live = snap.is_none();
    anyhow::ensure!(!outputs.is_empty(), "no outputs for region selection");
    #[cfg(target_os = "linux")]
    let logical_outputs: Vec<_> = outputs.iter().map(|(_, logical, ..)| *logical).collect();
    let options = |name: &str, logical, cx: &App| {
        let mut options = platform.overlay_options(name, logical, cx);
        if live {
            options.window_background = gpui::WindowBackgroundAppearance::Transparent;
        }
        options
    };
    let (selection, done) = Selection::new(windows);
    let result: Result<Vec<WindowHandle<Overlay>>> = cx.update(|cx| {
        let selection = cx.new(|_| selection);
        #[cfg(target_os = "linux")]
        if platform.surfaces == platform::Surfaces::Fullscreen {
            let frames: Vec<_> = outputs
                .into_iter()
                .map(
                    |(name, logical, image_width, tiles)| ui::overlay::FullscreenFrame {
                        display: platform::display_for(&name, logical, cx),
                        name,
                        logical,
                        image_width,
                        tiles,
                    },
                )
                .collect();
            let first = &frames[0];
            return cx
                .open_window(
                    options(&first.name, first.logical, cx),
                    move |window, cx| {
                        cx.new(|cx| Overlay::fullscreen(selection, frames, window, cx))
                    },
                )
                .map(|window| vec![window])
                .context("opening fullscreen selection overlay");
        }
        let mut overlays = Vec::with_capacity(outputs.len());
        for (name, logical, width, tiles) in outputs {
            let selection = selection.clone();
            let opened = cx.open_window(options(&name, logical, cx), move |window, cx| {
                #[cfg(windows)]
                platform::windows::focus_overlay(window, logical, cx);
                cx.new(|cx| {
                    if live {
                        Overlay::live(selection, logical, width, window, cx)
                    } else {
                        Overlay::new(selection, logical, width, tiles, window, cx)
                    }
                })
            });
            match opened {
                Ok(window) => overlays.push(window),
                Err(error) => {
                    for overlay in overlays {
                        let _ = overlay.update(cx, |_, window, _| window.remove_window());
                    }
                    return Err(error)
                        .with_context(|| format!("opening the selection overlay on {name}"));
                }
            }
        }
        Ok(overlays)
    });
    let overlays = match result {
        Ok(o) => o,
        Err(e) => {
            daemon(cx, |d, _| d.selecting = false);
            return Err(e);
        }
    };
    let barrier = if live {
        let result = cx.update(|cx| {
            use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
            let handles: Result<Vec<_>> = overlays
                .iter()
                .map(|overlay| {
                    overlay.update(cx, |_, window, _| {
                        Ok((
                            HasWindowHandle::window_handle(window)
                                .map_err(|e| anyhow!("selection window handle: {e}"))?
                                .as_raw(),
                            HasDisplayHandle::display_handle(window)
                                .map_err(|e| anyhow!("selection display handle: {e}"))?
                                .as_raw(),
                        ))
                    })?
                })
                .collect();
            platform::SelectionBarrier::new(&handles?)
        });
        match result {
            Ok(barrier) => Some(barrier),
            Err(error) => {
                end_selection(overlays, cx).await;
                return Err(error);
            }
        }
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let selection_focus = if platform.surfaces == platform::Surfaces::X11 {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let windows = cx.update(|cx| {
            overlays
                .iter()
                .zip(&logical_outputs)
                .filter_map(|(overlay, logical)| {
                    overlay
                        .update(cx, |_, window, _| {
                            match HasWindowHandle::window_handle(window).ok()?.as_raw() {
                                RawWindowHandle::Xcb(handle) => {
                                    Some((handle.window.get(), *logical))
                                }
                                RawWindowHandle::Xlib(handle) => {
                                    Some((handle.window as u32, *logical))
                                }
                                _ => None,
                            }
                        })
                        .ok()
                        .flatten()
                })
                .collect()
        });
        Some(
            cx.background_spawn(async move { platform::x11::SelectionFocus::acquire(windows) })
                .await,
        )
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    if let Some(Err(error)) = &selection_focus {
        cx.update(|cx| {
            for overlay in &overlays {
                let _ = overlay.update(cx, |_, window, _| window.remove_window());
            }
            cx.update_global::<Daemon, _>(|d, _| d.selecting = false);
        });
        bail!("focusing X11 selection: {error:#}");
    }
    log::debug!(
        "{} selection overlays opened {:.1}ms after preparation began",
        if live { "live" } else { "frozen" },
        started.elapsed().as_secs_f64() * 1e3
    );
    let rect = done.await.ok().flatten();
    #[cfg(target_os = "linux")]
    drop(selection_focus);
    #[cfg(windows)]
    if let Some(barrier) = barrier {
        // DwmFlush can dispatch queued GPUI frame callbacks. The windows must
        // remain registered until native hiding/composition has completed.
        barrier.wait()?;
    }
    end_selection_windows(overlays, cx).await;
    #[cfg(not(windows))]
    if let Some(barrier) = barrier {
        cx.update(|_| barrier.wait())?;
    } else {
        daemon(cx, |d, _| d.selecting = false);
    }
    #[cfg(windows)]
    if !live {
        daemon(cx, |d, _| d.selecting = false);
    }
    Ok(rect.filter(|r| !r.is_empty()))
}

/// Hands each output's frozen frame to the early overlay on its display.
/// Fails (returning the windows to close) when an output has no early
/// overlay, e.g. one was hotplugged during the capture.
#[allow(clippy::type_complexity)]
fn adopt_early(
    early: EarlyOverlays,
    outputs: &[(String, IRect, u32, Arc<[ui::Tile]>)],
    windows: Vec<platform::WindowRect>,
    cx: &mut App,
) -> Result<
    (Vec<WindowHandle<Overlay>>, oneshot::Receiver<Option<IRect>>),
    Vec<WindowHandle<Overlay>>,
> {
    let all: Vec<WindowHandle<Overlay>> = early.windows.iter().map(|(_, w)| *w).collect();
    let mut matched = Vec::with_capacity(outputs.len());
    for (name, logical, width, tiles) in outputs {
        let display = platform::display_for(name, *logical, cx);
        let Some((_, window)) = early.windows.iter().find(|(id, _)| Some(*id) == display) else {
            return Err(all);
        };
        matched.push((*window, *logical, *width, tiles.clone()));
    }
    for (window, logical, width, tiles) in &matched {
        let (logical, width, tiles) = (*logical, *width, tiles.clone());
        if window
            .update(cx, |o, _, cx| o.set_frame(logical, width, tiles, cx))
            .is_err()
        {
            return Err(all);
        }
    }
    let used: Vec<WindowHandle<Overlay>> = matched.iter().map(|(w, ..)| *w).collect();
    for window in all.iter().filter(|w| !used.contains(w)) {
        let _ = window.update(cx, |_, window, _| window.remove_window());
    }
    early.selection.update(cx, |s, _| s.set_windows(windows));
    Ok((used, early.done))
}

/// Closes early overlays that could not be used; the selection stays
/// claimed for the overlays that replace them.
async fn end_selection_windows(
    overlays: impl IntoIterator<Item = WindowHandle<Overlay>>,
    cx: &mut AsyncApp,
) {
    #[cfg(windows)]
    for overlay in overlays {
        if let Err(error) = platform::windows::hide_overlay(overlay, cx).await {
            log::error!("hiding selection overlay: {error:#}");
        }
        cx.update(|cx| {
            let _ = overlay.update(cx, |_, window, _| window.remove_window());
        });
    }
    #[cfg(not(windows))]
    cx.update(|cx| {
        for overlay in overlays {
            let _ = overlay.update(cx, |_, window, _| window.remove_window());
        }
    });
}

fn add_shot(
    image: Arc<Image>,
    scale: f32,
    source: Option<PathBuf>,
    cx: &mut AsyncApp,
) -> Arc<Shot> {
    daemon(cx, |d, cx| {
        let id = d.next_id;
        d.next_id += 1;
        let shot = Arc::new(Shot::new(id, image, scale, source));
        d.shots.push_back(shot.clone());
        while d.shots.len() > KEEP_SHOTS {
            if let Some(old) = d.shots.pop_front() {
                d.docs.remove(&old.id);
            }
        }
        // GPU tiles are built off the main thread so a later "open editor"
        // only uploads them.
        let warm = shot.clone();
        cx.background_spawn(async move {
            warm.tiles();
        })
        .detach();
        shot
    })
}

async fn finish_capture(
    shot: Arc<Shot>,
    opts: CaptureOptions,
    config: Arc<Config>,
    cx: &mut AsyncApp,
) -> Result<Response> {
    let (clipboard, notifier) = daemon(cx, |d, _| {
        (d.platform.clipboard.clone(), d.platform.notifier.clone())
    });

    if !opts.text && opts.clipboard.unwrap_or(config.copy_to_clipboard) {
        match &clipboard {
            Some(c) => {
                c.set_image(shot.image.clone());
            }
            None => log::warn!("clipboard unavailable; capture not copied"),
        }
    }

    // Cache for `open-last` across daemon restarts. Queued before the first
    // await, so queue order is shot-id order.
    daemon(cx, |d, _| d.cache.store(shot.image.clone(), shot.scale));

    let saved = if opts.save {
        let image = shot.image.clone();
        let path = config.new_save_path(shot.taken);
        Some(
            cx.background_spawn(async move {
                image.save_png(&path, Compression::Balanced).map(|_| path)
            })
            .await?,
        )
    } else {
        None
    };

    if opts.text {
        return extract_text(shot, &opts, config, clipboard, notifier, cx).await;
    }

    if opts.notify.unwrap_or(daemon(cx, |d, _| d.notify)) {
        match notifier {
            Some(n) => {
                let (id, image) = (shot.id, shot.image.clone());
                let body = match &saved {
                    Some(p) => format!("Saved to {}", ui::editor::tilde(p)),
                    None => format!("{}×{} — click to annotate", image.width, image.height),
                };
                let timeout = config.notification_timeout_ms;
                cx.background_spawn(async move {
                    if let Err(e) = n.shot_captured(id, &image, &body, timeout) {
                        log::warn!("notification failed: {e:#}");
                    }
                })
                .detach();
            }
            None => log::warn!("notifications unavailable"),
        }
    }

    if opts.edit.unwrap_or(config.open_editor) {
        let id = shot.id;
        cx.update(|cx| open_editor(id, EditorTool::Default, cx))?;
    }

    Ok(Response::Shot {
        id: shot.id,
        width: shot.image.width,
        height: shot.image.height,
        saved,
    })
}

/// `capture … text`: reads the capture's text and copies it in place of
/// the image. The notification shows what was read, so a keybind user
/// sees the result (or why there is none) without a terminal.
async fn extract_text(
    shot: Arc<Shot>,
    opts: &CaptureOptions,
    config: Arc<Config>,
    clipboard: Option<Arc<dyn Clipboard>>,
    notifier: Option<Arc<dyn Notifier>>,
    cx: &mut AsyncApp,
) -> Result<Response> {
    let (image, scale) = (shot.image.clone(), shot.scale);
    let languages = config.ocr.languages.clone();
    let result = cx
        .background_spawn(async move {
            crate::ocr::extract(
                &image,
                image.bounds(),
                crate::ocr::upscale_for(scale),
                &languages,
            )
        })
        .await;
    let notify = opts.notify.unwrap_or(daemon(cx, |d, _| d.notify));
    let message = match &result {
        Ok(found) if found.text.is_empty() => ("No text found".to_string(), String::new()),
        Ok(found) => (
            format!("Copied {} of text", plural(found.lines.len(), "line")),
            preview(&found.text),
        ),
        Err(e) => ("Text extraction failed".to_string(), format!("{e:#}")),
    };
    if notify && let Some(n) = notifier {
        cx.background_spawn(async move {
            if let Err(e) = n.message(&message.0, &message.1) {
                log::warn!("notification failed: {e:#}");
            }
        })
        .detach();
    }
    let found = result?;
    if !found.text.is_empty() && opts.clipboard.unwrap_or(config.copy_to_clipboard) {
        match &clipboard {
            Some(c) => c.set_text(&found.text),
            None => log::warn!("clipboard unavailable; text not copied"),
        }
    }
    if opts.edit.unwrap_or(config.open_editor) {
        let id = shot.id;
        cx.update(|cx| open_editor(id, EditorTool::Default, cx))?;
    }
    Ok(Response::Text {
        id: shot.id,
        text: found.text,
    })
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The first lines of `text`, short enough for a notification body.
fn preview(text: &str) -> String {
    const MAX_LINES: usize = 4;
    const MAX_CHARS: usize = 240;
    let mut out: String = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if let Some((cut, _)) = out.char_indices().nth(MAX_CHARS) {
        out.truncate(cut);
        out.push('…');
    } else if text.lines().filter(|l| !l.trim().is_empty()).count() > MAX_LINES {
        out.push('…');
    }
    out
}

/// `quikscope notifications on|off|toggle|status`. Changes are written to the
/// config file first, so the daemon never reports a state that a restart
/// would lose.
async fn set_notifications(switch: Switch, cx: &mut AsyncApp) -> Result<Option<Response>> {
    let (current, notifier) = daemon(cx, |d, _| (d.notify, d.platform.notifier.clone()));
    let on = switch.apply(current);
    if switch != Switch::Status {
        cx.update(|cx| -> Result<()> {
            // UI writes and IPC toggles share the app thread, so their atomic
            // read/edit/replace operations cannot overwrite each other's keys.
            config::persist_values(
                &config::config_path(),
                &[(&["notify"], toml_edit::Value::from(on))],
            )?;
            let mut config = (*cx.global::<Daemon>().config).clone();
            config.notify = on;
            apply_config(Arc::new(config), cx);
            Ok(())
        })?;
        log::info!("capture notifications {}", if on { "on" } else { "off" });
        // Feedback for keybind users; shown even when turning them off.
        if on != current
            && let Some(n) = notifier
        {
            let summary = if on {
                "Screenshot notifications on"
            } else {
                "Screenshot notifications off"
            };
            cx.background_spawn(async move {
                if let Err(e) = n.message(summary, "") {
                    log::warn!("notification failed: {e:#}");
                }
            })
            .detach();
        }
    }
    Ok(Some(Response::Notifications(on)))
}

/// Every settings entry point shares the same in-process config generation.
/// Existing editor tools retain their current styles; future editors/captures
/// and save/clipboard services immediately use these new defaults.
fn apply_config(config: Arc<Config>, cx: &mut App) {
    cx.update_global::<Daemon, _>(|d, _| {
        #[cfg(windows)]
        if d.config.hotkeys != config.hotkeys
            && let Some(shell) = &d.platform.shell
        {
            shell.update_hotkeys(&config.hotkeys);
        }
        d.notify = config.notify;
        d.config = config.clone();
    });
    cx.update_global::<EditorServices, _>(|services, _| services.config = config.clone());
    let settings = cx.global::<Daemon>().settings;
    // UI edits invoke this while the Settings entity is borrowed. Refresh only
    // after that update ends; external IPC/tray changes use the same path.
    cx.defer(move |cx| {
        if let Some(settings) = settings {
            let _ = settings.update(cx, |view, window, cx| {
                view.refresh_config(config, window, cx)
            });
        }
    });
}

fn open_settings(welcome: bool, cx: &mut App) -> Result<()> {
    let (config, existing) = cx.read_global::<Daemon, _>(|d, _| (d.config.clone(), d.settings));
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return Ok(());
    }
    let screen = cx
        .displays()
        .first()
        .map(|display| display.bounds().size)
        .unwrap_or(size(px(1280.), px(720.)));
    let (preferred_w, preferred_h) = if welcome {
        (640_f32, 540_f32)
    } else {
        (760_f32, 660_f32)
    };
    let bounds = size(
        px(preferred_w.min(f32::from(screen.width) * 0.95)),
        px(preferred_h.min(f32::from(screen.height) * 0.95)),
    );
    let path = config::config_path();
    let handle = cx.open_window(
        WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(
                    if welcome {
                        "Welcome to quikscope"
                    } else {
                        "quikscope — Settings"
                    }
                    .into(),
                ),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(bounds, cx)),
            app_id: Some("quikscope-settings".into()),
            #[cfg(target_os = "linux")]
            icon: Some(crate::ui::window_icon()),
            focus: true,
            show: true,
            window_min_size: Some(size(px(360.), px(320.))),
            ..Default::default()
        },
        move |window, cx| {
            let view = cx
                .new(|cx| Settings::new(config, path, welcome, Arc::new(apply_config), window, cx));
            window.activate_window();
            view
        },
    )?;
    cx.update_global::<Daemon, _>(|d, _| d.settings = Some(handle));
    log::debug!(
        "{} window opened",
        if welcome { "welcome" } else { "settings" }
    );
    Ok(())
}

fn shot_response(shot: &Shot) -> Response {
    Response::Shot {
        id: shot.id,
        width: shot.image.width,
        height: shot.image.height,
        saved: None,
    }
}

async fn open_last(cx: &mut AsyncApp) -> Result<Option<Response>> {
    if let Some(shot) = daemon(cx, |d, _| d.shots.back().cloned()) {
        cx.update(|cx| open_editor(shot.id, EditorTool::Last, cx))?;
        return Ok(Some(shot_response(&shot)));
    }
    // Nothing captured since the daemon started: fall back to the cache,
    // then to the newest saved screenshot.
    let save_dir = daemon(cx, |d, _| d.config.save_dir());
    let loaded = cx
        .background_spawn(async move {
            match Image::read_raw(&config::last_capture_path()) {
                Ok((img, scale)) => Ok((img, scale, None)),
                Err(_) => {
                    let newest =
                        newest_image(&save_dir).context("no previous screenshot to open")?;
                    Image::load(&newest).map(|img| (img, 1.0, Some(newest)))
                }
            }
        })
        .await?;
    let (image, scale, source) = loaded;
    let shot = add_shot(Arc::new(image), scale, source, cx);
    cx.update(|cx| open_editor(shot.id, EditorTool::Last, cx))?;
    Ok(Some(shot_response(&shot)))
}

async fn open_path(path: PathBuf, cx: &mut AsyncApp) -> Result<Option<Response>> {
    let p = path.clone();
    let image = cx.background_spawn(async move { Image::load(&p) }).await?;
    let shot = add_shot(Arc::new(image), 1.0, Some(path), cx);
    cx.update(|cx| open_editor(shot.id, EditorTool::Default, cx))?;
    Ok(Some(shot_response(&shot)))
}

fn newest_image(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| {
                    matches!(
                        x.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "webp"
                    )
                })
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// Logical size for a new editor window: the image at 1:1 plus toolbar and
/// margins, at least 1040×480 (one toolbar row) where the screen allows,
/// never more than 90% of the screen. Unlike `clamp`, the preferred minimum
/// yields to the screen limit, so small or HiDPI-scaled displays (1024 px,
/// 1920 px at 2×) get a smaller window instead of a panic; the toolbar then
/// wraps.
fn editor_window_size(image_w: f32, image_h: f32, screen_w: f32, screen_h: f32) -> (f32, f32) {
    let max_w = (screen_w * 0.9).max(1.0);
    let max_h = (screen_h * 0.9).max(1.0);
    (
        (image_w + 48.0).max(1040.0).min(max_w),
        (image_h + 112.0).max(480.0).min(max_h),
    )
}

/// Which tool a newly opened editor starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorTool {
    /// The editor's default (arrow).
    Default,
    /// The tool last chosen in any editor (`open-last`), so reopening the
    /// last screenshot continues where the previous session stopped.
    Last,
}

/// Opens (or focuses) the editor for shot `id`, keeping annotations from a
/// previous editor session of the same shot.
fn open_editor(id: u64, tool: EditorTool, cx: &mut App) -> Result<()> {
    let started = Instant::now();
    let (shot, existing) = cx.read_global::<Daemon, _>(|d, _| {
        (
            d.shots.iter().find(|s| s.id == id).cloned(),
            d.editors.get(&id).copied(),
        )
    });
    let shot = shot.with_context(|| format!("shot {id} is no longer in memory"))?;
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return Ok(());
    }

    let doc = cx.update_global::<Daemon, _>(|d, cx| {
        d.docs
            .entry(id)
            .or_insert_with(|| cx.new(|_| Document::new(shot.image.clone(), shot.scale)))
            .clone()
    });

    // Size the window to the screenshot at 1:1 (logical), within the screen.
    let screen = cx
        .displays()
        .iter()
        .map(|d| d.bounds().size)
        .max_by_key(|s| (f32::from(s.width) * f32::from(s.height)) as i64)
        .unwrap_or(size(px(1920.), px(1080.)));
    let (w, h) = editor_window_size(
        shot.image.width as f32 / shot.scale,
        shot.image.height as f32 / shot.scale,
        f32::from(screen.width),
        f32::from(screen.height),
    );
    let title = match &shot.source {
        Some(p) => format!(
            "quikscope — {}",
            p.file_name().unwrap_or_default().to_string_lossy()
        ),
        None => format!("quikscope — {}×{}", shot.image.width, shot.image.height),
    };

    let handle = cx.open_window(
        WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(size(px(w), px(h)), cx)),
            app_id: Some("quikscope".into()),
            #[cfg(target_os = "linux")]
            icon: Some(crate::ui::window_icon()),
            focus: true,
            show: true,
            window_min_size: Some(size(px(480.), px(320.))),
            ..Default::default()
        },
        move |window, cx| {
            let last = match tool {
                EditorTool::Last => cx.try_global::<LastTool>().map(|t| t.0),
                EditorTool::Default => None,
            };
            let editor = cx.new(|cx| {
                let mut editor = Editor::new(shot, doc, window, cx);
                if let Some(tool) = last {
                    editor.tool = tool;
                }
                editor
            });
            window.activate_window();
            editor
        },
    )?;
    cx.update_global::<Daemon, _>(|d, _| {
        d.editors.insert(id, handle);
    });
    log::debug!(
        "editor window opened in {:.1}ms",
        started.elapsed().as_secs_f64() * 1e3
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_replaces_stale_socket_but_not_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        {
            let _stale = UnixListener::bind(&path).unwrap();
        }
        assert!(path.exists(), "dropping a listener leaves the file behind");
        let live = bind(&path).expect("stale socket is replaced");
        let err = bind(&path).unwrap_err();
        assert!(err.to_string().contains("already listening"), "{err}");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        drop(live);
    }

    #[test]
    fn editor_window_size_fits_small_and_scaled_displays() {
        // 1024 px wide, and a 1920×1080 panel at 2× (960×540 logical).
        for (sw, sh) in [(1024.0, 768.0), (960.0, 540.0), (640.0, 360.0), (0.0, 0.0)] {
            let (w, h) = editor_window_size(2000.0, 1500.0, sw, sh);
            assert!(w >= 1.0 && w <= (sw * 0.9f32).max(1.0), "{sw}: width {w}");
            assert!(h >= 1.0 && h <= (sh * 0.9f32).max(1.0), "{sh}: height {h}");
        }
        let (w, h) = editor_window_size(100.0, 50.0, 960.0, 540.0);
        assert_eq!((w, h), (864.0, 480.0), "minimum yields to the screen");
    }

    #[test]
    fn editor_window_size_prefers_one_to_one_with_a_toolbar_row() {
        assert_eq!(
            editor_window_size(100.0, 50.0, 2560.0, 1440.0),
            (1040.0, 480.0)
        );
        assert_eq!(
            editor_window_size(1200.0, 700.0, 2560.0, 1440.0),
            (1248.0, 812.0)
        );
        assert_eq!(
            editor_window_size(5120.0, 1440.0, 2560.0, 1440.0),
            (2304.0, 1296.0)
        );
    }

    #[test]
    fn newest_image_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(newest_image(dir.path()), None);
        std::fs::write(dir.path().join("a.png"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("b.JPG"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        assert_eq!(newest_image(dir.path()), Some(dir.path().join("b.JPG")));
    }
}
