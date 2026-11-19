//! Matroska reading on `matroska-demuxer`, with our own cue and cluster
//! scanning around what it lacks: packet durations, a shared origin,
//! keyframe-accurate seeking and a fix for its relative-cue bug.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use matroska_demuxer::{Frame, MatroskaFile, TrackEntry, TrackType};
use mp4_atom::{Atom, Avcc};

use super::super::{Codec, Packet, Track, TrackKind, nal, nanos, nominal_fps};
use super::ebml::{self, Element};
use super::{AAC_PREFIX, AV1, AVC, OPUS};

/// Packets waiting for their successor's time as duration; past this many
/// the oldest goes out with whatever duration the file states.
const DURATION_LOOKAHEAD: usize = 256;
/// How far apart in time tracks may be interleaved in the file.
const MAX_INTERLEAVE: u64 = 4_000_000_000;
/// First guess of how far before a seek target to start reading.
const SEEK_PREROLL: u64 = 500_000_000;
/// Clusters at the start whose video block times give the frame rate.
const RATE_CLUSTERS: usize = 8;

type Reader = MatroskaFile<CueFix<BufReader<File>>>;

/// Reads H.264, Opus, AAC and AV1 tracks of Matroska/WebM files, including
/// unfinished ones (unknown sizes, no cues or duration).
pub struct MkvDemuxer {
    path: PathBuf,
    file: Reader,
    scan: Scan,
    tracks: Vec<Track>,
    info: Vec<TrackInfo>,
    by_number: HashMap<u64, usize>,
    /// Nanoseconds per tick.
    scale: u64,
    /// Time (ns) every reported pts is relative to.
    origin: u64,
    duration: Duration,
    frame: Frame,
    /// Packets from a seek's look-ahead, read before the file.
    replay: VecDeque<Raw>,
    /// Packets waiting for durations, in file order.
    queue: VecDeque<Raw>,
    /// Sequence number of `queue[0]`.
    head: u64,
    /// Per track, the sequence number of its queued packet still lacking a
    /// duration.
    open: Vec<Option<u64>>,
    /// Per track, encoder priming (ns) still to drop: CodecDelay, while
    /// reading from the start of the file.
    priming: Vec<u64>,
    eof: bool,
}

struct TrackInfo {
    video: bool,
    codec: Codec,
    /// `avcC` length size and parameter sets for H.264.
    h264: Option<(usize, Vec<u8>)>,
    default_duration: Option<u64>,
    /// CodecDelay (ns) of AAC tracks, whose decoder does not trim priming
    /// itself: subtracted from block times, and the packets it mostly
    /// covers are dropped. Opus keeps it (see the module docs of
    /// [`super::super`]).
    codec_delay: u64,
    /// Length (ns) of one AAC frame, for priming without block durations.
    aac_frame: Option<u64>,
}

struct Raw {
    track: usize,
    /// ns, file timeline.
    time: u64,
    data: Vec<u8>,
    keyframe: bool,
    /// From the next packet of the track.
    duration: Option<u64>,
    /// From a BlockDuration.
    block_duration: Option<u64>,
}

/// What a raw scan of the file found.
struct Scan {
    /// Cue times (ticks), ascending.
    cues: Vec<u64>,
    /// Positions of CueRelativePosition IDs to hide (see [`CueFix`]).
    patches: Vec<u64>,
    first_cluster: Option<Element>,
    /// Timestamp of the first cluster (ticks).
    first_cluster_time: Option<u64>,
    /// Earliest block time in the first cluster (ticks).
    first_block: Option<u64>,
    /// (track number, time in ticks) of the blocks of the first few
    /// clusters.
    early_blocks: Vec<(u64, u64)>,
    file_len: u64,
}

impl MkvDemuxer {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_inner(path).with_context(|| format!("Reading {}", path.display()))
    }

    fn open_inner(path: &Path) -> Result<Self> {
        let scan = scan(&mut BufReader::new(File::open(path)?))?;
        let file = open_file(path, &scan.patches)?;
        let scale = file.info().timestamp_scale().get();

        let mut tracks = Vec::new();
        let mut info = Vec::new();
        let mut by_number = HashMap::new();
        for entry in file.tracks() {
            match read_track(entry) {
                Ok(Some((track, track_info))) => {
                    by_number.insert(entry.track_number().get(), tracks.len());
                    tracks.push(track);
                    info.push(track_info);
                }
                Ok(None) => {}
                Err(e) => log::warn!("Skipping Matroska track {}: {e:#}", entry.track_number()),
            }
        }

        // Nominal rates from the block times: files we write carry no
        // DefaultDuration for (variable-rate) video.
        for (&number, &index) in &by_number {
            if let TrackKind::Video { fps, .. } = &mut tracks[index].kind {
                let mut times: Vec<u64> = scan
                    .early_blocks
                    .iter()
                    .filter(|&&(track, _)| track == number)
                    .map(|&(_, time)| time)
                    .collect();
                // Presentation times, which B-frames put out of order.
                times.sort_unstable();
                let rate = nominal_fps(times.windows(2).map(|w| w[1] - w[0]), 1e9 / scale as f64);
                if rate > 0.0 {
                    *fps = rate;
                }
            }
        }
        let origin = scan.first_block.unwrap_or(0).saturating_mul(scale);
        let end = match file.info().duration() {
            Some(ticks) => (ticks * scale as f64) as u64,
            None => last_block_end(path, &scan, &by_number, &info, scale).unwrap_or(origin),
        };
        let priming = info.iter().map(|i| i.codec_delay).collect();
        Ok(MkvDemuxer {
            path: path.to_owned(),
            file,
            scan,
            open: vec![None; tracks.len()],
            priming,
            tracks,
            info,
            by_number,
            scale,
            origin,
            duration: nanos(i128::from(end.saturating_sub(origin))),
            frame: Frame::default(),
            replay: VecDeque::new(),
            queue: VecDeque::new(),
            head: 0,
            eof: false,
        })
    }

    /// The next frame of a track we read, straight from the file.
    fn read_raw(&mut self) -> Result<Option<Raw>> {
        loop {
            match self.file.next_frame(&mut self.frame) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) => {
                    // A crash can cut the last block short.
                    log::warn!("Matroska read stopped: {e}");
                    return Ok(None);
                }
            }
            let Some(&track) = self.by_number.get(&self.frame.track) else {
                continue;
            };
            let info = &self.info[track];
            let block_duration = self.frame.duration.map(|d| d.saturating_mul(self.scale));
            let priming = &mut self.priming[track];
            if *priming > 0 {
                let length = block_duration.or(info.default_duration).or(info.aac_frame);
                let hidden = length.map_or(0, |length| (*priming).min(length));
                *priming = length.map_or(0, |length| priming.saturating_sub(length));
                // Mostly priming: playing it would put sound early.
                if length.is_some_and(|length| hidden > length - hidden) {
                    continue;
                }
            }
            let data = std::mem::take(&mut self.frame.data);
            let keyframe = self.frame.is_keyframe.unwrap_or_else(|| match info.codec {
                Codec::H264 => info
                    .h264
                    .as_ref()
                    .is_some_and(|(len, _)| nal::sample_is_idr(&data, *len)),
                Codec::Av1 => av1_has_sequence_header(&data),
                Codec::Opus | Codec::Aac => true,
            });
            return Ok(Some(Raw {
                track,
                // Matroska: CodecDelay is subtracted from every block time.
                time: self
                    .frame
                    .timestamp
                    .saturating_mul(self.scale)
                    .saturating_sub(info.codec_delay),
                data,
                keyframe,
                duration: None,
                block_duration,
            }));
        }
    }

    fn pull(&mut self) -> Result<Option<Raw>> {
        match self.replay.pop_front() {
            Some(raw) => Ok(Some(raw)),
            None => self.read_raw(),
        }
    }

    fn enqueue(&mut self, raw: Raw) {
        if let Some(seq) = self.open[raw.track] {
            let previous = &mut self.queue[(seq - self.head) as usize];
            previous.duration = Some(raw.time.saturating_sub(previous.time));
        }
        self.open[raw.track] = Some(self.head + self.queue.len() as u64);
        self.queue.push_back(raw);
    }

    fn packet(&self, raw: Raw) -> Packet {
        let info = &self.info[raw.track];
        let duration = raw
            .duration
            .or(raw.block_duration)
            .or(info.default_duration)
            .or_else(|| {
                (info.codec == Codec::Opus)
                    .then(|| opus::packet::get_nb_samples(&raw.data, 48_000).ok())
                    .flatten()
                    .map(|samples| samples as u64 * 1_000_000_000 / 48_000)
            })
            .or(info.aac_frame)
            .or_else(|| match self.tracks[raw.track].kind {
                // The last picture of a file without DefaultDuration.
                TrackKind::Video { fps, .. } if fps > 0.0 => Some((1e9 / fps).round() as u64),
                _ => None,
            });
        let data = match &info.h264 {
            Some((length_size, params)) => {
                nal::sample_to_annexb(&raw.data, *length_size, raw.keyframe, params)
            }
            None => raw.data,
        };
        Packet {
            track: raw.track,
            data,
            pts: nanos(i128::from(raw.time) - i128::from(self.origin)),
            duration: duration.map(|d| nanos(i128::from(d))),
            keyframe: raw.keyframe,
        }
    }

    /// Positions the reader at or before `time` (ns): through a cue, by the
    /// demuxer's linear cluster search, or by reopening the file. Returns
    /// whether it reopened (nothing earlier exists).
    fn position(&mut self, time: u64) -> Result<bool> {
        self.replay.clear();
        self.queue.clear();
        self.open.iter_mut().for_each(|o| *o = None);
        self.eof = false;
        let ticks = time / self.scale;
        let cue = self.scan.cues.iter().rev().find(|&&c| c <= ticks).copied();
        // Its linear search misreads a target before the first cluster, so
        // only use it past that.
        let linear = self.scan.cues.is_empty()
            && self
                .scan
                .first_cluster_time
                .is_some_and(|first| ticks > first);
        let position = cue.or(linear.then_some(ticks)).filter(|_| time > 0);
        // Priming only precedes a track's first packets.
        let from_first_cluster = position.is_none_or(|ticks| {
            self.scan
                .first_cluster_time
                .is_none_or(|first| ticks <= first)
        });
        for (priming, info) in self.priming.iter_mut().zip(&self.info) {
            *priming = if from_first_cluster {
                info.codec_delay
            } else {
                0
            };
        }
        match position {
            Some(ticks) => {
                self.file.seek(ticks)?;
                Ok(false)
            }
            None => {
                self.file = open_file(&self.path, &self.scan.patches)?;
                Ok(true)
            }
        }
    }

    /// Reads from the current position until every track is past `target`
    /// (ns), keeping per video track the packets from the last keyframe at
    /// or before it and per audio track from the last packet starting at or
    /// before it. `None` when the position was too late for that.
    fn collect(&mut self, target: u64, from_start: bool) -> Result<Option<VecDeque<Raw>>> {
        let count = self.tracks.len();
        let mut keyed = vec![false; count];
        let mut done = vec![false; count];
        let mut seen = vec![false; count];
        let mut too_late = false;
        let mut kept: Vec<Raw> = Vec::new();
        while let Some(raw) = self.read_raw()? {
            let track = raw.track;
            let past = raw.time > target;
            seen[track] = true;
            if done[track] {
                // Already past the target: keep everything.
            } else if self.info[track].video {
                if raw.keyframe {
                    if !past {
                        kept.retain(|p| p.track != track);
                    }
                    keyed[track] = true;
                    done[track] = past;
                } else if !keyed[track] {
                    // Mid-GOP: undecodable without an earlier keyframe.
                    too_late = true;
                    continue;
                }
            } else if past {
                done[track] = true;
            } else {
                kept.retain(|p| p.track != track);
            }
            let beyond = raw.time > target.saturating_add(MAX_INTERLEAVE);
            kept.push(raw);
            if beyond || done.iter().all(|&d| d) {
                break;
            }
        }
        // An audio track without packets near the target (one that ended
        // early, or a gap) has nothing to resume from: reading further back
        // for it would only re-read the file, on every seek. Video still
        // needs its last keyframe, however far back.
        let complete = !too_late
            && seen.iter().any(|&s| s)
            && (0..count).all(|t| seen[t] || !self.info[t].video);
        Ok((complete || from_start).then(|| kept.into()))
    }
}

impl super::super::Demuxer for MkvDemuxer {
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn duration(&self) -> Duration {
        self.duration
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let flush = self.eof || self.queue.len() > DURATION_LOOKAHEAD;
            if let Some(raw) = self
                .queue
                .pop_front_if(|front| flush || front.duration.is_some())
            {
                if self.open[raw.track] == Some(self.head) {
                    self.open[raw.track] = None;
                }
                self.head += 1;
                return Ok(Some(self.packet(raw)));
            }
            if self.eof {
                return Ok(None);
            }
            match self.pull()? {
                Some(raw) => self.enqueue(raw),
                None => self.eof = true,
            }
        }
    }

    fn seek(&mut self, to: Duration) -> Result<()> {
        // Block times are whole ticks; a target computed from the original
        // times may sit a rounding error before the stored one.
        let target = self
            .origin
            .saturating_add(u64::try_from(to.as_nanos()).unwrap_or(u64::MAX))
            .saturating_add(self.scale / 2);
        let mut preroll = SEEK_PREROLL;
        loop {
            let from_start = self.position(target.saturating_sub(preroll))?;
            if let Some(kept) = self.collect(target, from_start)? {
                self.replay = kept;
                return Ok(());
            }
            preroll = preroll.saturating_mul(4);
        }
    }
}

fn open_file(path: &Path, patches: &[u64]) -> Result<Reader> {
    let reader = CueFix {
        inner: BufReader::new(File::open(path)?),
        pos: 0,
        patches: patches.to_vec(),
    };
    Ok(MatroskaFile::open(reader)?)
}

fn read_track(entry: &TrackEntry) -> Result<Option<(Track, TrackInfo)>> {
    if entry.content_encodings().is_some_and(|e| !e.is_empty()) {
        anyhow::bail!("compressed or encrypted blocks are not supported");
    }
    // EBML strings may carry zero padding (mkv-element writes a terminator).
    let id = entry.codec_id().trim_end_matches('\0');
    let private = entry.codec_private().unwrap_or_default().to_vec();
    let video = entry.track_type() == TrackType::Video;
    let codec = match id {
        AVC => Codec::H264,
        AV1 => Codec::Av1,
        OPUS => Codec::Opus,
        _ if id.starts_with(AAC_PREFIX) && !private.is_empty() => Codec::Aac,
        _ => {
            log::debug!("Skipping a Matroska track of {id}");
            return Ok(None);
        }
    };
    let default_duration = entry.default_duration().map(|d| d.get());
    let kind = if video {
        let v = entry
            .video()
            .context("video track without video settings")?;
        TrackKind::Video {
            width: u32::try_from(v.pixel_width().get())?,
            height: u32::try_from(v.pixel_height().get())?,
            fps: default_duration.map_or(0.0, |d| 1e9 / d as f64),
        }
    } else {
        let a = entry
            .audio()
            .context("audio track without audio settings")?;
        TrackKind::Audio {
            rate: a.sampling_frequency() as u32,
            channels: u16::try_from(a.channels().get())?,
        }
    };
    let h264 = if codec == Codec::H264 {
        let avcc = Avcc::decode_body(&mut private.as_slice())?;
        Some((usize::from(avcc.length_size), nal::parameter_sets(&avcc)))
    } else {
        None
    };
    let aac_frame = match kind {
        TrackKind::Audio { rate, .. } if codec == Codec::Aac && rate > 0 => {
            Some(1024 * 1_000_000_000 / u64::from(rate))
        }
        _ => None,
    };
    let track = Track {
        codec,
        kind,
        codec_private: private,
    };
    let info = TrackInfo {
        video,
        codec,
        h264,
        default_duration,
        // The Opus decoder trims its pre-skip, which CodecDelay repeats;
        // video packets are references for later pictures, never dropped.
        codec_delay: match codec {
            Codec::Opus | Codec::H264 | Codec::Av1 => 0,
            Codec::Aac => entry.codec_delay().unwrap_or(0),
        },
        aac_frame,
    };
    Ok(Some((track, info)))
}

/// Cues, CueRelativePosition locations and the first cluster's times.
fn scan<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    let file_len = r.seek(SeekFrom::End(0))?;
    let header = ebml::element_at(r, 0)?;
    let segment = ebml::element_at(r, header.end().context("Unknown-size EBML header")?)?;
    anyhow::ensure!(segment.id == ebml::SEGMENT, "No Matroska segment");

    let mut cues_at = None;
    let mut first_cluster = None;
    let mut pos = segment.data;
    while let Ok(el) = ebml::element_at(r, pos) {
        match el.id {
            ebml::SEEK_HEAD => {
                for seek in ebml::children(r, &el).iter().filter(|e| e.id == ebml::SEEK) {
                    let parts = ebml::children(r, seek);
                    let id = parts.iter().find(|e| e.id == ebml::SEEK_ID);
                    let position = parts.iter().find(|e| e.id == ebml::SEEK_POSITION);
                    if let (Some(id), Some(position)) = (id, position)
                        && ebml::read_binary(r, id, 4)? == ebml::CUES.to_be_bytes()
                    {
                        cues_at = segment.data.checked_add(ebml::read_uint(r, position)?);
                    }
                }
            }
            ebml::CUES => cues_at = Some(pos),
            ebml::CLUSTER => {
                first_cluster = Some(el);
                break;
            }
            _ => {}
        }
        let Some(end) = el.end() else { break };
        pos = end;
    }

    let mut cues = Vec::new();
    let mut patches = Vec::new();
    if let Some(at) = cues_at
        && let Ok(el) = ebml::element_at(r, at)
        && el.id == ebml::CUES
    {
        for point in ebml::children(r, &el)
            .iter()
            .filter(|e| e.id == ebml::CUE_POINT)
        {
            for part in ebml::children(r, point) {
                match part.id {
                    ebml::CUE_TIME => cues.push(ebml::read_uint(r, &part)?),
                    ebml::CUE_TRACK_POSITIONS => patches.extend(
                        ebml::children(r, &part)
                            .iter()
                            .filter(|e| e.id == ebml::CUE_RELATIVE_POSITION)
                            .map(|e| e.start),
                    ),
                    _ => {}
                }
            }
        }
        cues.sort_unstable();
    }

    let (first_cluster_time, first_block) = match &first_cluster {
        Some(cluster) => {
            let (time, blocks, _) = ebml::cluster_blocks(r, cluster, file_len);
            let first = time.and_then(|t| {
                blocks
                    .iter()
                    .map(|b| t.saturating_add_signed(i64::from(b.relative)))
                    .min()
            });
            (time, first)
        }
        None => (None, None),
    };
    let early_blocks =
        first_cluster.map_or_else(Vec::new, |cluster| early_blocks(r, cluster.start, file_len));
    Ok(Scan {
        cues,
        patches,
        first_cluster,
        first_cluster_time,
        first_block,
        early_blocks,
        file_len,
    })
}

/// (track number, time in ticks) of the blocks in the first
/// [`RATE_CLUSTERS`] clusters from `pos`.
fn early_blocks<R: Read + Seek>(r: &mut R, mut pos: u64, file_len: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut clusters = 0;
    while clusters < RATE_CLUSTERS && pos < file_len {
        let Ok(el) = ebml::element_at(r, pos) else {
            break;
        };
        let end = if el.id == ebml::CLUSTER {
            clusters += 1;
            let (time, blocks, end) = ebml::cluster_blocks(r, &el, file_len);
            if let Some(time) = time {
                out.extend(
                    blocks
                        .iter()
                        .map(|b| (b.track, time.saturating_add_signed(i64::from(b.relative)))),
                );
            }
            el.end().unwrap_or(end)
        } else {
            let Some(end) = el.end() else { break };
            end
        };
        if end <= pos {
            break;
        }
        pos = end;
    }
    out
}

/// End (ns) of the last block, for files without a duration: walks the
/// clusters by their sizes, then the blocks of the last one.
fn last_block_end(
    path: &Path,
    scan: &Scan,
    by_number: &HashMap<u64, usize>,
    info: &[TrackInfo],
    scale: u64,
) -> Option<u64> {
    let r = &mut BufReader::new(File::open(path).ok()?);
    let mut last = scan.first_cluster?;
    let mut pos = last.start;
    while pos < scan.file_len {
        let Ok(el) = ebml::element_at(r, pos) else {
            break;
        };
        let end = if el.id == ebml::CLUSTER {
            last = el;
            el.end()
                .unwrap_or_else(|| ebml::cluster_blocks(r, &el, scan.file_len).2)
        } else {
            // Cues or tags between or after clusters.
            let Some(end) = el.end() else { break };
            end
        };
        if end <= pos {
            break;
        }
        pos = end;
    }
    let (time, blocks, _) = ebml::cluster_blocks(r, &last, scan.file_len);
    let time = time?;
    blocks
        .iter()
        .map(|b| {
            let start = time
                .saturating_add_signed(i64::from(b.relative))
                .saturating_mul(scale);
            let duration = b.duration.map(|d| d.saturating_mul(scale)).or_else(|| {
                by_number
                    .get(&b.track)
                    .and_then(|&t| info[t].default_duration)
            });
            start.saturating_add(duration.unwrap_or(0))
        })
        .max()
}

/// Whether an AV1 temporal unit starts a coded video sequence (carries a
/// sequence header OBU), for blocks without a keyframe flag.
fn av1_has_sequence_header(mut data: &[u8]) -> bool {
    while let Some(&header) = data.first() {
        let kind = (header >> 3) & 0x0F;
        if kind == 1 {
            return true;
        }
        let extension = usize::from(header & 0x04 != 0);
        if header & 0x02 == 0 {
            // No size field: the OBU runs to the end.
            return false;
        }
        let mut size = 0usize;
        let mut i = 1 + extension;
        loop {
            let Some(&byte) = data.get(i) else {
                return false;
            };
            size |= usize::from(byte & 0x7F) << (7 * (i - 1 - extension));
            i += 1;
            if byte & 0x80 == 0 || i - 1 - extension >= 8 {
                break;
            }
        }
        let Some(rest) = data.get(i + size..) else {
            return false;
        };
        data = rest;
    }
    false
}

/// Reader that shows the demuxer each CueRelativePosition element as a Void
/// (same one-byte ID length): `matroska-demuxer` 0.8 adds relative
/// positions to the *first* cluster instead of the cue's, so hiding them
/// makes it seek to the cue's cluster and scan forward, which is correct.
struct CueFix<R> {
    inner: R,
    pos: u64,
    /// Sorted file positions of the bytes to replace.
    patches: Vec<u64>,
}

impl<R: Read> Read for CueFix<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let end = self.pos + n as u64;
        let first = self.patches.partition_point(|&p| p < self.pos);
        for &patch in self.patches[first..].iter().take_while(|&&p| p < end) {
            buf[(patch - self.pos) as usize] = ebml::VOID as u8;
        }
        self.pos = end;
        Ok(n)
    }
}

impl<R: Seek> Seek for CueFix<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}
