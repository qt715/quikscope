//! Software H.264 through OpenH264 (I420 in and out).
//!
//! The encoder is driven through the raw C API: the safe wrapper hard-codes
//! a single slice (so OpenH264's slice threads never run) and initialises
//! lazily on the first picture. The decoder uses the wrapper for its
//! lifetime but decodes through the raw call, because only that carries a
//! timestamp from each access unit to the picture it produces.

use std::collections::BTreeMap;
use std::ptr::null_mut;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use openh264::OpenH264API;
use openh264::decoder::Decoder as WrappedDecoder;
use openh264_sys2::{
    API, CAMERA_VIDEO_REAL_TIME, CM_BT709, CONSTANT_ID, CP_BT709,
    DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER, ENCODER_OPTION_DATAFORMAT,
    ENCODER_OPTION_TRACE_LEVEL, ISVCEncoder, ISVCEncoderVtbl, MEDIUM_COMPLEXITY, PRO_BASELINE,
    RC_BITRATE_MODE, SBufferInfo, SEncParamExt, SFrameBSInfo, SM_FIXEDSLCNUM_SLICE,
    SM_SINGLE_SLICE, SSourcePicture, TRC_BT709, VF_UNDEF, WELS_LOG_QUIET, cmResultSuccess,
    dsErrorFree, dsFramePending, videoFormatI420, videoFrameTypeIDR, videoFrameTypeInvalid,
    videoFrameTypeSkip,
};

use super::{
    DecodedPicture, EncodedFrame, VideoDecoder, VideoEncoder, VideoEncoderConfig, Yuv420, YuvLayout,
};

/// H.264 level 5.2, the highest OpenH264 encodes: macroblocks per picture,
/// and per second. Each side may be at most `sqrt(8 * MAX_FRAME_MBS)`
/// macroblocks, so very thin pictures fail too.
const MAX_FRAME_MBS: u64 = 36_864;
const MAX_MBS_PER_SECOND: u64 = 2_073_600;
/// OpenH264 runs at most this many encoder threads, one per slice.
const SLICES: u32 = 4;
/// OpenH264 refuses narrower or shorter pictures; smaller ones are encoded
/// padded to this and cropped back in the SPS.
const MIN_SIDE: u32 = 16;

/// An owned `ISVCEncoder`.
struct RawEncoder {
    api: OpenH264API,
    encoder: *mut ISVCEncoder,
}

// SAFETY: an OpenH264 encoder has no thread affinity; `&mut self` methods
// keep calls on it sequential.
unsafe impl Send for RawEncoder {}

impl RawEncoder {
    fn new() -> Result<RawEncoder> {
        let api = OpenH264API::from_source();
        let mut encoder = null_mut();
        // SAFETY: plain out-parameter constructor.
        let status = unsafe { api.WelsCreateSVCEncoder(&raw mut encoder) };
        ensure!(
            status == 0 && !encoder.is_null(),
            "OpenH264 could not create an encoder ({status})"
        );
        Ok(RawEncoder { api, encoder })
    }

    fn vtbl(&self) -> &ISVCEncoderVtbl {
        // SAFETY: `encoder` points at a live COM-style object whose first
        // field is its vtable.
        unsafe { &**self.encoder }
    }

    fn check(status: i32, what: &str) -> Result<()> {
        ensure!(
            status == cmResultSuccess,
            "OpenH264 {what} failed ({status})"
        );
        Ok(())
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        // SAFETY: created by `WelsCreateSVCEncoder`, destroyed exactly once;
        // Uninitialize is a no-op on an encoder that never initialised.
        unsafe {
            if let Some(uninitialize) = self.vtbl().Uninitialize {
                uninitialize(self.encoder);
            }
            self.api.WelsDestroySVCEncoder(self.encoder);
        }
    }
}

/// Constrained Baseline OpenH264 encoder, bitrate rate control, no skipped
/// pictures, BT.709 limited range signalled in the VUI.
pub struct Encoder {
    raw: RawEncoder,
    config: VideoEncoderConfig,
    /// The size OpenH264 encodes: the picture size, or larger if that is
    /// below [`MIN_SIDE`].
    coded: (u32, u32),
}

impl Encoder {
    pub fn open(config: &VideoEncoderConfig) -> Result<Encoder> {
        let &VideoEncoderConfig { width, height, .. } = config;
        ensure!(
            width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0,
            "H.264 needs even picture sizes, not {width}x{height}"
        );
        ensure!(
            fits_level(width, height, config.fps),
            "{width}x{height} at {} fps is beyond H.264 level 5.2, which OpenH264 encodes at most",
            config.fps
        );
        let coded = (width.max(MIN_SIDE), height.max(MIN_SIDE));
        // Several slices let OpenH264 encode them in parallel. It refuses
        // slice layouts its rate control cannot handle for some sizes, so
        // fall back to one slice rather than failing.
        let raw = Self::init(config, coded, SLICES).or_else(|err| {
            log::debug!("OpenH264 with {SLICES} slices: {err:#}; retrying with one");
            Self::init(config, coded, 1)
        })?;
        Ok(Encoder {
            raw,
            config: *config,
            coded,
        })
    }

    fn init(config: &VideoEncoderConfig, coded: (u32, u32), slices: u32) -> Result<RawEncoder> {
        let raw = RawEncoder::new()?;
        let vtbl = raw.vtbl();
        let mut quiet = WELS_LOG_QUIET;
        let mut params = SEncParamExt::default();
        // SAFETY: valid encoder; the option and parameter structs outlive
        // the calls, which only read/fill them.
        unsafe {
            RawEncoder::check(
                vtbl.SetOption.context("SetOption")?(
                    raw.encoder,
                    ENCODER_OPTION_TRACE_LEVEL,
                    (&raw mut quiet).cast(),
                ),
                "SetOption(trace level)",
            )?;
            RawEncoder::check(
                vtbl.GetDefaultParams.context("GetDefaultParams")?(raw.encoder, &raw mut params),
                "GetDefaultParams",
            )?;
        }

        let bitrate = i32::try_from(config.bitrate).unwrap_or(i32::MAX);
        let fps = config.fps.max(1) as f32;
        params.iUsageType = CAMERA_VIDEO_REAL_TIME;
        params.iPicWidth = coded.0 as i32;
        params.iPicHeight = coded.1 as i32;
        params.iRCMode = RC_BITRATE_MODE;
        params.iTargetBitrate = bitrate;
        params.fMaxFrameRate = fps;
        // Every picture is encoded: a skipped one would leave a hole in the
        // timeline the caller cannot see.
        params.bEnableFrameSkip = false;
        params.uiIntraPeriod = config.keyframe_interval;
        params.eSpsPpsIdStrategy = CONSTANT_ID;
        params.iComplexityMode = MEDIUM_COMPLEXITY;
        // 0 = one thread per core, capped by OpenH264 (and by `slices`).
        params.iMultipleThreadIdc = 0;
        // Load balancing moves slice boundaries by how fast each thread
        // happened to be, which makes the output (and rate control)
        // differ from run to run; fixed slices keep it reproducible.
        params.bUseLoadBalancing = false;
        // Background detection codes blocks it deems static as skipped, which
        // drops faint flat changes (fades, gray-on-gray UI) for a few pictures.
        params.bEnableBackgroundDetection = false;
        params.iSpatialLayerNum = 1;
        params.iTemporalLayerNum = 1;
        let layer = &mut params.sSpatialLayers[0];
        layer.iVideoWidth = coded.0 as i32;
        layer.iVideoHeight = coded.1 as i32;
        layer.fFrameRate = fps;
        layer.iSpatialBitrate = bitrate;
        layer.uiProfileIdc = PRO_BASELINE;
        if slices > 1 {
            layer.sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
            layer.sSliceArgument.uiSliceNum = slices;
        } else {
            layer.sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
            layer.sSliceArgument.uiSliceNum = 1;
        }
        // Without these, players assume BT.601 for SD sizes.
        layer.bVideoSignalTypePresent = true;
        layer.uiVideoFormat = VF_UNDEF as u8;
        layer.bFullRange = false;
        layer.bColorDescriptionPresent = true;
        layer.uiColorPrimaries = CP_BT709 as u8;
        layer.uiTransferCharacteristics = TRC_BT709 as u8;
        layer.uiColorMatrix = CM_BT709 as u8;

        let mut format = videoFormatI420;
        // SAFETY: as above.
        unsafe {
            RawEncoder::check(
                vtbl.InitializeExt.context("InitializeExt")?(raw.encoder, &raw const params),
                "InitializeExt",
            )?;
            RawEncoder::check(
                vtbl.SetOption.context("SetOption")?(
                    raw.encoder,
                    ENCODER_OPTION_DATAFORMAT,
                    (&raw mut format).cast(),
                ),
                "SetOption(data format)",
            )?;
        }
        Ok(raw)
    }
}

/// Whether `width`×`height` pictures at `fps` stay within H.264 level 5.2
/// (OpenH264's own `InitializeExt` still has the final word).
pub fn fits_level(width: u32, height: u32, fps: u32) -> bool {
    let (columns, rows) = (
        u64::from(width.max(MIN_SIDE).div_ceil(16)),
        u64::from(height.max(MIN_SIDE).div_ceil(16)),
    );
    let frame = columns * rows;
    frame <= MAX_FRAME_MBS
        && columns * columns <= 8 * MAX_FRAME_MBS
        && rows * rows <= 8 * MAX_FRAME_MBS
        && frame * u64::from(fps.max(1)) <= MAX_MBS_PER_SECOND
}

impl VideoEncoder for Encoder {
    fn name(&self) -> &str {
        "OpenH264"
    }

    fn input_layout(&self) -> YuvLayout {
        YuvLayout::I420
    }

    fn encode(
        &mut self,
        picture: &Yuv420,
        pts: Duration,
        keyframe: bool,
    ) -> Result<Vec<EncodedFrame>> {
        let (width, height) = (self.config.width, self.config.height);
        ensure!(
            picture.layout == YuvLayout::I420
                && (picture.width, picture.height) == (width, height)
                && picture.data.len() == (width * height * 3 / 2) as usize,
            "OpenH264 expects {width}x{height} I420, got {}x{} {:?}",
            picture.width,
            picture.height,
            picture.layout
        );
        let padded;
        let (picture, width, height) = if self.coded == (width, height) {
            (picture, width, height)
        } else {
            padded = pad_i420(picture, self.coded);
            (&padded, self.coded.0, self.coded.1)
        };
        let luma = (width * height) as usize;
        let data = picture.data.as_ptr().cast_mut();
        let source = SSourcePicture {
            iColorFormat: videoFormatI420,
            iStride: [width as i32, width as i32 / 2, width as i32 / 2, 0],
            // SAFETY: offsets inside `picture.data` (length checked above);
            // OpenH264 only reads the planes.
            pData: unsafe { [data, data.add(luma), data.add(luma + luma / 4), null_mut()] },
            iPicWidth: width as i32,
            iPicHeight: height as i32,
            uiTimeStamp: pts.as_millis().try_into().unwrap_or(i64::MAX),
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };
        let mut info = SFrameBSInfo::default();
        let vtbl = self.raw.vtbl();
        // SAFETY: valid, initialised encoder; `source` borrows `picture` for
        // the call; `info` points into encoder-owned buffers that stay valid
        // until the next EncodeFrame, and is copied out before that.
        unsafe {
            if keyframe {
                RawEncoder::check(
                    vtbl.ForceIntraFrame.context("ForceIntraFrame")?(self.raw.encoder, true),
                    "ForceIntraFrame",
                )?;
            }
            RawEncoder::check(
                vtbl.EncodeFrame.context("EncodeFrame")?(
                    self.raw.encoder,
                    &raw const source,
                    &raw mut info,
                ),
                "EncodeFrame",
            )?;
        }
        ensure!(
            info.eFrameType != videoFrameTypeInvalid,
            "OpenH264 produced no picture"
        );
        // Frame skipping is off, so this only happens if OpenH264 ignores
        // that; nothing was coded for this picture.
        if info.eFrameType == videoFrameTypeSkip {
            log::warn!("OpenH264 skipped the picture at {pts:?}");
            return Ok(Vec::new());
        }
        let mut data = Vec::with_capacity(usize::try_from(info.iFrameSizeInBytes).unwrap_or(0));
        for layer in &info.sLayerInfo[..usize::try_from(info.iLayerNum).unwrap_or(0)] {
            // SAFETY: OpenH264 fills `iNalCount` lengths and a buffer holding
            // those NAL units back to back, Annex-B start codes included.
            unsafe {
                let lengths = std::slice::from_raw_parts(
                    layer.pNalLengthInByte,
                    usize::try_from(layer.iNalCount).unwrap_or(0),
                );
                let size = lengths.iter().map(|&n| n as usize).sum();
                data.extend_from_slice(std::slice::from_raw_parts(layer.pBsBuf, size));
            }
        }
        if self.coded != (self.config.width, self.config.height) {
            data = crop_sps(&data, self.config.width, self.config.height)?;
        }
        Ok(vec![EncodedFrame {
            data,
            pts,
            keyframe: info.eFrameType == videoFrameTypeIDR,
        }])
    }

    fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        // Every EncodeFrame returns its picture's access unit immediately.
        Ok(Vec::new())
    }
}

/// `picture` enlarged to `coded` by repeating its last column and row.
fn pad_i420(picture: &Yuv420, (coded_width, coded_height): (u32, u32)) -> Yuv420 {
    let (w, h) = (picture.width as usize, picture.height as usize);
    let (cw, ch) = (coded_width as usize, coded_height as usize);
    let (y, chroma) = picture.data.split_at(w * h);
    let (u, v) = chroma.split_at(w * h / 4);
    let mut data = Vec::with_capacity(cw * ch * 3 / 2);
    for (plane, pw, ph, cpw, cph) in [
        (y, w, h, cw, ch),
        (u, w / 2, h / 2, cw / 2, ch / 2),
        (v, w / 2, h / 2, cw / 2, ch / 2),
    ] {
        for row in 0..cph {
            let src = &plane[row.min(ph - 1) * pw..][..pw];
            data.extend_from_slice(src);
            data.extend(std::iter::repeat_n(src[pw - 1], cpw - pw));
        }
    }
    Yuv420 {
        width: coded_width,
        height: coded_height,
        layout: YuvLayout::I420,
        data,
    }
}

/// Rewrites every SPS in an Annex-B `access_unit` to crop the decoded
/// picture to its top-left `width`×`height`.
fn crop_sps(access_unit: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut starts = Vec::new();
    for i in 0..access_unit.len().saturating_sub(2) {
        if access_unit[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
        }
    }
    let mut out = Vec::with_capacity(access_unit.len() + 8);
    out.extend_from_slice(&access_unit[..starts.first().map_or(access_unit.len(), |&s| s)]);
    for (n, &start) in starts.iter().enumerate() {
        // The next start code's leading zero bytes are not part of this NAL.
        let mut end = starts
            .get(n + 1)
            .map_or(access_unit.len(), |&next| next - 3);
        let next_code = end;
        while starts.get(n + 1).is_some() && end > start && access_unit[end - 1] == 0 {
            end -= 1;
        }
        let nal = &access_unit[start..end];
        if nal.first().is_some_and(|header| header & 0x1f == 7) {
            out.extend(sps_with_crop(nal, width, height)?);
        } else {
            out.extend_from_slice(nal);
        }
        out.extend_from_slice(&access_unit[end..next_code]);
        if starts.get(n + 1).is_some() {
            out.extend_from_slice(&[0, 0, 1]);
        }
    }
    Ok(out)
}

/// The fields of a sequence parameter set this crate needs.
pub(super) struct Sps {
    pub profile_idc: u32,
    pub width_in_mbs: u32,
    /// Frame height in macroblocks (map units doubled for field coding).
    pub height_in_mbs: u32,
    pub frame_mbs_only: bool,
    /// Bit offsets in the RBSP of frame_cropping_flag and of what follows
    /// the cropping fields.
    crop_at: usize,
    rest_at: usize,
}

/// Parses an SPS NAL unit (header byte, then escaped payload).
pub(super) fn parse_sps(nal: &[u8]) -> Result<Sps> {
    let rbsp = unescape(nal.get(1..).context("empty SPS")?);
    parse_sps_rbsp(&rbsp)
}

fn parse_sps_rbsp(rbsp: &[u8]) -> Result<Sps> {
    let mut reader = BitReader { data: rbsp, pos: 0 };
    let profile_idc = reader.bits(8)?;
    reader.bits(16)?; // constraint flags, level
    reader.ue()?; // seq_parameter_set_id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile_idc) {
        let chroma_format_idc = reader.ue()?;
        if chroma_format_idc == 3 {
            reader.bits(1)?; // separate_colour_plane_flag
        }
        reader.ue()?; // bit_depth_luma_minus8
        reader.ue()?; // bit_depth_chroma_minus8
        reader.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if reader.bits(1)? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if reader.bits(1)? == 1 {
                    // scaling_list(): delta-coded until a zero next scale.
                    let size = if list < 6 { 16 } else { 64 };
                    let mut next = 8;
                    for _ in 0..size {
                        if next != 0 {
                            next = (next + reader.se()? + 256) % 256;
                        }
                    }
                }
            }
        }
    }
    reader.ue()?; // log2_max_frame_num_minus4
    match reader.ue()? {
        0 => {
            reader.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            reader.bits(1)?; // delta_pic_order_always_zero_flag
            reader.se()?; // offset_for_non_ref_pic
            reader.se()?; // offset_for_top_to_bottom_field
            for _ in 0..reader.ue()? {
                reader.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    reader.ue()?; // max_num_ref_frames
    reader.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_in_mbs = reader.ue()? + 1;
    let map_units = reader.ue()? + 1;
    let frame_mbs_only = reader.bits(1)? == 1;
    if !frame_mbs_only {
        reader.bits(1)?; // mb_adaptive_frame_field_flag
    }
    reader.bits(1)?; // direct_8x8_inference_flag
    let crop_at = reader.pos;
    if reader.bits(1)? == 1 {
        for _ in 0..4 {
            reader.ue()?;
        }
    }
    Ok(Sps {
        profile_idc,
        width_in_mbs,
        height_in_mbs: map_units * if frame_mbs_only { 1 } else { 2 },
        frame_mbs_only,
        crop_at,
        rest_at: reader.pos,
    })
}

/// `sps` (header byte, then escaped payload) with its frame cropping set to
/// `width`×`height` (4:2:0 progressive: crop units of 2 pixels).
fn sps_with_crop(sps: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let rbsp = unescape(&sps[1..]);
    let Sps {
        width_in_mbs,
        height_in_mbs,
        frame_mbs_only,
        crop_at,
        rest_at,
        ..
    } = parse_sps_rbsp(&rbsp)?;
    ensure!(frame_mbs_only, "interlaced SPS");
    let stop_bit = (0..rbsp.len() * 8)
        .rev()
        .find(|&bit| rbsp[bit / 8] & (0x80 >> (bit % 8)) != 0)
        .context("SPS without stop bit")?;
    ensure!(stop_bit >= rest_at, "truncated SPS");
    ensure!(
        width <= width_in_mbs * 16 && height <= height_in_mbs * 16,
        "crop {width}x{height} outside the coded picture"
    );

    let mut writer = BitWriter::default();
    writer.copy(&rbsp, 0, crop_at);
    writer.bits(1, 1); // frame_cropping_flag
    writer.ue(0); // left
    writer.ue((width_in_mbs * 16 - width) / 2);
    writer.ue(0); // top
    writer.ue((height_in_mbs * 16 - height) / 2);
    writer.copy(&rbsp, rest_at, stop_bit);
    writer.bits(1, 1); // rbsp_stop_one_bit, then zeros to the byte end
    let mut out = vec![sps[0]];
    escape(&writer.finish(), &mut out);
    Ok(out)
}

/// Removes emulation prevention bytes (`00 00 03` → `00 00`).
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Appends `rbsp` with emulation prevention bytes inserted.
fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bits(&mut self, count: u32) -> Result<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self.data.get(self.pos / 8).context("truncated SPS")?;
            value = value << 1 | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Ok(value)
    }

    /// Exp-Golomb `ue(v)`.
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            ensure!(zeros < 32, "invalid Exp-Golomb code in SPS");
        }
        Ok((1 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Exp-Golomb `se(v)`.
    fn se(&mut self) -> Result<i32> {
        let code = self.ue()?;
        let magnitude = code.div_ceil(2) as i32;
        Ok(if code % 2 == 1 { magnitude } else { -magnitude })
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn bits(&mut self, count: u32, value: u32) {
        for shift in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = (value >> shift & 1) as u8;
            *self.bytes.last_mut().expect("pushed above") |= bit << (7 - self.bits % 8);
            self.bits += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let coded = value + 1;
        let length = 32 - coded.leading_zeros();
        self.bits(length - 1, 0);
        self.bits(length, coded);
    }

    /// Appends bits `from..to` of `data`.
    fn copy(&mut self, data: &[u8], from: usize, to: usize) {
        for bit in from..to {
            self.bits(1, u32::from(data[bit / 8] >> (7 - bit % 8) & 1));
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// OpenH264 decoder (progressive 8-bit 4:2:0, any profile, B-frames
/// included).
///
/// OpenH264 reorders B-frames with heuristics, so pictures are additionally
/// kept back until every access unit with an earlier pts has produced its
/// picture: output is always in pts order.
pub struct Decoder {
    decoder: WrappedDecoder,
    /// Timestamps (ns) of access units whose picture has not appeared yet,
    /// with multiplicity.
    pending: BTreeMap<u64, usize>,
    /// Decoded pictures not yet returned, by (pts ns, arrival).
    ready: BTreeMap<(u64, u64), Yuv420>,
    arrivals: u64,
}

/// More pictures held back than a DPB can hold means the earliest pending
/// access unit will never produce one (a parameter-set-only or broken unit).
const MAX_HELD: usize = 16;

impl Decoder {
    pub fn open() -> Result<Decoder> {
        let decoder = WrappedDecoder::new().map_err(|e| anyhow!("OpenH264 decoder: {e}"))?;
        Ok(Decoder {
            decoder,
            pending: BTreeMap::new(),
            ready: BTreeMap::new(),
            arrivals: 0,
        })
    }

    /// Runs one raw decode call; `None` input drains one buffered picture.
    fn decode_raw(&mut self, input: Option<(&[u8], u64)>) -> Result<Option<(u64, Yuv420)>> {
        let mut planes = [null_mut::<u8>(); 3];
        let mut info = SBufferInfo::default();
        // SAFETY: the wrapper owns a live, initialised decoder; `input` and
        // the out-parameters outlive the call. Returned planes point into
        // decoder-owned buffers that stay valid until the next call, and are
        // copied out before that.
        let state = unsafe {
            let raw = self.decoder.raw_api();
            match input {
                Some((access_unit, pts)) => {
                    info.uiInBsTimeStamp = pts;
                    raw.decode_frame_no_delay(
                        access_unit.as_ptr(),
                        i32::try_from(access_unit.len()).context("access unit too large")?,
                        planes.as_mut_ptr(),
                        &raw mut info,
                    )
                }
                None => raw.flush_frame(planes.as_mut_ptr(), &raw mut info),
            }
        };
        let picture = if info.iBufferStatus == 1 && planes.iter().all(|p| !p.is_null()) {
            // SAFETY: OpenH264 reported a picture; the union member for
            // system-memory output is the one it fills.
            let buffer = unsafe { info.UsrData.sSystemBuffer };
            Some((info.uiOutYuvTimeStamp, copy_i420(&planes, &buffer)?))
        } else {
            None
        };
        if state & !dsFramePending != dsErrorFree && picture.is_none() {
            bail!("OpenH264 could not decode the access unit (state {state:#x})");
        }
        Ok(picture)
    }

    fn take_pending(&mut self, pts: u64) {
        if let Some(count) = self.pending.get_mut(&pts) {
            *count -= 1;
            if *count == 0 {
                self.pending.remove(&pts);
            }
        }
    }

    fn hold(&mut self, (pts, picture): (u64, Yuv420)) {
        self.take_pending(pts);
        self.ready.insert((pts, self.arrivals), picture);
        self.arrivals += 1;
    }

    /// Returns held pictures that no pending access unit can precede.
    fn release(&mut self, all: bool) -> Vec<DecodedPicture> {
        let mut out = Vec::new();
        while let Some(&(pts, _)) = self.ready.keys().next() {
            let blocked = self
                .pending
                .first_key_value()
                .is_some_and(|(&p, _)| p < pts);
            if blocked && !all && self.ready.len() <= MAX_HELD {
                break;
            }
            // Pending units before this picture are now lost causes.
            self.pending.retain(|&p, _| p >= pts);
            let (_, picture) = self.ready.pop_first().expect("checked above");
            out.push(DecodedPicture {
                picture,
                pts: Duration::from_nanos(pts),
            });
        }
        out
    }

    fn frames_buffered(&mut self) -> Result<usize> {
        let mut count = 0i32;
        // SAFETY: valid decoder; plain integer out-parameter.
        let status = unsafe {
            self.decoder.raw_api().get_option(
                DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER,
                (&raw mut count).cast(),
            )
        };
        ensure!(status == 0, "OpenH264 GetOption failed ({status})");
        Ok(usize::try_from(count).unwrap_or(0))
    }
}

impl VideoDecoder for Decoder {
    fn name(&self) -> &str {
        "OpenH264"
    }

    fn decode(&mut self, access_unit: &[u8], pts: Duration) -> Result<Vec<DecodedPicture>> {
        let ts = u64::try_from(pts.as_nanos()).context("pts out of range")?;
        *self.pending.entry(ts).or_default() += 1;
        match self.decode_raw(Some((access_unit, ts))) {
            Ok(Some(picture)) => self.hold(picture),
            Ok(None) => {}
            Err(err) => {
                self.take_pending(ts);
                return Err(err);
            }
        }
        Ok(self.release(false))
    }

    fn flush(&mut self) -> Result<Vec<DecodedPicture>> {
        for _ in 0..self.frames_buffered()? {
            match self.decode_raw(None)? {
                Some(picture) => self.hold(picture),
                None => break,
            }
        }
        let out = self.release(true);
        self.pending.clear();
        Ok(out)
    }
}

/// Copies OpenH264's padded planes into a tight I420 picture, dropping a
/// trailing odd column/row (H.264 cropping allows odd sizes; [`Yuv420`]
/// does not).
fn copy_i420(planes: &[*mut u8; 3], buffer: &openh264_sys2::SSysMEMBuffer) -> Result<Yuv420> {
    let width = u32::try_from(buffer.iWidth).context("picture width")? & !1;
    let height = u32::try_from(buffer.iHeight).context("picture height")? & !1;
    ensure!(
        width > 0 && height > 0,
        "decoded picture is {}x{}",
        buffer.iWidth,
        buffer.iHeight
    );
    let (w, h) = (width as usize, height as usize);
    let luma_stride = usize::try_from(buffer.iStride[0]).context("stride")?;
    let chroma_stride = usize::try_from(buffer.iStride[1]).context("stride")?;
    let mut data = Vec::with_capacity(w * h * 3 / 2);
    // SAFETY: OpenH264's planes hold `iHeight` rows of `iStride[0]` luma
    // bytes and `iHeight/2` rows of `iStride[1]` bytes per chroma plane; we
    // read within the (even-cropped) picture only.
    unsafe {
        for row in 0..h {
            data.extend_from_slice(std::slice::from_raw_parts(
                planes[0].add(row * luma_stride),
                w,
            ));
        }
        for plane in &planes[1..] {
            for row in 0..h / 2 {
                data.extend_from_slice(std::slice::from_raw_parts(
                    plane.add(row * chroma_stride),
                    w / 2,
                ));
            }
        }
    }
    Ok(Yuv420 {
        width,
        height,
        layout: YuvLayout::I420,
        data,
    })
}
