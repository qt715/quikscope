//! Hardware H.264 through Vulkan Video (`gpu-video`), NV12 in and out.
//!
//! One `gpu_video` instance/device is created lazily and shared by every
//! encoder and decoder while any is alive, then released, so an idle daemon
//! holds no video device. A GPU without Vulkan Video H.264 support makes
//! [`Encoder::open`] / [`Decoder::open`] fail, and the caller falls back to
//! OpenH264.

// VideoCodecs owns this file.
