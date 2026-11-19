//! Matroska muxing (`mkv-element` elements) and demuxing (`matroska-demuxer`).
//!
//! Timestamps use a 1 ms TimestampScale. Block times are signed 16-bit
//! offsets from their cluster's time, so 1 ms lets a cluster span ±32 s
//! (keyframe intervals stay far below); 1 µs would cap clusters at 32 ms.
//! It is also what WebM mandates and every player expects, and rounding to
//! whole milliseconds is far below any audible or visible A/V offset.

mod demux;
mod ebml;
mod mux;

pub use demux::MkvDemuxer;
pub use mux::MkvMuxer;

/// Nanoseconds per Matroska tick in the files we write.
const TIMESTAMP_SCALE: u64 = 1_000_000;

const AVC: &str = "V_MPEG4/ISO/AVC";
const OPUS: &str = "A_OPUS";
const AV1: &str = "V_AV1";
/// `A_AAC` and the legacy `A_AAC/MPEG4/LC`-style ids.
const AAC_PREFIX: &str = "A_AAC";
