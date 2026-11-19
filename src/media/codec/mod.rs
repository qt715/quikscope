//! Elementary-stream codecs without FFmpeg.
//!
//! - H.264: Vulkan Video through `gpu-video` (hardware, NVIDIA/AMD/Intel on
//!   Linux and Windows) with OpenH264 (software) as the fallback, for both
//!   encoding and decoding. H.264 crosses this API as **Annex-B** access
//!   units: start codes, SPS/PPS in-band before every keyframe.
//! - Opus (libopus) for every audio track we write; AAC-LC (symphonia) for
//!   reading third-party files.
//! - Audio resampling (rubato).
//!
//! Pictures are YUV 4:2:0, BT.709 limited range, tightly packed; see
//! [`super::convert`] for BGRA conversion.

use std::time::Duration;

use anyhow::Result;

use super::{AudioSpec, EncoderPreference};

pub mod aac;
pub mod h264_soft;
pub mod h264_vulkan;
pub mod opus;
pub mod resample;

/// Plane layout of a [`Yuv420`] picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvLayout {
    /// Y plane, then interleaved UV (what Vulkan Video uses).
    Nv12,
    /// Y, U, V planes (what OpenH264 uses).
    I420,
}

/// A 4:2:0 picture with even `width` and `height`, BT.709 limited range.
/// `data` holds `width*height*3/2` bytes in `layout`, rows unpadded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Yuv420 {
    pub width: u32,
    pub height: u32,
    pub layout: YuvLayout,
    pub data: Vec<u8>,
}

/// One H.264 access unit (Annex-B) in presentation order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts: Duration,
    pub keyframe: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEncoderConfig {
    /// Even pixel sizes.
    pub width: u32,
    pub height: u32,
    /// Nominal rate, for rate control; timestamps come with each picture.
    pub fps: u32,
    /// Target average bits per second.
    pub bitrate: u32,
    /// A keyframe at least every this many pictures (also forced on demand).
    pub keyframe_interval: u32,
}

impl VideoEncoderConfig {
    /// Settings for screen content: sharp text needs far more bits than
    /// camera video of the same size.
    pub fn for_screen(width: u32, height: u32, fps: u32) -> Self {
        let pixels = u64::from(width) * u64::from(height) * u64::from(fps.max(1));
        VideoEncoderConfig {
            width,
            height,
            fps,
            bitrate: (pixels / 8).clamp(2_000_000, 80_000_000) as u32,
            keyframe_interval: fps.max(1),
        }
    }
}

pub trait VideoEncoder: Send {
    /// For logs, e.g. "Vulkan H.264 (NVIDIA GeForce RTX 5090)" or "OpenH264".
    fn name(&self) -> &str;
    /// The layout [`encode`](Self::encode) expects.
    fn input_layout(&self) -> YuvLayout;
    /// Encodes one picture (its size must match the config). Returns the
    /// access units finished so far; there are no B-frames, so decode order
    /// is presentation order.
    fn encode(&mut self, picture: &Yuv420, pts: Duration, keyframe: bool)
    -> Result<Vec<EncodedFrame>>;
    /// Drains everything still in flight.
    fn finish(&mut self) -> Result<Vec<EncodedFrame>>;
}

/// The best working H.264 encoder: Vulkan Video unless `preference` is
/// [`EncoderPreference::Software`] or no GPU supports it, else OpenH264.
pub fn h264_encoder(
    config: &VideoEncoderConfig,
    preference: EncoderPreference,
) -> Result<Box<dyn VideoEncoder>> {
    let _ = (config, preference);
    todo!("VideoCodecs: pick h264_vulkan, fall back to h264_soft")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedPicture {
    pub picture: Yuv420,
    pub pts: Duration,
}

pub trait VideoDecoder: Send {
    fn name(&self) -> &str;
    /// Feeds one Annex-B access unit (SPS/PPS in-band before keyframes, as
    /// demuxers deliver them). Returns pictures completed so far, in
    /// presentation order, each with the pts of the access unit it came from.
    fn decode(&mut self, access_unit: &[u8], pts: Duration) -> Result<Vec<DecodedPicture>>;
    /// Returns the pictures still buffered (call at end of stream, and before
    /// discarding the decoder after a seek).
    fn flush(&mut self) -> Result<Vec<DecodedPicture>>;
}

/// The best working H.264 decoder: Vulkan Video unless `preference` is
/// [`EncoderPreference::Software`] or unavailable, else OpenH264.
pub fn h264_decoder(preference: EncoderPreference) -> Result<Box<dyn VideoDecoder>> {
    let _ = preference;
    todo!("VideoCodecs: pick h264_vulkan, fall back to h264_soft")
}

/// Decodes one compressed audio track to interleaved f32 at its own
/// [`spec`](AudioDecoder::spec).
pub trait AudioDecoder: Send {
    fn spec(&self) -> AudioSpec;
    fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>>;
    /// Forgets decoder state (after a seek).
    fn reset(&mut self);
}

/// Opens a decoder for a demuxed audio track (`codec_private` as the
/// container stores it: OpusHead for Opus, AudioSpecificConfig for AAC).
pub fn audio_decoder(
    codec: super::container::Codec,
    codec_private: &[u8],
    spec: AudioSpec,
) -> Result<Box<dyn AudioDecoder>> {
    let _ = (codec, codec_private, spec);
    todo!("AudioCodecs: opus::Decoder or aac::Decoder")
}
