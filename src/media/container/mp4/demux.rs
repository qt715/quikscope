//! Sample index over `moov` tables and `moof` runs; `mdat` is never loaded
//! whole.
//!
//! Files may be damaged or crafted, so every count, size and offset read
//! from them is checked against the file length before it sizes a loop or
//! an allocation: an index never holds more samples than the file has
//! bytes, and box scanning only moves forward.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use mp4_atom::{
    Atom, Decode, DecodeAtom, Elst, Header, Moof, Moov, Mvex, Mvhd, ReadFrom, Stbl, StszSamples,
    Trak, Trex,
};

use super::super::{Codec, Packet, Track, TrackKind, nal, nanos, nominal_fps, ticks_to_nanos};
use super::NON_SYNC_BIT;
use crate::media::codec::opus::{ChannelMapping, Head};

trait Source: Read + Seek + Send {}
impl<T: Read + Seek + Send> Source for T {}

/// A forward gap between samples up to this size is read through rather
/// than sought over, which would empty the read buffer.
const READ_THROUGH: u64 = 64 * 1024;

/// Reads H.264, AAC, Opus and AV1 tracks of MP4/MOV files, fragmented or
/// not, including animated AVIF (`pict` tracks). Other tracks are skipped.
pub struct Mp4Demuxer {
    reader: Box<dyn Source>,
    /// Where `reader` is, `u64::MAX` when unknown.
    pos: u64,
    tracks: Vec<Track>,
    index: Vec<TrackIndex>,
    /// Presentation time (ns) every reported pts is relative to.
    origin: i128,
    duration: Duration,
}

struct TrackIndex {
    /// In decode order.
    samples: Vec<SampleRef>,
    /// Indices of sync samples.
    sync: Vec<usize>,
    cursor: usize,
    timeline: Timeline,
    video: bool,
    /// `avcC` length size and parameter sets for H.264.
    h264: Option<(usize, Vec<u8>)>,
    /// Decode time of the next fragment's first sample without `tfdt`.
    next_dts: u64,
}

/// Maps a track's decode times to presentation times.
#[derive(Clone, Copy)]
struct Timeline {
    timescale: u32,
    /// Media time (ticks) presented at `lead` (edit list).
    media_start: i64,
    /// Presentation time (ns) of `media_start`: leading empty edits.
    lead: i128,
}

#[derive(Clone, Copy)]
struct SampleRef {
    offset: u64,
    size: u32,
    dts: u64,
    cts: i32,
    duration: u32,
    sync: bool,
}

impl Mp4Demuxer {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
        Self::from_reader(BufReader::new(file))
            .with_context(|| format!("Reading {}", path.display()))
    }

    /// Reads from any seekable source, e.g. an in-memory animated AVIF.
    pub fn from_reader(mut reader: impl Read + Seek + Send + 'static) -> Result<Self> {
        let len = reader.seek(SeekFrom::End(0))?;
        let mut moov = None;
        let mut moofs = Vec::new();
        let mut pos = 0;
        while pos + 8 <= len {
            reader.seek(SeekFrom::Start(pos))?;
            let Ok(header) = Header::read_from(&mut reader) else {
                break;
            };
            let body_pos = reader.stream_position()?;
            // A box always ends after its header, so the scan only moves
            // forward; a size that overflows is damage, not a box.
            let end = match header.size {
                Some(size) => body_pos
                    .checked_add(size as u64)
                    .with_context(|| format!("MP4 {} box size overflows", header.kind))?,
                None => len,
            };
            match header.kind {
                Moov::KIND | Moof::KIND if end > len => {
                    // A crash cut this box: everything before it is usable.
                    log::warn!("MP4 ends inside a {} box", header.kind);
                    break;
                }
                Moov::KIND => moov = Some(parse_moov(&read_body(&mut reader, body_pos, end)?)?),
                Moof::KIND => {
                    let body = read_body(&mut reader, body_pos, end)?;
                    match Moof::decode_atom(&header, &mut body.as_slice()) {
                        Ok(moof) => moofs.push((pos, moof)),
                        Err(e) => {
                            log::warn!("Skipping an unreadable MP4 fragment: {e}");
                        }
                    }
                }
                _ => {}
            }
            if header.size.is_none() {
                break;
            }
            pos = end;
        }
        let (mvhd, mvex, traks) = moov.context("No moov box")?;

        // Every real sample occupies at least one byte of the file.
        let mut budget = len;
        let mut tracks = Vec::new();
        let mut index = Vec::new();
        let mut by_id = HashMap::new();
        for trak in &traks {
            match read_trak(trak, &mvhd, &mut budget) {
                Ok(Some((track, track_index))) => {
                    by_id.insert(trak.tkhd.track_id, tracks.len());
                    tracks.push(track);
                    index.push(track_index);
                }
                Ok(None) => {}
                Err(e) => log::warn!("Skipping MP4 track {}: {e:#}", trak.tkhd.track_id),
            }
        }
        let trex: HashMap<u32, &Trex> = mvex
            .as_ref()
            .map(|mvex| mvex.trex.iter().map(|t| (t.track_id, t)).collect())
            .unwrap_or_default();
        for (moof_pos, moof) in &moofs {
            if let Err(e) =
                add_fragment(*moof_pos, moof, &by_id, &trex, &mut index, len, &mut budget)
            {
                log::warn!("Ignoring the rest of the MP4 fragments: {e:#}");
                break;
            }
        }
        for track in &mut index {
            // Samples a crash cut short.
            track.samples.retain(|s| {
                s.size > 0
                    && s.offset
                        .checked_add(u64::from(s.size))
                        .is_some_and(|end| end <= len)
            });
            if !track.video {
                // Encoder priming (AAC) that the edit list hides: a packet
                // mostly before the track's start would play early. Video
                // keeps such samples, later pictures may reference them.
                let timeline = track.timeline;
                track.samples.retain(|s| {
                    let start = timeline.presentation(s);
                    let length = ticks_to_nanos(i128::from(s.duration), timeline.timescale);
                    2 * start + length > 2 * timeline.lead
                });
            }
            track.sync = (0..track.samples.len())
                .filter(|&i| track.samples[i].sync)
                .collect();
        }
        // Fragments can add frames to tracks whose moov tables were empty.
        for (track, track_index) in tracks.iter_mut().zip(&index) {
            if let TrackKind::Video { fps, .. } = &mut track.kind {
                *fps = nominal_fps(
                    track_index.samples.iter().map(|s| u64::from(s.duration)),
                    f64::from(track_index.timeline.timescale),
                );
            }
        }

        // An edit list's start is the track's start, even for samples it
        // shows only partly.
        let origin = index
            .iter()
            .flat_map(|t| {
                t.samples
                    .iter()
                    .map(move |s| t.timeline.presentation(s).max(t.timeline.lead))
            })
            .min()
            .unwrap_or(0);
        let end = index
            .iter()
            .flat_map(|t| {
                t.samples.iter().map(move |s| {
                    t.timeline.presentation(s)
                        + ticks_to_nanos(i128::from(s.duration), t.timeline.timescale)
                })
            })
            .max()
            .unwrap_or(origin);
        Ok(Mp4Demuxer {
            reader: Box::new(reader),
            pos: u64::MAX,
            tracks,
            index,
            origin,
            duration: nanos(end - origin),
        })
    }

    /// Moves the reader to `offset`, reading through short forward gaps.
    /// The position stays unknown if that fails.
    fn reach(&mut self, offset: u64) -> io::Result<()> {
        match offset.checked_sub(std::mem::replace(&mut self.pos, u64::MAX)) {
            Some(0) => {}
            Some(gap) if gap <= READ_THROUGH => {
                let skipped = io::copy(&mut (&mut self.reader).take(gap), &mut io::sink())?;
                if skipped < gap {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
            }
            _ => {
                self.reader.seek(SeekFrom::Start(offset))?;
            }
        }
        self.pos = offset;
        Ok(())
    }
}

impl Timeline {
    /// Presentation time in ns, before subtracting the file origin.
    fn presentation(&self, sample: &SampleRef) -> i128 {
        let media = i128::from(sample.dts) + i128::from(sample.cts) - i128::from(self.media_start);
        self.lead + ticks_to_nanos(media, self.timescale)
    }
}

impl super::super::Demuxer for Mp4Demuxer {
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn duration(&self) -> Duration {
        self.duration
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let Some(track) = (0..self.index.len())
            .filter(|&i| self.index[i].cursor < self.index[i].samples.len())
            .min_by_key(|&i| self.index[i].samples[self.index[i].cursor].offset)
        else {
            return Ok(None);
        };
        let index = &mut self.index[track];
        let sample = index.samples[index.cursor];
        index.cursor += 1;
        self.reach(sample.offset)?;
        let mut data = vec![0; sample.size as usize];
        self.pos = u64::MAX;
        self.reader.read_exact(&mut data)?;
        self.pos = sample.offset + u64::from(sample.size);
        let index = &self.index[track];
        if let Some((length_size, params)) = &index.h264 {
            data = nal::sample_to_annexb(&data, *length_size, sample.sync, params);
        }
        Ok(Some(Packet {
            track,
            data,
            pts: nanos(index.timeline.presentation(&sample) - self.origin),
            duration: Some(nanos(ticks_to_nanos(
                i128::from(sample.duration),
                index.timeline.timescale,
            ))),
            keyframe: sample.sync,
        }))
    }

    fn seek(&mut self, to: Duration) -> Result<()> {
        let to = self.origin + to.as_nanos() as i128;
        for index in &mut self.index {
            let timeline = index.timeline;
            // A target computed from the original times may sit a rounding
            // error before the stored one.
            let target = to + ticks_to_nanos(1, timeline.timescale) / 2;
            index.cursor = if index.video {
                // Sync samples present in decode order with increasing times.
                let found = index
                    .sync
                    .partition_point(|&i| timeline.presentation(&index.samples[i]) <= target);
                index
                    .sync
                    .get(found.saturating_sub(1))
                    .copied()
                    .unwrap_or(0)
            } else {
                let next = index
                    .samples
                    .partition_point(|s| timeline.presentation(s) <= target);
                // The packet playing at `to`, if any: one that ended before
                // it (the track ended, or pauses) would only play stale.
                match next.checked_sub(1) {
                    Some(last)
                        if timeline.presentation(&index.samples[last])
                            + ticks_to_nanos(
                                i128::from(index.samples[last].duration),
                                timeline.timescale,
                            )
                            > to =>
                    {
                        last
                    }
                    _ => next,
                }
            };
        }
        Ok(())
    }
}

fn read_body(reader: &mut impl Read, start: u64, end: u64) -> Result<Vec<u8>> {
    let mut body = vec![0; usize::try_from(end - start)?];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// The boxes of a container box's body, each with its header.
fn children(mut body: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if body.len() < 8 {
            return None;
        }
        let kind = [body[4], body[5], body[6], body[7]];
        let size = match u32::from_be_bytes([body[0], body[1], body[2], body[3]]) {
            0 => body.len(),
            1 => usize::try_from(u64::from_be_bytes(body.get(8..16)?.try_into().ok()?)).ok()?,
            n => n as usize,
        };
        let child = body.get(..size.max(8))?;
        body = &body[child.len()..];
        Some((kind, child))
    })
}

/// mvhd, mvex and every `trak` that parses (one unreadable track does not
/// hide the others).
fn parse_moov(body: &[u8]) -> Result<(Mvhd, Option<Mvex>, Vec<Trak>)> {
    let mut mvhd = None;
    let mut mvex = None;
    let mut traks = Vec::new();
    for (kind, mut child) in children(body) {
        match &kind {
            b"mvhd" => mvhd = Some(Mvhd::decode(&mut child)?),
            b"mvex" => mvex = Some(Mvex::decode(&mut child)?),
            b"trak" => match Trak::decode(&mut child) {
                Ok(trak) => traks.push(trak),
                Err(e) => log::warn!("Skipping an unreadable MP4 track: {e}"),
            },
            _ => {}
        }
    }
    Ok((mvhd.context("No mvhd box")?, mvex, traks))
}

/// The track and its sample index, or `None` for codecs we do not read.
/// Takes the samples it indexes from `budget`.
fn read_trak(trak: &Trak, mvhd: &Mvhd, budget: &mut u64) -> Result<Option<(Track, TrackIndex)>> {
    let mdia = &trak.mdia;
    let video = match mdia.hdlr.handler.as_ref() {
        b"vide" | b"pict" => true,
        b"soun" => false,
        _ => return Ok(None),
    };
    let timescale = mdia.mdhd.timescale;
    let stbl = &mdia.minf.stbl;
    let Some(entry) = stbl.stsd.codecs.first() else {
        return Ok(None);
    };
    let mut h264 = None;
    let mut pre_skip = 0;
    let (codec, kind, codec_private) = match entry {
        mp4_atom::Codec::Avc1(avc) => {
            h264 = Some((
                usize::from(avc.avcc.length_size),
                nal::parameter_sets(&avc.avcc),
            ));
            (Codec::H264, visual(&avc.visual), atom_body(&avc.avcc)?)
        }
        mp4_atom::Codec::Avc3(avc) => {
            h264 = Some((
                usize::from(avc.avcc.length_size),
                nal::parameter_sets(&avc.avcc),
            ));
            (Codec::H264, visual(&avc.visual), atom_body(&avc.avcc)?)
        }
        mp4_atom::Codec::Av01(av01) => (Codec::Av1, visual(&av01.visual), atom_body(&av01.av1c)?),
        mp4_atom::Codec::Opus(opus) => {
            let dops = &opus.dops;
            pre_skip = i64::from(dops.pre_skip);
            let head = Head {
                channels: dops.output_channel_count,
                pre_skip: dops.pre_skip,
                input_rate: dops.input_sample_rate,
                gain: dops.output_gain,
                mapping: dops.channel_mapping.as_ref().map(|m| ChannelMapping {
                    family: m.family,
                    streams: m.stream_count,
                    coupled: m.coupled_count,
                    table: m.channel_mapping.clone(),
                }),
            };
            let kind = TrackKind::Audio {
                rate: 48_000,
                channels: u16::from(dops.output_channel_count),
            };
            (Codec::Opus, kind, head.to_bytes())
        }
        mp4_atom::Codec::Mp4a(mp4a) => {
            let config = &mp4a.esds.es_desc.dec_config;
            // MPEG-4 audio, MPEG-2 AAC LC/SSR.
            if !matches!(config.object_type_indication, 0x40 | 0x66 | 0x67) {
                return Ok(None);
            }
            let asc = match &config.dec_specific {
                Some(specific) if !specific.raw.is_empty() => specific.raw.clone(),
                Some(s) => {
                    let bits = u16::from(s.profile) << 11
                        | u16::from(s.freq_index) << 7
                        | u16::from(s.chan_conf) << 3;
                    bits.to_be_bytes().to_vec()
                }
                None => bail!("AAC track without AudioSpecificConfig"),
            };
            let rate = match u32::from(mp4a.audio.sample_rate.integer()) {
                0 => timescale,
                rate => rate,
            };
            let kind = TrackKind::Audio {
                rate,
                channels: mp4a.audio.channel_count,
            };
            (Codec::Aac, kind, asc)
        }
        other => {
            log::debug!("Skipping an MP4 track of {other:?}");
            return Ok(None);
        }
    };
    let kind = match kind {
        TrackKind::Video { width: 0, .. } | TrackKind::Video { height: 0, .. } => {
            TrackKind::Video {
                width: u32::from(trak.tkhd.width.integer()),
                height: u32::from(trak.tkhd.height.integer()),
                fps: 0.0,
            }
        }
        kind => kind,
    };

    let (lead_movie, media_time) = edit(trak.edts.as_ref().and_then(|e| e.elst.as_ref()));
    // Opus pre-skip stays in the packet times (the decoder trims it), so
    // only edits beyond it move the track.
    let media_start = (media_time - pre_skip).max(0);
    let samples = table_samples(stbl, video, budget)?;
    let next_dts = samples
        .last()
        .map_or(0, |s| s.dts.saturating_add(u64::from(s.duration)));
    let track = Track {
        codec,
        kind,
        codec_private,
    };
    let index = TrackIndex {
        samples,
        sync: Vec::new(),
        cursor: 0,
        timeline: Timeline {
            timescale,
            media_start,
            lead: ticks_to_nanos(i128::from(lead_movie), mvhd.timescale),
        },
        video,
        h264,
        next_dts,
    };
    Ok(Some((track, index)))
}

fn visual(visual: &mp4_atom::Visual) -> TrackKind {
    TrackKind::Video {
        width: u32::from(visual.width),
        height: u32::from(visual.height),
        fps: 0.0,
    }
}

/// A box's payload as Matroska/our API store codec configuration.
fn atom_body(atom: &impl Atom) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    atom.encode_body(&mut body)?;
    Ok(body)
}

/// Leading empty edits (movie timescale) and the media time the first real
/// edit starts at. Later edits are rare outside editing tools and ignored.
fn edit(elst: Option<&Elst>) -> (u64, i64) {
    let mut lead = 0u64;
    for entry in elst.map_or(&[][..], |e| &e.entries) {
        match entry.media_time {
            None => lead = lead.saturating_add(entry.segment_duration),
            Some(time) => return (lead, i64::try_from(time).unwrap_or(0)),
        }
    }
    (lead, 0)
}

/// Expands classic sample tables, taking their samples from `budget`.
fn table_samples(stbl: &Stbl, video: bool, budget: &mut u64) -> Result<Vec<SampleRef>> {
    let (count, fixed, sizes) = match &stbl.stsz.samples {
        StszSamples::Identical { count, size } => (u64::from(*count), *size, &[][..]),
        StszSamples::Different { sizes } => (sizes.len() as u64, 0, &sizes[..]),
    };
    if count == 0 {
        return Ok(Vec::new());
    }
    ensure!(
        count <= *budget,
        "{count} samples cannot fit in what is left of the file"
    );
    *budget -= count;
    let count = count as usize;
    let chunks: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (Some(stco), _) => stco.entries.iter().map(|&o| u64::from(o)).collect(),
        (None, Some(co64)) => co64.entries.clone(),
        (None, None) => bail!("Sample table without chunk offsets"),
    };
    let mut durations = stbl
        .stts
        .entries
        .iter()
        .flat_map(|e| std::iter::repeat_n(e.sample_delta, e.sample_count as usize));
    let mut offsets = stbl.ctts.iter().flat_map(|ctts| {
        ctts.entries
            .iter()
            .flat_map(|e| std::iter::repeat_n(e.sample_offset, e.sample_count as usize))
    });
    let sync: Option<std::collections::HashSet<u32>> = stbl
        .stss
        .as_ref()
        .filter(|_| video)
        .map(|stss| stss.entries.iter().copied().collect());

    let mut samples = Vec::new();
    let mut dts = 0u64;
    let stsc = &stbl.stsc.entries;
    // `stsc` runs are in chunk order: one pass with a cursor.
    let mut run = 0;
    'chunks: for (chunk, &chunk_offset) in chunks.iter().enumerate() {
        let number = chunk as u64 + 1;
        while stsc
            .get(run + 1)
            .is_some_and(|e| u64::from(e.first_chunk) <= number)
        {
            run += 1;
        }
        let per_chunk = stsc
            .get(run)
            .filter(|e| u64::from(e.first_chunk) <= number)
            .map_or(0, |e| e.samples_per_chunk);
        let mut offset = chunk_offset;
        for _ in 0..per_chunk {
            let i = samples.len();
            if i == count {
                break 'chunks;
            }
            let size = sizes.get(i).copied().unwrap_or(fixed);
            let duration = durations.next().unwrap_or(0);
            samples.push(SampleRef {
                offset,
                size,
                dts,
                cts: offsets.next().map_or(0, |o| i32::try_from(o).unwrap_or(0)),
                duration,
                sync: sync.as_ref().is_none_or(|s| s.contains(&(i as u32 + 1))),
            });
            offset = offset.saturating_add(u64::from(size));
            dts = dts.saturating_add(u64::from(duration));
        }
    }
    Ok(samples)
}

/// Appends a fragment's samples to the tracks it describes, taking them
/// from `budget`. Data past the end of the file (`len`) ends a run.
fn add_fragment(
    moof_pos: u64,
    moof: &Moof,
    by_id: &HashMap<u32, usize>,
    trex: &HashMap<u32, &Trex>,
    index: &mut [TrackIndex],
    len: u64,
    budget: &mut u64,
) -> Result<()> {
    // Without explicit bases, a traf's data follows the previous one's.
    let mut previous_end = moof_pos;
    for traf in &moof.traf {
        let tfhd = &traf.tfhd;
        let Some(&track) = by_id.get(&tfhd.track_id) else {
            continue;
        };
        let track = &mut index[track];
        let defaults = trex.get(&tfhd.track_id);
        let base = tfhd
            .base_data_offset
            .unwrap_or(if tfhd.default_base_is_moof {
                moof_pos
            } else {
                previous_end
            });
        let mut dts = traf
            .tfdt
            .as_ref()
            .map_or(track.next_dts, |t| t.base_media_decode_time);
        let mut data = base;
        for trun in &traf.trun {
            if let Some(offset) = trun.data_offset {
                data = base.saturating_add_signed(i64::from(offset));
            }
            for entry in trun.samples.iter() {
                if data >= len {
                    break;
                }
                ensure!(*budget > 0, "more samples than the file has bytes");
                *budget -= 1;
                let duration = entry
                    .duration
                    .or(tfhd.default_sample_duration)
                    .or(defaults.map(|d| d.default_sample_duration))
                    .unwrap_or(0);
                let size = entry
                    .size
                    .or(tfhd.default_sample_size)
                    .or(defaults.map(|d| d.default_sample_size))
                    .unwrap_or(0);
                let flags = entry
                    .flags
                    .or(tfhd.default_sample_flags)
                    .or(defaults.map(|d| d.default_sample_flags))
                    .unwrap_or(0);
                track.samples.push(SampleRef {
                    offset: data,
                    size,
                    dts,
                    cts: entry.cts.map_or(0, |c| i32::try_from(c).unwrap_or(0)),
                    duration,
                    sync: !track.video || flags & NON_SYNC_BIT == 0,
                });
                data = data.saturating_add(u64::from(size));
                dts = dts.saturating_add(u64::from(duration));
            }
        }
        previous_end = data;
        track.next_dts = dts;
    }
    Ok(())
}
