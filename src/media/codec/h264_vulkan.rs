//! Hardware H.264 through Vulkan Video (`gpu-video`), NV12 in and out.
//!
//! One `gpu_video` instance/device is created lazily and shared by every
//! encoder and decoder while any is alive, then released, so an idle daemon
//! holds no video device. A GPU without Vulkan Video H.264 support makes
//! [`Encoder::open`] / [`Decoder::open`] fail, and the caller falls back to
//! OpenH264.
//!
//! gpu-video hands results to a callback on its own waiter thread; a channel
//! brings them back to the caller's thread. Timestamps travel through
//! gpu-video's opaque `u64` pts as nanoseconds, so they come back exact.

use std::num::NonZeroU32;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use gpu_video::capabilities::{DeviceType, EncodeProfileCapabilities, VideoAdapterInfo};
use gpu_video::parameters::{
    ColorRange, ColorSpace, DecoderParameters, EncoderOutputParameters, EncoderParametersH264,
    EncoderPreset, EncoderUsage, ProfileH264, RateControl, VideoDeviceDescriptor,
    VideoInstanceDescriptor, VideoParameters,
};
use gpu_video::{
    BytesDecoderH264, BytesEncoderH264, DecoderEvent, EncodedInputChunk, EncodedOutputChunk,
    InputFrame, OutputFrame, RawFrameData, RawFrameRef, VideoAdapter, VideoDevice, VideoInstance,
};

use super::{
    DecodedPicture, EncodedFrame, VideoDecoder, VideoEncoder, VideoEncoderConfig, Yuv420, YuvLayout,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Usage {
    Encode = 0,
    Decode = 1,
}

/// A Vulkan video device; alive while an encoder or decoder holds it.
struct Gpu {
    // Declared before the instance it comes from, so dropped first.
    device: VideoDevice,
    _instance: VideoInstance,
    name: String,
    can_encode: bool,
    can_decode: bool,
}

impl Gpu {
    fn supports(&self, usage: Usage) -> bool {
        match usage {
            Usage::Encode => self.can_encode,
            Usage::Decode => self.can_decode,
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        log::debug!("Vulkan Video: released {}", self.name);
    }
}

struct Shared {
    gpu: Weak<Gpu>,
    /// Why no adapter can encode / decode H.264, once established: asking
    /// again would only repeat the Vulkan instance setup to fail the same way.
    unsupported: [Option<String>; 2],
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    gpu: Weak::new(),
    unsupported: [None, None],
});

/// The shared device if it supports `usage`, else a newly opened one.
fn gpu(usage: Usage) -> Result<Arc<Gpu>> {
    let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(gpu) = shared.gpu.upgrade()
        && gpu.supports(usage)
    {
        return Ok(gpu);
    }
    if let Some(reason) = &shared.unsupported[usage as usize] {
        bail!("{reason}");
    }
    match open_gpu(usage)? {
        Ok(gpu) => {
            let gpu = Arc::new(gpu);
            shared.gpu = Arc::downgrade(&gpu);
            Ok(gpu)
        }
        Err(reason) => {
            shared.unsupported[usage as usize] = Some(reason.clone());
            bail!("{reason}")
        }
    }
}

/// Opens the best adapter for `usage`: discrete before integrated GPUs, and
/// among those one that can also do the other direction, so an editor that
/// decodes and encodes shares one device. `Ok(Err(reason))` means no
/// adapter can ever do it in this process.
fn open_gpu(usage: Usage) -> Result<Result<Gpu, String>> {
    let instance = match VideoInstance::new(&VideoInstanceDescriptor::default()) {
        Ok(instance) => instance,
        Err(err) => return Ok(Err(format!("Vulkan Video unavailable: {err}"))),
    };
    let rank = |info: &VideoAdapterInfo| match info.device_type {
        DeviceType::DiscreteGpu => 0u8,
        DeviceType::IntegratedGpu => 1,
        DeviceType::VirtualGpu => 2,
        DeviceType::Other | DeviceType::Cpu => 3,
    };
    let mut best: Option<((u8, bool), [bool; 2], VideoAdapter<'_>)> = None;
    let adapters = match instance.iter_adapters() {
        Ok(adapters) => adapters,
        Err(err) => return Ok(Err(format!("Vulkan Video unavailable: {err}"))),
    };
    for adapter in adapters {
        let info = adapter.info();
        let can = [encodes_h264(info), decodes_h264(info)];
        log::debug!(
            "Vulkan Video adapter {} ({:?}): H.264 encode {}, decode {}",
            info.name,
            info.device_type,
            can[0],
            can[1]
        );
        if !can[usage as usize] {
            continue;
        }
        let key = (rank(info), !(can[0] && can[1]));
        if best.as_ref().is_none_or(|(best_key, ..)| key < *best_key) {
            best = Some((key, can, adapter));
        }
    }
    let Some((_, [can_encode, can_decode], adapter)) = best else {
        let what = match usage {
            Usage::Encode => "encode",
            Usage::Decode => "decode",
        };
        return Ok(Err(format!("no Vulkan Video adapter can {what} H.264")));
    };
    let name = adapter.info().name.clone();
    let device = adapter
        .create_device(&VideoDeviceDescriptor::default())
        .with_context(|| format!("opening Vulkan video device {name}"))?;
    log::info!("Vulkan Video: opened {name} (H.264 encode {can_encode}, decode {can_decode})");
    Ok(Ok(Gpu {
        device,
        // A shared handle: the adapters borrowed the local one.
        _instance: instance.clone(),
        name,
        can_encode,
        can_decode,
    }))
}

fn encodes_h264(info: &VideoAdapterInfo) -> bool {
    info.encode_capabilities
        .h264
        .is_some_and(|caps| caps.max_profile().is_some())
}

fn decodes_h264(info: &VideoAdapterInfo) -> bool {
    info.decode_capabilities.h264.is_some_and(|caps| {
        caps.baseline_profile.is_some()
            || caps.main_profile.is_some()
            || caps.high_profile.is_some()
    })
}

fn pts_to_u64(pts: Duration) -> Result<u64> {
    u64::try_from(pts.as_nanos()).context("pts out of range")
}

fn pts_from_u64(pts: Option<u64>) -> Result<Duration> {
    pts.map(Duration::from_nanos)
        .context("gpu-video returned a result without its timestamp")
}

/// Vulkan Video H.264 encoder: High profile (or the best the GPU has), no
/// B-frames, SPS/PPS before every IDR.
pub struct Encoder {
    // Declared before `_gpu`, so the session goes before the device.
    encoder: BytesEncoderH264,
    chunks: Receiver<EncodedOutputChunk<Vec<u8>>>,
    config: VideoEncoderConfig,
    name: String,
    _gpu: Arc<Gpu>,
}

impl Encoder {
    pub fn open(config: &VideoEncoderConfig) -> Result<Encoder> {
        let gpu = gpu(Usage::Encode)?;
        let caps = gpu
            .device
            .encode_capabilities()
            .h264
            .context("device lost its H.264 encode support")?;
        let (profile, limits): (ProfileH264, EncodeProfileCapabilities) = [
            (ProfileH264::High, caps.high_profile),
            (ProfileH264::Main, caps.main_profile),
            (ProfileH264::Baseline, caps.baseline_profile),
        ]
        .into_iter()
        .find_map(|(profile, limits)| Some((profile, limits?)))
        .context("no H.264 encode profile")?;
        let VideoEncoderConfig {
            width,
            height,
            fps,
            bitrate,
            keyframe_interval,
        } = *config;
        ensure!(
            (limits.min_width..=limits.max_width).contains(&width)
                && (limits.min_height..=limits.max_height).contains(&height),
            "{} encodes {}x{} to {}x{} pictures, not {width}x{height}",
            gpu.name,
            limits.min_width,
            limits.min_height,
            limits.max_width,
            limits.max_height
        );
        // VBR lets static screens cost almost nothing and bursts (scrolling,
        // video) use up to twice the average; the 2 s buffer allows long
        // bursts, which a file (unlike a stream) can afford.
        let buffer = Duration::from_secs(2);
        let rate_control = if limits.rate_control.vbr_supported {
            RateControl::VariableBitrate {
                average_bitrate: u64::from(bitrate),
                max_bitrate: u64::from(bitrate) * 2,
                virtual_buffer_size: buffer,
            }
        } else if limits.rate_control.cbr_supported {
            RateControl::ConstantBitrate {
                bitrate: u64::from(bitrate),
                virtual_buffer_size: buffer,
            }
        } else {
            RateControl::EncoderDefault
        };
        let parameters = EncoderParametersH264 {
            input_parameters: VideoParameters {
                width: NonZeroU32::new(width).context("zero width")?,
                height: NonZeroU32::new(height).context("zero height")?,
                target_framerate: fps.max(1).into(),
            },
            output_parameters: EncoderOutputParameters {
                idr_period: NonZeroU32::new(keyframe_interval),
                rate_control,
                max_references: None,
                profile,
                preset: EncoderPreset::HighQuality,
                usage_flags: Some(EncoderUsage::Recording),
                inline_stream_params: Some(true),
                color_space: Some(ColorSpace::BT709),
                color_range: Some(ColorRange::Limited),
            },
            max_in_flight_submissions: None,
        };
        let (sender, chunks) = mpsc::channel();
        let encoder = gpu
            .device
            .create_bytes_encoder_h264(parameters, move |chunk| {
                // The receiver only goes away with the encoder.
                let _ = sender.send(chunk);
            })
            .with_context(|| format!("creating a Vulkan H.264 encoder on {}", gpu.name))?;
        log::info!(
            "Vulkan H.264 encoder on {}: {width}x{height}@{fps}, {profile:?}, {rate_control:?}, \
             IDR every {keyframe_interval}",
            gpu.name
        );
        Ok(Encoder {
            encoder,
            chunks,
            config: *config,
            name: format!("Vulkan H.264 ({})", gpu.name),
            _gpu: gpu,
        })
    }

    fn drain(&self) -> Result<Vec<EncodedFrame>> {
        self.chunks
            .try_iter()
            .map(|chunk| {
                Ok(EncodedFrame {
                    pts: pts_from_u64(chunk.pts)?,
                    data: chunk.data,
                    keyframe: chunk.is_keyframe,
                })
            })
            .collect()
    }
}

impl VideoEncoder for Encoder {
    fn name(&self) -> &str {
        &self.name
    }

    fn input_layout(&self) -> YuvLayout {
        YuvLayout::Nv12
    }

    fn encode(
        &mut self,
        picture: &Yuv420,
        pts: Duration,
        keyframe: bool,
    ) -> Result<Vec<EncodedFrame>> {
        let (width, height) = (self.config.width, self.config.height);
        ensure!(
            picture.layout == YuvLayout::Nv12 && (picture.width, picture.height) == (width, height),
            "Vulkan encoder expects {width}x{height} NV12, got {}x{} {:?}",
            picture.width,
            picture.height,
            picture.layout
        );
        let frame = InputFrame {
            data: RawFrameRef {
                frame: &picture.data,
                width,
                height,
            },
            pts: Some(pts_to_u64(pts)?),
        };
        self.encoder
            .encode(&frame, keyframe)
            .context("Vulkan H.264 encode")?;
        self.drain()
    }

    fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        self.encoder.flush().context("Vulkan H.264 encoder flush")?;
        self.drain()
    }
}

/// Vulkan Video H.264 decoder; output in presentation order (gpu-video
/// sorts by picture order count).
pub struct Decoder {
    // Declared before `_gpu`, so the session goes before the device.
    decoder: BytesDecoderH264,
    frames: Receiver<OutputFrame<RawFrameData>>,
    name: String,
    _gpu: Arc<Gpu>,
}

impl Decoder {
    pub fn open() -> Result<Decoder> {
        let gpu = gpu(Usage::Decode)?;
        let (sender, frames) = mpsc::channel();
        let decoder = gpu
            .device
            .create_bytes_decoder_h264(DecoderParameters::default(), move |frame| {
                // The receiver only goes away with the decoder.
                let _ = sender.send(frame);
            })
            .with_context(|| format!("creating a Vulkan H.264 decoder on {}", gpu.name))?;
        Ok(Decoder {
            decoder,
            frames,
            name: format!("Vulkan H.264 ({})", gpu.name),
            _gpu: gpu,
        })
    }

    fn drain(&self) -> Result<Vec<DecodedPicture>> {
        self.frames
            .try_iter()
            .map(|frame| {
                Ok(DecodedPicture {
                    pts: pts_from_u64(frame.metadata.pts)?,
                    picture: nv12_picture(frame.data)?,
                })
            })
            .collect()
    }
}

impl VideoDecoder for Decoder {
    fn name(&self) -> &str {
        &self.name
    }

    fn decode(&mut self, access_unit: &[u8], pts: Duration) -> Result<Vec<DecodedPicture>> {
        self.decoder
            .decode(EncodedInputChunk {
                data: access_unit,
                pts: Some(pts_to_u64(pts)?),
            })
            .context("Vulkan H.264 decode")?;
        // The Annex-B parser only ends an access unit when the next one
        // starts; ours arrive whole, so end it now instead of a picture late.
        self.decoder
            .process_event(DecoderEvent::SignalFrameEnd)
            .context("Vulkan H.264 decode")?;
        self.drain()
    }

    fn flush(&mut self) -> Result<Vec<DecodedPicture>> {
        self.decoder.flush().context("Vulkan H.264 decoder flush")?;
        self.drain()
    }
}

/// Wraps gpu-video's NV12 output (luma rows `width` bytes, chroma rows
/// `width/2*2` bytes), dropping a trailing odd column/row: H.264 cropping
/// allows odd sizes, [`Yuv420`] does not.
fn nv12_picture(frame: RawFrameData) -> Result<Yuv420> {
    let RawFrameData {
        frame: data,
        width,
        height,
    } = frame;
    let (even_width, even_height) = (width & !1, height & !1);
    ensure!(
        even_width > 0 && even_height > 0,
        "decoded picture is {width}x{height}"
    );
    let (w, h, ew, eh) = (
        width as usize,
        height as usize,
        even_width as usize,
        even_height as usize,
    );
    let chroma_stride = w / 2 * 2;
    ensure!(
        data.len() >= w * h + chroma_stride * (h / 2),
        "decoded picture holds {} bytes, too few for {width}x{height} NV12",
        data.len()
    );
    if (ew, eh) == (w, h) {
        let mut data = data;
        data.truncate(w * h * 3 / 2);
        return Ok(Yuv420 {
            width,
            height,
            layout: YuvLayout::Nv12,
            data,
        });
    }
    let mut cropped = Vec::with_capacity(ew * eh * 3 / 2);
    for row in data[..w * eh].chunks_exact(w) {
        cropped.extend_from_slice(&row[..ew]);
    }
    for row in data[w * h..][..chroma_stride * (eh / 2)].chunks_exact(chroma_stride) {
        cropped.extend_from_slice(&row[..ew]);
    }
    Ok(Yuv420 {
        width: even_width,
        height: even_height,
        layout: YuvLayout::Nv12,
        data: cropped,
    })
}

/// For tests: whether a Vulkan device is currently held by anything here.
#[cfg(test)]
pub(crate) fn device_held() -> bool {
    SHARED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gpu
        .strong_count()
        > 0
}
