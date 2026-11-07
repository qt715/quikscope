//! Native macOS services. Cocoa shell initialization runs on GPUI's main thread.
use super::{Capture, Clipboard, EventSink, Notifier, Platform, Surfaces};
use std::sync::Arc;

mod capture;
mod clipboard;
mod desktop;
mod notify;
pub mod shell;
pub use desktop::selection_outputs;
pub use shell::autostart;

pub fn connect(events: EventSink) -> Platform {
    let notifier = notify::Notifications::new(events.clone())
        .map(|n| Arc::new(n) as Arc<dyn Notifier>)
        .map_err(|e| log::warn!("macOS notifications unavailable: {e:#}"))
        .ok();
    let shell = shell::Shell::new(events)
        .map_err(|e| log::warn!("macOS menu bar unavailable: {e:#}"))
        .ok();
    Platform {
        name: "macos",
        capture: Some(Arc::new(capture::ScreenCapture) as Arc<dyn Capture>),
        desktop: Arc::new(desktop::MacDesktop),
        clipboard: Some(Arc::new(clipboard::Pasteboard) as Arc<dyn Clipboard>),
        notifier,
        surfaces: Surfaces::MacOS,
        capture_may_prompt: true,
        shell,
    }
}

/// Hide native panels before GPUI schedules their destruction. This must run
/// outside App borrows: Cocoa ordering may synchronously dispatch GPUI callbacks.
pub(crate) fn hide_overlays(handles: &[raw_window_handle::RawWindowHandle]) -> anyhow::Result<()> {
    use objc2::{msg_send, runtime::AnyObject};
    for handle in handles {
        let raw_window_handle::RawWindowHandle::AppKit(handle) = handle else {
            anyhow::bail!("mixed macOS selection surfaces");
        };
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let window: *mut AnyObject = msg_send![view, window];
            if !window.is_null() {
                let _: () = msg_send![window, orderOut: std::ptr::null::<AnyObject>()];
            }
        }
    }
    // NSWindow ordering is sent to WindowServer before a new ScreenCaptureKit
    // stream starts; stream frames therefore cannot include the hidden panels.
    Ok(())
}
