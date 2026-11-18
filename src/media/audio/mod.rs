//! Audio devices for recordings: listing them and capturing from them.
//! Linux uses PulseAudio's API (served by PipeWire on current desktops),
//! Windows WASAPI (loopback for outputs), macOS ScreenCaptureKit for desktop
//! audio and Core Audio for inputs.

use anyhow::Result;

use super::{AudioDevice, AudioSource};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

/// Every device recordings can include, the two defaults
/// ([`super::DEFAULT_OUTPUT`], [`super::DEFAULT_INPUT`]) first.
pub fn devices() -> Result<Vec<AudioDevice>> {
    sys::devices()
}

/// Starts capturing device `id` (an id from [`devices`] or one of the
/// defaults).
pub fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    sys::open(id)
}
