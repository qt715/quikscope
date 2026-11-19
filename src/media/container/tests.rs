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

/// When `ffprobe` is installed, it must find both streams.
fn ffprobe(path: &Path) {
    let Ok(output) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
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
        stdout.contains("h264") && stdout.contains("opus"),
        "ffprobe streams: {stdout}"
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
    // One fragment per keyframe interval.
    assert!(boxes.iter().filter(|b| *b == "moof").count() >= (FRAMES / GOP) as usize);
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
