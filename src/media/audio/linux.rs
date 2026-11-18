//! Audio devices on linux.

use anyhow::Result;

use crate::media::{AudioDevice, AudioSource};

pub(super) fn devices() -> Result<Vec<AudioDevice>> {
    todo!("audio devices: linux")
}

pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let _ = id;
    todo!("audio capture: linux")
}
