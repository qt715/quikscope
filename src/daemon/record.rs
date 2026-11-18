//! Daemon side of screen recording: one recording at a time, started and
//! stopped by the same request, with tray, outline and notification state.

use gpui::AsyncApp;

use crate::ipc::{RecordAction, RecordOptions, Response};
use crate::media::Kind;

pub(super) async fn toggle(options: RecordOptions, cx: &mut AsyncApp) -> anyhow::Result<Option<Response>> {
    let _ = (options, cx);
    todo!("record controller")
}

pub(super) async fn control(action: RecordAction, cx: &mut AsyncApp) -> anyhow::Result<Option<Response>> {
    let _ = (action, cx);
    todo!("record controller")
}

pub(super) async fn audio_devices(cx: &mut AsyncApp) -> anyhow::Result<Option<Response>> {
    let _ = cx;
    todo!("record controller")
}

pub(super) fn set_audio(id: String, enabled: bool, cx: &mut gpui::App) {
    let _ = (id, enabled, cx);
    todo!("record controller")
}

pub(super) fn set_fps(kind: Kind, fps: u32, cx: &mut gpui::App) {
    let _ = (kind, fps, cx);
    todo!("record controller")
}
