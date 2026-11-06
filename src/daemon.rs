//! The long-running daemon: owns the GPUI application (so editor windows
//! open without GPU start-up cost), the capture/clipboard/notification
//! threads, recent shots, and the IPC socket.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
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
    App, AppContext as _, AsyncApp, BorrowAppContext as _, Entity, Global, QuitMode, WindowBounds,
    WindowHandle, WindowOptions, px, size,
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
    next_id: u64,
    selecting: bool,
    /// Capture notifications; starts from the config, changed by
    /// `quickscope notifications …` (which also persists it).
    notify: bool,
}

impl Global for Daemon {}

/// Binds the socket, refusing to replace a live daemon.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!(
                "a quickscope daemon is already listening on {}",
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
    let config = Arc::new(Config::load()?);
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
                next_id: 1,
                selecting: false,
            });
            if let Some(options) = prewarm {
                prewarm_gpu(options, cx);
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
        .name("quickscope-ipc".into())
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
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line)? == 0 {
        // Connected and closed without a request (a liveness probe).
        return Ok(());
    }
    let response = match Request::parse(line.trim()) {
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
                let snap = snapshot(&capturer, None, cursor, cx).await?;
                let Some(rect) = select_region(&snap, &platform, cx).await? else {
                    return Ok(None);
                };
                cx.background_spawn(async move { snap.compose(rect) })
                    .await
                    .context("the selection lies outside every output")?
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

/// Shows the frozen overlays and waits for a selection in global logical
/// coordinates. `None` means the user cancelled.
async fn select_region(
    snap: &Snapshot,
    platform: &Arc<Platform>,
    cx: &mut AsyncApp,
) -> Result<Option<IRect>> {
    let started = Instant::now();
    if daemon(cx, |d, _| std::mem::replace(&mut d.selecting, true)) {
        bail!("a selection is already in progress");
    }
    // The window list and each output's GPU tiles are prepared in parallel;
    // neither depends on the other.
    let desktop = platform.desktop.clone();
    let windows = cx.background_spawn(async move { desktop.visible_windows() });
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
    let mut outputs: Vec<(String, IRect, u32, Arc<[ui::Tile]>)> = Vec::new();
    for task in tile_tasks {
        outputs.push(task.await);
    }
    let windows = windows.await;

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
                    platform.overlay_options(&first.name, first.logical, cx),
                    move |window, cx| {
                        cx.new(|cx| Overlay::fullscreen(selection, frames, window, cx))
                    },
                )
                .map(|window| vec![window])
                .context("opening fullscreen selection overlay");
        }
        outputs
            .into_iter()
            .map(|(name, logical, width, tiles)| {
                let selection = selection.clone();
                cx.open_window(
                    platform.overlay_options(&name, logical, cx),
                    move |window, cx| {
                        #[cfg(windows)]
                        platform::windows::focus_overlay(window, logical);
                        cx.new(|cx| Overlay::new(selection, logical, width, tiles, window, cx))
                    },
                )
                .with_context(|| format!("opening the selection overlay on {name}"))
            })
            .collect()
    });
    let overlays = match result {
        Ok(o) => o,
        Err(e) => {
            daemon(cx, |d, _| d.selecting = false);
            return Err(e);
        }
    };
    #[cfg(target_os = "linux")]
    let selection_focus = if platform.surfaces == platform::Surfaces::X11 {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let windows = cx.update(|cx| {
            overlays
                .iter()
                .zip(&snap.outputs)
                .filter_map(|(overlay, output)| {
                    overlay
                        .update(cx, |_, window, _| {
                            match HasWindowHandle::window_handle(window).ok()?.as_raw() {
                                RawWindowHandle::Xcb(handle) => {
                                    Some((handle.window.get(), output.logical))
                                }
                                RawWindowHandle::Xlib(handle) => {
                                    Some((handle.window as u32, output.logical))
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
        "selection overlays opened {:.1}ms after the snapshot",
        started.elapsed().as_secs_f64() * 1e3
    );
    let rect = done.await.ok().flatten();
    #[cfg(target_os = "linux")]
    drop(selection_focus);
    cx.update(|cx| {
        for o in overlays {
            let _ = o.update(cx, |_, window, _| window.remove_window());
        }
        cx.update_global::<Daemon, _>(|d, _| d.selecting = false);
    });
    Ok(rect.filter(|r| !r.is_empty()))
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

/// `quickscope notifications on|off|toggle|status`. Changes are written to the
/// config file first, so the daemon never reports a state that a restart
/// would lose.
async fn set_notifications(switch: Switch, cx: &mut AsyncApp) -> Result<Option<Response>> {
    let (current, notifier) = daemon(cx, |d, _| (d.notify, d.platform.notifier.clone()));
    let on = switch.apply(current);
    if switch != Switch::Status {
        cx.background_spawn(
            async move { config::persist_bool(&config::config_path(), "notify", on) },
        )
        .await?;
        daemon(cx, |d, _| d.notify = on);
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
            "quickscope — {}",
            p.file_name().unwrap_or_default().to_string_lossy()
        ),
        None => format!("quickscope — {}×{}", shot.image.width, shot.image.height),
    };

    let handle = cx.open_window(
        WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(size(px(w), px(h)), cx)),
            app_id: Some("quickscope".into()),
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
