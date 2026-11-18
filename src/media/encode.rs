//! Encoder selection, hardware conversion and muxing shared by recording/export.

use super::EncoderPreference;
use anyhow::{Context as _, Result, bail, ensure};
use ffmpeg::{
    ChannelLayout, Dictionary, Packet, Rational, codec, encoder, ffi, filter, format, frame,
};
use ffmpeg_next as ffmpeg;
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) fn check(code: c_int) -> Result<()> {
    if code < 0 {
        Err(ffmpeg::Error::from(code).into())
    } else {
        Ok(())
    }
}

pub(crate) fn again(error: ffmpeg::Error) -> bool {
    matches!(error, ffmpeg::Error::Other { errno } if errno == ffmpeg::error::EAGAIN)
}

fn reference(input: &frame::Video) -> Result<frame::Video> {
    // Video::clone copies pixels into a software allocation; hardware frames
    // instead need a reference to their existing buffers and hw_frames_ctx.
    let mut output = frame::Video::empty();
    unsafe {
        check(ffi::av_frame_ref(output.as_mut_ptr(), input.as_ptr()))?;
    }
    Ok(output)
}

pub(crate) fn resample(
    context: &mut ffmpeg::software::resampling::Context,
    input: &frame::Audio,
) -> Result<frame::Audio> {
    let capacity =
        unsafe { ffi::swr_get_out_samples(context.as_mut_ptr(), input.samples() as i32) };
    check(capacity)?;
    let definition = context.output();
    let mut output = frame::Audio::new(
        definition.format,
        capacity.max(1) as usize,
        definition.channel_layout,
    );
    context.run(input, &mut output)?;
    Ok(output)
}

#[cfg(all(target_arch = "x86_64", not(windows)))]
type LogArgs = *mut ffi::__va_list_tag;
#[cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]
type LogArgs = *mut c_char;
#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
type LogArgs = ffi::va_list;

/// FFmpeg warns on every DMA-BUF import that it cannot wait for implicit
/// fences; screencopy only sends `ready` after the compositor's copy has
/// completed, so there is nothing to wait for and the warning is noise.
const UNSYNCED_DMABUF: &[u8] = b"No support for synchronization when importing DMA-BUFs";

unsafe extern "C" fn logger(
    object: *mut c_void,
    level: c_int,
    message: *const c_char,
    args: LogArgs,
) {
    let mut severity = if level <= ffi::AV_LOG_ERROR {
        log::Level::Error
    } else if level <= ffi::AV_LOG_WARNING {
        log::Level::Warn
    } else {
        log::Level::Debug
    };
    if !message.is_null()
        && unsafe { CStr::from_ptr(message) }
            .to_bytes()
            .starts_with(UNSYNCED_DMABUF)
    {
        severity = log::Level::Debug;
    }
    if !log::log_enabled!(target: "quikscope::media", severity) {
        return;
    }
    let mut line = [0 as c_char; 2048];
    let mut prefix = 1;
    unsafe {
        ffi::av_log_format_line2(
            object,
            level,
            message,
            args,
            line.as_mut_ptr(),
            line.len() as i32,
            &mut prefix,
        );
        log::log!(target: "quikscope::media", severity, "{}", CStr::from_ptr(line.as_ptr()).to_string_lossy().trim_end());
    }
}

pub(crate) fn route_logs() {
    unsafe {
        ffi::av_log_set_level(
            if log::log_enabled!(target: "quikscope::media", log::Level::Debug) {
                ffi::AV_LOG_DEBUG
            } else {
                ffi::AV_LOG_WARNING
            },
        );
        ffi::av_log_set_callback(Some(logger));
    }
}

pub(crate) fn partial(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    name.into()
}

/// A transaction owns only its partial file; failed exports never remove an
/// existing final file, and dropping an unfinished writer removes the partial.
pub(crate) struct OutputFile {
    pub path: PathBuf,
    final_path: PathBuf,
    committed: bool,
}
impl OutputFile {
    pub fn new(path: &Path) -> Self {
        Self {
            path: partial(path),
            final_path: path.to_owned(),
            committed: false,
        }
    }
    pub fn commit(mut self) -> Result<()> {
        std::fs::rename(&self.path, &self.final_path)?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for OutputFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) struct Graph {
    graph: filter::Graph,
}
impl Graph {
    pub fn new(input: &frame::Video, spec: &str) -> Result<Self> {
        Self::with_device(input, spec, None)
    }

    fn with_device(input: &frame::Video, spec: &str, device_name: Option<&CStr>) -> Result<Self> {
        let mut graph = filter::Graph::new();
        unsafe {
            (*graph.as_mut_ptr()).nb_threads = 8;
        }
        let pixel: ffi::AVPixelFormat = input.format().into();
        let args = format!(
            "video_size={}x{}:pix_fmt={}:time_base=1/90000:pixel_aspect=1/1",
            input.width(),
            input.height(),
            pixel as i32
        );
        unsafe {
            let hw = (*input.as_ptr()).hw_frames_ctx;
            if hw.is_null() {
                graph.add(
                    &filter::find("buffer").context("FFmpeg buffer filter unavailable")?,
                    "in",
                    &args,
                )?;
            } else {
                let source = ffi::avfilter_graph_alloc_filter(
                    graph.as_mut_ptr(),
                    filter::find("buffer")
                        .context("FFmpeg buffer filter unavailable")?
                        .as_ptr(),
                    c"in".as_ptr(),
                );
                ensure!(!source.is_null(), "Allocating hardware buffer source");
                let parameters = ffi::av_buffersrc_parameters_alloc();
                ensure!(!parameters.is_null(), "Allocating buffer source parameters");
                (*parameters).format = pixel as i32;
                (*parameters).width = input.width() as i32;
                (*parameters).height = input.height() as i32;
                (*parameters).time_base = ffi::AVRational { num: 1, den: 90000 };
                (*parameters).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
                (*parameters).hw_frames_ctx = hw;
                let result = ffi::av_buffersrc_parameters_set(source, parameters);
                ffi::av_free(parameters.cast());
                check(result)?;
                check(ffi::avfilter_init_str(source, std::ptr::null()))?;
            }
        }
        graph.add(
            &filter::find("buffersink").context("FFmpeg buffersink unavailable")?,
            "out",
            "",
        )?;
        if let Some((before, after)) = spec
            .strip_suffix(",hwupload")
            .map(|before| (before, ""))
            .or_else(|| spec.split_once(",hwupload,"))
        {
            // Parsing initializes filters immediately, so upload needs its
            // device before avfilter_init_str rather than after graph parsing.
            let mut device = std::ptr::null_mut();
            let result = unsafe {
                ffi::av_hwdevice_ctx_create(
                    &mut device,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    device_name.map_or(std::ptr::null(), CStr::as_ptr),
                    std::ptr::null_mut(),
                    0,
                )
            };
            check(result).context("Creating VAAPI upload device")?;
            unsafe {
                let upload = ffi::avfilter_graph_alloc_filter(
                    graph.as_mut_ptr(),
                    filter::find("hwupload")
                        .context("FFmpeg hwupload filter unavailable")?
                        .as_ptr(),
                    c"upload".as_ptr(),
                );
                if upload.is_null() {
                    ffi::av_buffer_unref(&mut device);
                    bail!("Allocating upload filter");
                }
                (*upload).hw_device_ctx = device;
                check(ffi::avfilter_init_str(upload, std::ptr::null()))?;
            }
            graph
                .output("in", 0)?
                .input("upload", 0)?
                .parse(&format!("[in]{before}[upload]"))?;
            let after = if after.is_empty() { "null" } else { after };
            graph
                .output("upload", 0)?
                .input("out", 0)?
                .parse(&format!("[upload]{after}[out]"))?;
        } else {
            graph
                .output("in", 0)?
                .input("out", 0)?
                .parse(spec)
                .with_context(|| format!("Building FFmpeg filter graph {spec}"))?;
        }
        unsafe {
            let frames = (*input.as_ptr()).hw_frames_ctx;
            let device = if !frames.is_null() {
                (*((*frames).data.cast::<ffi::AVHWFramesContext>())).device_ref
            } else if let Some(mut upload) = graph.get("upload") {
                (*upload.as_mut_ptr()).hw_device_ctx
            } else {
                std::ptr::null_mut()
            };
            if !device.is_null() {
                for index in 0..(*graph.as_mut_ptr()).nb_filters as usize {
                    let filter = *(*graph.as_mut_ptr()).filters.add(index);
                    if (*filter).hw_device_ctx.is_null() {
                        (*filter).hw_device_ctx = ffi::av_buffer_ref(device);
                    }
                }
            }
        }
        graph
            .validate()
            .with_context(|| format!("Configuring FFmpeg filter graph {spec}"))?;
        Ok(Self { graph })
    }
    pub fn palette(input: &frame::Video, palette: &frame::Video, scale: &str) -> Result<Self> {
        let mut graph = filter::Graph::new();
        unsafe {
            (*graph.as_mut_ptr()).nb_threads = 8;
        }
        for (name, frame) in [("in", input), ("palette", palette)] {
            let pixel: ffi::AVPixelFormat = frame.format().into();
            graph.add(
                &filter::find("buffer").context("FFmpeg buffer filter unavailable")?,
                name,
                &format!(
                    "video_size={}x{}:pix_fmt={}:time_base=1/90000:pixel_aspect=1/1",
                    frame.width(),
                    frame.height(),
                    pixel as i32
                ),
            )?;
        }
        graph.add(
            &filter::find("buffersink").context("FFmpeg buffersink unavailable")?,
            "out",
            "",
        )?;
        graph
            .output("in", 0)?
            .output("palette", 0)?
            .input("out", 0)?
            .parse(&format!(
                "[in]{scale}[scaled];[scaled][palette]paletteuse=dither=sierra2_4a[out]"
            ))?;
        graph.validate()?;
        graph.get("palette").unwrap().source().add(palette)?;
        graph.get("palette").unwrap().source().flush()?;
        Ok(Self { graph })
    }

    pub fn push(&mut self, input: &frame::Video) -> Result<()> {
        self.graph.get("in").unwrap().source().add(input)?;
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        self.graph.get("in").unwrap().source().flush()?;
        Ok(())
    }
    pub fn pull(&mut self) -> Result<Option<frame::Video>> {
        let mut output = frame::Video::empty();
        match self.graph.get("out").unwrap().sink().frame(&mut output) {
            Ok(()) => Ok(Some(output)),
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(e) if again(e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn hardware(frame: &frame::Video) -> bool {
    unsafe { !(*frame.as_ptr()).hw_frames_ctx.is_null() }
}

fn software_format(frame: &frame::Video) -> String {
    unsafe {
        let context = (*frame.as_ptr()).hw_frames_ctx;
        if context.is_null() {
            return "bgra".into();
        }
        let context = &*((*context).data.cast::<ffi::AVHWFramesContext>());
        CStr::from_ptr(ffi::av_get_pix_fmt_name(context.sw_format))
            .to_string_lossy()
            .into_owned()
    }
}

fn graphs(input: &frame::Video, name: &str, width: u32, height: u32, pixel: &str) -> Vec<String> {
    let cpu = format!(
        "scale={width}:{height}:flags=area:out_color_matrix=bt709:out_range=tv,format={pixel}"
    );
    let hw = hardware(input);
    let download = if hw {
        format!("hwdownload,format={},", software_format(input))
    } else {
        String::new()
    };
    let mut paths = Vec::new();
    if hw && input.width() == width && input.height() == height {
        let codec = encoder::find_by_name(name);
        if let Some(codec) = codec {
            unsafe {
                let mut index = 0;
                loop {
                    let config = ffi::avcodec_get_hw_config(codec.as_ptr(), index);
                    if config.is_null() {
                        break;
                    }
                    let pixel: ffi::AVPixelFormat = input.format().into();
                    if (*config).pix_fmt == pixel
                        && matches!(software_format(input).as_str(), "nv12" | "yuv420p")
                    {
                        paths.push("null".into());
                        break;
                    }
                    index += 1;
                }
            }
        }
    }
    match name {
        "h264_vaapi" => {
            if input.format() == format::Pixel::VAAPI {
                paths.push(format!("scale_vaapi=w={width}:h={height}:format=nv12"));
            } else if hw {
                paths.push(format!(
                    "hwmap=derive_device=vaapi,scale_vaapi=w={width}:h={height}:format=nv12"
                ));
            }
            paths.push(format!("{download}{cpu},hwupload"));
        }
        "h264_nvenc" => {
            if input.format() == format::Pixel::CUDA {
                paths.push(format!("scale_cuda=w={width}:h={height}:format=nv12"));
            } else if hw {
                paths.push(format!(
                    "hwmap=derive_device=cuda,scale_cuda=w={width}:h={height}:format=nv12"
                ));
            }
            paths.push(format!("{download}{cpu},hwupload_cuda"));
            if input.format() == format::Pixel::DRM_PRIME {
                paths.push(format!("hwmap=derive_device=vaapi,scale_vaapi=w={width}:h={height}:format=nv12,hwdownload,format=nv12,hwupload_cuda"));
            }
        }
        "h264_qsv" => {
            if hw {
                paths.push(format!(
                    "hwmap=derive_device=qsv,scale_qsv=w={width}:h={height}:format=nv12"
                ));
            }
            paths.push(format!("{download}{cpu}"));
        }
        "h264_amf" | "h264_mf" | "h264_videotoolbox" => {
            if hw && input.width() == width && input.height() == height {
                paths.push("null".into());
                let device = if name == "h264_videotoolbox" {
                    "videotoolbox"
                } else {
                    "d3d11va"
                };
                paths.push(format!("hwmap=derive_device={device}"));
            }
            paths.push(format!("{download}{cpu}"));
        }
        _ => paths.push(format!("{download}{cpu}")),
    }
    if input.format() == format::Pixel::DRM_PRIME && !matches!(name, "h264_vaapi" | "h264_nvenc") {
        paths.push(format!(
            "hwmap=derive_device=vaapi,hwdownload,format={},{cpu}",
            software_format(input)
        ));
    }
    paths
}

fn open_video(name: &str, test: &frame::Video, fps: u32, base: Rational) -> Result<encoder::Video> {
    let codec = encoder::find_by_name(name)
        .with_context(|| format!("FFmpeg encoder {name} unavailable"))?;
    let mut encoder = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    encoder.set_width(test.width());
    encoder.set_height(test.height());
    encoder.set_format(test.format());
    encoder.set_time_base(base);
    encoder.set_frame_rate(Some((fps as i32, 1)));
    encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    encoder.set_gop(fps * 2);
    encoder.set_max_b_frames(0);
    unsafe {
        let context = &mut *encoder.as_mut_ptr();
        context.thread_count = 8;
        context.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        context.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        context.colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
        context.color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        if matches!(
            test.format(),
            format::Pixel::RGB24 | format::Pixel::RGBA | format::Pixel::BGRA | format::Pixel::BGRZ
        ) {
            context.colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
            context.color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            context.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
        }
        let hw = (*test.as_ptr()).hw_frames_ctx;
        if !hw.is_null() {
            context.hw_frames_ctx = ffi::av_buffer_ref(hw);
        }
    }
    let mut options = Dictionary::new();
    match name {
        "libx264" => {
            options.set("preset", "veryfast");
            options.set("crf", "18");
        }
        "h264_nvenc" => {
            options.set("preset", "p4");
            options.set("rc", "vbr");
            options.set("cq", "19");
        }
        "libwebp_anim" => {
            options.set("quality", "80");
            options.set("loop", "0");
        }
        "libaom-av1" => {
            options.set("cpu-used", "8");
            options.set("crf", "28");
            options.set("lag-in-frames", "0");
        }
        "libsvtav1" => {
            options.set("preset", "10");
            options.set("crf", "28");
        }
        "libjxl_anim" => {
            options.set("effort", "3");
            options.set("distance", "1");
        }
        _ => {}
    }
    Ok(encoder.open_with(options)?)
}

fn download(input: &frame::Video) -> Result<frame::Video> {
    let mut output = frame::Video::empty();
    unsafe {
        let context = (*input.as_ptr()).hw_frames_ctx;
        ensure!(!context.is_null(), "Hardware frame has no hw_frames_ctx");
        let context = &*((*context).data.cast::<ffi::AVHWFramesContext>());
        output.set_format(context.sw_format.into());
        // DRM PRIME's mapping implementation understands object offsets,
        // modifiers and plane strides; manual fd mmap cannot handle tiled BOs.
        let mapped = ffi::av_hwframe_map(
            output.as_mut_ptr(),
            input.as_ptr(),
            ffi::AV_HWFRAME_MAP_READ as i32,
        );
        if mapped >= 0 {
            return Ok(output);
        }
        ffi::av_frame_unref(output.as_mut_ptr());
        check(ffi::av_hwframe_transfer_data(
            output.as_mut_ptr(),
            input.as_ptr(),
            0,
        ))
        .context("Downloading hardware frame")?;
        check(ffi::av_frame_copy_props(
            output.as_mut_ptr(),
            input.as_ptr(),
        ))?;
    }
    Ok(output)
}

pub(crate) struct VideoEncoder {
    pub name: String,
    graph: Graph,
    encoder: encoder::Video,
    base: Rational,
    fps: u32,
    download: bool,
}
impl VideoEncoder {
    pub fn select(
        input: &frame::Video,
        fps: u32,
        preference: EncoderPreference,
        master: bool,
    ) -> Result<Self> {
        let names: &[&str] = if master {
            &["ffv1"]
        } else if preference == EncoderPreference::Software {
            &["libx264", "libopenh264"]
        } else {
            &[
                "h264_nvenc",
                "h264_vaapi",
                "h264_qsv",
                "h264_amf",
                "h264_mf",
                "h264_videotoolbox",
                "libx264",
                "libopenh264",
            ]
        };
        let width = input.width() & !1;
        let height = input.height() & !1;
        ensure!(
            width > 0 && height > 0 && fps > 0,
            "Invalid recording size or frame rate"
        );
        let mut failures = Vec::new();
        for &name in names {
            if encoder::find_by_name(name).is_none() {
                continue;
            }
            let pixel = if master {
                "bgr0"
            } else if name == "libopenh264" {
                "yuv420p"
            } else {
                "nv12"
            };
            let mut cpu = None;
            let mut paths: std::collections::VecDeque<_> =
                graphs(input, name, width, height, pixel)
                    .into_iter()
                    .map(|spec| (spec, false))
                    .collect();
            while let Some((spec, downloaded)) = paths.pop_front() {
                #[cfg(target_os = "linux")]
                let mut devices: Vec<Option<std::ffi::CString>> = vec![None];
                #[cfg(not(target_os = "linux"))]
                let devices: Vec<Option<std::ffi::CString>> = vec![None];
                #[cfg(target_os = "linux")]
                if spec.ends_with(",hwupload") {
                    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.file_name()
                                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
                        })
                        .collect();
                    nodes.sort();
                    devices.extend(
                        nodes
                            .into_iter()
                            .filter_map(|path| {
                                std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()
                            })
                            .map(Some),
                    );
                }
                for device in devices {
                    let result = (|| -> Result<Self> {
                        let input = if downloaded {
                            cpu.as_ref().unwrap()
                        } else {
                            input
                        };
                        let mut graph = Graph::with_device(input, &spec, device.as_deref())?;
                        let mut test = reference(input)?;
                        test.set_pts(Some(0));
                        graph.push(&test)?;
                        let test = graph.pull()?.context("Filter produced no probe frame")?;
                        let base = Rational(1, 90000);
                        let mut probe = open_video(name, &test, fps, base)?;
                        probe.send_frame(&test)?;
                        probe.send_eof()?;
                        let mut packet = Packet::empty();
                        probe
                            .receive_packet(&mut packet)
                            .context("Encoder rejected probe frame")?;
                        let encoder = open_video(name, &test, fps, base)?;
                        log::info!("media encoder: {name}; {spec}");
                        Ok(Self {
                            name: name.into(),
                            graph,
                            encoder,
                            base,
                            fps,
                            download: downloaded,
                        })
                    })();
                    match result {
                        Ok(encoder) => return Ok(encoder),
                        Err(e) => {
                            log::debug!("{name}: {e:#}");
                            failures.push(format!("{name}: {e:#}"));
                        }
                    }
                }
                if paths.is_empty() && !downloaded && hardware(input) {
                    cpu = download(input).ok();
                    if let Some(cpu) = &cpu {
                        paths.extend(
                            graphs(cpu, name, width, height, pixel)
                                .into_iter()
                                .map(|spec| (spec, true)),
                        );
                    }
                }
            }
        }
        bail!(
            "No FFmpeg encoder accepted the input: {}",
            failures.join("; ")
        )
    }

    pub fn named(
        input: &frame::Video,
        fps: u32,
        name: &str,
        pixel: &str,
        filter: Option<&str>,
    ) -> Result<Self> {
        let spec = filter
            .map(str::to_owned)
            .unwrap_or_else(|| format!("format={pixel}"));
        let graph = Graph::new(input, &spec)?;
        Self::filtered(input, fps, name, graph)
    }

    pub fn filtered(input: &frame::Video, fps: u32, name: &str, mut graph: Graph) -> Result<Self> {
        let mut test = reference(input)?;
        test.set_pts(Some(0));
        graph.push(&test)?;
        let test = graph.pull()?.context("Filter produced no frame")?;
        let base = if name == "gif" {
            Rational(1, 100)
        } else {
            Rational(1, 90000)
        };
        let encoder = open_video(name, &test, fps, base)?;
        // The probe frame configured the graph but is not part of the output.
        Ok(Self {
            name: name.into(),
            graph,
            encoder,
            base,
            fps,
            download: false,
        })
    }
}

struct AudioEncoder {
    encoder: encoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
    pending: std::collections::VecDeque<f32>,
    position: i64,
}
impl AudioEncoder {
    fn new(opus: bool) -> Result<Self> {
        let codec = if opus {
            encoder::find_by_name("libopus").or_else(|| encoder::find_by_name("aac"))
        } else {
            encoder::find_by_name("aac")
        }
        .context("FFmpeg AAC/Opus encoder unavailable")?;
        let mut encoder = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()?;
        encoder.set_rate(48000);
        encoder.set_channel_layout(ChannelLayout::STEREO);
        let sample = codec
            .audio()?
            .formats()
            .and_then(|mut x| x.next())
            .context("Audio encoder has no sample formats")?;
        encoder.set_format(sample);
        encoder.set_time_base((1, 48000));
        encoder.set_bit_rate(192000);
        encoder.set_flags(codec::Flags::GLOBAL_HEADER);
        let encoder = encoder.open_as(codec)?;
        let resampler = ffmpeg::software::resampling::Context::get(
            format::Sample::F32(format::sample::Type::Packed),
            ChannelLayout::STEREO,
            48000,
            sample,
            ChannelLayout::STEREO,
            48000,
        )?;
        Ok(Self {
            encoder,
            resampler,
            pending: Default::default(),
            position: 0,
        })
    }
}

pub(crate) struct Muxer {
    output: format::context::Output,
    video: VideoEncoder,
    audio: Option<AudioEncoder>,
    video_base: Rational,
    audio_base: Rational,
}
impl Muxer {
    pub fn new(path: &Path, mux: &str, video: VideoEncoder, audio: bool) -> Result<Self> {
        let mut output = format::output_as(path, mux)
            .with_context(|| format!("Opening FFmpeg muxer {mux} for {}", path.display()))?;
        {
            let mut stream = output.add_stream(video.encoder.codec())?;
            stream.set_parameters(&video.encoder);
            stream.set_time_base(video.base);
            stream.set_rate((video.fps as i32, 1));
            stream.set_avg_frame_rate((video.fps as i32, 1));
        }
        let audio = if audio {
            Some(AudioEncoder::new(mux == "matroska")?)
        } else {
            None
        };
        if let Some(audio) = &audio {
            let mut stream = output.add_stream(audio.encoder.codec())?;
            stream.set_parameters(&audio.encoder);
            stream.set_time_base((1, 48000));
        }
        let mut options = Dictionary::new();
        if mux == "mp4" {
            options.set("movflags", "+faststart");
        }
        if mux == "gif" || mux == "webp" {
            options.set("loop", "0");
        }
        output.write_header_with(options)?;
        let video_base = output.stream(0).unwrap().time_base();
        let audio_base = output
            .stream(1)
            .map_or(Rational(1, 48000), |s| s.time_base());
        Ok(Self {
            output,
            video,
            audio,
            video_base,
            audio_base,
        })
    }

    pub fn name(&self) -> &str {
        &self.video.name
    }

    pub fn video(&mut self, input: &frame::Video, time: Duration) -> Result<()> {
        let mut input = if self.video.download {
            download(input)?
        } else {
            reference(input)?
        };
        input.set_pts(Some((time.as_secs_f64() * 90000.0).round() as i64));
        self.video.graph.push(&input)?;
        while let Some(mut frame) = self.video.graph.pull()? {
            let pts = frame.pts().unwrap_or(0);
            frame.set_pts(Some(ffmpeg::Rescale::rescale(
                &pts,
                Rational(1, 90000),
                self.video.base,
            )));
            unsafe {
                (*frame.as_mut_ptr()).duration =
                    (1.0 / self.video.fps as f64 / f64::from(self.video.base)).round() as i64;
            }
            self.video.encoder.send_frame(&frame)?;
            self.video_packets()?;
        }
        Ok(())
    }

    fn video_packets(&mut self) -> Result<()> {
        loop {
            let mut packet = Packet::empty();
            match self.video.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(0);
                    if packet.duration() <= 0 {
                        packet.set_duration(
                            (1.0 / self.video.fps as f64 / f64::from(self.video.base)).round()
                                as i64,
                        );
                    }
                    packet.rescale_ts(self.video.base, self.video_base);
                    packet.write_interleaved(&mut self.output)?;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) if again(e) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Sequential, already mixed, stereo 48kHz samples.
    pub fn audio(&mut self, samples: &[f32]) -> Result<()> {
        if let Some(audio) = &mut self.audio {
            audio.pending.extend(samples);
        }
        self.audio_frames(false)
    }

    fn audio_frames(&mut self, finish: bool) -> Result<()> {
        let Some(audio) = &mut self.audio else {
            return Ok(());
        };
        let size = audio.encoder.frame_size().max(1) as usize;
        while audio.pending.len() >= size * 2 || (finish && !audio.pending.is_empty()) {
            let mut input = frame::Audio::new(
                format::Sample::F32(format::sample::Type::Packed),
                size,
                ChannelLayout::STEREO,
            );
            input.set_rate(48000);
            let values = unsafe {
                std::slice::from_raw_parts_mut(
                    input.data_mut(0).as_mut_ptr().cast::<f32>(),
                    size * 2,
                )
            };
            for value in values {
                *value = audio.pending.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0);
            }
            let mut converted = frame::Audio::empty();
            audio.resampler.run(&input, &mut converted)?;
            converted.set_pts(Some(audio.position));
            audio.position += converted.samples() as i64;
            audio.encoder.send_frame(&converted)?;
            Self::audio_packets(audio, &mut self.output, self.audio_base)?;
        }
        Ok(())
    }

    fn audio_packets(
        audio: &mut AudioEncoder,
        output: &mut format::context::Output,
        base: Rational,
    ) -> Result<()> {
        loop {
            let mut packet = Packet::empty();
            match audio.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(1);
                    packet.rescale_ts((1, 48000), base);
                    packet.write_interleaved(output)?;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) if again(e) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.video.graph.flush()?;
        while let Some(mut frame) = self.video.graph.pull()? {
            let pts = frame.pts().unwrap_or(0);
            frame.set_pts(Some(ffmpeg::Rescale::rescale(
                &pts,
                Rational(1, 90000),
                self.video.base,
            )));
            self.video.encoder.send_frame(&frame)?;
            self.video_packets()?;
        }
        self.video.encoder.send_eof()?;
        self.video_packets()?;
        self.audio_frames(true)?;
        if let Some(audio) = &mut self.audio {
            audio.encoder.send_eof()?;
            Self::audio_packets(audio, &mut self.output, self.audio_base)?;
        }
        self.output.write_trailer()?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{media::Frame, raster::Image};
    use std::time::Instant;

    #[test]
    fn drm_prime_vaapi_mapping() -> Result<()> {
        if !Path::new("/dev/dri/renderD129").exists() {
            return Ok(());
        }
        crate::media::init();
        let cpu = Frame::from_image(
            &Image::filled(2560, 1440, [30, 90, 180, 255]),
            Instant::now(),
        )
        .into_av();
        let mut upload =
            Graph::with_device(&cpu, "format=nv12,hwupload", Some(c"/dev/dri/renderD129"))?;
        upload.push(&cpu)?;
        let vaapi = upload.pull()?.context("VAAPI upload produced no frame")?;
        let mut map = Graph::new(&vaapi, "hwmap=mode=read,format=drm_prime")?;
        map.push(&vaapi)?;
        let drm = map.pull()?.context("DRM mapping produced no frame")?;
        assert_eq!(drm.format(), format::Pixel::DRM_PRIME);
        let graph = Graph::new(
            &drm,
            "hwmap=derive_device=vaapi,scale_vaapi=w=2560:h=1440:format=nv12",
        )?;
        let video = VideoEncoder::filtered(&drm, 30, "h264_vaapi", graph)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("vaapi.mp4");
        let start = Instant::now();
        let mut mux = Muxer::new(&path, "mp4", video, false)?;
        for index in 0..12 {
            mux.video(&drm, Duration::from_secs_f64(index as f64 / 30.0))?;
        }
        mux.finish()?;
        let elapsed = start.elapsed();
        let mut decoder = crate::media::decode::Decoder::open(&path)?;
        let mut count = 0;
        while decoder.next_frame()?.is_some() {
            count += 1;
        }
        assert_eq!(count, 12);
        eprintln!(
            "DRM PRIME → VAAPI 2560x1440: h264_vaapi, 12 frames in {elapsed:?} ({:.2} ms/frame)",
            elapsed.as_secs_f64() * 1000.0 / 12.0
        );
        let video = VideoEncoder::select(&drm, 30, EncoderPreference::Auto, false)?;
        let selected = video.name.clone();
        let path = directory.path().join("auto.mp4");
        let start = Instant::now();
        let mut mux = Muxer::new(&path, "mp4", video, false)?;
        for index in 0..12 {
            mux.video(&drm, Duration::from_secs_f64(index as f64 / 30.0))?;
        }
        mux.finish()?;
        let elapsed = start.elapsed();
        let mut decoder = crate::media::decode::Decoder::open(&path)?;
        let mut count = 0;
        while decoder.next_frame()?.is_some() {
            count += 1;
        }
        assert_eq!(count, 12);
        eprintln!(
            "DRM PRIME → Auto 2560x1440: {selected}, 12 frames in {elapsed:?} ({:.2} ms/frame)",
            elapsed.as_secs_f64() * 1000.0 / 12.0
        );
        Ok(())
    }
}
