//! Fragmented writing, plus the classic rewrite on finish.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use mp4_atom::{
    Atom, Audio, Avc1, Avcc, Dinf, Dops, Dref, DrefEntry, Edts, Elst, ElstEntry, Encode, FourCC,
    Ftyp, Hdlr, Header, Mdat, Mdhd, Mdia, Mehd, Mfhd, Minf, Moof, Moov, Mvex, Mvhd, Opus, Smhd,
    Stbl, Stsd, Tfdt, Tfhd, Tkhd, Traf, Trak, Trex, Trun, TrunEntry, TrunSamples, Url, Visual,
    Vmhd,
};

use super::super::{Codec, Container, Packet, Track, TrackKind, nal, to_ticks};
use super::{NON_SYNC_FLAGS, SYNC_FLAGS, SampleTable};
use crate::media::codec::opus::{self, Head};

const VIDEO_TIMESCALE: u32 = 90_000;
/// `mvhd`/`tkhd`/`elst`/`mehd` units: fine enough that per-track start
/// offsets survive to within 11 µs.
const MOVIE_TIMESCALE: u32 = 90_000;
/// Fragment length when no video keyframes set the pace.
const AUDIO_FRAGMENT: Duration = Duration::from_secs(1);
/// Longest fragment, for long keyframe intervals: bounds what a crash loses.
/// Also how long the init segment waits for every track to start.
const MAX_FRAGMENT: Duration = Duration::from_secs(4);
/// Media time between `sync_data` calls: written fragments survive a power
/// loss, not only a crash, at the cost of one flush to disk this often.
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Writes H.264 and Opus tracks as fragmented MP4, or MP4/MOV finished
/// classic. Nothing reaches the file before the first video keyframe (its
/// SPS/PPS make the `avcC`) and, for up to [`MAX_FRAGMENT`], before every
/// track has started (the init segment's edit lists hold their starts);
/// packets until then are buffered.
pub struct Mp4Muxer {
    path: PathBuf,
    out: BufWriter<File>,
    /// Bytes written so far (the file position).
    pos: u64,
    layout: Layout,
    /// pts written as media time 0; fixed when the init segment is written.
    origin: Option<Duration>,
    sequence: u32,
    /// Position of the `mehd` duration, patched on finish.
    mehd_value: u64,
    /// Present when the file is rewritten classic on finish.
    classic: Option<Classic>,
    /// pts up to which written fragments were last synced to disk.
    synced: Duration,
}

/// What the `ftyp` and `moov` boxes are built from.
struct Layout {
    container: Container,
    tracks: Vec<MuxTrack>,
}

struct MuxTrack {
    codec: Codec,
    kind: TrackKind,
    timescale: u32,
    dops: Option<Dops>,
    avcc: Option<Avcc>,
    /// Samples not yet in a fragment, in pts order. The newest waits for its
    /// successor, whose pts gives its duration.
    pending: VecDeque<Sample>,
    last_pts: Option<Duration>,
    /// Decode time (ticks from origin) of the first written sample.
    start: Option<u64>,
    /// Whether the init segment's edit list holds `start`: then the
    /// fragments' decode times count from it.
    start_edited: bool,
    /// Decode time of the next sample to write.
    dts: u64,
    written: u64,
}

struct Sample {
    pts: Duration,
    /// Length-prefixed for H.264.
    data: Vec<u8>,
    sync: bool,
    duration: Option<Duration>,
}

/// One track's samples in a fragment.
struct Run {
    track: usize,
    samples: Vec<Sample>,
    durations: Vec<u32>,
    len: u64,
}

#[derive(Default)]
struct Classic {
    tables: Vec<SampleTable>,
    /// (file offset, length) of every fragment's `mdat` payload.
    payloads: Vec<(u64, u64)>,
    /// Sum of payload lengths so far.
    payload_len: u64,
}

impl Mp4Muxer {
    pub fn create(path: &Path, container: Container, tracks: &[Track]) -> Result<Self> {
        ensure!(!tracks.is_empty(), "An MP4 needs at least one track");
        let tracks = tracks
            .iter()
            .map(|track| {
                let (timescale, dops) = match (track.codec, track.kind) {
                    (Codec::H264, TrackKind::Video { .. }) => (VIDEO_TIMESCALE, None),
                    (Codec::Opus, TrackKind::Audio { .. }) => {
                        (opus::RATE, Some(dops_from_head(&track.codec_private)?))
                    }
                    (codec, kind) => bail!("Cannot write {codec:?} as {kind:?} to MP4"),
                };
                Ok(MuxTrack {
                    codec: track.codec,
                    kind: track.kind,
                    timescale,
                    dops,
                    avcc: None,
                    pending: VecDeque::new(),
                    last_pts: None,
                    start: None,
                    start_edited: false,
                    dts: 0,
                    written: 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let file = File::create(path).with_context(|| format!("Creating {}", path.display()))?;
        let fragmented = container == Container::Mp4 { fragmented: true };
        Ok(Mp4Muxer {
            path: path.to_owned(),
            out: BufWriter::with_capacity(1 << 20, file),
            pos: 0,
            classic: (!fragmented).then(|| Classic {
                tables: tracks.iter().map(|_| SampleTable::default()).collect(),
                ..Default::default()
            }),
            layout: Layout { container, tracks },
            origin: None,
            sequence: 0,
            mehd_value: 0,
            synced: Duration::ZERO,
        })
    }

    /// Writes every pending sample whose duration is known (all of them when
    /// `last`) as one fragment, writing the init segment first if needed.
    fn write_fragment(&mut self, last: bool) -> Result<()> {
        let tracks = &mut self.layout.tracks;
        if tracks.iter().all(|t| t.pending.is_empty()) {
            return Ok(());
        }
        if self.origin.is_none() {
            if tracks
                .iter()
                .any(|t| t.codec == Codec::H264 && t.avcc.is_none())
            {
                // No keyframe yet: the avcC is unknown.
                ensure!(!last, "No H.264 keyframe was written");
                return Ok(());
            }
            let first = tracks
                .iter()
                .filter_map(|t| t.pending.front().map(|s| s.pts))
                .min();
            let newest = tracks.iter().filter_map(|t| t.last_pts).max();
            let waited = newest.zip(first).map(|(n, f)| n.saturating_sub(f));
            // A track's start goes in the edit list, where every player
            // places it; one that starts only after the init segment is
            // written has to rely on its `tfdt` instead, which FFmpeg adds
            // to the track's duration.
            if !last
                && tracks.iter().any(|t| t.pending.is_empty())
                && waited.is_some_and(|w| w < MAX_FRAGMENT)
            {
                return Ok(());
            }
            self.origin = first;
            let origin = first.unwrap_or_default();
            for track in tracks.iter_mut() {
                if let Some(sample) = track.pending.front() {
                    let start = to_ticks(sample.pts.saturating_sub(origin), track.timescale);
                    track.start = Some(start);
                    track.dts = start;
                    track.start_edited = true;
                }
            }
            self.write_init()?;
        }
        let origin = self.origin.unwrap_or_default();

        let mut runs = Vec::new();
        let mut trafs = Vec::new();
        for (index, track) in self.layout.tracks.iter_mut().enumerate() {
            let count = if last {
                track.pending.len()
            } else {
                track.pending.len().saturating_sub(1)
            };
            if count == 0 {
                continue;
            }
            let samples: Vec<Sample> = track.pending.drain(..count).collect();
            if track.start.is_none() {
                let start = to_ticks(samples[0].pts.saturating_sub(origin), track.timescale);
                track.start = Some(start);
                track.dts = start;
            }
            let base = if track.start_edited {
                track.dts - track.start.unwrap_or(0)
            } else {
                track.dts
            };
            let mut entries = Vec::with_capacity(samples.len());
            let mut durations = Vec::with_capacity(samples.len());
            for (i, sample) in samples.iter().enumerate() {
                let next = samples
                    .get(i + 1)
                    .or(track.pending.front())
                    .map(|s| s.pts)
                    .or_else(|| sample.duration.map(|d| sample.pts + d));
                // Durations reach the next pts from the decode time actually
                // written, so rounding never accumulates.
                let duration = match next {
                    Some(next) => to_ticks(next.saturating_sub(origin), track.timescale)
                        .saturating_sub(track.dts),
                    None => track.average_duration(),
                }
                .clamp(1, u64::from(u32::MAX)) as u32;
                track.dts += u64::from(duration);
                track.written += 1;
                durations.push(duration);
                entries.push(TrunEntry {
                    duration: Some(duration),
                    size: Some(u32::try_from(sample.data.len()).context("Sample over 4 GiB")?),
                    flags: Some(if sample.sync {
                        SYNC_FLAGS
                    } else {
                        NON_SYNC_FLAGS
                    }),
                    cts: None,
                });
            }
            trafs.push(Traf {
                tfhd: Tfhd {
                    track_id: index as u32 + 1,
                    default_base_is_moof: true,
                    ..Default::default()
                },
                tfdt: Some(Tfdt {
                    base_media_decode_time: base,
                }),
                trun: vec![Trun {
                    data_offset: Some(0),
                    samples: TrunSamples::Explicit(entries),
                }],
                ..Default::default()
            });
            let len = samples.iter().map(|s| s.data.len() as u64).sum();
            runs.push(Run {
                track: index,
                samples,
                durations,
                len,
            });
        }
        if runs.is_empty() {
            return Ok(());
        }

        self.sequence += 1;
        let mut moof = Moof {
            mfhd: Mfhd {
                sequence_number: self.sequence,
            },
            traf: trafs,
        };
        let payload: u64 = runs.iter().map(|r| r.len).sum();
        let mdat = encode(&Header {
            kind: Mdat::KIND,
            size: Some(usize::try_from(payload)?),
        })?;
        // trun data offsets are fixed-width, so the moof size does not
        // depend on their values.
        let mut offset = (encode(&moof)?.len() + mdat.len()) as u64;
        for (traf, run) in moof.traf.iter_mut().zip(&runs) {
            traf.trun[0].data_offset = Some(i32::try_from(offset).context("Fragment over 2 GiB")?);
            offset += run.len;
        }
        let moof = encode(&moof)?;

        let payload_pos = self.pos + (moof.len() + mdat.len()) as u64;
        self.out.write_all(&moof)?;
        self.out.write_all(&mdat)?;
        let mut run_offset = 0;
        for run in &runs {
            if let Some(classic) = &mut self.classic {
                let table = &mut classic.tables[run.track];
                table.start_chunk(classic.payload_len + run_offset);
                for (sample, &duration) in run.samples.iter().zip(&run.durations) {
                    table.push(sample.data.len() as u32, duration, sample.sync);
                }
            }
            for sample in &run.samples {
                self.out.write_all(&sample.data)?;
            }
            run_offset += run.len;
        }
        if let Some(classic) = &mut self.classic {
            classic.payloads.push((payload_pos, payload));
            classic.payload_len += payload;
        }
        self.pos = payload_pos + payload;
        self.out.flush()?;
        let newest = runs
            .iter()
            .filter_map(|r| r.samples.last().map(|s| s.pts))
            .max()
            .unwrap_or_default();
        if newest.saturating_sub(self.synced) >= SYNC_INTERVAL {
            self.synced = newest;
            if let Err(e) = self.out.get_ref().sync_data() {
                log::warn!("Syncing {} to disk failed: {e}", self.path.display());
            }
        }
        Ok(())
    }

    /// `ftyp` and the fragmented `moov` (empty sample tables, `mvex`).
    fn write_init(&mut self) -> Result<()> {
        let layout = &self.layout;
        let ftyp = encode(&layout.ftyp(true))?;
        let mvhd = layout.mvhd(0);
        let mvhd_len = encode(&mvhd)?.len() as u64;
        let trak = (0..layout.tracks.len())
            .map(|i| layout.trak(i, Stbl::default(), 0, layout.fragmented_edits(i)))
            .collect::<Result<Vec<_>>>()?;
        let moov = encode(&Moov {
            mvhd,
            mvex: Some(Mvex {
                mehd: Some(Mehd::default()),
                trex: (1..=layout.tracks.len() as u32)
                    .map(|track_id| Trex {
                        track_id,
                        default_sample_description_index: 1,
                        ..Default::default()
                    })
                    .collect(),
            }),
            trak,
            ..Default::default()
        })?;
        // mvex follows mvhd; skip the moov, mvex and mehd headers and the
        // mehd version/flags.
        self.mehd_value = ftyp.len() as u64 + 8 + mvhd_len + 8 + 12;
        self.out.write_all(&ftyp)?;
        self.out.write_all(&moov)?;
        self.pos = (ftyp.len() + moov.len()) as u64;
        Ok(())
    }
}

impl Layout {
    fn ftyp(&self, fragmented: bool) -> Ftyp {
        let brands: &[&[u8; 4]] = match (self.container, fragmented) {
            (Container::Mov, _) => &[b"qt  "],
            (_, true) => &[b"isom", b"iso6", b"iso2", b"avc1", b"mp41"],
            (_, false) => &[b"isom", b"iso2", b"avc1", b"mp41"],
        };
        Ftyp {
            major_brand: FourCC::new(brands[0]),
            minor_version: 0x200,
            compatible_brands: brands.iter().map(|b| FourCC::new(b)).collect(),
        }
    }

    fn mvhd(&self, duration: u64) -> Mvhd {
        Mvhd {
            timescale: MOVIE_TIMESCALE,
            duration,
            rate: 1.into(),
            volume: 1.into(),
            next_track_id: self.tracks.len() as u32 + 1,
            ..Default::default()
        }
    }

    /// The edit list of a fragmented track: an empty edit for a late start
    /// known when the init segment was written, and the Opus pre-skip. A
    /// zero duration spans the whole fragmented media.
    fn fragmented_edits(&self, index: usize) -> Option<Edts> {
        let track = &self.tracks[index];
        let skip = track.dops.as_ref().map(|d| u64::from(d.pre_skip));
        let start = match track.start {
            Some(start) if track.start_edited => movie_time(start, track.timescale),
            _ => 0,
        };
        let mut entries = Vec::new();
        if start > 0 {
            entries.push(ElstEntry {
                segment_duration: start,
                media_time: None,
                media_rate: 1.into(),
            });
        }
        if start > 0 || skip.is_some() {
            entries.push(ElstEntry {
                segment_duration: 0,
                media_time: Some(skip.unwrap_or(0)),
                media_rate: 1.into(),
            });
        }
        (!entries.is_empty()).then_some(Edts {
            elst: Some(Elst { entries }),
        })
    }

    /// Edit list and presented length (movie timescale) of a classic track
    /// whose samples span `media_duration`: an empty edit for a late start,
    /// and the Opus pre-skip trimmed.
    fn classic_edits(&self, index: usize, media_duration: u64) -> (Option<Edts>, u64) {
        let track = &self.tracks[index];
        let skip = track.dops.as_ref().map_or(0, |d| u64::from(d.pre_skip));
        let start = movie_time(track.start.unwrap_or(0), track.timescale);
        let shown = movie_time(media_duration.saturating_sub(skip), track.timescale);
        let mut entries = Vec::new();
        if start > 0 {
            entries.push(ElstEntry {
                segment_duration: start,
                media_time: None,
                media_rate: 1.into(),
            });
        }
        if skip > 0 || start > 0 {
            entries.push(ElstEntry {
                segment_duration: shown,
                media_time: Some(skip),
                media_rate: 1.into(),
            });
        }
        let edts = (!entries.is_empty()).then_some(Edts {
            elst: Some(Elst { entries }),
        });
        (edts, start + shown)
    }

    /// Presented end of the longest track, in the movie timescale.
    fn movie_duration(&self) -> u64 {
        (0..self.tracks.len())
            .map(|i| {
                let track = &self.tracks[i];
                self.classic_edits(i, track.dts.saturating_sub(track.start.unwrap_or(0)))
                    .1
            })
            .max()
            .unwrap_or(0)
    }

    fn trak(&self, index: usize, stbl: Stbl, duration: u64, edts: Option<Edts>) -> Result<Trak> {
        let track = &self.tracks[index];
        let (width, height, entry) = match (track.codec, track.kind) {
            (Codec::H264, TrackKind::Video { width, height, .. }) => {
                let (width, height) = (u16::try_from(width)?, u16::try_from(height)?);
                let avc1 = Avc1 {
                    visual: Visual {
                        data_reference_index: 1,
                        width,
                        height,
                        ..Default::default()
                    },
                    avcc: track.avcc.clone().context("H.264 track without avcC")?,
                    ..Default::default()
                };
                (width, height, mp4_atom::Codec::Avc1(avc1))
            }
            (Codec::Opus, TrackKind::Audio { .. }) => {
                let dops = track.dops.clone().context("Opus track without dOps")?;
                let opus = Opus {
                    audio: Audio {
                        data_reference_index: 1,
                        channel_count: u16::from(dops.output_channel_count),
                        sample_size: 16,
                        sample_rate: (opus::RATE as u16).into(),
                    },
                    dops,
                    btrt: None,
                };
                (0, 0, mp4_atom::Codec::Opus(opus))
            }
            _ => bail!("Unsupported MP4 track"),
        };
        let video = track.codec == Codec::H264;
        // QuickTime resolves media through an alias data reference (flag 1:
        // this file); ISO files use a self-contained url.
        let dref = if self.container == Container::Mov {
            DrefEntry::Unknown(FourCC::new(b"alis"), vec![0, 0, 0, 1])
        } else {
            DrefEntry::Url(Url::default())
        };
        let media_duration = stbl
            .stts
            .entries
            .iter()
            .map(|e| u64::from(e.sample_count) * u64::from(e.sample_delta))
            .sum();
        Ok(Trak {
            tkhd: Tkhd {
                track_id: index as u32 + 1,
                duration,
                enabled: true,
                in_movie: true,
                volume: if video { 0.into() } else { 1.into() },
                width: width.into(),
                height: height.into(),
                ..Default::default()
            },
            edts,
            mdia: Mdia {
                mdhd: Mdhd {
                    timescale: track.timescale,
                    duration: media_duration,
                    language: "und".into(),
                    ..Default::default()
                },
                hdlr: Hdlr {
                    handler: FourCC::new(if video { b"vide" } else { b"soun" }),
                    name: if video {
                        "VideoHandler"
                    } else {
                        "SoundHandler"
                    }
                    .into(),
                },
                minf: Minf {
                    vmhd: video.then(Vmhd::default),
                    smhd: (!video).then(Smhd::default),
                    dinf: Dinf {
                        dref: Dref {
                            entries: vec![dref],
                        },
                    },
                    stbl: Stbl {
                        stsd: Stsd {
                            codecs: vec![entry],
                        },
                        ..stbl
                    },
                    ..Default::default()
                },
            },
            ..Default::default()
        })
    }

    /// Rewrites the fragmented file at `path` as ftyp, moov, mdat, copying
    /// the sample data fragment by fragment into a sibling file that then
    /// replaces it.
    fn rewrite_classic(&self, path: &Path, classic: &Classic) -> Result<()> {
        let ftyp = encode(&self.ftyp(false))?;
        let mdat = encode(&Header {
            kind: Mdat::KIND,
            size: Some(usize::try_from(classic.payload_len)?),
        })?;
        // Chunk offsets depend on the moov size, which grows if they need
        // co64; one more round settles it.
        let mut shift = (ftyp.len() + mdat.len()) as u64;
        let moov = loop {
            let mut trak = Vec::new();
            for (index, table) in classic.tables.iter().enumerate() {
                if table.is_empty() {
                    continue;
                }
                let (edts, duration) = self.classic_edits(index, table.duration());
                trak.push(self.trak(index, table.stbl(Stsd::default(), shift), duration, edts)?);
            }
            let moov = encode(&Moov {
                mvhd: self.mvhd(self.movie_duration()),
                trak,
                ..Default::default()
            })?;
            let settled = (ftyp.len() + moov.len() + mdat.len()) as u64;
            if settled == shift {
                break moov;
            }
            shift = settled;
        };

        let mut name = path.file_name().unwrap_or_default().to_owned();
        name.push(".classic");
        let temp = path.with_file_name(name);
        let result = (|| -> Result<()> {
            let mut src = File::open(path)?;
            let mut out = BufWriter::with_capacity(1 << 20, File::create(&temp)?);
            out.write_all(&ftyp)?;
            out.write_all(&moov)?;
            out.write_all(&mdat)?;
            for &(offset, len) in &classic.payloads {
                src.seek(SeekFrom::Start(offset))?;
                let copied = io::copy(&mut (&mut src).take(len), &mut out)?;
                ensure!(copied == len, "The fragmented file ended early");
            }
            let file = out.into_inner().map_err(|e| e.into_error())?;
            file.sync_data()?;
            // Windows cannot replace a file that is still open.
            drop((file, src));
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.with_context(|| format!("Rewriting {} as a classic MP4", path.display()))
    }
}

impl MuxTrack {
    /// Duration for a final sample with no successor or packet duration.
    fn average_duration(&self) -> u64 {
        match self.start {
            Some(start) if self.written > 0 => (self.dts - start) / self.written,
            _ => match self.kind {
                TrackKind::Video { fps, .. } if fps > 0.0 => {
                    (f64::from(self.timescale) / fps).round() as u64
                }
                TrackKind::Video { .. } => u64::from(self.timescale / 30),
                // One 20 ms Opus packet.
                TrackKind::Audio { .. } => u64::from(self.timescale / 50),
            },
        }
    }
}

impl super::super::Muxer for Mp4Muxer {
    fn write(&mut self, packet: Packet) -> Result<()> {
        let index = packet.track;
        let has_video = self
            .layout
            .tracks
            .iter()
            .any(|t| matches!(t.kind, TrackKind::Video { .. }));
        let track = self
            .layout
            .tracks
            .get_mut(index)
            .with_context(|| format!("No track {index}"))?;
        ensure!(
            track.last_pts.is_none_or(|last| packet.pts > last),
            "Track {index}: pts {:?} does not increase",
            packet.pts
        );
        if self.origin.is_some_and(|origin| packet.pts < origin) {
            log::warn!("Dropping a track {index} packet from before the file start");
            return Ok(());
        }
        let data = match track.codec {
            Codec::H264 => {
                let avcc = match &track.avcc {
                    Some(avcc) => avcc,
                    None if packet.keyframe => {
                        track.avcc.insert(nal::avcc_from_keyframe(&packet.data)?)
                    }
                    None => {
                        log::debug!("Dropping H.264 frames before the first keyframe");
                        return Ok(());
                    }
                };
                let mut sample = Vec::new();
                nal::annexb_to_sample(&packet.data, avcc, &mut sample)?;
                sample
            }
            _ => packet.data,
        };
        let video = track.codec == Codec::H264;
        let cut_at_keyframe = video && packet.keyframe && !track.pending.is_empty();
        track.last_pts = Some(packet.pts);
        track.pending.push_back(Sample {
            pts: packet.pts,
            data,
            // Every Opus packet decodes on its own.
            sync: packet.keyframe || !video,
            duration: packet.duration,
        });

        let first = self
            .layout
            .tracks
            .iter()
            .filter_map(|t| t.pending.front().map(|s| s.pts))
            .min()
            .unwrap_or(packet.pts);
        let span = packet.pts.saturating_sub(first);
        if cut_at_keyframe || span >= MAX_FRAGMENT || (!has_video && span >= AUDIO_FRAGMENT) {
            self.write_fragment(false)?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.write_fragment(true)?;
        ensure!(self.origin.is_some(), "No packets were written");
        let duration = self.layout.movie_duration();
        self.out.seek(SeekFrom::Start(self.mehd_value))?;
        self.out.write_all(&duration.to_be_bytes())?;
        self.out.flush()?;
        let Mp4Muxer {
            path,
            out,
            layout,
            classic,
            ..
        } = *self;
        drop(out);
        match classic {
            Some(classic) => layout.rewrite_classic(&path, &classic),
            None => Ok(()),
        }
    }
}

fn encode(atom: &impl Encode) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    atom.encode(&mut buf)?;
    Ok(buf)
}

fn movie_time(ticks: u64, timescale: u32) -> u64 {
    (u128::from(ticks) * u128::from(MOVIE_TIMESCALE) / u128::from(timescale.max(1))) as u64
}

/// `dOps` from an OpusHead: the same fields, big-endian, without the magic.
fn dops_from_head(head: &[u8]) -> Result<Dops> {
    let head = Head::parse(head)?;
    Ok(Dops {
        output_channel_count: head.channels,
        pre_skip: head.pre_skip,
        input_sample_rate: head.input_rate,
        output_gain: head.gain,
        channel_mapping: head.mapping.map(|m| mp4_atom::OpusChannelMapping {
            family: m.family,
            stream_count: m.streams,
            coupled_count: m.coupled,
            channel_mapping: m.table,
        }),
    })
}
