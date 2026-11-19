//! Mux → demux round trips with a real OpenH264 stream and real Opus
//! packets: bytes, times, keyframes, durations, track offsets, seeking and
//! crash recovery. `ffprobe`/`ffmpeg` add an external check when installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{Encoder, EncoderConfig, FrameType, IntraFramePeriod};
use openh264::formats::YUVBuffer;

use super::*;
use crate::media::codec::opus::{self, BITRATE, FRAME};

const WIDTH: usize = 64;
const HEIGHT: usize = 48;
const FRAMES: u64 = 120;
const GOP: u64 = 15;
const FRAME_NS: u64 = 33_333_333;
const AUDIO_START: Duration = Duration::from_secs(2);
const AUDIO_PACKETS: u64 = 100;
const AUDIO_NS: u64 = 20_000_000;

struct Fixture {
    tracks: Vec<Track>,
    /// In pts order across tracks, as a recorder delivers them.
    packets: Vec<Packet>,
}

fn fixture() -> Fixture {
    let config = EncoderConfig::new()
        .skip_frames(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(GOP as u32));
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
    let mut packets = Vec::new();
    for i in 0..FRAMES {
        let mut yuv = vec![128u8; WIDTH * HEIGHT * 3 / 2];
        for (j, y) in yuv[..WIDTH * HEIGHT].iter_mut().enumerate() {
            *y = ((j % WIDTH) as u64 * 3 + i * 5 + (j / WIDTH) as u64) as u8;
        }
        let stream = encoder
            .encode(&YUVBuffer::from_vec(yuv, WIDTH, HEIGHT))
            .unwrap();
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        packets.push(Packet {
            track: 0,
            data: stream.to_vec(),
            pts: Duration::from_nanos(i * FRAME_NS),
            duration: Some(Duration::from_nanos(FRAME_NS)),
            keyframe,
        });
    }
    assert!(packets[0].keyframe && packets[GOP as usize].keyframe);

    let mut opus = opus::Encoder::new(BITRATE).unwrap();
    for i in 0..AUDIO_PACKETS {
        let samples: Vec<f32> = (0..FRAME * 2)
            .map(|s| ((i as usize * FRAME + s / 2) as f32 * 0.05).sin() * 0.3)
            .collect();
        packets.push(Packet {
            track: 1,
            data: opus.encode(&samples).unwrap(),
            pts: AUDIO_START + Duration::from_nanos(i * AUDIO_NS),
            duration: Some(Duration::from_nanos(AUDIO_NS)),
            keyframe: true,
        });
    }
    packets.sort_by_key(|p| p.pts);
    Fixture {
        tracks: vec![
            Track {
                codec: Codec::H264,
                kind: TrackKind::Video {
                    width: WIDTH as u32,
                    height: HEIGHT as u32,
                    fps: 30.0,
                },
                codec_private: Vec::new(),
            },
            Track {
                codec: Codec::Opus,
                kind: TrackKind::Audio {
                    rate: 48_000,
                    channels: 2,
                },
                codec_private: opus.head(),
            },
        ],
        packets,
    }
}

fn mux(fixture: &Fixture, path: &Path, container: Container, finish: bool) {
    let mut muxer = create_muxer(path, container, &fixture.tracks).unwrap();
    for packet in &fixture.packets {
        muxer.write(packet.clone()).unwrap();
    }
    if finish {
        muxer.finish().unwrap();
    }
}

fn read_all(demuxer: &mut dyn Demuxer) -> Vec<Packet> {
    std::iter::from_fn(|| demuxer.next_packet().unwrap()).collect()
}

/// NAL units without access unit delimiters, to compare streams that
/// differ only in start-code length or where parameter sets travel.
fn nals(annexb: &[u8]) -> Vec<Vec<u8>> {
    nal::annexb_nals(annexb)
        .filter(|n| n[0] & 0x1F != 9)
        .map(<[u8]>::to_vec)
        .collect()
}

fn close(a: Duration, b: Duration, tolerance: Duration) -> bool {
    a.abs_diff(b) <= tolerance
}

fn first_of(packets: &[Packet], track: usize) -> &Packet {
    packets.iter().find(|p| p.track == track).unwrap()
}

/// Every packet survives with its bytes, flags and times; track offsets,
/// duration and seeking hold.
fn check(fixture: &Fixture, path: &Path, tolerance: Duration) {
    let mut demuxer = open_demuxer(path).unwrap().expect("recognised");
    let tracks = demuxer.tracks().to_vec();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].codec, Codec::H264);
    let TrackKind::Video { width, height, fps } = tracks[0].kind else {
        panic!("video track expected");
    };
    assert_eq!((width, height), (WIDTH as u32, HEIGHT as u32));
    assert!((fps - 30.0).abs() < 0.1, "fps {fps}");
    assert_eq!(tracks[1].codec, Codec::Opus);
    assert_eq!(
        tracks[1].kind,
        TrackKind::Audio {
            rate: 48_000,
            channels: 2
        }
    );
    assert_eq!(tracks[1].codec_private, fixture.tracks[1].codec_private);
    let total = Duration::from_nanos(FRAMES * FRAME_NS)
        .max(AUDIO_START + Duration::from_nanos(AUDIO_PACKETS * AUDIO_NS));
    assert!(
        close(demuxer.duration(), total, tolerance),
        "duration {:?}",
        demuxer.duration()
    );

    let packets = read_all(demuxer.as_mut());
    for track in 0..2 {
        let written: Vec<&Packet> = fixture
            .packets
            .iter()
            .filter(|p| p.track == track)
            .collect();
        let read: Vec<&Packet> = packets.iter().filter(|p| p.track == track).collect();
        assert_eq!(read.len(), written.len(), "track {track} packet count");
        for (r, w) in read.iter().zip(&written) {
            assert!(
                close(r.pts, w.pts, tolerance),
                "track {track}: {:?} vs {:?}",
                r.pts,
                w.pts
            );
            assert_eq!(r.keyframe, w.keyframe, "keyframe at {:?}", w.pts);
            let duration = r.duration.expect("duration");
            assert!(
                close(duration, w.duration.unwrap(), tolerance),
                "duration {duration:?} at {:?}",
                w.pts
            );
            if track == 0 {
                assert_eq!(nals(&r.data), nals(&w.data), "H.264 at {:?}", w.pts);
            } else {
                assert_eq!(r.data, w.data);
            }
        }
    }
    // The audio offset survives: picture at 0, sound two seconds later.
    assert_eq!(first_of(&packets, 0).pts, Duration::ZERO);
    assert!(close(first_of(&packets, 1).pts, AUDIO_START, tolerance));

    check_seek(demuxer.as_mut(), tolerance);
}

fn check_seek(demuxer: &mut dyn Demuxer, tolerance: Duration) {
    let frame = |i: u64| Duration::from_nanos(i * FRAME_NS);
    // Video keyframes every 0.5 s; audio from 2 s in 20 ms packets.
    demuxer.seek(Duration::from_millis(2550)).unwrap();
    let after: Vec<Packet> = (0..100)
        .map_while(|_| demuxer.next_packet().unwrap())
        .collect();
    let video = first_of(&after, 0);
    assert!(video.keyframe);
    assert!(
        close(video.pts, frame(75), tolerance),
        "video after seek {:?}",
        video.pts
    );
    let audio = first_of(&after, 1);
    assert!(
        close(audio.pts, Duration::from_millis(2540), tolerance),
        "audio after seek {:?}",
        audio.pts
    );
    // Decodable from there: the keyframe carries its parameter sets.
    let mut decoder = Decoder::new().unwrap();
    assert!(decoder.decode(&video.data).unwrap().is_some());

    // Before the audio starts, audio resumes at its first packet.
    demuxer.seek(Duration::from_millis(1700)).unwrap();
    let after: Vec<Packet> = (0..80)
        .map_while(|_| demuxer.next_packet().unwrap())
        .collect();
    assert!(close(first_of(&after, 0).pts, frame(45), tolerance));
    assert!(close(first_of(&after, 1).pts, AUDIO_START, tolerance));

    // Exactly on a keyframe, and back to the start.
    demuxer.seek(frame(30)).unwrap();
    assert!(close(
        demuxer.next_packet().unwrap().unwrap().pts,
        frame(30),
        tolerance
    ));
    demuxer.seek(Duration::ZERO).unwrap();
    let first = demuxer.next_packet().unwrap().unwrap();
    assert_eq!((first.track, first.pts), (0, Duration::ZERO));

    // Past the end: the last keyframe.
    demuxer.seek(Duration::from_secs(60)).unwrap();
    let after: Vec<Packet> = std::iter::from_fn(|| demuxer.next_packet().unwrap()).collect();
    assert!(close(first_of(&after, 0).pts, frame(105), tolerance));
}

/// When `ffprobe` is installed, it must find both streams and the length
/// the picture and sound span (FFmpeg misreads late-starting fragmented
/// tracks whose start is not in the edit list).
fn ffprobe(path: &Path) {
    let Ok(output) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name:format=duration",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
    else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.trim().is_empty(),
        "ffprobe: {stderr}"
    );
    assert!(
        stdout.contains("codec_name=h264") && stdout.contains("codec_name=opus"),
        "ffprobe streams: {stdout}"
    );
    let duration: f64 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("duration="))
        .and_then(|d| d.parse().ok())
        .expect("ffprobe format duration");
    let total = (FRAMES * FRAME_NS).max(AUDIO_START.as_nanos() as u64 + AUDIO_PACKETS * AUDIO_NS);
    assert!(
        (duration - total as f64 / 1e9).abs() < 0.02,
        "ffprobe duration {duration} s"
    );
}

fn temp(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    (dir, path)
}

fn top_level_boxes(path: &Path) -> Vec<String> {
    let data = std::fs::read(path).unwrap();
    let mut pos = 0;
    let mut kinds = Vec::new();
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        kinds.push(String::from_utf8_lossy(&data[pos + 4..pos + 8]).into_owned());
        let size = if size == 1 {
            u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize
        } else {
            size
        };
        pos += size.max(8);
    }
    kinds
}

const MP4_TOLERANCE: Duration = Duration::from_micros(12);
/// Whole-millisecond block times: half a tick each for pts, a tick for
/// durations derived from them.
const MKV_TOLERANCE: Duration = Duration::from_millis(1);

#[test]
fn fragmented_mp4_round_trip() {
    let fixture = fixture();
    let (_dir, path) = temp("clip.mp4");
    mux(&fixture, &path, Container::Mp4 { fragmented: true }, true);
    let boxes = top_level_boxes(&path);
    assert_eq!(&boxes[..3], ["ftyp", "moov", "moof"]);
    // A fragment per keyframe interval, except that the init segment waits
    // for the audio (two seconds in): the five intervals until then share
    // the first fragment.
    assert_eq!(boxes.iter().filter(|b| *b == "moof").count(), 4);
    check(&fixture, &path, MP4_TOLERANCE);
    ffprobe(&path);
}

#[test]
fn classic_mp4_round_trip() {
    let fixture = fixture();
    let (dir, path) = temp("clip.mp4");
    mux(&fixture, &path, Container::Mp4 { fragmented: false }, true);
    assert_eq!(top_level_boxes(&path), ["ftyp", "moov", "mdat"]);
    // The rewrite's temporary file is gone.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    check(&fixture, &path, MP4_TOLERANCE);
    ffprobe(&path);
}

#[test]
fn mov_round_trip() {
    let fixture = fixture();
    let (_dir, path) = temp("clip.mov");
    mux(&fixture, &path, Container::Mov, true);
    assert_eq!(top_level_boxes(&path), ["ftyp", "moov", "mdat"]);
    assert_eq!(&std::fs::read(&path).unwrap()[8..12], b"qt  ");
    check(&fixture, &path, MP4_TOLERANCE);
    ffprobe(&path);
}

#[test]
fn mkv_round_trip() {
    let fixture = fixture();
    let (_dir, path) = temp("clip.mkv");
    mux(&fixture, &path, Container::Mkv, true);
    check(&fixture, &path, MKV_TOLERANCE);
    ffprobe(&path);
}

/// A crash mid-fragment loses only that fragment.
#[test]
fn truncated_fragmented_mp4_keeps_complete_fragments() {
    let fixture = fixture();
    let (_dir, path) = temp("crash.mp4");
    mux(&fixture, &path, Container::Mp4 { fragmented: true }, true);
    let data = std::fs::read(&path).unwrap();
    let last_moof = data.windows(4).rposition(|w| w == b"moof").unwrap() - 4;
    for cut in [data.len() - 100, last_moof + 20] {
        std::fs::write(&path, &data[..cut]).unwrap();
        let mut demuxer = open_demuxer(&path).unwrap().unwrap();
        let packets = read_all(demuxer.as_mut());
        let videos = packets.iter().filter(|p| p.track == 0).count();
        assert!(
            videos >= 90 && packets.len() < fixture.packets.len(),
            "{videos} frames, {} packets after cut at {cut}",
            packets.len()
        );
        // Whatever survives is intact.
        for read in &packets {
            let written = fixture
                .packets
                .iter()
                .find(|w| w.track == read.track && close(w.pts, read.pts, MP4_TOLERANCE))
                .unwrap();
            if read.track == 0 {
                assert_eq!(nals(&read.data), nals(&written.data));
            } else {
                assert_eq!(read.data, written.data);
            }
        }
        // Seeking still works within what survived.
        demuxer.seek(Duration::from_millis(1100)).unwrap();
        let video = std::iter::from_fn(|| demuxer.next_packet().unwrap())
            .find(|p| p.track == 0)
            .unwrap();
        assert!(close(
            video.pts,
            Duration::from_nanos(30 * FRAME_NS),
            MP4_TOLERANCE
        ));
    }
}

/// A recorder that dies without `finish` leaves playable files.
#[test]
fn unfinished_files_play() {
    let fixture = fixture();
    for (name, container) in [
        ("crash.mp4", Container::Mp4 { fragmented: true }),
        ("crash.mov", Container::Mov),
        ("crash.mkv", Container::Mkv),
    ] {
        let (_dir, path) = temp(name);
        mux(&fixture, &path, container, false);
        let mut demuxer = open_demuxer(&path).unwrap().unwrap();
        assert!(
            demuxer.duration() >= Duration::from_secs(3),
            "{name}: {:?}",
            demuxer.duration()
        );
        let packets = read_all(demuxer.as_mut());
        let videos = packets.iter().filter(|p| p.track == 0).count();
        // Only the last fragment or cluster is missing.
        assert!(videos >= 100, "{name}: {videos} frames");
        assert!(close(first_of(&packets, 1).pts, AUDIO_START, MKV_TOLERANCE));
        demuxer.seek(Duration::from_millis(2550)).unwrap();
        let video = std::iter::from_fn(|| demuxer.next_packet().unwrap())
            .find(|p| p.track == 0)
            .unwrap();
        assert!(video.keyframe);
        assert!(
            close(
                video.pts,
                Duration::from_nanos(75 * FRAME_NS),
                MKV_TOLERANCE
            ),
            "{name}: {:?}",
            video.pts
        );
    }
}

/// Files remuxed by FFmpeg: edit lists, CueRelativePosition (which
/// `matroska-demuxer` mishandles on its own) and FFmpeg's Opus timing.
#[test]
fn ffmpeg_remuxed_files() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let fixture = fixture();
    let (dir, source) = temp("source.mkv");
    mux(&fixture, &source, Container::Mkv, true);
    // FFmpeg's MP4 muxer starts the Opus edit at its first packet's
    // pre-skip-adjusted time, moving the sound 6.5 ms early.
    for (name, tolerance) in [
        ("remux.mp4", Duration::from_millis(7)),
        ("remux.mkv", MKV_TOLERANCE),
    ] {
        let path = dir.path().join(name);
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&source)
            .args(["-c", "copy"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut demuxer = open_demuxer(&path).unwrap().unwrap();
        let packets = read_all(demuxer.as_mut());
        assert_eq!(
            packets.iter().filter(|p| p.track == 0).count(),
            FRAMES as usize
        );
        assert_eq!(
            packets.iter().filter(|p| p.track == 1).count(),
            AUDIO_PACKETS as usize
        );
        assert_eq!(first_of(&packets, 0).pts, Duration::ZERO, "{name}");
        let audio = first_of(&packets, 1).pts;
        assert!(
            close(audio, AUDIO_START, tolerance),
            "{name}: audio at {audio:?}"
        );
        check_seek(demuxer.as_mut(), tolerance);
    }
}

#[test]
fn sniffs_content_not_extension() {
    let (dir, path) = temp("clip.mkv");
    std::fs::write(&path, b"GIF89a\x01\x00\x01\x00").unwrap();
    assert!(open_demuxer(&path).unwrap().is_none());
    let short = dir.path().join("short.mp4");
    std::fs::write(&short, b"\0\0").unwrap();
    assert!(open_demuxer(&short).unwrap().is_none());
    assert_eq!(sniff(b"\0\0\0\x20ftyp"), Some(Format::Mp4));
    assert_eq!(sniff(b"\0\0\0\x08wide"), Some(Format::Mp4));
    assert_eq!(
        sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81]),
        Some(Format::Matroska)
    );
}

/// The fixture with pictures 31..=44 missing (a static screen), as a
/// recorder that skips unchanged pictures writes it.
fn variable_rate(fixture: &Fixture) -> Fixture {
    Fixture {
        tracks: fixture.tracks.clone(),
        packets: fixture
            .packets
            .iter()
            .filter(|p| p.track != 0 || !(31..45).contains(&(p.pts.as_nanos() as u64 / FRAME_NS)))
            .cloned()
            .collect(),
    }
}

/// Skipped pictures do not lower the frame rate demuxers report.
#[test]
fn variable_rate_video_reports_its_nominal_rate() {
    let fixture = variable_rate(&fixture());
    for (name, container) in [
        ("vfr.mp4", Container::Mp4 { fragmented: true }),
        ("vfr.mov", Container::Mov),
        ("vfr.mkv", Container::Mkv),
    ] {
        let (_dir, path) = temp(name);
        mux(&fixture, &path, container, true);
        let demuxer = open_demuxer(&path).unwrap().unwrap();
        let TrackKind::Video { fps, .. } = demuxer.tracks()[0].kind else {
            panic!("{name}: video track expected");
        };
        assert!((fps - 30.0).abs() < 0.1, "{name}: {fps} fps");
    }
}

fn deltas(times: &[u64]) -> impl Iterator<Item = u64> + '_ {
    times.windows(2).map(|w| w[1] - w[0])
}

/// Frame times at `fps` in units of `1 / units`, rounded to whole units.
fn steady(fps: f64, units: f64, frames: u64) -> Vec<u64> {
    (0..frames)
        .map(|k| (k as f64 * units / fps).round() as u64)
        .collect()
}

#[test]
fn nominal_fps_of_jittered_present_times() {
    // 60 fps in 0.1 ms units, each picture presented up to 1.2 ms early or
    // late, so intervals run from 14 to 19 ms.
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let times: Vec<u64> = (0..300u64)
        .map(|k| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let jitter = (seed >> 33) % 25; // 0..=24
            (k * 10_000).div_ceil(60) + 1_000 + jitter - 12
        })
        .collect();
    let range = (deltas(&times).min().unwrap(), deltas(&times).max().unwrap());
    assert!(range.0 <= 145 && range.1 >= 185, "{range:?}");
    assert_eq!(nominal_fps(deltas(&times), 10_000.0), 60.0);

    // Intervals (ms) of a 60 fps Windows recording stamped with DXGI present
    // times: mostly 15 and 16 ms, late pictures at 23 ms, dropped ones at
    // 31 ms and more. The old mean of intervals below 1.5 times the shortest
    // reported 63.9.
    let recorded = [
        16, 15, 23, 31, 15, 16, 15, 23, 31, 16, 16, 16, 15, 16, 40, 31, 16, 15, 16, 15, 17, 23, 15,
        16, 15, 15, 16, 38, 16, 15, 16, 17, 15, 23, 31, 16, 15, 16, 16, 23, 33, 31, 16, 15, 16, 16,
        17, 23, 31, 17, 33, 15, 16, 15, 16, 16, 24, 16, 15, 16, 16, 15, 15, 23, 31, 16, 15, 16, 15,
        40, 15, 16, 16, 15, 15, 24, 16, 15, 16, 15, 16, 15, 39, 16, 15, 15, 16, 15, 39, 15, 15, 16,
        23, 31, 15, 16, 15, 15, 24, 16, 16, 16, 14, 16, 16, 23, 31, 14, 16, 16, 23, 15, 16, 15, 15,
        16, 17, 22, 17,
    ];
    assert_eq!(nominal_fps(recorded.into_iter(), 1_000.0), 60.0);
}

#[test]
fn nominal_fps_ignores_static_stretches_and_dropped_pictures() {
    // 60 fps on a 1 ms clock (16 and 17 ms intervals), with three static
    // stretches (2.5 s, 50 ms, 1.7 s) and every 37th picture dropped.
    let times: Vec<u64> = steady(60.0, 1_000.0, 900)
        .into_iter()
        .enumerate()
        .filter(|&(k, _)| {
            !(100..250).contains(&k) && !(300..303).contains(&k) && !(500..600).contains(&k)
        })
        .filter(|&(k, _)| k % 37 != 0)
        .map(|(_, t)| t)
        .collect();
    assert_eq!(nominal_fps(deltas(&times), 1_000.0), 60.0);
    // Static half the time: each second a burst of three 30 fps intervals,
    // then three slow updates.
    let times: Vec<u64> = (0..20u64)
        .flat_map(|second| [0, 33, 67, 100, 400, 700].map(|t| second * 1_000 + t))
        .collect();
    assert_eq!(nominal_fps(deltas(&times), 1_000.0), 30.0);
}

#[test]
fn nominal_fps_of_steady_rates() {
    for (fps, units, expected) in [
        (30.0, 1_000.0, 30.0),
        (24.0, 1_000.0, 24.0),
        (25.0, 1_000.0, 25.0),
        (144.0, 1_000.0, 144.0),
        (30_000.0 / 1001.0, 90_000.0, 30_000.0 / 1001.0),
        (24_000.0 / 1001.0, 1_000.0, 24_000.0 / 1001.0),
    ] {
        let times = steady(fps, units, 300);
        assert_eq!(nominal_fps(deltas(&times), units), expected, "{fps} fps");
    }
    // Uncommon rates are reported as measured.
    let fps = nominal_fps(deltas(&steady(75.0, 1e6, 300)), 1e6);
    assert!((fps - 75.0).abs() < 0.01, "{fps}");
    assert_eq!(nominal_fps(std::iter::empty(), 1_000.0), 0.0);
    assert_eq!(nominal_fps([0, 0].into_iter(), 1_000.0), 0.0);
}

/// After a seek past the end of an audio track, nothing of it comes back:
/// the seek must not reach back for audio that has ended (which re-read the
/// file from the start).
#[test]
fn seeking_past_an_early_audio_end() {
    let mut fixture = fixture();
    fixture
        .packets
        .retain(|p| p.track == 0 || p.pts < AUDIO_START + Duration::from_millis(500));
    for (name, container) in [
        ("short.mp4", Container::Mp4 { fragmented: true }),
        ("short.mkv", Container::Mkv),
    ] {
        let (_dir, path) = temp(name);
        mux(&fixture, &path, container, true);
        let mut demuxer = open_demuxer(&path).unwrap().unwrap();
        demuxer.seek(Duration::from_millis(3550)).unwrap();
        let after = read_all(demuxer.as_mut());
        assert!(after.iter().all(|p| p.track == 0), "{name}: stale audio");
        assert!(after[0].keyframe, "{name}");
        assert!(
            close(
                after[0].pts,
                Duration::from_nanos(105 * FRAME_NS),
                MKV_TOLERANCE
            ),
            "{name}: {:?}",
            after[0].pts
        );
    }
}

/// Opens `data` as a file and reads, seeks and reads again, failing the
/// test on a panic or if that takes longer than a few seconds (an allocation
/// the size of a forged count aborts the test binary instead).
fn survives(data: &[u8], what: &str) {
    let (_dir, path) = temp("hostile");
    std::fs::write(&path, data).unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let read = std::panic::catch_unwind(|| {
            if let Ok(Some(mut demuxer)) = open_demuxer(&path) {
                while let Ok(Some(_)) = demuxer.next_packet() {}
                let _ = demuxer.seek(Duration::from_millis(1500));
                while let Ok(Some(_)) = demuxer.next_packet() {}
            }
        });
        let _ = done.send(read.is_ok());
    });
    match finished.recv_timeout(Duration::from_secs(20)) {
        Ok(true) => {}
        Ok(false) => panic!("{what}: the demuxer panicked"),
        Err(_) => panic!("{what}: still reading after 20 s"),
    }
}

/// Re-encodes the `moov` and every `moof` of an MP4 after `edit`, keeping
/// the other boxes' bytes.
fn edit_boxes(data: &[u8], mut edit: impl FnMut(&mut mp4_atom::Any)) -> Vec<u8> {
    use mp4_atom::{Any, Decode, Encode};
    let mut reader = std::io::Cursor::new(data);
    let mut out = Vec::new();
    while (reader.position() as usize) < data.len() {
        let start = reader.position() as usize;
        let mut atom = Any::decode(&mut reader).unwrap();
        let end = reader.position() as usize;
        if matches!(atom, Any::Moov(_) | Any::Moof(_)) {
            edit(&mut atom);
            atom.encode(&mut out).unwrap();
        } else {
            out.extend_from_slice(&data[start..end]);
        }
    }
    out
}

/// Damaged or crafted MP4s with counts, sizes and offsets far beyond the
/// file fail or play what is real, quickly, without huge allocations.
#[test]
fn hostile_mp4s_fail_quickly() {
    use mp4_atom::{Any, StscEntry, StszSamples, SttsEntry, TrunSamples};
    let fixture = fixture();
    let (dir, classic) = temp("classic.mp4");
    mux(
        &fixture,
        &classic,
        Container::Mp4 { fragmented: false },
        true,
    );
    let classic = std::fs::read(&classic).unwrap();
    let fragmented = dir.path().join("fragmented.mp4");
    mux(
        &fixture,
        &fragmented,
        Container::Mp4 { fragmented: true },
        true,
    );
    let fragmented = std::fs::read(&fragmented).unwrap();

    survives(
        &edit_boxes(&classic, |atom| {
            if let Any::Moov(moov) = atom {
                let stbl = &mut moov.trak[0].mdia.minf.stbl;
                stbl.stsz.samples = StszSamples::Identical {
                    count: u32::MAX,
                    size: 1,
                };
                stbl.stsc.entries = vec![StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: u32::MAX,
                    sample_description_index: 1,
                }];
            }
        }),
        "stsz count of 2³²",
    );
    survives(
        &edit_boxes(&classic, |atom| {
            if let Any::Moov(moov) = atom {
                let stbl = &mut moov.trak[1].mdia.minf.stbl;
                stbl.stts.entries = vec![SttsEntry {
                    sample_count: u32::MAX,
                    sample_delta: u32::MAX,
                }];
                // Thousands of runs, every chunk at the end of the offsets.
                stbl.stsc.entries = (1..5000)
                    .map(|first_chunk| StscEntry {
                        first_chunk,
                        samples_per_chunk: 1,
                        sample_description_index: 1,
                    })
                    .collect();
                stbl.co64 = Some(mp4_atom::Co64 {
                    entries: vec![u64::MAX - 1; 5000],
                });
                stbl.stco = None;
            }
        }),
        "stts count of 2³², chunks at 2⁶⁴",
    );
    // A 64-bit box size that wraps the position back to an earlier box.
    let mut wrap = classic.clone();
    let ftyp = u32::from_be_bytes(classic[..4].try_into().unwrap()) as usize;
    let mut free = vec![0, 0, 0, 1, b'f', b'r', b'e', b'e'];
    free.extend_from_slice(&(u64::MAX - 7).to_be_bytes());
    wrap.splice(ftyp..ftyp, free);
    survives(&wrap, "wrapping box size");
    for size in [0, 1] {
        survives(
            &edit_boxes(&fragmented, |atom| {
                if let Any::Moof(moof) = atom {
                    for traf in &mut moof.traf {
                        traf.tfhd.default_sample_size = Some(size);
                        for trun in &mut traf.trun {
                            trun.samples = TrunSamples::Inherited {
                                count: u32::MAX,
                                first_flags: None,
                            };
                        }
                    }
                }
            }),
            &format!("trun count of 2³² with {size}-byte samples"),
        );
    }
}

/// Seeded byte mutations and truncations of every container we write:
/// never a panic, a hang or an abort.
#[test]
fn mutated_files_never_panic_or_hang() {
    let fixture = fixture();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for (name, container) in [
        ("fuzz.mp4", Container::Mp4 { fragmented: true }),
        ("fuzz.mov", Container::Mov),
        ("fuzz.mkv", Container::Mkv),
    ] {
        let (_dir, path) = temp(name);
        mux(&fixture, &path, container, true);
        let source = std::fs::read(&path).unwrap();
        for i in 0..150 {
            let mut data = source.clone();
            for _ in 0..1 + random() % 8 {
                let at = (random() % data.len() as u64) as usize;
                data[at] = random() as u8;
            }
            survives(&data, &format!("{name}: mutation {i}"));
        }
        for i in 0..40 {
            survives(
                &source[..source.len() * i / 40],
                &format!("{name}: cut at {i}/40"),
            );
        }
    }
}

/// Third-party AAC: the encoder's priming, which the MP4 edit list and the
/// Matroska CodecDelay hide, does not play before the picture.
#[test]
fn aac_priming_is_hidden() {
    let encoders = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    if !encoders.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("libx264")) {
        eprintln!("skipping: no ffmpeg CLI with libx264");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for name in ["aac.mp4", "aac.mkv"] {
        let path = dir.path().join(name);
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=160x120:rate=30")
            .args(["-f", "lavfi", "-i", "sine=f=440:r=48000", "-t", "2"])
            .args(["-c:v", "libx264", "-c:a", "aac"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a", "-show_entries"])
            .args(["packet=pts", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .unwrap();
        let stored = String::from_utf8_lossy(&probe.stdout).lines().count();

        let mut demuxer = open_demuxer(&path).unwrap().unwrap();
        let audio = demuxer
            .tracks()
            .iter()
            .position(|t| t.codec == Codec::Aac)
            .unwrap();
        let packets = read_all(demuxer.as_mut());
        assert_eq!(
            first_of(&packets, 1 - audio).pts,
            Duration::ZERO,
            "{name}: picture"
        );
        let sound: Vec<Duration> = packets
            .iter()
            .filter(|p| p.track == audio)
            .map(|p| p.pts)
            .collect();
        // One priming frame (1024 samples) is dropped; the first real one
        // starts with the picture.
        assert_eq!(sound.len(), stored - 1, "{name}: AAC packets");
        assert!(
            sound[0] < Duration::from_millis(1),
            "{name}: {:?}",
            sound[0]
        );
        assert!(sound.windows(2).all(|w| w[1] > w[0]), "{name}: AAC times");
    }
}
