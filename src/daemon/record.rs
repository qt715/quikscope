//! Daemon side of screen recording: one recording at a time, started and
//! stopped by the same request, with tray, outline and notification state.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, AsyncApp};

use super::{
    Daemon, EditorTool, Shown, claim_selection, daemon, insert_shot, notify_error, notify_failure,
    open_editor, select_frozen, select_region,
};
use crate::config::{self, Config};
use crate::geom::IRect;
use crate::ipc::{Mode, RecordAction, RecordOptions, RecordState, RecordStatus, Response};
use crate::media::record::{Recorded, Session, SessionConfig};
use crate::media::{self, EncoderPreference, Kind, OutputFormat};
use crate::output::OutputFile;
use crate::platform::{Platform, RecordTarget, RecordingStatus, TrayState};
use crate::raster::Image;
use crate::ui::overlay::Selected;
use crate::ui::{self, Animation, Shot};

/// Smallest region worth recording, in logical pixels per side; encoders
/// also need even dimensions of at least 2.
const MIN_SIDE: i32 = 8;

/// The recording slot. `Starting` covers selection and encoder start-up,
/// `Stopping` the final encode (or a discard's teardown), so a second key
/// press in either phase is refused instead of starting a parallel
/// recording.
#[derive(Default)]
pub(super) enum Slot {
    #[default]
    Idle,
    Starting,
    Active(Box<Active>),
    Stopping(Kind),
}

pub(super) struct Active {
    id: u64,
    kind: Kind,
    format: OutputFormat,
    session: Session,
    /// The reserved file and, for animations, the lossless master: what a
    /// failed save can still recover.
    path: PathBuf,
    master: PathBuf,
    /// Dropping it removes the outline.
    outline: Option<Box<dyn Send>>,
    finish: Finish,
    /// Physical pixels per logical pixel of the recorded output.
    scale: f32,
}

/// What happens when the recording stops, fixed when it starts.
struct Finish {
    notify: bool,
    clipboard: bool,
    edit: bool,
}

/// Starts a recording, or stops the running one: one key binding toggles.
/// Failures reach the user through the caller's notification, since
/// keybinds and tray menus have no terminal to print them in.
pub(super) async fn toggle(options: RecordOptions, cx: &mut AsyncApp) -> Result<Option<Response>> {
    match daemon(cx, |d, _| match d.recording {
        _ if d.quitting => Some("quikscope is quitting"),
        Slot::Idle => None,
        Slot::Starting => Some("a recording is already starting"),
        Slot::Active(_) => Some(""),
        Slot::Stopping(_) => Some("the previous recording is still being saved"),
    }) {
        Some("") => return stop(cx).await,
        Some(busy) => bail!(busy),
        None => {}
    }
    daemon(cx, |d, _| d.recording = Slot::Starting);
    // Devices plugged in since the last listing show up in the tray.
    cx.update(refresh_devices);
    let result = start(options, cx).await;
    if !matches!(result, Ok(Some(_))) {
        daemon(cx, |d, _| d.recording = Slot::Idle);
    }
    cx.update(update_tray);
    result
}

async fn start(o: RecordOptions, cx: &mut AsyncApp) -> Result<Option<Response>> {
    let (config, platform) = daemon(cx, |d, _| (d.config.clone(), d.platform.clone()));
    let recorder = platform.recorder.clone().with_context(|| {
        format!(
            "screen recording is not available on the {} backend (see the daemon log)",
            platform.name
        )
    })?;
    let rc = &config.record;
    let kind = o.kind;
    let format = o.format.unwrap_or_else(|| match kind {
        Kind::Video => rc.video_format(),
        Kind::Animation => rc.animation_format(),
    });
    let fps = match kind {
        Kind::Video => o.fps.unwrap_or(rc.video_fps),
        // GIF delays have 10 ms resolution; above 60 fps animations only grow.
        Kind::Animation => o.fps.unwrap_or(rc.animation_fps).min(60),
    };
    let cursor = o.cursor.unwrap_or(rc.cursor);

    let Some(rect) = target_rect(&o, &platform, cursor, cx).await? else {
        return Ok(None);
    };
    let outputs = {
        let platform = platform.clone();
        cx.background_spawn(async move { platform.selection_outputs() })
            .await?
    };
    let (target, scale) = record_target(rect, &outputs)
        .with_context(|| format!("the area {rect} is not on any output"))?;
    log::info!(
        "recording {} of {} ({}) at {fps} fps as {}",
        target.region,
        target.output,
        kind.as_str(),
        format.label()
    );

    let source = {
        let (recorder, target) = (recorder.clone(), target.clone());
        cx.background_spawn(async move { recorder.open(&target, fps, cursor) })
            .await
            .context("starting screen capture")?
    };
    let now = SystemTime::now();
    let candidate = rc.new_save_path(now, format);
    let (id, masters) = daemon(cx, |d, _| (d.media.allocate(), d.masters.dir.clone()));
    let master = masters.join(format!("{id}-{}.master", unix_secs(now)));
    let audio = match kind {
        Kind::Video => o.audio.clone().unwrap_or_else(|| rc.audio.clone()),
        Kind::Animation => Vec::new(),
    };
    let encoder = EncoderPreference::parse(&rc.encoder).unwrap_or_default();
    let fragmented = rc.mp4_fragmented;
    let (session, path) = {
        let master = master.clone();
        cx.background_spawn(async move {
            std::fs::create_dir_all(&masters)
                .with_context(|| format!("creating {}", masters.display()))?;
            let output = OutputFile::reserve(&candidate)?;
            let path = output.path().to_owned();
            let session_config = SessionConfig {
                output,
                format,
                master,
                fps,
                audio,
                encoder,
                fragmented,
            };
            Ok::<_, anyhow::Error>((Session::start(session_config, source)?, path))
        })
        .await?
    };
    if daemon(cx, |d, _| d.quitting) {
        // Nothing was recorded yet; quitting does not wait for a start.
        cx.background_spawn(async move { session.cancel() }).await;
        bail!("quikscope is quitting");
    }
    let warnings = session.warnings().join("\n");
    if !warnings.is_empty() {
        notify_error("Recording without some audio", &warnings, cx);
    }
    let (width, height) = session.size();
    // Whole outputs get no outline: there is no "outside" left to draw in.
    let outline = (rc.outline && target.region != target.output_rect)
        .then(|| recorder.outline(target.region))
        .and_then(|result| {
            result
                .map_err(|e| log::warn!("recording outline unavailable: {e:#}"))
                .ok()
        });
    let finish = Finish {
        notify: o.notify.unwrap_or(rc.notify),
        clipboard: o.clipboard.unwrap_or(rc.copy_to_clipboard),
        edit: o.edit.unwrap_or(rc.open_editor),
    };
    daemon(cx, |d, _| {
        d.recording = Slot::Active(Box::new(Active {
            id,
            kind,
            format,
            session,
            path,
            master,
            outline,
            finish,
            scale,
        }))
    });
    watch_failure(id, cx);
    Ok(Some(Response::Recording {
        kind,
        width,
        height,
    }))
}

/// The logical rectangle to record, `None` when the user cancels selection.
async fn target_rect(
    o: &RecordOptions,
    platform: &Arc<Platform>,
    cursor: bool,
    cx: &mut AsyncApp,
) -> Result<Option<IRect>> {
    if let Some(rect) = o.geometry {
        return Ok(Some(rect));
    }
    let desktop = platform.desktop.clone();
    Ok(Some(match o.mode {
        Mode::Region => {
            let live_unavailable = {
                let platform = platform.clone();
                cx.background_spawn(async move { platform.live_selection_unavailable() })
                    .await
            };
            // Recording shows the live desktop, so select over it; frozen
            // selection is only the fallback where live overlays cannot work.
            let selected = match live_unavailable {
                None => {
                    cx.update(claim_selection)?;
                    let selected = select_region(None, platform, None, false, cx).await;
                    daemon(cx, |d, _| d.selecting = false);
                    selected?
                }
                Some(reason) => {
                    log::warn!("live region selection unavailable ({reason}); using frozen");
                    let capturer = platform
                        .capture
                        .clone()
                        .context("screen capture is unavailable (see the daemon log)")?;
                    select_frozen(platform, &capturer, cursor, false, Instant::now(), cx)
                        .await?
                        .map(|(_, selected)| selected)
                }
            };
            match selected {
                Some(Selected::Region(rect)) => rect,
                _ => return Ok(None),
            }
        }
        Mode::Output | Mode::Screen => {
            let name = match o.output.clone() {
                Some(name) => Some(name),
                None => {
                    cx.background_spawn(async move { desktop.focused_output() })
                        .await
                }
            };
            let platform = platform.clone();
            let outputs = cx
                .background_spawn(async move { platform.selection_outputs() })
                .await?;
            let output = match &name {
                Some(name) => outputs.iter().find(|(n, ..)| n == name),
                None => outputs.first(),
            }
            .with_context(|| match &name {
                Some(name) => format!("no output named {name:?}"),
                None => "no outputs to record".into(),
            })?;
            output.1
        }
        Mode::Window => cx
            .background_spawn(async move { desktop.active_window() })
            .await
            .with_context(|| {
                format!(
                    "window recording needs a focused window the {} backend can locate",
                    platform.name
                )
            })?,
    }))
}

/// The output that holds most of `rect`, `rect` clipped to it, and that
/// output's scale. Recordings cover one output: a frame spanning outputs of
/// different scales has no single pixel size.
fn record_target(rect: IRect, outputs: &[(String, IRect, u32)]) -> Option<(RecordTarget, f32)> {
    let (name, output, physical_width) = outputs
        .iter()
        .filter_map(|(name, output, physical)| {
            let clipped = rect.intersect(output)?;
            Some((clipped.w as i64 * clipped.h as i64, name, output, physical))
        })
        .max_by_key(|(area, ..)| *area)
        .map(|(_, name, output, physical)| (name, *output, *physical))?;
    let region = rect.intersect(&output)?;
    if region.w < MIN_SIDE || region.h < MIN_SIDE {
        return None;
    }
    let scale = physical_width as f32 / output.w.max(1) as f32;
    Some((
        RecordTarget {
            output: name.clone(),
            output_rect: output,
            region,
        },
        scale,
    ))
}

/// Stops the recording if capture or encoding fails underneath it, keeping
/// what was recorded so far.
fn watch_failure(id: u64, cx: &mut AsyncApp) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let failure = daemon(cx, |d, _| match &d.recording {
                Slot::Active(a) if a.id == id => Ok(a.session.failure()),
                _ => Err(()),
            });
            match failure {
                Err(()) => return,
                Ok(None) => {}
                Ok(Some(message)) => {
                    log::error!("recording stopped: {message}");
                    notify_error("Recording stopped", &message, cx);
                    if let Some(active) = take_active(cx)
                        && let Err(e) = save(active, cx).await
                    {
                        log::error!("saving the interrupted recording: {e:#}");
                        notify_failure("Recording failed", &e, cx);
                    }
                    return;
                }
            }
        }
    })
    .detach();
}

/// Moves the running recording out of the slot, which reads `Stopping`
/// until [`save`] (or a discard) finishes.
fn take_active(cx: &mut AsyncApp) -> Option<Box<Active>> {
    daemon(cx, |d, _| match std::mem::take(&mut d.recording) {
        Slot::Active(active) => {
            d.recording = Slot::Stopping(active.kind);
            Some(active)
        }
        other => {
            d.recording = other;
            None
        }
    })
}

/// Stops the running recording, saves it and tells the user.
async fn stop(cx: &mut AsyncApp) -> Result<Option<Response>> {
    let active = take_active(cx).context("nothing is being recorded")?;
    save(active, cx).await
}

/// Stops and saves a running recording and waits for a save in progress,
/// before the daemon quits. A recording still starting has nothing to save:
/// `start` discards it when it sees the daemon quitting.
pub(super) async fn finish_for_quit(cx: &mut AsyncApp) {
    loop {
        if let Some(active) = take_active(cx) {
            if let Err(e) = save(active, cx).await {
                log::error!("saving the recording before quitting: {e:#}");
                notify_failure("Recording failed", &e, cx);
            }
            continue;
        }
        if !daemon(cx, |d, _| matches!(d.recording, Slot::Stopping(_))) {
            return;
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// What a stopped session left behind.
enum Saved {
    Done(Recorded),
    /// The final animation could not be encoded; its lossless master
    /// survived and opens in the annotator, which can export it again.
    MasterOnly {
        first: Image,
        info: media::MediaInfo,
        error: anyhow::Error,
    },
}

async fn save(active: Box<Active>, cx: &mut AsyncApp) -> Result<Option<Response>> {
    let Active {
        id,
        kind,
        format,
        session,
        path,
        master,
        outline,
        finish,
        scale,
    } = *active;
    drop(outline);
    cx.update(update_tray);
    let size = session.size();
    let duration = session.elapsed();
    let saved = {
        let master = master.clone();
        cx.background_spawn(async move {
            match session.stop() {
                Ok(recorded) => Ok(Saved::Done(recorded)),
                Err(error) => salvage(kind, format, path, master, size, duration, error),
            }
        })
        .await
    };
    daemon(cx, |d, _| d.recording = Slot::Idle);
    cx.update(update_tray);
    match saved.context("finishing the recording")? {
        Saved::Done(recorded) => {
            log::info!(
                "recorded {}×{} {:.1}s with {} to {}",
                recorded.width,
                recorded.height,
                recorded.duration.as_secs_f64(),
                recorded.encoder,
                recorded.path.display()
            );
            finish_recording(id, kind, scale, recorded, finish, cx).await
        }
        Saved::MasterOnly { first, info, error } => {
            log::error!("exporting the animation failed; keeping its master: {error:#}");
            let thumbnail = first.thumbnail(384);
            let shot = Shot::new(id, Arc::new(first), scale, None).with_animation(Animation {
                path: master,
                info,
                format,
            });
            insert_shot(shot, cx);
            let (notifier, timeout) = daemon(cx, |d, _| {
                (
                    d.platform.notifier.clone(),
                    d.config.notification_timeout_ms,
                )
            });
            let body = format!("{error:#} — click to open it and export it from the editor");
            if let Some(n) = notifier {
                cx.background_spawn(async move {
                    if let Err(e) =
                        n.shot_captured(id, &thumbnail, "Animation export failed", &body, timeout)
                    {
                        log::warn!("notification failed: {e:#}");
                    }
                })
                .detach();
            }
            if finish.edit && !daemon(cx, |d, _| d.quitting) {
                cx.update(|cx| open_editor(id, EditorTool::Default, cx))?;
            }
            Err(anyhow::Error::new(Shown(error.context(
                "exporting the animation failed; the recording opens in the editor from its notification",
            ))))
        }
    }
}

/// Recovers what `Session::stop` left when it failed. The session publishes
/// a video before making its thumbnail, and records an animation's master
/// before encoding the final file, so either may still be usable. An
/// unpublished reservation is removed when the session drops, so a
/// non-empty file at `path` was saved.
fn salvage(
    kind: Kind,
    format: OutputFormat,
    path: PathBuf,
    master: PathBuf,
    (width, height): (u32, u32),
    duration: Duration,
    error: anyhow::Error,
) -> Result<Saved> {
    let source = match kind {
        Kind::Video => path.clone(),
        Kind::Animation => master.clone(),
    };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
        log::warn!("recording saved, but finishing it failed: {error:#}");
        // The notification needs some image; a blank one beats none.
        let thumbnail = first_frame(&source)
            .map(|(image, _)| image.thumbnail(384))
            .unwrap_or_else(|_| Image::filled(1, 1, [0, 0, 0, 0]));
        return Ok(Saved::Done(Recorded {
            path,
            format,
            master: source,
            width,
            height,
            duration,
            thumbnail,
            encoder: "unknown".into(),
        }));
    }
    if kind == Kind::Animation
        && let Ok((first, info)) = first_frame(&master)
    {
        return Ok(Saved::MasterOnly { first, info, error });
    }
    Err(error)
}

async fn finish_recording(
    id: u64,
    kind: Kind,
    scale: f32,
    recorded: Recorded,
    finish: Finish,
    cx: &mut AsyncApp,
) -> Result<Option<Response>> {
    match kind {
        Kind::Video => daemon(cx, |d, _| d.media.add_video(id, recorded.path.clone())),
        Kind::Animation => {
            let master = recorded.master.clone();
            let (first, info) = cx
                .background_spawn(async move { first_frame(&master) })
                .await
                .context("reading the recorded animation")?;
            let shot = Shot::new(id, Arc::new(first), scale, Some(recorded.path.clone()))
                .with_animation(Animation {
                    path: recorded.master.clone(),
                    info,
                    format: recorded.format,
                });
            insert_shot(shot, cx);
        }
    }

    let (clipboard, notifier, timeout, quitting) = daemon(cx, |d, _| {
        (
            d.platform.clipboard.clone(),
            d.platform.notifier.clone(),
            d.config.notification_timeout_ms,
            d.quitting,
        )
    });
    if finish.clipboard {
        match clipboard {
            Some(c) => c.set_files(std::slice::from_ref(&recorded.path)),
            None => log::warn!("clipboard unavailable; recording not copied"),
        }
    }
    if finish.notify {
        match notifier {
            Some(n) => {
                let summary = match kind {
                    Kind::Video => "Recording saved",
                    Kind::Animation => "Animation saved",
                };
                let body = format!(
                    "{} · {} · {}×{} — click to edit",
                    ui::editor::tilde(&recorded.path),
                    clock(recorded.duration),
                    recorded.width,
                    recorded.height
                );
                let thumbnail = recorded.thumbnail.clone();
                cx.background_spawn(async move {
                    if let Err(e) = n.shot_captured(id, &thumbnail, summary, &body, timeout) {
                        log::warn!("notification failed: {e:#}");
                    }
                })
                .detach();
            }
            None => log::warn!("notifications unavailable"),
        }
    }
    // Quitting closes every window; opening one now would only flash.
    if finish.edit && !quitting {
        open_media(id, cx).await?;
    }
    Ok(Some(Response::Recorded {
        id,
        width: recorded.width,
        height: recorded.height,
        duration_ms: recorded.duration.as_millis() as u64,
        path: recorded.path,
    }))
}

/// First frame of an animation and its facts.
pub(super) fn first_frame(path: &Path) -> Result<(Image, media::MediaInfo)> {
    let mut decoder = media::decode::Decoder::open(path)?;
    let info = decoder.info().clone();
    let frame = decoder
        .next_frame()?
        .with_context(|| format!("{} has no frames", path.display()))?;
    Ok((frame.image, info))
}

/// Opens what id `id` names: a shot (screenshot or animation) in the
/// annotator, or a recorded video in the video editor.
pub(super) async fn open_media(id: u64, cx: &mut AsyncApp) -> Result<()> {
    match daemon(cx, |d, _| d.media.video(id)) {
        Some(path) => super::open_video(id, path, None, cx).await,
        None => cx.update(|cx| open_editor(id, EditorTool::Default, cx)),
    }
}

pub(super) async fn control(action: RecordAction, cx: &mut AsyncApp) -> Result<Option<Response>> {
    match action {
        RecordAction::Stop => stop(cx).await,
        RecordAction::Cancel => {
            let active = take_active(cx).context("nothing is being recorded")?;
            let Active {
                session, outline, ..
            } = *active;
            drop(outline);
            cx.update(update_tray);
            // The slot stays `Stopping` until the capture and encoder are
            // gone: a new recording must not race their teardown.
            cx.background_spawn(async move { session.cancel() }).await;
            daemon(cx, |d, _| d.recording = Slot::Idle);
            cx.update(update_tray);
            log::info!("recording discarded");
            Ok(Some(Response::RecordStatus(RecordStatus::Idle)))
        }
        RecordAction::Pause | RecordAction::Resume | RecordAction::TogglePause => {
            let found = daemon(cx, |d, _| match &d.recording {
                Slot::Active(a) => {
                    let pause = match action {
                        RecordAction::Pause => true,
                        RecordAction::Resume => false,
                        _ => !a.session.is_paused(),
                    };
                    if pause {
                        a.session.pause();
                    } else {
                        a.session.resume();
                    }
                    true
                }
                _ => false,
            });
            if !found {
                bail!("nothing is being recorded");
            }
            cx.update(update_tray);
            Ok(Some(status(cx)))
        }
        RecordAction::Status => Ok(Some(status(cx))),
    }
}

fn status(cx: &mut AsyncApp) -> Response {
    daemon(cx, |d, _| {
        Response::RecordStatus(match &d.recording {
            Slot::Active(a) => RecordStatus::Recording(RecordState {
                kind: a.kind,
                elapsed_ms: a.session.elapsed().as_millis() as u64,
                paused: a.session.is_paused(),
            }),
            Slot::Stopping(_) => RecordStatus::Saving,
            Slot::Idle | Slot::Starting => RecordStatus::Idle,
        })
    })
}

pub(super) async fn audio_devices(cx: &mut AsyncApp) -> Result<Option<Response>> {
    let devices = cx
        .background_spawn(async { media::audio::devices() })
        .await?;
    daemon(cx, |d, _| d.audio_devices = devices.clone());
    cx.update(update_tray);
    Ok(Some(Response::AudioDevices(devices)))
}

/// This daemon's lossless animation masters (cache, not user files): one
/// directory per instance (per socket), locked while the daemon runs, so
/// another instance starting up never removes masters this one still uses.
pub(super) struct Masters {
    pub(super) dir: PathBuf,
    /// Held for the daemon's lifetime; the OS releases it when we exit,
    /// however that happens.
    _lock: Option<File>,
}

const LOCK: &str = "lock";

impl Masters {
    /// Claims this instance's directory under `root`. Masters an earlier
    /// run of this instance left behind belong to shots that no longer
    /// exist: that directory is moved aside first and removed with every
    /// other directory no running daemon holds (see [`clean_masters`]).
    pub(super) fn claim(root: &Path, socket: &Path) -> Masters {
        let dir = root.join(instance_name(socket));
        // `bind` already refused to start beside a live daemon on this
        // socket, so nobody else holds this directory.
        if dir.exists() {
            let aside = root.join(format!(
                "{}.old-{}",
                instance_name(socket),
                std::process::id()
            ));
            if let Err(e) = std::fs::rename(&dir, &aside) {
                log::warn!("moving old animation masters aside: {e}");
            }
        }
        let lock = std::fs::create_dir_all(&dir)
            .and_then(|()| {
                File::options()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(dir.join(LOCK))
            })
            .and_then(|file| file.try_lock().map(|()| file).map_err(std::io::Error::from))
            .map_err(|e| log::warn!("locking {}: {e}", dir.display()))
            .ok();
        Masters { dir, _lock: lock }
    }
}

/// Removes master directories under `root` that no running daemon holds,
/// except this instance's `own`, and pre-release flat masters (older builds
/// kept them directly in `root`, without a lock) not touched for a day.
pub(super) fn clean_masters(root: &Path, own: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own {
            continue;
        }
        let removed = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if !lock_is_free(&path.join(LOCK)) {
                continue;
            }
            std::fs::remove_dir_all(&path)
        } else {
            let stale = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| {
                t.elapsed()
                    .is_ok_and(|age| age > Duration::from_secs(86400))
            });
            if !stale {
                continue;
            }
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("removing old animation masters {}: {e}", path.display());
        }
    }
}

/// Whether no running daemon holds the lock at `path`. A directory without
/// a lock file belongs to nobody: daemons create it before any master.
fn lock_is_free(path: &Path) -> bool {
    match File::options().write(true).open(path) {
        // Dropping the file releases the probe lock before removal.
        Ok(file) => file.try_lock().is_ok(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// A readable directory name that differs per socket path.
fn instance_name(socket: &Path) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::hash::DefaultHasher::new();
    socket.hash(&mut hasher);
    let stem = socket
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stem}-{:08x}", hasher.finish() as u32)
}

/// Lists audio devices in the background and refreshes the tray with them.
pub(super) fn refresh_devices(cx: &mut App) {
    cx.spawn(async move |cx| {
        match cx.background_spawn(async { media::audio::devices() }).await {
            Ok(devices) => daemon(cx, |d, _| d.audio_devices = devices),
            Err(e) => log::warn!("listing audio devices: {e:#}"),
        }
        cx.update(update_tray);
    })
    .detach();
}

/// Tray menu: include or leave out an audio device; persisted.
pub(super) fn set_audio(id: String, enabled: bool, cx: &mut App) {
    let mut audio = cx.global::<Daemon>().config.record.audio.clone();
    audio.retain(|device| *device != id);
    if enabled {
        audio.push(id);
    }
    let value = toml_edit::Value::Array(audio.iter().map(String::as_str).collect());
    persist(&[(&["record", "audio"], value)], cx);
}

/// Tray menu: frame rate for future recordings of `kind`; persisted.
pub(super) fn set_fps(kind: Kind, fps: u32, cx: &mut App) {
    let key = match kind {
        Kind::Video => "video_fps",
        Kind::Animation => "animation_fps",
    };
    persist(
        &[(&["record", key], toml_edit::Value::from(i64::from(fps)))],
        cx,
    );
}

/// Writes tray choices to the config; applying it refreshes the tray.
fn persist(values: &[(&[&str], toml_edit::Value)], cx: &mut App) {
    match config::persist_values(&config::config_path(), values) {
        Ok(config) => super::apply_config(Arc::new(config), cx),
        Err(e) => {
            log::error!("saving recording settings: {e:#}");
            // The menu already shows the choice; put back what is in effect.
            update_tray(cx);
        }
    }
}

/// Shows or removes the Linux tray as `record.tray` changes. Starting and
/// stopping the D-Bus service blocks, so both happen off the main thread.
#[cfg(target_os = "linux")]
pub(super) fn set_tray(cx: &mut App) {
    use gpui::BorrowAppContext as _;
    let (on, events, record) = cx.read_global::<Daemon, _>(|d, _| {
        (
            d.config.record.tray,
            d.events.clone(),
            d.config.record.clone(),
        )
    });
    if !on {
        if let Some(tray) = cx.update_global::<Daemon, _>(|d, _| d.tray.take()) {
            cx.background_spawn(async move { drop(tray) }).detach();
        }
        return;
    }
    cx.spawn(async move |cx| {
        let tray = cx
            .background_spawn(async move { crate::platform::status_notifier(events, &record) })
            .await;
        // The setting may have changed again while the service started.
        let unused = cx.update(|cx| {
            let unused = cx.update_global::<Daemon, _>(|d, _| {
                if d.config.record.tray && d.tray.is_none() {
                    d.tray = tray;
                    None
                } else {
                    tray
                }
            });
            update_tray(cx);
            unused
        });
        if let Some(tray) = unused {
            cx.background_spawn(async move { drop(tray) }).await;
        }
    })
    .detach();
}

/// Pushes recording state, audio devices and frame rates to the tray.
pub(super) fn update_tray(cx: &mut App) {
    let d = cx.global::<Daemon>();
    if let Some(tray) = &d.tray {
        tray.update(&tray_state(d));
    }
}

fn tray_state(d: &Daemon) -> TrayState {
    let config: &Config = &d.config;
    TrayState {
        recording: match &d.recording {
            Slot::Active(a) => Some(RecordingStatus {
                kind: a.kind,
                elapsed: a.session.elapsed(),
                at: Instant::now(),
                paused: a.session.is_paused(),
                saving: false,
            }),
            Slot::Stopping(kind) => Some(RecordingStatus {
                kind: *kind,
                elapsed: Duration::ZERO,
                at: Instant::now(),
                paused: true,
                saving: true,
            }),
            Slot::Idle | Slot::Starting => None,
        },
        audio: d
            .audio_devices
            .iter()
            .map(|device| (device.clone(), config.record.audio.contains(&device.id)))
            .collect(),
        video_fps: config.record.video_fps,
        animation_fps: config.record.animation_fps,
    }
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `m:ss` (or `h:mm:ss`).
fn clock(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs / 3600 {
        0 => format!("{}:{:02}", secs / 60, secs % 60),
        h => format!("{h}:{:02}:{:02}", secs / 60 % 60, secs % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs() -> Vec<(String, IRect, u32)> {
        vec![
            ("DP-1".into(), IRect::new(0, 0, 2560, 1440), 2560),
            ("DP-2".into(), IRect::new(2560, 0, 1920, 1080), 3840),
        ]
    }

    #[test]
    fn target_uses_the_output_holding_most_of_the_area_and_clips_to_it() {
        let (target, scale) = record_target(IRect::new(2400, 100, 400, 300), &outputs()).unwrap();
        assert_eq!(target.output, "DP-2");
        assert_eq!(target.region, IRect::new(2560, 100, 240, 300));
        assert_eq!(scale, 2.0);
        let (target, scale) = record_target(IRect::new(2300, 100, 400, 300), &outputs()).unwrap();
        assert_eq!(target.output, "DP-1");
        assert_eq!(target.region, IRect::new(2300, 100, 260, 300));
        assert_eq!(scale, 1.0);
    }

    #[test]
    fn target_rejects_areas_off_screen_or_too_small() {
        assert!(record_target(IRect::new(-500, -500, 100, 100), &outputs()).is_none());
        assert!(record_target(IRect::new(10, 10, 4, 300), &outputs()).is_none());
    }

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(Duration::from_millis(5_900)), "0:05");
        assert_eq!(clock(Duration::from_secs(754)), "12:34");
        assert_eq!(clock(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn masters_survive_another_instance_and_stale_ones_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let a = Masters::claim(root, Path::new("/run/user/1/quikscope-wayland-1.sock"));
        std::fs::write(a.dir.join("1-100.master"), b"live").unwrap();
        // A second daemon (another display) starting up keeps A's masters.
        let b = Masters::claim(root, Path::new("/run/user/1/quikscope-x11-0.sock"));
        assert_ne!(a.dir, b.dir);
        clean_masters(root, &b.dir);
        assert!(a.dir.join("1-100.master").exists());
        // A restart of A after it died: the old masters go, the new
        // directory is empty and held.
        let dead = a.dir.clone();
        drop(a);
        let a = Masters::claim(root, Path::new("/run/user/1/quikscope-wayland-1.sock"));
        assert_eq!(a.dir, dead);
        assert!(!a.dir.join("1-100.master").exists());
        clean_masters(root, &a.dir);
        let mut left: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        left.sort();
        let mut expected = vec![a.dir.clone(), b.dir.clone()];
        expected.sort();
        assert_eq!(left, expected, "the moved-aside directory is removed");
        // A directory whose daemon exited without cleaning up is removed.
        drop(b);
        clean_masters(root, &a.dir);
        assert_eq!(
            std::fs::read_dir(root).unwrap().count(),
            1,
            "only the live instance remains"
        );
        assert!(!lock_is_free(&a.dir.join(LOCK)), "held while A runs");
    }
}
