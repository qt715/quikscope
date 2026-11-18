//! Audio devices on macos.

use anyhow::Result;

use crate::media::{AudioDevice, AudioSource};

pub(super) fn devices() -> Result<Vec<AudioDevice>> {
    todo!("audio devices: macos")
}

pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let _ = id;
    todo!("audio capture: macos")
}
