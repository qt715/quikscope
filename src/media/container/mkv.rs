//! Matroska muxing (`mkv-element` elements) and demuxing (`matroska-demuxer`).

// Containers owns this file.
use super::{Packet, Track};
use anyhow::Result;
use std::path::Path;

pub struct MkvMuxer {}

impl MkvMuxer {
    pub fn create(path: &Path, tracks: &[Track]) -> Result<Self> {
        let _ = (path, tracks);
        todo!("Containers")
    }
}

impl super::Muxer for MkvMuxer {
    fn write(&mut self, packet: Packet) -> Result<()> {
        let _ = packet;
        todo!("Containers")
    }
    fn finish(self: Box<Self>) -> Result<()> {
        todo!("Containers")
    }
}
