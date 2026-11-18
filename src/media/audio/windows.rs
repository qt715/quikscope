//! Audio devices on windows.

use anyhow::Result;

use crate::media::{AudioDevice, AudioSource};

pub(super) fn devices() -> Result<Vec<AudioDevice>> {
    todo!("audio devices: windows")
}

pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let _ = id;
    todo!("audio capture: windows")
}
