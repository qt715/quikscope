//! The lightweight video editor: playback, trim, mute and export.

use std::path::PathBuf;

use gpui::{Context, IntoElement, Pixels, Render, Size, Window};

use crate::media::MediaInfo;

/// One open video. Config and clipboard come from the
/// [`super::editor::EditorServices`] global, like the annotator.
pub struct VideoEditor {
    _private: (),
}

impl VideoEditor {
    pub fn new(
        path: PathBuf,
        info: MediaInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> VideoEditor {
        let _ = (path, info, window, cx);
        todo!("VideoEditor agent")
    }
}

/// Logical window size for a video of `info` on a screen of `screen`.
pub fn window_size(info: &MediaInfo, screen: Size<Pixels>) -> Size<Pixels> {
    let _ = (info, screen);
    todo!("VideoEditor agent")
}

impl Render for VideoEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = (window, cx);
        gpui::div()
    }
}
