//! Round trips through every H.264 encoder/decoder pairing. Vulkan cases
//! skip (with a note) on machines without a Vulkan Video H.264 GPU.

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::{
    DecodedPicture, EncodedFrame, VideoDecoder, VideoEncoder, VideoEncoderConfig, Yuv420,
    YuvLayout, h264_decoder, h264_encoder, h264_soft, h264_vulkan,
};
use crate::media::EncoderPreference;
use crate::media::convert::to_yuv420;
use crate::media::frame::PixelFormat;

/// Vulkan tests share one process-wide device and count its file
/// descriptors, so they run one at a time.
static GPU: Mutex<()> = Mutex::new(());

fn gpu_lock() -> MutexGuard<'static, ()> {
    GPU.lock().unwrap_or_else(PoisonError::into_inner)
}

const FPS: u32 = 30;

fn pts(index: usize) -> Duration {
    // Not a whole number of microseconds: timestamps must come back exact.
    Duration::from_nanos(index as u64 * 1_000_000_000 / u64::from(FPS))
}

/// Screen-like BGRA content: a gradient scrolling right next to dense
/// glyph-like strokes shifting down, so motion and sharp edges both count.
fn screen(width: u32, height: u32, index: usize) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = &mut data[(y * w + x) * 4..][..4];
            if x < w / 2 {
                let (cx, cy) = (x / 6, (y + index) / 10);
                let (gx, gy) = (x % 6, (y + index) % 10);
                let seed = (cx * 7919 + cy * 104_729) % 13;
                let stroke = (gx == seed % 4 && gy < 8) || (gy == 2 + seed % 5 && gx < 5);
                let v = if stroke { 20 } else { 235 };
                px.copy_from_slice(&[v, v, v, 255]);
            } else {
                let g = ((x + index * 4) * 255 / w.max(1)) as u8;
                let b = (y * 255 / h.max(1)) as u8;
                px.copy_from_slice(&[b, g, 255 - g / 2, 255]);
            }
        }
    }
    data
}

fn pictures(width: u32, height: u32, count: usize, layout: YuvLayout) -> Vec<Yuv420> {
    (0..count)
        .map(|i| {
            let bgra = screen(width, height, i);
            to_yuv420(
                &bgra,
                width as usize * 4,
                PixelFormat::Bgrx,
                width,
                height,
                layout,
            )
        })
        .collect()
}

fn encode(
    encoder: &mut dyn VideoEncoder,
    pictures: &[Yuv420],
    forced: &[usize],
) -> Vec<EncodedFrame> {
    let mut frames = Vec::new();
    for (i, picture) in pictures.iter().enumerate() {
        frames.extend(
            encoder
                .encode(picture, pts(i), forced.contains(&i))
                .unwrap(),
        );
    }
    frames.extend(encoder.finish().unwrap());
    frames
}

fn decode(decoder: &mut dyn VideoDecoder, frames: &[EncodedFrame]) -> Vec<DecodedPicture> {
    let mut pictures = Vec::new();
    for frame in frames {
        pictures.extend(decoder.decode(&frame.data, frame.pts).unwrap());
    }
    pictures.extend(decoder.flush().unwrap());
    pictures
}

/// Y, then U, then V, whatever the layout.
fn planar(picture: &Yuv420) -> Vec<u8> {
    let luma = (picture.width * picture.height) as usize;
    match picture.layout {
        YuvLayout::I420 => picture.data.clone(),
        YuvLayout::Nv12 => {
            let uv = &picture.data[luma..];
            let mut out = picture.data[..luma].to_vec();
            out.extend(uv.iter().step_by(2));
            out.extend(uv.iter().skip(1).step_by(2));
            out
        }
    }
}

fn psnr(a: &Yuv420, b: &Yuv420) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let (a, b) = (planar(a), planar(b));
    let mse = a
        .iter()
        .zip(&b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// NAL unit types of an Annex-B access unit.
fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            types.push(data[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

fn config(width: u32, height: u32) -> VideoEncoderConfig {
    VideoEncoderConfig {
        keyframe_interval: 15,
        ..VideoEncoderConfig::for_screen(width, height, FPS)
    }
}

/// Encodes 40 pictures (an IDR forced at 7), checks the stream, and
/// returns it with the source pictures.
fn encode_checked(
    encoder: &mut dyn VideoEncoder,
    width: u32,
    height: u32,
) -> (Vec<EncodedFrame>, Vec<Yuv420>) {
    const COUNT: usize = 40;
    let source = pictures(width, height, COUNT, encoder.input_layout());
    let frames = encode(encoder, &source, &[7]);
    let name = encoder.name();
    assert_eq!(frames.len(), COUNT, "{name}: one access unit per picture");
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(frame.pts, pts(i), "{name}: pts of access unit {i}");
        let types = nal_types(&frame.data);
        assert_eq!(
            frame.keyframe,
            types.contains(&5),
            "{name}: AU {i} has {types:?}"
        );
        if frame.keyframe {
            assert!(
                types.contains(&7) && types.contains(&8),
                "{name}: AU {i} lacks SPS/PPS"
            );
        }
    }
    // The interval counts from the last IDR, forced ones included.
    let keyframes: Vec<usize> = (0..COUNT).filter(|&i| frames[i].keyframe).collect();
    assert_eq!(keyframes, [0, 7, 22, 37], "{name}: keyframe positions");
    (frames, source)
}

fn check_decoded(decoder_name: &str, decoded: &[DecodedPicture], source: &[Yuv420]) {
    assert_eq!(decoded.len(), source.len(), "{decoder_name}: picture count");
    for (i, (picture, original)) in decoded.iter().zip(source).enumerate() {
        assert_eq!(picture.pts, pts(i), "{decoder_name}: pts of picture {i}");
        let quality = psnr(&picture.picture, original);
        assert!(
            quality > 30.0,
            "{decoder_name}: picture {i} PSNR {quality:.1} dB"
        );
    }
}

/// The Vulkan encoder, or `None` (with a note) on machines without one.
fn vulkan_encoder(config: &VideoEncoderConfig) -> Option<h264_vulkan::Encoder> {
    match h264_vulkan::Encoder::open(config) {
        Ok(encoder) => Some(encoder),
        Err(err) => {
            eprintln!("skipping Vulkan H.264 encode: {err:#}");
            None
        }
    }
}

fn vulkan_decoder() -> Option<h264_vulkan::Decoder> {
    match h264_vulkan::Decoder::open() {
        Ok(decoder) => Some(decoder),
        Err(err) => {
            eprintln!("skipping Vulkan H.264 decode: {err:#}");
            None
        }
    }
}

#[test]
fn openh264_round_trip() {
    let (width, height) = (320, 180);
    let mut encoder = h264_soft::Encoder::open(&config(width, height)).unwrap();
    let (frames, source) = encode_checked(&mut encoder, width, height);
    let mut decoder = h264_soft::Decoder::open().unwrap();
    check_decoded("OpenH264", &decode(&mut decoder, &frames), &source);
}

#[test]
fn vulkan_round_trips_and_cross_decodes_with_openh264() {
    let _gpu = gpu_lock();
    let (width, height) = (320, 180);
    let Some(mut encoder) = vulkan_encoder(&config(width, height)) else {
        return;
    };
    let (frames, source) = encode_checked(&mut encoder, width, height);
    drop(encoder);
    let mut soft = h264_soft::Decoder::open().unwrap();
    check_decoded("OpenH264 ← Vulkan", &decode(&mut soft, &frames), &source);
    let Some(mut decoder) = vulkan_decoder() else {
        return;
    };
    check_decoded("Vulkan ← Vulkan", &decode(&mut decoder, &frames), &source);

    let mut encoder = h264_soft::Encoder::open(&config(width, height)).unwrap();
    let (frames, source) = encode_checked(&mut encoder, width, height);
    check_decoded("Vulkan ← OpenH264", &decode(&mut decoder, &frames), &source);
}

/// Scrolling colour bars under a row of black/white blocks spelling `index`
/// in binary: mostly skipped macroblocks with sharp changes in between, the
/// kind of picture that exposed stale bitstream padding in gpu-video.
fn index_pattern(width: u32, height: u32, index: usize) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = if y < 16 && x < 128 {
                let level = if index >> (7 - x / 16) & 1 == 1 {
                    255
                } else {
                    0
                };
                [level, level, level, 255]
            } else {
                match (x + index * 4) / 40 % 3 {
                    0 => [40, 40, 200, 255],
                    1 => [40, 200, 40, 255],
                    _ => [200, 40, 40, 255],
                }
            };
            data[(y * w + x) * 4..][..4].copy_from_slice(&px);
        }
    }
    data
}

/// OpenH264's 4-slice pictures decode on the GPU exactly as OpenH264 decodes
/// them, the same on every run.
#[test]
fn vulkan_decodes_multi_slice_streams_exactly() {
    let _gpu = gpu_lock();
    let (width, height) = (320, 240);
    let mut encoder = h264_soft::Encoder::open(&config(width, height)).unwrap();
    let source: Vec<Yuv420> = (0..90)
        .map(|i| {
            let bgra = index_pattern(width, height, i);
            to_yuv420(
                &bgra,
                width as usize * 4,
                PixelFormat::Bgrx,
                width,
                height,
                YuvLayout::I420,
            )
        })
        .collect();
    let frames = encode(&mut encoder, &source, &[]);
    assert_eq!(
        nal_types(&frames[1].data),
        [1, 1, 1, 1],
        "4 slices per picture"
    );
    let reference: Vec<Vec<u8>> = decode(&mut h264_soft::Decoder::open().unwrap(), &frames)
        .iter()
        .map(|p| planar(&p.picture))
        .collect();
    for run in 0..5 {
        let Some(mut decoder) = vulkan_decoder() else {
            return;
        };
        let decoded = decode(&mut decoder, &frames);
        assert_eq!(decoded.len(), reference.len(), "run {run}");
        for (i, (picture, reference)) in decoded.iter().zip(&reference).enumerate() {
            assert_eq!(picture.pts, pts(i), "run {run}");
            assert!(
                planar(&picture.picture) == *reference,
                "run {run}: picture {i} differs from OpenH264's ({:.1} dB from the source)",
                psnr(&picture.picture, &source[i])
            );
        }
    }
}

#[test]
fn edge_sizes_round_trip_through_the_chosen_codecs() {
    let _gpu = gpu_lock();
    for (width, height) in [(2, 2), (6, 18), (16, 16), (1366, 768)] {
        for preference in [EncoderPreference::Auto, EncoderPreference::Software] {
            let mut encoder = h264_encoder(&config(width, height), preference).unwrap();
            let source = pictures(width, height, 3, encoder.input_layout());
            let frames = encode(encoder.as_mut(), &source, &[]);
            let name = format!("{width}x{height} {}", encoder.name());
            drop(encoder);
            assert_eq!(frames.len(), 3, "{name}");
            assert!(frames[0].keyframe, "{name}");
            let mut decoder = h264_decoder(preference).unwrap();
            let decoded = decode(decoder.as_mut(), &frames);
            check_decoded(&format!("{name} → {}", decoder.name()), &decoded, &source);
        }
    }
}

/// Faint flat changes (a gray block stepping a few levels per picture next to
/// scrolling content) must show up in the very picture they happen in.
/// OpenH264's background detection used to skip them now and then.
#[test]
fn openh264_keeps_faint_changes() {
    let (width, height) = (320u32, 240u32);
    let (w, h) = (width as usize, height as usize);
    for run in 0..6 {
        let mut encoder = h264_soft::Encoder::open(&config(width, height)).unwrap();
        let mut decoder = h264_soft::Decoder::open().unwrap();
        for i in 0..16usize {
            let level = (40 + 14 * i) as u8;
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let v = if (100..132).contains(&x) && (100..132).contains(&y) {
                        level
                    } else if (x + i * 3) / 8 % 2 == 0 && y > 180 {
                        230
                    } else {
                        60
                    };
                    bgra[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
                }
            }
            let picture = to_yuv420(
                &bgra,
                w * 4,
                PixelFormat::Bgrx,
                width,
                height,
                YuvLayout::I420,
            );
            let expected = f64::from(picture.data[110 * w + 110]);
            let mut decoded = Vec::new();
            for frame in encoder.encode(&picture, pts(i), false).unwrap() {
                decoded.extend(decoder.decode(&frame.data, frame.pts).unwrap());
            }
            assert_eq!(decoded.len(), 1, "run {run}: picture {i} decoded at once");
            let luma = &decoded[0].picture.data;
            let block: f64 = (104..128)
                .flat_map(|y| (104..128).map(move |x| f64::from(luma[y * w + x])))
                .sum::<f64>()
                / (24.0 * 24.0);
            assert!(
                (block - expected).abs() < 4.0,
                "run {run}: picture {i} block at Y {block:.1}, want {expected}"
            );
        }
    }
}

/// The same pictures always encode to the same bytes, whatever the timing
/// of OpenH264's slice threads.
#[test]
fn openh264_output_is_deterministic() {
    let (width, height) = (640, 360);
    let source = pictures(width, height, 12, YuvLayout::I420);
    let streams: Vec<Vec<EncodedFrame>> = (0..4)
        .map(|_| {
            let mut encoder = h264_soft::Encoder::open(&config(width, height)).unwrap();
            encode(&mut encoder, &source, &[])
        })
        .collect();
    for stream in &streams[1..] {
        assert!(stream == &streams[0], "encodes differ");
    }
}

#[test]
fn openh264_refuses_pictures_beyond_its_level() {
    let err = h264_soft::Encoder::open(&config(4096, 2160))
        .err()
        .expect("4096x2160 refused");
    assert!(err.to_string().contains("at most 3840x2160"), "{err}");
    assert!(h264_soft::Encoder::open(&config(2160, 3840)).is_ok());
}

#[test]
fn vulkan_device_lives_only_as_long_as_its_codecs() {
    {
        let _gpu = gpu_lock();
        assert!(!h264_vulkan::device_held());
        let config = config(320, 180);
        let Some(mut encoder) = vulkan_encoder(&config) else {
            return;
        };
        let decoder = vulkan_decoder();
        assert!(h264_vulkan::device_held());
        encode(&mut encoder, &pictures(320, 180, 2, YuvLayout::Nv12), &[]);
        drop(encoder);
        assert_eq!(
            decoder.is_some(),
            h264_vulkan::device_held(),
            "the decoder keeps it"
        );
        drop(decoder);
        assert!(!h264_vulkan::device_held());
    }
    // Other tests in this binary open GPU devices concurrently, so the
    // driver's device files are counted in a process of their own.
    #[cfg(target_os = "linux")]
    {
        let inner = concat!(
            module_path!(),
            "::gpu_device_files_close_with_the_last_codec"
        );
        let inner = inner.split_once("::").expect("crate-qualified path").1;
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                inner,
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(FD_PROBE, "1")
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "isolated fd check failed:\n{log}");
        assert!(
            log.contains("1 passed"),
            "isolated fd check did not run:\n{log}"
        );
    }
}

/// Marks the child process of the fd check, where nothing else runs.
#[cfg(target_os = "linux")]
const FD_PROBE: &str = "QUIKSCOPE_TEST_GPU_FD_PROBE";

/// Device files a GPU driver opened in this process.
#[cfg(target_os = "linux")]
fn gpu_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.starts_with("/dev/nvidia") || target.starts_with("/dev/dri"))
        .count()
}

/// Run by `vulkan_device_lives_only_as_long_as_its_codecs` in a process of
/// its own.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "run in isolation by vulkan_device_lives_only_as_long_as_its_codecs"]
fn gpu_device_files_close_with_the_last_codec() {
    if std::env::var_os(FD_PROBE).is_none() {
        eprintln!("skipping: only meaningful alone in its process");
        return;
    }
    let before = gpu_fds();
    let config = config(320, 180);
    let Some(mut encoder) = vulkan_encoder(&config) else {
        return;
    };
    let decoder = vulkan_decoder();
    let open = gpu_fds();
    encode(&mut encoder, &pictures(320, 180, 2, YuvLayout::Nv12), &[]);
    drop(encoder);
    drop(decoder);
    let after = gpu_fds();
    println!("GPU device fds: {before} before, {open} while open, {after} after");
    assert!(open > before, "opening a device should open device files");
    assert_eq!(after, before, "device files left open after the last codec");
}

/// Splits an Annex-B stream with one slice per picture into access units.
fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            let begin = if i > 0 && stream[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            starts.push((begin, stream[i + 3] & 0x1f, stream.get(i + 4).copied()));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units: Vec<Vec<u8>> = Vec::new();
    let mut seen_slice = false;
    for (n, &(begin, kind, first)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(stream.len(), |s| s.0);
        let slice = kind == 1 || kind == 5;
        // A new picture starts at a parameter set/SEI/AUD after a slice, or
        // at a slice whose first_mb_in_slice (leading ue(v) bit) is zero.
        let new_unit = seen_slice
            && (matches!(kind, 6..=9) || (slice && first.is_some_and(|b| b & 0x80 != 0)));
        if units.is_empty() || new_unit {
            units.push(Vec::new());
            seen_slice = false;
        }
        seen_slice |= slice;
        units
            .last_mut()
            .unwrap()
            .extend_from_slice(&stream[begin..end]);
    }
    units
}

fn run(command: &mut Command) -> Vec<u8> {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Access units with their pts, in decode order.
type Stream = Vec<(Vec<u8>, Duration)>;

/// A High-profile x264 stream with B-pyramids, plus ffmpeg's own decode as
/// I420 pictures in presentation order. `None` without an ffmpeg CLI that
/// has libx264.
fn x264_b_frames(dir: &Path) -> Option<(Stream, Vec<Yuv420>)> {
    let encoders = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    if !encoders.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("libx264")) {
        eprintln!("skipping: no ffmpeg CLI with libx264 to make a B-frame stream");
        return None;
    }
    let (width, height, count) = (192, 108, 30);
    let mp4 = dir.join("b.mp4");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={width}x{height}:rate=30"))
        .args([
            "-frames:v",
            "30",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-bf",
            "3",
            "-b_strategy",
            "0",
            "-g",
            "12",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&mp4));
    let packets = String::from_utf8(run(Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "packet=pts",
            "-of",
            "csv=p=0",
        ])
        .arg(&mp4)))
    .unwrap();
    let pts: Vec<Duration> = packets
        .lines()
        .map(|line| Duration::from_millis(line.trim().trim_end_matches(',').parse().unwrap()))
        .collect();
    let annexb = run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&mp4)
        .args([
            "-c",
            "copy",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
            "-",
        ]));
    let units = access_units(&annexb);
    assert_eq!(units.len(), pts.len(), "access units vs packets");
    assert!(
        pts.windows(2).any(|w| w[1] < w[0]),
        "stream has no reordered B-frames"
    );
    let raw = run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&mp4)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"]));
    let size = (width * height * 3 / 2) as usize;
    assert_eq!(raw.len(), size * count);
    let reference = raw
        .chunks_exact(size)
        .map(|data| Yuv420 {
            width,
            height,
            layout: YuvLayout::I420,
            data: data.to_vec(),
        })
        .collect();
    Some((units.into_iter().zip(pts).collect(), reference))
}

/// Decodes `stream` and compares with ffmpeg's decode: same pictures in the
/// same order with the right pts, at least `min_psnr` dB alike.
fn check_reordering(
    decoder: &mut dyn VideoDecoder,
    stream: &[(Vec<u8>, Duration)],
    reference: &[Yuv420],
    min_psnr: f64,
) {
    let name = decoder.name().to_owned();
    let mut decoded = Vec::new();
    for (unit, pts) in stream {
        decoded.extend(decoder.decode(unit, *pts).unwrap());
    }
    decoded.extend(decoder.flush().unwrap());
    let mut expected: Vec<Duration> = stream.iter().map(|(_, pts)| *pts).collect();
    expected.sort();
    let got: Vec<Duration> = decoded.iter().map(|p| p.pts).collect();
    assert_eq!(got, expected, "{name}: presentation order and pts");
    for (i, (picture, reference)) in decoded.iter().zip(reference).enumerate() {
        let quality = psnr(&picture.picture, reference);
        assert!(
            quality >= min_psnr,
            "{name}: picture {i} is {quality:.1} dB from ffmpeg's"
        );
    }
}

#[test]
fn decoders_reorder_b_frames_into_presentation_order() {
    let dir = tempfile::tempdir().unwrap();
    let Some((stream, reference)) = x264_b_frames(dir.path()) else {
        return;
    };
    // OpenH264's B-frame prediction is a few rounding steps off the spec
    // (70-85 dB from ffmpeg on some pictures); a misplaced picture would
    // score under 30.
    check_reordering(
        &mut h264_soft::Decoder::open().unwrap(),
        &stream,
        &reference,
        60.0,
    );
    let _gpu = gpu_lock();
    if let Some(mut decoder) = vulkan_decoder() {
        // Hardware decoding is bit-exact.
        check_reordering(&mut decoder, &stream, &reference, f64::INFINITY);
    }
}

/// Encode speed at 2560x1440 (run with `--release --nocapture`).
#[test]
fn encode_speed_1440p() {
    let _gpu = gpu_lock();
    let (width, height, count) = (2560, 1440, 30);
    let config = VideoEncoderConfig::for_screen(width, height, 60);
    let mut encoders: Vec<Box<dyn VideoEncoder>> =
        vec![Box::new(h264_soft::Encoder::open(&config).unwrap())];
    if let Some(encoder) = vulkan_encoder(&config) {
        encoders.push(Box::new(encoder));
    }
    for mut encoder in encoders {
        let source = pictures(width, height, count, encoder.input_layout());
        let start = Instant::now();
        let frames = encode(encoder.as_mut(), &source, &[]);
        let elapsed = start.elapsed();
        assert_eq!(frames.len(), count);
        let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
        println!(
            "{}: {:.1} ms/picture at 2560x1440, {} kbit/s at 60 fps",
            encoder.name(),
            elapsed.as_secs_f64() * 1000.0 / count as f64,
            bytes * 8 * 60 / count / 1000
        );
    }
}
