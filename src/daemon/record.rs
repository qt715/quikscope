//! Daemon side of screen recording: one recording at a time, started and
//! stopped by the same request, with tray, outline and notification state.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, AsyncApp};

use super::{
    Daemon, EditorTool, claim_selection, daemon, insert_shot, open_editor, select_frozen,
    select_region,
};
use crate::config::{self, Config};
use crate::geom::IRect;
use crate::ipc::{Mode, RecordAction, RecordOptions, RecordState, Response};
use crate::media::record::{Recorded, Session, SessionConfig};
use crate::media::{self, EncoderPreference, Kind};
use crate::platform::{Platform, RecordTarget, RecordingStatus, TrayState};
use crate::ui::overlay::Selected;
use crate::ui::{self, Animation, Shot};

/// Smallest region worth recording, in logical pixels per side; encoders
/// also need even dimensions of at least 2.
const MIN_SIDE: i32 = 8;

/// The recording slot. `Starting` covers selection and encoder start-up,
/// `Stopping` the final encode, so a second key press in either phase is
/// refused instead of starting a parallel recording.
#[derive(Default)]
pub(super) enum Slot {
    #[default]
    Idle,
    Starting,
    Active(Box<Active>),
    Stopping,
}

pub(super) struct Active {
    id: u64,
    kind: Kind,
    session: Session,
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
pub(super) async fn toggle(options: RecordOptions, cx: &mut AsyncApp) -> Result<Option<Response>> {
    match daemon(cx, |d, _| match d.recording {
        Slot::Idle => None,
        Slot::Starting => Some("a recording is already starting"),
        Slot::Active(_) => Some(""),
        Slot::Stopping => Some("the previous recording is still being saved"),
    }) {
        Some("") => return stop(cx).await,
        Some(busy) => bail!(busy),
        None => {}
    }
    daemon(cx, |d, _| d.recording = Slot::Starting);
    let result = start(options, cx).await;
    if !matches!(result, Ok(Some(_))) {
        daemon(cx, |d, _| d.recording = Slot::Idle);
    }
    if let Err(e) = &result {
        // Keybinds and tray menus have no terminal to print the error in.
        super::notify_error("Recording failed", &format!("{e:#}"), cx);
    }
    update_tray(cx);
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
    let path = rc.new_save_path(now, format);
    let id = daemon(cx, |d, _| {
        let id = d.next_id;
        d.next_id += 1;
        id
    });
    let master = config::cache_dir()
        .join("recordings")
        .join(format!("{id}-{}.mkv", unix_secs(now)));
    let session_config = SessionConfig {
        path: path.clone(),
        format,
        master,
        fps,
        audio: match kind {
            Kind::Video => o.audio.clone().unwrap_or_else(|| rc.audio.clone()),
            Kind::Animation => Vec::new(),
        },
        encoder: EncoderPreference::parse(&rc.encoder).unwrap_or_default(),
    };
    let session = cx
        .background_spawn(async move {
            for dir in [session_config.path.parent(), session_config.master.parent()]
                .into_iter()
                .flatten()
            {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            Session::start(session_config, source)
        })
        .await?;
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
            session,
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
                    super::notify_error("Recording stopped", &message, cx);
                    if let Err(e) = stop(cx).await {
                        log::error!("saving the interrupted recording: {e:#}");
                    }
                    return;
                }
            }
        }
    })
    .detach();
}

/// Stops the running recording, saves it and tells the user.
async fn stop(cx: &mut AsyncApp) -> Result<Option<Response>> {
    let active = daemon(cx, |d, _| match std::mem::replace(&mut d.recording, Slot::Stopping) {
        Slot::Active(active) => Some(active),
        other => {
            d.recording = other;
            None
        }
    });
    let Some(active) = active else {
        bail!("nothing is being recorded");
    };
    let Active {
        id,
        kind,
        session,
        outline,
        finish,
        scale,
    } = *active;
    drop(outline);
    update_tray(cx);
    let result = cx.background_spawn(async move { session.stop() }).await;
    daemon(cx, |d, _| d.recording = Slot::Idle);
    update_tray(cx);
    let recorded = result.context("finishing the recording")?;
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

async fn finish_recording(
    id: u64,
    kind: Kind,
    scale: f32,
    recorded: Recorded,
    finish: Finish,
    cx: &mut AsyncApp,
) -> Result<Option<Response>> {
    match kind {
        Kind::Video => daemon(cx, |d, _| {
            d.videos.insert(id, recorded.path.clone());
        }),
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

    let (clipboard, notifier, timeout) = daemon(cx, |d, _| {
        (
            d.platform.clipboard.clone(),
            d.platform.notifier.clone(),
            d.config.notification_timeout_ms,
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
    if finish.edit {
        cx.update(|cx| open_media(id, cx))?;
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
pub(super) fn first_frame(path: &std::path::Path) -> Result<(crate::raster::Image, media::MediaInfo)> {
    let mut decoder = media::decode::Decoder::open(path)?;
    let info = decoder.info().clone();
    let frame = decoder
        .next_frame()?
        .with_context(|| format!("{} has no frames", path.display()))?;
    Ok((frame.image, info))
}

/// Opens what id `id` names: a shot (screenshot or animation) in the
/// annotator, or a recorded video in the video editor.
pub(super) fn open_media(id: u64, cx: &mut App) -> Result<()> {
    let video = cx.read_global::<Daemon, _>(|d, _| d.videos.get(&id).cloned());
    match video {
        Some(path) => super::open_video(id, path, cx),
        None => open_editor(id, EditorTool::Default, cx),
    }
}

pub(super) async fn control(action: RecordAction, cx: &mut AsyncApp) -> Result<Option<Response>> {
    match action {
        RecordAction::Stop => stop(cx).await,
        RecordAction::Cancel => {
            let active = daemon(cx, |d, _| match std::mem::take(&mut d.recording) {
                Slot::Active(active) => Some(active),
                other => {
                    d.recording = other;
                    None
                }
            });
            let Some(active) = active else {
                bail!("nothing is being recorded");
            };
            let Active {
                session, outline, ..
            } = *active;
            drop(outline);
            update_tray(cx);
            cx.background_spawn(async move { session.cancel() }).await;
            log::info!("recording discarded");
            Ok(Some(Response::RecordStatus(None)))
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
            update_tray(cx);
            Ok(Some(status(cx)))
        }
        RecordAction::Status => Ok(Some(status(cx))),
    }
}

fn status(cx: &mut AsyncApp) -> Response {
    daemon(cx, |d, _| {
        Response::RecordStatus(match &d.recording {
            Slot::Active(a) => Some(RecordState {
                kind: a.kind,
                elapsed_ms: a.session.elapsed().as_millis() as u64,
                paused: a.session.is_paused(),
            }),
            _ => None,
        })
    })
}

pub(super) async fn audio_devices(cx: &mut AsyncApp) -> Result<Option<Response>> {
    let devices = cx
        .background_spawn(async { media::audio::devices() })
        .await?;
    daemon(cx, |d, _| d.audio_devices = devices.clone());
    update_tray(cx);
    Ok(Some(Response::AudioDevices(devices)))
}

/// Lists audio devices in the background and refreshes the tray with them.
pub(super) fn refresh_devices(cx: &mut App) {
    cx.spawn(async move |cx| {
        match cx
            .background_spawn(async { media::audio::devices() })
            .await
        {
            Ok(devices) => daemon(cx, |d, _| d.audio_devices = devices),
            Err(e) => log::warn!("listing audio devices: {e:#}"),
        }
        update_tray(cx);
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
    persist(&[(&["record", key], toml_edit::Value::from(i64::from(fps)))], cx);
}

fn persist(values: &[(&[&str], toml_edit::Value)], cx: &mut App) {
    match config::persist_values(&config::config_path(), values) {
        Ok(config) => super::apply_config(Arc::new(config), cx),
        Err(e) => log::error!("saving recording settings: {e:#}"),
    }
    cx.spawn(async move |cx| update_tray(cx)).detach();
}

/// Pushes recording state, audio devices and frame rates to the tray.
pub(super) fn update_tray(cx: &mut AsyncApp) {
    let (tray, state) = daemon(cx, |d, _| (d.platform.tray.clone(), tray_state(d)));
    if let Some(tray) = tray {
        tray.update(&state);
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
            }),
            _ => None,
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
}
