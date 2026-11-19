//! ISO BMFF (MP4/MOV) muxing and demuxing on `mp4-atom` boxes.

// Containers owns this file.
use super::{Container, Packet, Track};
use anyhow::Result;
use std::path::Path;

pub struct Mp4Muxer {}

impl Mp4Muxer {
    pub fn create(path: &Path, container: Container, tracks: &[Track]) -> Result<Self> {
        let _ = (path, container, tracks);
        todo!("Containers")
    }
}

impl super::Muxer for Mp4Muxer {
    fn write(&mut self, packet: Packet) -> Result<()> {
        let _ = packet;
        todo!("Containers")
    }
    fn finish(self: Box<Self>) -> Result<()> {
        todo!("Containers")
    }
}
