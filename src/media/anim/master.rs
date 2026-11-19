//! Lossless recording master for animations.

// Animations owns this file. Contract (see local://media-plan.md):
// pub struct MasterWriter; MasterWriter::create(path: &Path, width: u32, height: u32) -> Result<Self>
//   push(&mut self, frame: &Image /* BGRA */, pts: Duration) -> Result<()>  (pts strictly increasing)
//   finish(self, end: Duration) -> Result<()>  (end = recording length; last frame lasts until it)
// pub struct MasterReader: super::AnimDecoder; MasterReader::open(path) -> Result<Self>
//   seek(&mut self, to: Duration) -> Result<()> (via keyframe index; next_frame returns the frame showing at `to`)
// pub fn is_master(path: &Path) -> bool  (magic check)
