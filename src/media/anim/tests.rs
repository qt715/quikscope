//! Encode/decode round trips through files, as the editor and exporter use
//! them.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::master::{MasterReader, MasterWriter, is_master};
use super::*;

const MS: fn(u64) -> Duration = Duration::from_millis;

/// Variable-delay frames with distinct colors and a moving square, so
/// both the full-frame and the changed-rectangle paths are exercised.
fn frames(width: u32, height: u32) -> Vec<(Image, Duration)> {
    let colors = [
        [40, 40, 220, 255],
        [40, 200, 40, 255],
        [220, 60, 40, 255],
        [230, 230, 230, 255],
    ];
    let durations = [MS(1000), MS(120), MS(80), MS(40)];
    colors
        .iter()
        .zip(durations)
        .enumerate()
        .map(|(i, (&color, duration))| {
            let mut image = Image::filled(width, height, color);
            let x = 2 + 6 * i as u32;
            for y in 4..12 {
                for x in x..x + 8 {
                    let p = (y * width + x) as usize * 4;
                    image.data[p..p + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
            (image, duration)
        })
        .collect()
}

fn encode(format: OutputFormat, path: &Path, frames: &[(Image, Duration)]) {
    let (w, h) = (frames[0].0.width, frames[0].0.height);
    let mut encoder = create_encoder(format, path, w, h).unwrap();
    for (image, duration) in frames {
        encoder.push(image, *duration).unwrap();
    }
    encoder.finish().unwrap();
}

fn decode(path: &Path) -> (AnimInfo, Vec<AnimFrame>) {
    let mut decoder = open_decoder(path).unwrap().expect("recognised");
    let info = decoder.info().clone();
    let mut out = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        out.push(frame);
    }
    // Rewinding plays the same frames again.
    decoder.rewind().unwrap();
    let again = decoder.next_frame().unwrap().unwrap();
    assert_eq!(again, out[0]);
    (info, out)
}

/// Largest per-channel difference averaged over the image.
fn mean_error(a: &Image, b: &Image) -> f64 {
    let sum: u64 = a
        .data
        .iter()
        .zip(&b.data)
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum();
    sum as f64 / a.data.len() as f64
}

fn check_round_trip(format: OutputFormat, ext: &str, tolerance: f64, exact_timing: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("anim.{ext}"));
    let input = frames(64, 48);
    encode(format, &path, &input);
    let (info, output) = decode(&path);
    assert_eq!((info.width, info.height), (64, 48));
    assert_eq!(info.frames, input.len() as u64, "{ext} frame count");
    assert_eq!(output.len(), input.len());
    let mut pts = Duration::ZERO;
    let mut expected_end = Duration::ZERO;
    for (i, ((image, duration), frame)) in input.iter().zip(&output).enumerate() {
        expected_end += *duration;
        assert_eq!(frame.pts, pts, "{ext} frame {i} pts");
        pts += frame.duration;
        if exact_timing {
            assert_eq!(frame.duration, *duration, "{ext} frame {i} duration");
        } else {
            assert!(
                frame.duration.abs_diff(*duration) <= MS(10),
                "{ext} frame {i}: {:?}",
                frame.duration
            );
            assert!(
                pts.abs_diff(expected_end) <= MS(10),
                "{ext} frame {i} cumulative: {pts:?}"
            );
        }
        let error = mean_error(image, &frame.image);
        assert!(error <= tolerance, "{ext} frame {i} differs by {error}");
    }
    assert_eq!(info.duration, pts);
    validate_with_ffprobe(&path, input.len());
}

/// Cross-checks a file with FFmpeg's demuxers and decoders when ffprobe is
/// installed. An animated AVIF shows up as two streams, the primary still
/// image and the sequence; the longest one counts.
fn validate_with_ffprobe(path: &Path, frames: usize) {
    let Ok(output) = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames"])
        .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
    else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.trim().is_empty(),
        "ffprobe rejected {}: {stderr}",
        path.display()
    );
    let counted = stdout
        .lines()
        .filter_map(|line| line.trim().trim_end_matches(',').parse::<usize>().ok())
        .max();
    assert_eq!(
        counted,
        Some(frames),
        "ffprobe frame count of {}",
        path.display()
    );
}

#[test]
fn gif_round_trip() {
    check_round_trip(OutputFormat::Gif, "gif", 3.0, false);
}

#[test]
fn webp_round_trip() {
    check_round_trip(OutputFormat::Webp, "webp", 4.0, true);
}

#[test]
fn avif_round_trip() {
    check_round_trip(OutputFormat::Avif, "avif", 4.0, true);
}

#[test]
fn jxl_round_trip_is_lossless() {
    check_round_trip(OutputFormat::Jxl, "jxl", 0.0, true);
}

#[test]
fn jxl_keeps_alpha_and_large_frames_exactly() {
    // Larger than one 256x256 group (threaded path), odd size, alpha, a
    // frame equal to the one before, and one changing two distant spots
    // (coded as two partial frames).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.jxl");
    let mut input = frames(301, 263);
    for (i, (image, _)) in input.iter_mut().enumerate() {
        for (p, px) in image.data.chunks_exact_mut(4).enumerate() {
            px[3] = ((p * 7 + i * 31) % 256) as u8;
        }
    }
    input.push((input[3].0.clone(), MS(33)));
    let mut spots = input[3].0.clone();
    spots.data[..4].copy_from_slice(&[1, 2, 3, 4]);
    let end = spots.data.len();
    spots.data[end - 4..].copy_from_slice(&[5, 6, 7, 8]);
    input.push((spots, MS(1)));
    encode(OutputFormat::Jxl, &path, &input);
    let (info, output) = decode(&path);
    assert_eq!(info.frames, input.len() as u64);
    for ((image, duration), frame) in input.iter().zip(&output) {
        assert_eq!(frame.duration, *duration);
        assert!(frame.image.data == image.data, "JPEG XL changed pixels");
    }
    validate_with_ffprobe(&path, input.len());
}

#[test]
fn gif_keeps_long_run_timing_of_fast_frames() {
    // 60 fps cannot be shown by browsers (delays under 20 ms play at
    // 100 ms): frames are merged, but the length stays.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fast.gif");
    let input: Vec<_> = (0..60u8)
        .map(|i| {
            (
                Image::filled(16, 16, [i * 4, 0, 0, 255]),
                Duration::from_micros(16_667),
            )
        })
        .collect();
    encode(OutputFormat::Gif, &path, &input);
    let (info, output) = decode(&path);
    assert!(output.iter().all(|f| f.duration >= MS(20)));
    assert!(
        info.duration.abs_diff(Duration::from_secs(1)) <= MS(10),
        "{:?}",
        info.duration
    );
}

#[test]
fn gif_keeps_transparency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.gif");
    let mut first = Image::filled(16, 16, [0, 0, 255, 255]);
    first.data[..4].copy_from_slice(&[0, 0, 0, 0]);
    let second = Image::filled(16, 16, [0, 255, 0, 255]);
    // A small change (coded as a partial frame), then transparency where
    // that partial frame did not reach.
    let mut partial = second.clone();
    partial.data[(5 * 16 + 5) * 4..][..4].copy_from_slice(&[0, 0, 255, 255]);
    let mut third = second.clone();
    third.data[4..8].copy_from_slice(&[0, 0, 0, 0]);
    encode(
        OutputFormat::Gif,
        &path,
        &[
            (first, MS(100)),
            (second, MS(100)),
            (partial, MS(100)),
            (third, MS(100)),
        ],
    );
    let (_, output) = decode(&path);
    assert_eq!(output[0].image.pixel(0, 0)[3], 0);
    assert_eq!(output[1].image.pixel(0, 0), [0, 255, 0, 255]);
    assert_eq!(output[2].image.pixel(5, 5), [0, 0, 255, 255]);
    assert_eq!(output[3].image.pixel(1, 0)[3], 0);
    assert_eq!(output[3].image.pixel(0, 0), [0, 255, 0, 255]);
    assert_eq!(output[3].image.pixel(5, 5), [0, 255, 0, 255]);
}

#[test]
fn apng_composites_dispose_and_blend() {
    use ::png::{BlendOp, ColorType, DisposeOp};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("anim.png");
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = ::png::Encoder::new(std::io::BufWriter::new(file), 8, 8);
    encoder.set_color(ColorType::Rgba);
    encoder.set_animated(4, 0).unwrap();
    let mut writer = encoder.write_header().unwrap();
    let rgba = |w: usize, h: usize, px: [u8; 4]| px.repeat(w * h);
    // Opaque red, kept.
    writer.set_frame_delay(1, 10).unwrap();
    writer
        .write_image_data(&rgba(8, 8, [255, 0, 0, 255]))
        .unwrap();
    // Half-transparent blue blended over the middle, then undone.
    writer.set_frame_delay(1, 20).unwrap();
    writer.set_frame_dimension(4, 4).unwrap();
    writer.set_frame_position(2, 2).unwrap();
    writer.set_blend_op(BlendOp::Over).unwrap();
    writer.set_dispose_op(DisposeOp::Previous).unwrap();
    writer
        .write_image_data(&rgba(4, 4, [0, 0, 255, 128]))
        .unwrap();
    // Green corner with a zero delay, then cleared.
    writer.set_frame_delay(0, 100).unwrap();
    writer.set_frame_position(0, 0).unwrap();
    writer.set_frame_dimension(2, 2).unwrap();
    writer.set_blend_op(BlendOp::Source).unwrap();
    writer.set_dispose_op(DisposeOp::Background).unwrap();
    writer
        .write_image_data(&rgba(2, 2, [0, 255, 0, 255]))
        .unwrap();
    // An invisible pixel blended over.
    writer.set_frame_delay(3, 100).unwrap();
    writer.set_frame_dimension(1, 1).unwrap();
    writer.set_frame_position(7, 7).unwrap();
    writer.set_blend_op(BlendOp::Over).unwrap();
    writer.set_dispose_op(DisposeOp::None).unwrap();
    writer.write_image_data(&[0, 0, 0, 0]).unwrap();
    writer.finish().unwrap();

    let (info, frames) = decode(&path);
    assert_eq!(info.frames, 4);
    let durations: Vec<_> = frames.iter().map(|f| f.duration).collect();
    assert_eq!(durations, [MS(100), MS(50), MS(100), MS(30)]);
    let red = [0, 0, 255, 255];
    assert_eq!(frames[0].image.pixel(3, 3), red);
    assert_eq!(frames[1].image.pixel(3, 3), [128, 0, 127, 255]);
    assert_eq!(frames[1].image.pixel(0, 0), red);
    assert_eq!(frames[2].image.pixel(3, 3), red, "dispose to previous");
    assert_eq!(frames[2].image.pixel(1, 1), [0, 255, 0, 255]);
    assert_eq!(
        frames[3].image.pixel(1, 1),
        [0, 0, 0, 0],
        "dispose to background"
    );
    assert_eq!(frames[3].image.pixel(7, 7), red);
}

#[test]
fn still_png_is_one_frame_without_duration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("still.png");
    let image = image::RgbImage::from_pixel(5, 3, image::Rgb([10, 20, 30]));
    image.save(&path).unwrap();
    let (info, frames) = decode(&path);
    assert_eq!((info.frames, info.duration), (1, Duration::ZERO));
    assert_eq!(frames[0].image.pixel(4, 2), [30, 20, 10, 255]);
}

/// Files written by libavif (when `avifenc` is installed): alpha items,
/// grids, 10-bit, 4:2:0 limited range and a sequence with an alpha track.
#[test]
fn libavif_files_decode() {
    let dir = tempfile::tempdir().unwrap();
    let source = |name: &str, color: [u8; 3], alpha: u8| {
        let path = dir.path().join(name);
        // Grid tiles must be at least 64x64.
        let image = image::RgbaImage::from_fn(160, 128, |x, y| {
            let shade = ((x + y) / 2) as u8;
            image::Rgba([
                color[0].saturating_sub(shade),
                color[1],
                color[2].saturating_add(shade / 2),
                alpha,
            ])
        });
        image.save(&path).unwrap();
        (path, image)
    };
    let opaque = source("opaque.png", [200, 60, 30], 255);
    let half = source("half.png", [30, 200, 60], 128);
    let quarter = source("quarter.png", [60, 30, 200], 64);
    let check = |name: &str, args: &[&str], inputs: &[&(std::path::PathBuf, image::RgbaImage)]| {
        let out = dir.path().join(format!("{name}.avif"));
        let Ok(status) = Command::new("avifenc")
            .args(args)
            .args(inputs.iter().map(|(path, _)| path))
            .arg(&out)
            .output()
        else {
            return;
        };
        assert!(
            status.status.success(),
            "avifenc {name}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        let (info, frames) = decode(&out);
        assert_eq!(info.frames, inputs.len() as u64, "{name}");
        for (frame, (_, want)) in frames.iter().zip(inputs) {
            let mut want_bgra = Image::new(160, 128);
            to_bgra(want.as_raw(), 4, &mut want_bgra.data);
            let error = mean_error(&want_bgra, &frame.image);
            assert!(error < 3.0, "{name} differs by {error}");
        }
    };
    check("alpha", &["-q", "90"], &[&half]);
    check("grid", &["-q", "90", "--grid", "2x2"], &[&opaque]);
    check("ten", &["-q", "90", "-d", "10"], &[&opaque]);
    check(
        "limited",
        &["-q", "90", "--yuv", "420", "-r", "limited"],
        &[&opaque],
    );
    check(
        "sequence",
        &["-q", "90", "--timescale", "100"],
        &[&half, &quarter],
    );
}

#[test]
fn unknown_files_are_not_animations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "hello").unwrap();
    assert!(open_decoder(&path).unwrap().is_none());
    assert!(!is_master(&path));
}

fn master_frames(count: u32) -> Vec<(Image, Duration)> {
    (0..count)
        .map(|i| {
            let mut image = Image::filled(40, 30, [20, 30, 40, 255]);
            image.data[(i as usize * 4) % 4800..][..4].copy_from_slice(&[i as u8, 1, 2, 3]);
            (image, MS(u64::from(i) * 400))
        })
        .collect()
}

#[test]
fn master_round_trip_and_seek() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.qsm");
    let input = master_frames(12);
    let mut writer = MasterWriter::create(&path, 40, 30).unwrap();
    for (image, pts) in &input {
        writer.push(image, *pts).unwrap();
    }
    writer.finish(MS(4900)).unwrap();
    assert!(is_master(&path));

    let (info, frames) = decode(&path);
    assert_eq!(info.frames, 12);
    assert_eq!(info.duration, MS(4900));
    for ((image, pts), frame) in input.iter().zip(&frames) {
        assert_eq!(frame.pts, *pts);
        assert_eq!(frame.image, *image);
    }
    assert_eq!(frames[11].duration, MS(500));

    let mut reader = MasterReader::open(&path).unwrap();
    // Within a delta run, at a keyframe, backwards, and forwards from the
    // current position.
    for (to, expect) in [(1750, 4), (2000, 5), (100, 0), (4300, 10), (4899, 11)] {
        reader.seek(MS(to)).unwrap();
        let frame = reader.next_frame().unwrap().unwrap();
        assert_eq!(frame.image, input[expect].0, "seek to {to} ms");
    }
}

#[test]
fn cut_off_master_opens_up_to_its_last_complete_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crashed.qsm");
    let input = master_frames(8);
    let mut writer = MasterWriter::create(&path, 40, 30).unwrap();
    for (image, pts) in &input {
        writer.push(image, *pts).unwrap();
    }
    writer.finish(MS(3200)).unwrap();
    // Drop the index and half of the last frame's record.
    let data = std::fs::read(&path).unwrap();
    let records_end = u64::from_le_bytes(data[data.len() - 16..data.len() - 8].try_into().unwrap());
    std::fs::write(&path, &data[..records_end as usize - 10]).unwrap();

    let (info, frames) = decode(&path);
    assert_eq!(info.frames, 7);
    for ((image, _), frame) in input.iter().zip(&frames) {
        assert_eq!(frame.image, *image);
    }
}

/// Encoding speed per format and master throughput on desktop-like frames.
/// `cargo test --release --lib media::anim::tests::speed -- --ignored --nocapture`
#[test]
#[ignore = "benchmark"]
fn speed() {
    let dir = tempfile::tempdir().unwrap();
    let anim: Vec<_> = (0..30).map(|i| (desktop(640, 480, i), MS(33))).collect();
    for (format, ext) in [
        (OutputFormat::Gif, "gif"),
        (OutputFormat::Webp, "webp"),
        (OutputFormat::Avif, "avif"),
        (OutputFormat::Jxl, "jxl"),
    ] {
        let path = dir.path().join(format!("speed.{ext}"));
        let start = Instant::now();
        encode(format, &path, &anim);
        let took = start.elapsed();
        let size = std::fs::metadata(&path).unwrap().len();
        println!("{ext}: 640x480x30 in {took:.2?}, {} KiB", size / 1024);
    }

    // 30 distinct frames cycled to 3 s keep memory near 0.5 GB.
    let (w, h) = (2560, 1440);
    let desktops: Vec<_> = (0..30).map(|i| desktop(w, h, i)).collect();
    master_speed(
        dir.path(),
        "desktop",
        &desktops.iter().cycle().take(90).collect::<Vec<_>>(),
    );
    // Every pixel changes every frame (e.g. a playing video): fresh noise
    // over the wallpaper in each of 8 frames, cycled.
    let mut seed = 1u32;
    let noisy: Vec<_> = (0..8)
        .map(|i| {
            let mut image = desktop(w, h, i);
            for px in image.data.chunks_exact_mut(4) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                px[0] = px[0].wrapping_add(seed as u8 & 7);
                px[1] = px[1].wrapping_add((seed >> 8) as u8 & 7);
            }
            image
        })
        .collect();
    master_speed(
        dir.path(),
        "noisy",
        &noisy.iter().cycle().take(90).collect::<Vec<_>>(),
    );
}

fn master_speed(dir: &Path, name: &str, frames: &[&Image]) {
    let (w, h, n) = (frames[0].width, frames[0].height, frames.len() as u32);
    let path = dir.join(format!("{name}.qsm"));
    let start = Instant::now();
    let mut writer = MasterWriter::create(&path, w, h).unwrap();
    for (i, frame) in frames.iter().enumerate() {
        writer.push(frame, MS(i as u64 * 33)).unwrap();
    }
    writer.finish(MS(u64::from(n) * 33)).unwrap();
    let took = start.elapsed();
    let raw = f64::from(w * h * 4) * f64::from(n);
    let size = std::fs::metadata(&path).unwrap().len() as f64;
    println!(
        "master {name}: {n} frames {w}x{h} in {took:.2?} = {:.0} MB/s ({:.0} fps), ratio {:.1}:1",
        raw / took.as_secs_f64() / 1e6,
        f64::from(n) / took.as_secs_f64(),
        raw / size
    );
    let start = Instant::now();
    let mut reader = MasterReader::open(&path).unwrap();
    while reader.next_frame().unwrap().is_some() {}
    println!("master {name} decode: {:.2?}", start.elapsed());
}

/// A desktop-ish frame: a top bar, a photo-like (noisy gradient)
/// wallpaper, a window of text-like stripes that moves 4 px per frame, and
/// a small clock that changes every frame.
fn desktop(width: u32, height: u32, t: u32) -> Image {
    let mut image = Image::new(width, height);
    let wx = 100 + t * 4;
    for y in 0..height {
        for x in 0..width {
            let p = (y * width + x) as usize * 4;
            let noise = ((x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) >> 7) as u8 & 15;
            let in_window = x >= wx && x < wx + width / 2 && y >= 80 && y < 80 + height / 2;
            let px = if y < 32 {
                let clock = x >= width - 64 && ((x + y + t) % 7) < 3;
                if clock {
                    [200, 200, 200, 255]
                } else {
                    [40, 40, 40, 255]
                }
            } else if in_window {
                let text = (y / 4) % 5 != 0 && (((x - wx) * 7 + y * 3) % 23) < 9;
                if text {
                    [20, 20, 20, 255]
                } else {
                    [245, 245, 245, 255]
                }
            } else {
                [
                    (x * 200 / width) as u8 + noise,
                    (y * 200 / height) as u8 + noise,
                    120 + noise,
                    255,
                ]
            };
            image.data[p..p + 4].copy_from_slice(&px);
        }
    }
    image
}
