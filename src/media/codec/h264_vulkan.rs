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
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use gpu_video::capabilities::{
    DecodeH264Capabilities, DeviceType, EncodeH264Capabilities, EncodeProfileCapabilities,
    VideoAdapterInfo,
};
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
    DecodedPicture, EncodedFrame, VideoDecoder, VideoEncoder, VideoEncoderConfig, Yuv420,
    YuvLayout, h264_soft,
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
    /// Another failure to open a device (loader, driver or device errors
    /// can be passing), and until when it stands.
    failed: [Option<(Instant, String)>; 2],
    /// Picture sizes the adapter that encoders open on encodes, once listed.
    encode_sizes: Option<Option<SizeRange>>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    gpu: Weak::new(),
    unsupported: [None, None],
    failed: [None, None],
    encode_sizes: None,
});

/// How long a failure other than "no adapter can do it" keeps Vulkan Video
/// from being tried again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

impl Shared {
    /// Why `usage` is not worth trying now, if so.
    fn refusal(&self, usage: Usage) -> Option<&str> {
        let index = usage as usize;
        self.unsupported[index].as_deref().or_else(|| {
            self.failed[index]
                .as_ref()
                .filter(|(until, _)| Instant::now() < *until)
                .map(|(_, reason)| reason.as_str())
        })
    }

    /// Records the outcome of an attempt to use Vulkan Video for `usage`.
    fn record<T>(&mut self, usage: Usage, outcome: &Result<Result<T, String>>) {
        let index = usage as usize;
        match outcome {
            Ok(Ok(_)) => self.failed[index] = None,
            Ok(Err(reason)) => self.unsupported[index] = Some(reason.clone()),
            Err(err) => {
                self.failed[index] = Some((Instant::now() + RETRY_AFTER, format!("{err:#}")));
            }
        }
    }
}

/// The shared device if it supports `usage`, else a newly opened one.
fn gpu(usage: Usage) -> Result<Arc<Gpu>> {
    let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(gpu) = shared.gpu.upgrade()
        && gpu.supports(usage)
    {
        return Ok(gpu);
    }
    if let Some(reason) = shared.refusal(usage) {
        bail!("{reason}");
    }
    let opened = open_gpu(usage);
    shared.record(usage, &opened);
    match opened? {
        Ok(gpu) => {
            let gpu = Arc::new(gpu);
            shared.gpu = Arc::downgrade(&gpu);
            Ok(gpu)
        }
        Err(reason) => bail!("{reason}"),
    }
}

/// The best adapter for `usage`, with what it can do (encode, decode):
/// discrete before integrated GPUs, and among those one that can also do
/// the other direction, so an editor that decodes and encodes shares one
/// device.
fn best_adapter(
    instance: &VideoInstance,
    usage: Usage,
) -> Result<Option<([bool; 2], VideoAdapter<'_>)>> {
    let rank = |info: &VideoAdapterInfo| match info.device_type {
        DeviceType::DiscreteGpu => 0u8,
        DeviceType::IntegratedGpu => 1,
        DeviceType::VirtualGpu => 2,
        DeviceType::Other | DeviceType::Cpu => 3,
    };
    let mut best: Option<((u8, bool), [bool; 2], VideoAdapter<'_>)> = None;
    let adapters = instance
        .iter_adapters()
        .context("listing Vulkan Video adapters")?;
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
    Ok(best.map(|(_, can, adapter)| (can, adapter)))
}

fn unsupported_reason(usage: Usage) -> String {
    let what = match usage {
        Usage::Encode => "encode",
        Usage::Decode => "decode",
    };
    format!("no Vulkan Video adapter can {what} H.264")
}

/// Opens the [best adapter](best_adapter) for `usage`. `Ok(Err(reason))`
/// means no adapter can ever do it in this process.
fn open_gpu(usage: Usage) -> Result<Result<Gpu, String>> {
    let instance = VideoInstance::new(&VideoInstanceDescriptor::default())
        .context("Vulkan Video unavailable")?;
    let Some(([can_encode, can_decode], adapter)) = best_adapter(&instance, usage)? else {
        return Ok(Err(unsupported_reason(usage)));
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

/// Smallest and largest picture an encoder accepts; the largest bounds the
/// macroblock-aligned coded size.
#[derive(Clone, Copy, Debug)]
struct SizeRange {
    min: (u32, u32),
    max: (u32, u32),
}

impl SizeRange {
    fn of(limits: &EncodeProfileCapabilities) -> Self {
        SizeRange {
            min: (limits.min_width, limits.min_height),
            max: (limits.max_width, limits.max_height),
        }
    }

    fn contains(&self, width: u32, height: u32) -> bool {
        width >= self.min.0
            && height >= self.min.1
            && width.next_multiple_of(16) <= self.max.0
            && height.next_multiple_of(16) <= self.max.1
    }
}

/// Whether [`Encoder::open`] can take `width`×`height` pictures, as far
/// as the adapter's limits tell. Lists the adapters once per process (no
/// device is opened) unless an encoder's device is open already.
pub fn encode_size_fits(width: u32, height: u32) -> bool {
    let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(gpu) = shared.gpu.upgrade()
        && gpu.can_encode
    {
        return encode_sizes(&gpu.device.encode_capabilities().h264)
            .is_some_and(|range| range.contains(width, height));
    }
    if shared.encode_sizes.is_none() {
        if shared.refusal(Usage::Encode).is_some() {
            return false;
        }
        let listed = list_encode_sizes();
        shared.record(Usage::Encode, &listed);
        match listed {
            Ok(Ok(range)) => shared.encode_sizes = Some(Some(range)),
            Ok(Err(_)) => shared.encode_sizes = Some(None),
            Err(err) => {
                log::debug!("Vulkan Video encode sizes unknown: {err:#}");
                return false;
            }
        }
    }
    shared
        .encode_sizes
        .flatten()
        .is_some_and(|range| range.contains(width, height))
}

fn list_encode_sizes() -> Result<Result<SizeRange, String>> {
    let instance = VideoInstance::new(&VideoInstanceDescriptor::default())
        .context("Vulkan Video unavailable")?;
    Ok(best_adapter(&instance, Usage::Encode)?
        .and_then(|(_, adapter)| encode_sizes(&adapter.info().encode_capabilities.h264))
        .ok_or_else(|| unsupported_reason(Usage::Encode)))
}

/// The profile [`Encoder::open`] uses and its limits.
fn encode_profile(
    caps: &EncodeH264Capabilities,
) -> Option<(ProfileH264, EncodeProfileCapabilities)> {
    [
        (ProfileH264::High, caps.high_profile),
        (ProfileH264::Main, caps.main_profile),
        (ProfileH264::Baseline, caps.baseline_profile),
    ]
    .into_iter()
    .find_map(|(profile, limits)| Some((profile, limits?)))
}

fn encode_sizes(caps: &Option<EncodeH264Capabilities>) -> Option<SizeRange> {
    encode_profile(caps.as_ref()?).map(|(_, limits)| SizeRange::of(&limits))
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
        let (profile, limits) = encode_profile(&caps).context("no H.264 encode profile")?;
        let VideoEncoderConfig {
            width,
            height,
            fps,
            bitrate,
            keyframe_interval,
        } = *config;
        let sizes = SizeRange::of(&limits);
        ensure!(
            sizes.contains(width, height),
            "{} encodes {}x{} to {}x{} pictures, not {width}x{height}",
            gpu.name,
            sizes.min.0,
            sizes.min.1,
            sizes.max.0,
            sizes.max.1
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
    caps: DecodeH264Capabilities,
    name: String,
    _gpu: Arc<Gpu>,
}

impl Decoder {
    pub fn open() -> Result<Decoder> {
        let gpu = gpu(Usage::Decode)?;
        let caps = gpu
            .device
            .decode_capabilities()
            .h264
            .context("device lost its H.264 decode support")?;
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
            caps,
            name: format!("Vulkan H.264 ({})", gpu.name),
            _gpu: gpu,
        })
    }

    /// Refuses a sequence whose profile or coded size the device does not
    /// support. Drivers do not all reject those themselves: RADV decodes
    /// pictures below its minimum size into garbage.
    fn check_sps(&self, nal: &[u8]) -> Result<()> {
        let sps = h264_soft::parse_sps(nal)?;
        let limits = match sps.profile_idc {
            66 => self.caps.baseline_profile,
            77 => self.caps.main_profile,
            100 => self.caps.high_profile,
            _ => None,
        }
        .with_context(|| format!("H.264 profile {} is not supported", sps.profile_idc))?;
        let (width, height) = (sps.width_in_mbs * 16, sps.height_in_mbs * 16);
        ensure!(
            (limits.min_width..=limits.max_width).contains(&width)
                && (limits.min_height..=limits.max_height).contains(&height),
            "{width}x{height} pictures are outside the decodable {}x{} to {}x{}",
            limits.min_width,
            limits.min_height,
            limits.max_width,
            limits.max_height
        );
        Ok(())
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
        for (start, end) in nal_units(access_unit) {
            if access_unit[start] & 0x1f == 7 {
                self.check_sps(&access_unit[start..end])?;
            }
        }
        self.decoder
            .decode(EncodedInputChunk {
                data: &aligned_access_unit(access_unit),
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

/// A multiple of every driver's `minBitstreamBufferSizeAlignment` (a power
/// of two; gpu-video does not expose it).
const BITSTREAM_ALIGNMENT: usize = 4096;
/// 3⁻¹ modulo [`BITSTREAM_ALIGNMENT`].
const INVERSE_OF_3: usize = 2731;
const _: () = assert!(3 * INVERSE_OF_3 % BITSTREAM_ALIGNMENT == 1);

/// `access_unit` with its last slice padded so that gpu-video uploads a
/// bitstream of exactly a multiple of [`BITSTREAM_ALIGNMENT`] bytes.
///
/// gpu-video 0.5.0 concatenates the slices (4-byte start code each, trailing
/// zero bytes stripped) into a pooled buffer and hands the GPU that length
/// rounded up to the alignment, over stale bytes from earlier pictures
/// (`upload_data` in wrappers/mem.rs). NVIDIA decodes those into pictures
/// with several slices: stale or gray macroblocks, differently each run.
///
/// The padding has to sit inside the last slice: gpu-video uploads nothing
/// but slice NAL units (a filler-data NAL unit would be dropped) and strips
/// trailing zero bytes. So it is `00 00 03` triplets, whose `03` is an
/// emulation prevention byte: the slice's RBSP gains only zero bytes after
/// its rbsp_stop_one_bit. For CABAC streams those are cabac_zero_words.
/// CAVLC streams, which need the padding most (their slices end where the
/// RBSP's last 1 bit is, so stale bytes would read as more macroblocks),
/// have no such syntax element, but more_rbsp_data() finds that same last
/// 1 bit before the zeros, so a conforming decoder reads the slice
/// unchanged. 3 is invertible modulo a power of two, so whole triplets
/// reach any length.
fn aligned_access_unit(access_unit: &[u8]) -> Vec<u8> {
    let mut length = 0;
    let mut last_slice_end = None;
    for (start, end) in nal_units(access_unit) {
        if matches!(access_unit[start] & 0x1f, 1 | 5) {
            length += 4 + end - start;
            last_slice_end = Some(end);
        }
    }
    let Some(end) = last_slice_end else {
        return access_unit.to_vec();
    };
    let missing = length.next_multiple_of(BITSTREAM_ALIGNMENT) - length;
    let words = missing * INVERSE_OF_3 % BITSTREAM_ALIGNMENT;
    let mut padded = Vec::with_capacity(access_unit.len() + words * 3);
    padded.extend_from_slice(&access_unit[..end]);
    for _ in 0..words {
        padded.extend_from_slice(&[0, 0, 3]);
    }
    padded.extend_from_slice(&access_unit[end..]);
    padded
}

/// The NAL units of an Annex-B access unit as byte ranges, start codes and
/// trailing zero bytes excluded (empty ones skipped), as gpu-video splits it.
fn nal_units(access_unit: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= access_unit.len() {
        if access_unit[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    (0..starts.len()).filter_map(move |n| {
        let start = starts[n];
        let next = starts
            .get(n + 1)
            .map_or(access_unit.len(), |&next| next - 3);
        let last = access_unit[start..next]
            .iter()
            .rposition(|&byte| byte != 0)?;
        Some((start, start + last + 1))
    })
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
