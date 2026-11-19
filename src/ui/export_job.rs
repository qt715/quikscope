//! Copy and Save of videos and animations, started from an editor window.
//!
//! The encode runs on a thread of its own and outlives the window that
//! started it, like a still-image save: closing the editor mid-export no
//! longer throws the work away. A copied file still reaches the clipboard,
//! and with the window gone a desktop notification says how it ended. Only
//! an explicit cancel stops an export.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use futures::StreamExt as _;
use gpui::{AppContext as _, Context};

use super::editor::{EditorServices, tilde};
use crate::media::OutputFormat;

/// What an export ends in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// Put on the clipboard as a file.
    Copy,
    /// Written into the recordings folder.
    Save,
}

/// Progress reports (0.0–1.0) from the export thread.
pub type Progress<'a> = &'a (dyn Fn(f32) + Sync);

/// A running export, as the window that started it sees it.
pub struct Export {
    pub output: Output,
    pub format: OutputFormat,
    /// f32 bits, 0.0–1.0.
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
}

impl Export {
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }

    /// Asks the export to stop; it removes its partial file and finishes as
    /// cancelled.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// How an export ended.
pub struct Finished {
    pub output: Output,
    pub format: OutputFormat,
    pub result: anyhow::Result<PathBuf>,
    pub cancelled: bool,
}

impl Finished {
    /// The toast for the outcome, and whether it reports an error.
    pub fn message(&self) -> (String, bool) {
        match (&self.result, self.output) {
            (Ok(path), Output::Save) => (format!("Saved {}", tilde(path)), false),
            (Ok(_), Output::Copy) => (format!("Copied {}", self.format.label()), false),
            (Err(_), _) if self.cancelled => ("Export cancelled".into(), false),
            (Err(e), _) => (format!("Export failed: {e:#}"), true),
        }
    }
}

/// Runs `work` on its own thread. The window repaints as progress moves by
/// whole percents; when the work ends, a copied file goes on the clipboard
/// and `finished` runs if the editor still exists, else a notification
/// reports the outcome (a cancel needs none).
pub fn start<T: 'static>(
    output: Output,
    format: OutputFormat,
    services: &EditorServices,
    work: impl FnOnce(Progress<'_>, &AtomicBool) -> anyhow::Result<PathBuf> + Send + 'static,
    finished: impl FnOnce(&mut T, &Finished, &mut Context<T>) + 'static,
    cx: &mut Context<T>,
) -> anyhow::Result<Export> {
    // Quitting waits for the file, and for its clipboard copy below.
    let job = super::jobs::begin()?;
    let progress = Arc::new(AtomicU32::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    // Progress repaints and the result arrive over one channel, so the
    // window wakes when something changed rather than on a timer.
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Option<anyhow::Result<PathBuf>>>();
    let (p, c) = (progress.clone(), cancel.clone());
    // A dedicated thread: an export runs for minutes and would otherwise
    // hold one of GPUI's few background workers.
    std::thread::Builder::new()
        .name("quikscope-export".into())
        .spawn(move || {
            let report = |f: f32| {
                let before = f32::from_bits(p.swap(f.to_bits(), Ordering::Relaxed));
                if (f * 100.0) as u32 != (before * 100.0) as u32 {
                    let _ = tx.unbounded_send(None);
                }
            };
            let result = work(&report, &c);
            let _ = tx.unbounded_send(Some(result));
        })?;
    let (clipboard, notifier) = (services.clipboard.clone(), services.notifier.clone());
    let stopped = cancel.clone();
    cx.spawn(async move |this, cx| {
        let result = loop {
            match rx.next().await {
                Some(Some(result)) => break result,
                // The window may be gone; the export carries on regardless.
                Some(None) => {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                None => break Err(anyhow::anyhow!("The export thread stopped")),
            }
        };
        let done = Finished {
            output,
            format,
            cancelled: stopped.load(Ordering::Acquire),
            result,
        };
        match &done.result {
            Ok(path) => {
                log::info!("exported {}", path.display());
                if output == Output::Copy
                    && let Some(c) = &clipboard
                {
                    c.set_files(std::slice::from_ref(path));
                }
            }
            Err(_) if done.cancelled => log::info!("export cancelled"),
            Err(e) => log::error!("export failed: {e:#}"),
        }
        drop(job);
        if this.update(cx, |e, cx| finished(e, &done, cx)).is_ok() || done.cancelled {
            return;
        }
        let Some(notifier) = notifier else { return };
        let (message, error) = done.message();
        let (summary, body) = match &done.result {
            Err(e) if error => ("Export failed".to_owned(), format!("{e:#}")),
            _ => (message, String::new()),
        };
        cx.background_spawn(async move {
            if let Err(e) = notifier.message(&summary, &body) {
                log::warn!("notification failed: {e:#}");
            }
        })
        .detach();
    })
    .detach();
    Ok(Export {
        output,
        format,
        progress,
        cancel,
    })
}
