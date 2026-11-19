//! Daemon side of screen recording: one recording at a time, started and
//! stopped by the same request, with tray, outline and notification state.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
use crate::media::record::{Cancelled, Recorded, Session, SessionConfig};
use crate::media::{self, EncoderPreference, Kind, OutputFormat};
use crate::output::OutputFile;
use crate::platform::{Platform, RecordTarget, RecordingStatus, TrayState};
use crate::raster::Image;
use crate::ui::jobs::{self, Job};
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
    Stopping(Saving),
}

/// A stopped recording being written, or a discarded one torn down.
#[derive(Clone)]
pub(super) struct Saving {
    kind: Kind,
    /// Fraction written (f32 bits), for status and the tray; `None` while
    /// a discard tears down.
    progress: Option<Arc<AtomicU32>>,
    /// Set by `record cancel` or the tray to stop the save: a video is
    /// discarded, an animation keeps its lossless master.
    cancel: Arc<AtomicBool>,
}

impl Saving {
    fn percent(&self) -> Option<u8> {
        self.progress
            .as_ref()
            .map(|p| (f32::from_bits(p.load(Ordering::Relaxed)).clamp(0.0, 1.0) * 100.0) as u8)
    }
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
    /// Quitting waits until the recording is saved or discarded.
    job: Job,
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
    // From here on quitting waits for the start to finish or give up.
    let job = jobs::begin()?;
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
                master: master.clone(),
                fps,
                audio,
                encoder,
                fragmented,
            };
            let session = Session::start(session_config, source)?;
            // Names the reserved output, so a recovery can tidy it up.
            if kind == Kind::Animation
                && let Err(e) =
                    std::fs::write(pending_marker(&master), path.to_str().unwrap_or_default())
            {
                log::warn!("marking {} as unsaved: {e}", master.display());
            }
            Ok::<_, anyhow::Error>((session, path))
        })
        .await?
    };
    if daemon(cx, |d, _| d.quitting) {
        // Nothing worth keeping was recorded yet.
        discard(session, &master, cx).await;
        bail!("quikscope is quitting");
    }
    let warnings = session.warnings().join("\n");
    if !warnings.is_empty() {
        notify_error("Recording started with a warning", &warnings, cx);
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
            job,
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
                    if let Some((active, saving)) = take_active(true, cx)
                        && let Err(e) = save(active, saving, cx).await
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
/// until [`save`] (with `progress`) or a discard finishes.
fn take_active(progress: bool, cx: &mut AsyncApp) -> Option<(Box<Active>, Saving)> {
    daemon(cx, |d, _| match std::mem::take(&mut d.recording) {
        Slot::Active(active) => {
            let saving = Saving {
                kind: active.kind,
                progress: progress.then(|| Arc::new(AtomicU32::new(0))),
                cancel: Arc::new(AtomicBool::new(false)),
            };
            d.recording = Slot::Stopping(saving.clone());
            Some((active, saving))
        }
        other => {
            d.recording = other;
            None
        }
    })
}

/// Stops the running recording, saves it and tells the user.
async fn stop(cx: &mut AsyncApp) -> Result<Option<Response>> {
    let (active, saving) = take_active(true, cx).context("nothing is being recorded")?;
    save(active, saving, cx).await
}

/// Stops and saves a running recording before the daemon quits. Saves
/// already under way, and a recording still starting, are jobs the quit
/// waits for (see [`jobs`]); `start` discards a recording when it sees the
/// daemon quitting.
pub(super) async fn stop_for_quit(cx: &mut AsyncApp) {
    if let Some((active, saving)) = take_active(true, cx)
        && let Err(e) = save(active, saving, cx).await
    {
        log::error!("saving the recording before quitting: {e:#}");
        notify_failure("Recording failed", &e, cx);
    }
}

/// Throws a recording away, master included: nothing is left to recover.
async fn discard(session: Session, master: &Path, cx: &mut AsyncApp) {
    let marker = pending_marker(master);
    cx.background_spawn(async move {
        session.cancel();
        let _ = std::fs::remove_file(marker);
    })
    .await;
}

/// Beside an animation's master while its final file is not saved; the
/// next daemon start recovers masters left with one (see [`recover`]).
fn pending_marker(master: &Path) -> PathBuf {
    let mut name = master.as_os_str().to_owned();
    name.push(PENDING);
    PathBuf::from(name)
}

const PENDING: &str = ".pending";

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
    /// A video whose save was cancelled: nothing kept.
    Discarded,
}

async fn save(active: Box<Active>, saving: Saving, cx: &mut AsyncApp) -> Result<Option<Response>> {
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
        job: _job,
    } = *active;
    drop(outline);
    cx.update(update_tray);
    let size = session.size();
    let duration = session.elapsed();
    let task = {
        let (master, saving) = (master.clone(), saving.clone());
        cx.background_spawn(async move {
            let report = |fraction: f32| {
                if let Some(progress) = &saving.progress {
                    progress.store(fraction.to_bits(), Ordering::Relaxed);
                }
            };
            match session.stop(&report, &saving.cancel) {
                Ok(recorded) => Ok(Saved::Done(recorded)),
                Err(error) => salvage(kind, format, path, master, size, duration, error),
            }
        })
    };
    // The tray follows the percentage while the save runs.
    let ticker = cx.spawn(async move |cx| {
        let mut shown = saving.percent();
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let now = saving.percent();
            if now != shown {
                shown = now;
                cx.update(update_tray);
            }
        }
    });
    let saved = task.await;
    drop(ticker);
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
            if kind == Kind::Animation {
                // Saved: a crash from now on has nothing to recover.
                let marker = pending_marker(&master);
                cx.background_spawn(async move { std::fs::remove_file(marker) })
                    .await
                    .unwrap_or_else(|e| log::warn!("removing the unsaved mark: {e}"));
            }
            finish_recording(id, kind, scale, recorded, finish, cx).await
        }
        Saved::Discarded => {
            log::info!("recording discarded while saving");
            Ok(None)
        }
        Saved::MasterOnly { first, info, error } => {
            let cancelled = error.is::<Cancelled>();
            if cancelled {
                log::info!("{error:#}");
            } else {
                log::error!("exporting the animation failed; keeping its master: {error:#}");
            }
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
            let summary = if cancelled {
                "Animation export cancelled"
            } else {
                "Animation export failed"
            };
            let body = format!("{error:#} — click to open it and export it from the editor");
            if let Some(n) = notifier {
                cx.background_spawn(async move {
                    if let Err(e) = n.shot_captured(id, &thumbnail, summary, &body, timeout) {
                        log::warn!("notification failed: {e:#}");
                    }
                })
                .detach();
            }
            if cancelled {
                return Ok(None);
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
    if kind == Kind::Video && error.is::<Cancelled>() {
        return Ok(Saved::Discarded);
    }
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

/// At start-up, recovers what a daemon that died (crash, `kill -9`, power
/// loss) was still writing: recordings partly written are published,
/// masters of unsaved animations open in the annotator, and leftover
/// masters of other runs are removed. One notification sums it up.
pub(super) async fn recover(masters_root: PathBuf, started: SystemTime, cx: &mut AsyncApp) {
    let (own, dirs, animation_format) = daemon(cx, |d, _| {
        (
            d.masters.dir.clone(),
            vec![d.config.record.save_dir(), d.config.save_dir()],
            d.config.record.animation_format(),
        )
    });
    let (masters, videos) = cx
        .background_spawn(async move {
            let masters = recover_masters(&masters_root, &own);
            clean_masters(&masters_root, &own);
            let abandoned: Vec<PathBuf> = masters.iter().filter_map(|(_, o)| o.clone()).collect();
            (masters, recover_parts(&dirs, started, &abandoned))
        })
        .await;
    let mut last = None;
    let mut animations = 0;
    for (master, output) in masters {
        let read = {
            let master = master.clone();
            cx.background_spawn(async move { first_frame(&master) })
                .await
        };
        let (first, info) = match read {
            Ok(read) => read,
            Err(e) => {
                log::warn!("recovered master {} is unreadable: {e:#}", master.display());
                let _ = std::fs::remove_file(&master);
                continue;
            }
        };
        let format = output
            .as_deref()
            .and_then(OutputFormat::from_path)
            .unwrap_or(animation_format);
        let thumbnail = first.thumbnail(384);
        let id = daemon(cx, |d, _| d.media.allocate());
        let shot = Shot::new(id, Arc::new(first), 1.0, None).with_animation(Animation {
            path: master,
            info,
            format,
        });
        insert_shot(shot, cx);
        animations += 1;
        last = Some((id, thumbnail));
    }
    for path in &videos {
        let id = daemon(cx, |d, _| {
            let id = d.media.allocate();
            d.media.add_video(id, path.clone());
            id
        });
        let p = path.clone();
        let thumbnail = cx
            .background_spawn(async move { media::decode::thumbnail(&p, 384) })
            .await
            .unwrap_or_else(|_| Image::filled(1, 1, [0, 0, 0, 0]));
        last = Some((id, thumbnail));
    }
    let Some((id, thumbnail)) = last else {
        return;
    };
    let count = animations + videos.len();
    let summary = if count == 1 {
        "Recovered a recording".to_owned()
    } else {
        format!("Recovered {count} recordings")
    };
    // Videos were cut short when quikscope stopped; animations were never
    // encoded (or their save was cancelled) and export from the editor.
    let mut lines: Vec<String> = videos
        .iter()
        .map(|p| format!("saved {}", ui::editor::tilde(p)))
        .collect();
    if animations > 0 {
        lines.push(format!("{animations} unsaved animation(s) to export"));
    }
    let body = format!("{} — click to open the newest", lines.join("; "));
    log::info!("{summary}: {body}");
    let (notifier, timeout) = daemon(cx, |d, _| {
        (
            d.platform.notifier.clone(),
            d.config.notification_timeout_ms,
        )
    });
    if let Some(n) = notifier {
        cx.background_spawn(async move {
            if let Err(e) = n.shot_captured(id, &thumbnail, &summary, &body, timeout) {
                log::warn!("notification failed: {e:#}");
            }
        })
        .detach();
    }
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
            // During a save, cancelling stops the encode: a video is
            // discarded, an animation keeps its lossless master (the save
            // reports which).
            let saving = daemon(cx, |d, _| match &d.recording {
                Slot::Stopping(s) if s.progress.is_some() => Some(s.clone()),
                _ => None,
            });
            if let Some(saving) = saving {
                saving.cancel.store(true, Ordering::Release);
                log::info!(
                    "cancelling the save of the {} recording",
                    saving.kind.as_str()
                );
                return Ok(Some(status(cx)));
            }
            let (active, _) = take_active(false, cx).context("nothing is being recorded")?;
            let Active {
                session,
                outline,
                master,
                job: _job,
                ..
            } = *active;
            drop(outline);
            cx.update(update_tray);
            // The slot stays `Stopping` until the capture and encoder are
            // gone: a new recording must not race their teardown.
            discard(session, &master, cx).await;
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
            Slot::Stopping(saving) => RecordStatus::Saving(saving.percent()),
            Slot::Idle | Slot::Starting => RecordStatus::Idle,
        })
    })
}

pub(super) async fn audio_devices(cx: &mut AsyncApp) -> Result<Option<Response>> {
    let devices = cx
        .background_spawn(async { media::audio::devices() })
        .await?;
    daemon(cx, |d, _| {
        d.audio_devices = devices.clone();
        d.devices_listed = Some(Instant::now());
    });
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
    /// other directory no running daemon holds (see [`clean_masters`]),
    /// after [`recover_masters`] took out those never saved.
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

/// Lossless masters of animations whose final file was never saved (the
/// daemon died while recording or encoding), moved from directories no
/// running daemon holds into this instance's `own`, before
/// [`clean_masters`] removes the rest. Each comes with the output it was
/// recorded for. A master is recovered once: its mark goes.
pub(super) fn recover_masters(root: &Path, own: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut recovered = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return recovered;
    };
    for dir in entries.flatten().map(|e| e.path()) {
        if dir == own || !dir.is_dir() || !lock_is_free(&dir.join(LOCK)) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for marker in files.flatten().map(|e| e.path()) {
            let Some(master_name) = marker
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(PENDING))
            else {
                continue;
            };
            let master = dir.join(master_name);
            let output = std::fs::read_to_string(&marker)
                .ok()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from);
            let _ = std::fs::remove_file(&marker);
            let moved = own.join(format!("recovered-{master_name}"));
            match std::fs::rename(&master, &moved) {
                Ok(()) => recovered.push((moved, output)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("recovering {}: {e}", master.display()),
            }
        }
    }
    recovered
}

/// Recordings a dead daemon was still writing in `dirs`, published under
/// a free name; partial files and empty placeholders that are of no use
/// are removed. Only files following [`OutputFile`]'s naming, of a process
/// that no longer runs, and older than `before` (this daemon's start) are
/// touched. A video's partial file is published when it plays (MP4, MOV
/// and Matroska are written in fragments that do); other partial files are
/// left for the user unless they belong to an `abandoned` output, an
/// animation whose master was recovered instead.
pub(super) fn recover_parts(
    dirs: &[PathBuf],
    before: SystemTime,
    abandoned: &[PathBuf],
) -> Vec<PathBuf> {
    let mut recovered = Vec::new();
    let mut seen = Vec::new();
    for dir in dirs {
        let Ok(canonical) = std::fs::canonicalize(dir) else {
            continue;
        };
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let part = entry.path();
            let Some((target, pid)) = part_owner(&part) else {
                continue;
            };
            let old = |m: &std::fs::Metadata| m.is_file() && m.modified().is_ok_and(|t| t < before);
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !old(&meta) || pid_alive(pid) {
                continue;
            }
            // The empty placeholder its writer reserved, if there is one.
            let placeholder = std::fs::metadata(&target).is_ok_and(|m| old(&m) && m.len() == 0);
            let tidy = |part: &Path| {
                let _ = std::fs::remove_file(part);
                if placeholder {
                    let _ = std::fs::remove_file(&target);
                }
            };
            if meta.len() == 0 || abandoned.contains(&target) {
                tidy(&part);
                continue;
            }
            if !is_recording(&target) || !plays(&part) {
                log::warn!(
                    "left {} from an earlier run: not a recording that plays",
                    part.display()
                );
                continue;
            }
            if placeholder {
                let _ = std::fs::remove_file(&target);
            }
            let published = OutputFile::reserve(&target).and_then(|output| {
                std::fs::rename(&part, output.part())
                    .with_context(|| format!("moving {}", part.display()))?;
                output.commit()
            });
            match published {
                Ok(path) => {
                    log::info!("recovered {} as {}", part.display(), path.display());
                    recovered.push(path);
                }
                Err(e) => log::warn!("recovering {}: {e:#}", part.display()),
            }
        }
    }
    recovered
}

/// The final path and writer pid of a partial file named by
/// [`OutputFile`]: `<final name>.<pid>.<n>.part`.
fn part_owner(part: &Path) -> Option<(PathBuf, u32)> {
    let rest = part.file_name()?.to_str()?.strip_suffix(".part")?;
    let (rest, n) = rest.rsplit_once('.')?;
    n.parse::<u64>().ok()?;
    let (name, pid) = rest.rsplit_once('.')?;
    let pid = pid.parse::<u32>().ok()?;
    (!name.is_empty()).then(|| (part.with_file_name(name), pid))
}

fn is_recording(path: &Path) -> bool {
    OutputFormat::from_path(path).is_some_and(|f| f.is_video())
}

/// Whether a container our demuxer opens holds any media.
fn plays(path: &Path) -> bool {
    matches!(
        media::container::open_demuxer(path),
        Ok(Some(demuxer)) if !demuxer.duration().is_zero()
    )
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 only asks whether the process exists.
    pid == 0
        || unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is closed before returning; `code` outlives the call.
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(process) => {
                let mut code = 0;
                let exited =
                    GetExitCodeProcess(process, &mut code).is_ok() && code != STILL_ACTIVE.0 as u32;
                let _ = CloseHandle(process);
                !exited
            }
            // Someone else's process: running.
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
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

/// How long a device listing stays fresh. Tray menus ask again every time
/// they open, and each listing opens a sound server connection.
const DEVICES_FRESH: Duration = Duration::from_secs(10);

/// Lists audio devices in the background, unless that happened moments
/// ago, and refreshes the tray with them.
pub(super) fn refresh_devices(cx: &mut App) {
    use gpui::BorrowAppContext as _;
    let fresh = cx.update_global::<Daemon, _>(|d, _| {
        let fresh = d
            .devices_listed
            .is_some_and(|at| at.elapsed() < DEVICES_FRESH);
        if !fresh {
            d.devices_listed = Some(Instant::now());
        }
        fresh
    });
    if fresh {
        return;
    }
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
                progress: None,
            }),
            Slot::Stopping(saving) => Some(RecordingStatus {
                kind: saving.kind,
                elapsed: Duration::ZERO,
                at: Instant::now(),
                paused: true,
                saving: true,
                progress: saving.percent(),
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

    /// A process id nothing runs under any more.
    fn dead_pid() -> u32 {
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/c", "exit"])
                .spawn()
        } else {
            std::process::Command::new("true").spawn()
        }
        .unwrap();
        child.wait().unwrap();
        child.id()
    }

    /// A short Opus-in-Matroska file, as a recording's partial file holds.
    fn write_mkv(path: &Path) {
        use media::codec::opus::{Encoder, FRAME};
        use media::container::{Codec, Container, Packet, Track, TrackKind, create_muxer};
        let mut opus = Encoder::new(64_000).unwrap();
        let packets: Vec<_> = (0..10u64)
            .map(|i| Packet {
                track: 0,
                data: opus.encode(&vec![0.0; FRAME * 2]).unwrap(),
                pts: Duration::from_millis(20 * i),
                duration: Some(Duration::from_millis(20)),
                keyframe: true,
            })
            .collect();
        let track = Track {
            codec: Codec::Opus,
            kind: TrackKind::Audio {
                rate: 48_000,
                channels: 2,
            },
            codec_private: opus.head(),
        };
        let mut muxer = create_muxer(path, Container::Mkv, &[track]).unwrap();
        for packet in packets {
            muxer.write(packet).unwrap();
        }
        muxer.finish().unwrap();
    }

    #[test]
    fn part_names_give_the_final_path_and_writer() {
        assert_eq!(
            part_owner(Path::new("/r/rec_1.mp4.4242.7.part")),
            Some((PathBuf::from("/r/rec_1.mp4"), 4242))
        );
        for other in [
            "/r/rec.mp4",
            "/r/rec.mp4.part",
            "/r/rec.mp4.x.7.part",
            "/r/.1.2.part",
        ] {
            assert_eq!(part_owner(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn crashed_writers_leave_playable_recordings_and_no_litter() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (dead, alive) = (dead_pid(), std::process::id());
        let file = |name: &str, bytes: &[u8]| std::fs::write(d.join(name), bytes).unwrap();
        // A classic MP4/MOV/MKV recording: placeholder plus a partial file
        // that plays.
        file("rec.mkv", b"");
        write_mkv(&d.join(format!("rec.mkv.{dead}.0.part")));
        // A screenshot that never got written.
        file("shot.png", b"");
        file(&format!("shot.png.{dead}.1.part"), b"");
        // Another writer, still running.
        file(&format!("live.mkv.{alive}.2.part"), b"busy");
        // Something partial that does not play: the user decides.
        file(&format!("junk.mp4.{dead}.3.part"), b"not a video");
        // An animation whose master was recovered instead.
        file("anim.gif", b"");
        file(&format!("anim.gif.{dead}.4.part"), b"GIF89a");

        let before = SystemTime::now() + Duration::from_secs(1);
        let recovered = recover_parts(&[d.to_owned(), d.to_owned()], before, &[d.join("anim.gif")]);
        assert_eq!(recovered, [d.join("rec.mkv")]);
        assert!(plays(&d.join("rec.mkv")));
        let mut left: Vec<_> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut expected = vec![
            format!("junk.mp4.{dead}.3.part"),
            format!("live.mkv.{alive}.2.part"),
            "rec.mkv".to_owned(),
        ];
        expected.sort();
        assert_eq!(left, expected);

        // Files from this run (newer than its start) are never touched.
        file(&format!("new.mkv.{dead}.5.part"), b"");
        assert!(recover_parts(&[d.to_owned()], SystemTime::UNIX_EPOCH, &[]).is_empty());
        assert!(d.join(format!("new.mkv.{dead}.5.part")).exists());
    }

    #[test]
    fn unsaved_masters_move_into_the_new_instance_once() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let old = root.join("quikscope-1-00000000.old-1");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("3-100.master"), b"master").unwrap();
        std::fs::write(old.join("3-100.master.pending"), b"/v/rec.webp").unwrap();
        std::fs::write(old.join("4-100.master"), b"saved").unwrap();
        std::fs::write(old.join("5-100.master.pending"), b"").unwrap();
        let own = Masters::claim(root, Path::new("/run/user/1/quikscope-1.sock"));
        let recovered = recover_masters(root, &own.dir);
        let moved = own.dir.join("recovered-3-100.master");
        assert_eq!(
            recovered,
            [(moved.clone(), Some(PathBuf::from("/v/rec.webp")))]
        );
        assert_eq!(std::fs::read(&moved).unwrap(), b"master");
        clean_masters(root, &own.dir);
        assert!(!old.exists(), "the rest goes");
        assert!(recover_masters(root, &own.dir).is_empty());
    }

    /// Waits until nobody holds `dir`'s lock. Dropping it here does not
    /// free it at once while a sibling test spawns a process: until that
    /// child execs, it shares every open file description, this lock's
    /// included. A daemon's lock is freed by its exit, which has no such
    /// window, so this is a test-only wait.
    fn released(dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !lock_is_free(&dir.join(LOCK)) {
            assert!(Instant::now() < deadline, "{} stays locked", dir.display());
            std::thread::sleep(Duration::from_millis(5));
        }
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
        released(&dead);
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
        let b_dir = b.dir.clone();
        drop(b);
        released(&b_dir);
        clean_masters(root, &a.dir);
        assert_eq!(
            std::fs::read_dir(root).unwrap().count(),
            1,
            "only the live instance remains"
        );
        assert!(!lock_is_free(&a.dir.join(LOCK)), "held while A runs");
    }
}
