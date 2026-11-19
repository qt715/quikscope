//! Streaming Matroska writer: header and tracks once the first video
//! keyframe is known, then whole clusters as they complete, then cues and
//! sizes on finish.

use std::fs::File;
use std::io::{BufWriter, Seek as _, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use mkv_element::io::blocking_impl::WriteTo;
use mkv_element::prelude::{
    Audio, Channels, CodecDelay, CodecId, CodecPrivate, CueClusterPosition, CuePoint, CueTime,
    CueTrack, CueTrackPositions, Cues, DocType, DocTypeReadVersion, DocTypeVersion, Ebml,
    EbmlMaxIdLength, EbmlMaxSizeLength, EbmlReadVersion, EbmlVersion, FlagDefault, FlagEnabled,
    FlagInterlaced, FlagLacing, Header, Info, Language, MuxingApp, PixelHeight, PixelWidth,
    SamplingFrequency, Seek, SeekHead, SeekId, SeekPosition, SeekPreRoll, SimpleBlock, Timestamp,
    TimestampScale, TrackEntry, TrackNumber, TrackType, TrackUid, Tracks, VInt64, Video,
    WritingApp,
};
use mkv_element::prelude::{Cluster, Element};
use mp4_atom::{Atom, Avcc};

use super::super::{Codec, Packet, Track, TrackKind, nal};
use super::{AVC, OPUS, TIMESTAMP_SCALE};
use crate::media::codec::opus::{Head, RATE};

/// Bytes kept at the segment start for the final SeekHead.
const SEEK_HEAD_SLOT: usize = 96;
/// Room for Info to grow when finish adds its duration (an 11-byte element
/// plus the 9-byte minimum of the Void that pads the slot).
const INFO_SPARE: usize = 24;
/// Cluster length when no video keyframes set the pace.
const AUDIO_CLUSTER: Duration = Duration::from_secs(1);
/// Longest cluster, for long keyframe intervals: bounds what a crash loses.
const MAX_CLUSTER: Duration = Duration::from_secs(4);
/// Media time between `sync_data` calls: written clusters survive a power
/// loss, not only a crash, at the cost of one flush to disk this often.
const SYNC_INTERVAL: Duration = Duration::from_secs(5);
/// Opus decoders need this much audio before a seek point (RFC 7845 §4.6).
const OPUS_SEEK_PRE_ROLL: u64 = 80_000_000;
const SEGMENT_ID: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
const INFO_ID: [u8; 4] = [0x15, 0x49, 0xA9, 0x66];
const TRACKS_ID: [u8; 4] = [0x16, 0x54, 0xAE, 0x6B];
const CUES_ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];

/// Writes H.264 and Opus tracks to Matroska. The segment's size is unknown
/// until finish, and only whole clusters reach the file, so a crash leaves a
/// playable file missing at most the last cluster.
pub struct MkvMuxer {
    out: BufWriter<File>,
    pos: u64,
    tracks: Vec<MkvTrack>,
    has_video: bool,
    /// Packets before the header is written (see [`MkvMuxer::write`]).
    pending: Vec<Packet>,
    /// pts written as time 0, fixed when the header is written.
    origin: Option<Duration>,
    segment_data: u64,
    info_pos: u64,
    info_slot: usize,
    cluster: Option<ClusterBuf>,
    cues: Vec<(u64, u64, u64)>,
    /// Latest presentation end (ns from origin).
    end: u64,
    /// pts up to which written clusters were last synced to disk.
    synced: Duration,
}

struct MkvTrack {
    track: Track,
    avcc: Option<Avcc>,
    last_pts: Option<Duration>,
}

struct ClusterBuf {
    timestamp: u64,
    first_pts: Duration,
    /// Track number of the keyframe that starts it, for a cue.
    cue: Option<u64>,
    body: Vec<u8>,
}

impl MkvMuxer {
    pub fn create(path: &Path, tracks: &[Track]) -> Result<Self> {
        ensure!(
            !tracks.is_empty(),
            "A Matroska file needs at least one track"
        );
        ensure!(tracks.len() < 127, "Too many tracks");
        for track in tracks {
            match (track.codec, track.kind) {
                (Codec::H264, TrackKind::Video { .. }) => {}
                (Codec::Opus, TrackKind::Audio { .. }) => {
                    Head::parse(&track.codec_private)?;
                }
                (codec, kind) => bail!("Cannot write {codec:?} as {kind:?} to Matroska"),
            }
        }
        let file = File::create(path).with_context(|| format!("Creating {}", path.display()))?;
        Ok(MkvMuxer {
            out: BufWriter::with_capacity(1 << 20, file),
            pos: 0,
            has_video: tracks.iter().any(|t| t.codec == Codec::H264),
            tracks: tracks
                .iter()
                .map(|track| MkvTrack {
                    track: track.clone(),
                    avcc: None,
                    last_pts: None,
                })
                .collect(),
            pending: Vec::new(),
            origin: None,
            segment_data: 0,
            info_pos: 0,
            info_slot: 0,
            cluster: None,
            cues: Vec::new(),
            end: 0,
            synced: Duration::ZERO,
        })
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// EBML header, segment start, SeekHead slot, Info and Tracks; then the
    /// packets held back so far.
    fn start(&mut self) -> Result<()> {
        self.origin = self.pending.iter().map(|p| p.pts).min();
        self.emit(&element(&Ebml {
            ebml_version: Some(EbmlVersion(1)),
            ebml_read_version: Some(EbmlReadVersion(1)),
            ebml_max_id_length: EbmlMaxIdLength(4),
            ebml_max_size_length: EbmlMaxSizeLength(8),
            doc_type: Some(DocType("matroska".into())),
            doc_type_version: Some(DocTypeVersion(4)),
            doc_type_read_version: Some(DocTypeReadVersion(2)),
            ..Default::default()
        })?)?;
        // An all-ones 8-byte size means "unknown" until finish patches it.
        self.emit(&SEGMENT_ID)?;
        self.emit(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
        self.segment_data = self.pos;

        // No duration until finish: a crashed file must not claim zero.
        let info = element(&info(None))?;
        self.info_slot = info.len() + INFO_SPARE;
        let info_rel = SEEK_HEAD_SLOT as u64;
        let tracks_rel = info_rel + self.info_slot as u64;
        let seek_head = seek_head(&[(INFO_ID, info_rel), (TRACKS_ID, tracks_rel)])?;
        self.emit(&fill(seek_head, SEEK_HEAD_SLOT)?)?;
        self.info_pos = self.pos;
        let slot = self.info_slot;
        self.emit(&fill(info, slot)?)?;
        let entries = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| track_entry(i as u64 + 1, t))
            .collect::<Result<Vec<_>>>()?;
        self.emit(&element(&Tracks {
            track_entry: entries,
            ..Default::default()
        })?)?;

        for packet in std::mem::take(&mut self.pending) {
            self.add(packet)?;
        }
        Ok(())
    }

    /// Appends a packet to the current cluster, closing it first at a video
    /// keyframe or when it grew too long.
    fn add(&mut self, packet: Packet) -> Result<()> {
        let origin = self.origin.unwrap_or_default();
        if packet.pts < origin {
            log::warn!(
                "Dropping a track {} packet from before the file start",
                packet.track
            );
            return Ok(());
        }
        let track = &self.tracks[packet.track];
        let video = track.track.codec == Codec::H264;
        // Every Opus packet decodes on its own (as in the MP4 muxer).
        let keyframe = packet.keyframe || !video;
        let data = match &track.avcc {
            Some(avcc) => {
                let mut sample = Vec::new();
                nal::annexb_to_sample(&packet.data, avcc, &mut sample)?;
                sample
            }
            None => packet.data,
        };
        let ms = ((packet.pts - origin).as_nanos() as u64 + TIMESTAMP_SCALE / 2) / TIMESTAMP_SCALE;
        let new_cluster = match &self.cluster {
            None => true,
            Some(cluster) => {
                let span = packet.pts.saturating_sub(cluster.first_pts);
                let relative = ms as i64 - cluster.timestamp as i64;
                (video && keyframe)
                    || span >= MAX_CLUSTER
                    || (!self.has_video && span >= AUDIO_CLUSTER)
                    || i16::try_from(relative).is_err()
            }
        };
        let number = packet.track as u64 + 1;
        if new_cluster {
            self.flush_cluster()?;
            let mut body = Vec::new();
            Timestamp(ms).write_to(&mut body)?;
            self.cluster = Some(ClusterBuf {
                timestamp: ms,
                first_pts: packet.pts,
                cue: (keyframe && (video || !self.has_video)).then_some(number),
                body,
            });
        }
        let cluster = self.cluster.as_mut().expect("created above");
        let relative = (ms as i64 - cluster.timestamp as i64) as i16;
        Header {
            id: SimpleBlock::ID,
            size: VInt64::new(4 + data.len() as u64),
        }
        .write_to(&mut cluster.body)?;
        // Track number as a 1-byte vint, time, flags (keyframe), no lacing.
        cluster.body.push(0x80 | number as u8);
        cluster.body.extend_from_slice(&relative.to_be_bytes());
        cluster.body.push(if keyframe { 0x80 } else { 0 });
        cluster.body.extend_from_slice(&data);

        let duration = packet
            .duration
            .unwrap_or_else(|| self.tracks[packet.track].frame_duration());
        let end = (packet.pts + duration - origin).as_nanos() as u64;
        self.end = self.end.max(end);
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<()> {
        let Some(cluster) = self.cluster.take() else {
            return Ok(());
        };
        if let Some(track) = cluster.cue {
            self.cues
                .push((cluster.timestamp, track, self.pos - self.segment_data));
        }
        let mut head = Vec::new();
        Header {
            id: Cluster::ID,
            size: VInt64::new(cluster.body.len() as u64),
        }
        .write_to(&mut head)?;
        self.emit(&head)?;
        self.emit(&cluster.body)?;
        self.out.flush()?;
        if cluster.first_pts.saturating_sub(self.synced) >= SYNC_INTERVAL {
            self.synced = cluster.first_pts;
            if let Err(e) = self.out.get_ref().sync_data() {
                log::warn!("Syncing a Matroska file to disk failed: {e}");
            }
        }
        Ok(())
    }
}

impl MkvTrack {
    /// Fallback length of a packet without a duration.
    fn frame_duration(&self) -> Duration {
        match self.track.kind {
            TrackKind::Video { fps, .. } if fps > 0.0 => Duration::from_secs_f64(1.0 / fps),
            TrackKind::Video { .. } => Duration::from_millis(33),
            TrackKind::Audio { .. } => Duration::from_millis(20),
        }
    }
}

impl super::super::Muxer for MkvMuxer {
    fn write(&mut self, packet: Packet) -> Result<()> {
        let index = packet.track;
        let track = self
            .tracks
            .get_mut(index)
            .with_context(|| format!("No track {index}"))?;
        ensure!(
            track.last_pts.is_none_or(|last| packet.pts > last),
            "Track {index}: pts {:?} does not increase",
            packet.pts
        );
        if track.track.codec == Codec::H264 && track.avcc.is_none() {
            if !packet.keyframe {
                log::debug!("Dropping H.264 frames before the first keyframe");
                return Ok(());
            }
            track.avcc = Some(nal::avcc_from_keyframe(&packet.data)?);
        }
        track.last_pts = Some(packet.pts);
        if self.origin.is_some() {
            return self.add(packet);
        }
        // Hold everything for a moment so audio that arrives a little after
        // the first keyframe can still start the file.
        self.pending.push(packet);
        let ready = self
            .tracks
            .iter()
            .all(|t| t.track.codec != Codec::H264 || t.avcc.is_some());
        let first = self.pending.iter().map(|p| p.pts).min().unwrap_or_default();
        let last = self.pending.iter().map(|p| p.pts).max().unwrap_or_default();
        if ready && last - first >= AUDIO_CLUSTER {
            self.start()?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        if self.origin.is_none() {
            ensure!(!self.pending.is_empty(), "No packets were written");
            ensure!(
                self.tracks
                    .iter()
                    .all(|t| t.track.codec != Codec::H264 || t.avcc.is_some()),
                "No H.264 keyframe was written"
            );
            self.start()?;
        }
        self.flush_cluster()?;

        let cues_rel = self.pos - self.segment_data;
        let mut seeks = vec![
            (INFO_ID, SEEK_HEAD_SLOT as u64),
            (TRACKS_ID, (SEEK_HEAD_SLOT + self.info_slot) as u64),
        ];
        if !self.cues.is_empty() {
            let cues = Cues {
                cue_point: self
                    .cues
                    .iter()
                    .map(|&(time, track, position)| CuePoint {
                        cue_time: CueTime(time),
                        cue_track_positions: vec![CueTrackPositions {
                            cue_track: CueTrack(track),
                            cue_cluster_position: CueClusterPosition(position),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            self.emit(&element(&cues)?)?;
            seeks.push((CUES_ID, cues_rel));
        }
        let segment_size = self.pos - self.segment_data;

        self.out.seek(SeekFrom::Start(self.segment_data))?;
        self.out
            .write_all(&fill(seek_head(&seeks)?, SEEK_HEAD_SLOT)?)?;
        self.out.seek(SeekFrom::Start(self.info_pos))?;
        let info = element(&info(Some(self.end as f64 / TIMESTAMP_SCALE as f64)))?;
        self.out.write_all(&fill(info, self.info_slot)?)?;
        self.out.seek(SeekFrom::Start(self.segment_data - 8))?;
        let mut size = segment_size.to_be_bytes();
        size[0] = 0x01;
        self.out.write_all(&size)?;
        self.out.flush()?;
        Ok(())
    }
}

fn element(e: &impl WriteTo) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    e.write_to(&mut buf)?;
    Ok(buf)
}

/// `bytes` padded to exactly `slot` bytes with a Void element (8-byte size,
/// so its header length never depends on the padding).
fn fill(mut bytes: Vec<u8>, slot: usize) -> Result<Vec<u8>> {
    let spare = slot
        .checked_sub(bytes.len())
        .filter(|&s| s == 0 || s >= 9)
        .context("Matroska header element outgrew its reserved space")?;
    if spare > 0 {
        let mut size = (spare as u64 - 9).to_be_bytes();
        size[0] = 0x01;
        bytes.push(0xEC);
        bytes.extend_from_slice(&size);
        bytes.resize(slot, 0);
    }
    Ok(bytes)
}

fn info(duration_ticks: Option<f64>) -> Info {
    Info {
        timestamp_scale: TimestampScale(TIMESTAMP_SCALE),
        duration: duration_ticks.map(mkv_element::prelude::Duration),
        muxing_app: MuxingApp("quikscope".into()),
        writing_app: WritingApp(concat!("quikscope ", env!("CARGO_PKG_VERSION")).into()),
        ..Default::default()
    }
}

fn seek_head(entries: &[([u8; 4], u64)]) -> Result<Vec<u8>> {
    element(&SeekHead {
        seek: entries
            .iter()
            .map(|(id, position)| Seek {
                crc32: None,
                void: None,
                seek_id: SeekId(id.to_vec().into()),
                seek_position: SeekPosition(*position),
            })
            .collect(),
        ..Default::default()
    })
}

fn track_entry(number: u64, track: &MkvTrack) -> Result<TrackEntry> {
    let mut entry = TrackEntry {
        track_number: TrackNumber(number),
        track_uid: TrackUid(number),
        flag_enabled: FlagEnabled(1),
        flag_default: FlagDefault(1),
        flag_lacing: FlagLacing(0),
        language: Language("und".into()),
        ..Default::default()
    };
    match track.track.kind {
        // No DefaultDuration: recordings skip pictures on a static screen,
        // and players would take a nominal rate for the real timing.
        TrackKind::Video { width, height, .. } => {
            let avcc = track.avcc.as_ref().context("H.264 track without avcC")?;
            let mut private = Vec::new();
            avcc.encode_body(&mut private)?;
            entry.track_type = TrackType(1);
            entry.codec_id = CodecId(AVC.into());
            entry.codec_private = Some(CodecPrivate(private.into()));
            entry.video = Some(Video {
                pixel_width: PixelWidth(u64::from(width)),
                pixel_height: PixelHeight(u64::from(height)),
                // Progressive.
                flag_interlaced: FlagInterlaced(2),
                ..Default::default()
            });
        }
        TrackKind::Audio { channels, .. } => {
            let head = Head::parse(&track.track.codec_private)?;
            entry.track_type = TrackType(2);
            entry.codec_id = CodecId(OPUS.into());
            entry.codec_private = Some(CodecPrivate(track.track.codec_private.clone().into()));
            entry.codec_delay =
                CodecDelay(u64::from(head.pre_skip) * 1_000_000_000 / u64::from(RATE));
            entry.seek_pre_roll = SeekPreRoll(OPUS_SEEK_PRE_ROLL);
            entry.audio = Some(Audio {
                sampling_frequency: SamplingFrequency(f64::from(RATE)),
                channels: Channels(u64::from(channels)),
                ..Default::default()
            });
        }
    }
    Ok(entry)
}
