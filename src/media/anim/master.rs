//! Lossless recording master for animations.
//!
//! Animations are recorded into this format first and encoded into GIF,
//! WebP, AVIF or JPEG XL afterwards, so trimming and annotating never stack
//! lossy generations. It has to keep up with live capture (2560x1440 at
//! 30 fps is ~440 MB/s of BGRA), so frames are compressed with zstd at a
//! low level on a few worker threads, and every frame except a keyframe
//! every [`KEY_INTERVAL`] is stored as the XOR against its predecessor:
//! desktop frames repeat most pixels, which XOR to long zero runs.
//!
//! Layout (integers little endian):
//! - header: `QSMASTER`, version u16, reserved u16, width u32, height u32,
//!   12 reserved bytes (32 bytes);
//! - one record per frame: `QSFR`, kind u8 (0 key, 1 delta), 3 reserved
//!   bytes, pts u64 (ns), payload length u32, then the payload (one zstd
//!   frame with content checksum);
//! - at finish, the index: `QSIX`, count u32, end u64 (ns), then per frame
//!   pts u64, record offset u64, payload length u32, kind u8, 3 reserved
//!   bytes; and the trailer: index offset u64, `QSMEND\0\0`.
//!
//! A master cut off by a crash has no trailer; it is opened by scanning the
//! records up to the last complete one, which is why every record carries
//! its own pts and length.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use super::{AnimDecoder, AnimFrame, AnimInfo};
use crate::raster::Image;

const MAGIC: &[u8; 8] = b"QSMASTER";
const VERSION: u16 = 1;
const HEADER_LEN: u64 = 32;
const RECORD_MAGIC: &[u8; 4] = b"QSFR";
const RECORD_HEADER_LEN: u64 = 20;
const INDEX_MAGIC: &[u8; 4] = b"QSIX";
const INDEX_ENTRY_LEN: usize = 24;
const TRAILER_MAGIC: &[u8; 8] = b"QSMEND\0\0";
const TRAILER_LEN: u64 = 16;
/// Longest stretch of delta frames; bounds the work of a seek.
const KEY_INTERVAL: Duration = Duration::from_secs(2);
/// Fast enough for live capture while still finding the zero runs.
const ZSTD_LEVEL: i32 = 1;

/// Whether `path` starts with the master magic.
pub fn is_master(path: &Path) -> bool {
    let mut head = [0; MAGIC.len()];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| has_magic(&head))
}

pub(super) fn has_magic(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    pts: Duration,
    offset: u64,
    len: u32,
    key: bool,
}

/// One frame on its way through the compression workers.
struct Job {
    previous: Option<Arc<Vec<u8>>>,
    current: Arc<Vec<u8>>,
    done: SyncSender<std::io::Result<Vec<u8>>>,
}

struct Pending {
    pts: Duration,
    key: bool,
    result: Receiver<std::io::Result<Vec<u8>>>,
}

pub struct MasterWriter {
    out: BufWriter<File>,
    path: PathBuf,
    width: u32,
    height: u32,
    offset: u64,
    index: Vec<Entry>,
    jobs: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    /// Frames being compressed, oldest first; written in this order.
    pending: VecDeque<Pending>,
    max_pending: usize,
    previous: Option<Arc<Vec<u8>>>,
    last_pts: Option<Duration>,
    last_key: Duration,
}

impl MasterWriter {
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        ensure!(width > 0 && height > 0, "empty {width}x{height} master");
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&[0; 2]);
        header.extend_from_slice(&width.to_le_bytes());
        header.extend_from_slice(&height.to_le_bytes());
        header.resize(HEADER_LEN as usize, 0);
        out.write_all(&header)?;

        let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
        let count = (threads / 2).clamp(2, 4);
        let (jobs, queue) = mpsc::sync_channel::<Job>(count);
        let queue = Arc::new(Mutex::new(queue));
        let workers = (0..count)
            .map(|i| {
                let queue = queue.clone();
                std::thread::Builder::new()
                    .name(format!("qs-master-{i}"))
                    .spawn(move || compress_worker(&queue))
                    .context("starting master compression thread")
            })
            .collect::<Result<_>>()?;

        Ok(MasterWriter {
            out,
            path: path.to_owned(),
            width,
            height,
            offset: HEADER_LEN,
            index: Vec::new(),
            jobs: Some(jobs),
            workers,
            pending: VecDeque::new(),
            // Each frame in flight holds a full copy; this bounds memory
            // while keeping every worker busy.
            max_pending: count + 1,
            previous: None,
            last_pts: None,
            last_key: Duration::ZERO,
        })
    }

    /// Appends a BGRA frame shown from `pts` (strictly increasing).
    pub fn push(&mut self, frame: &Image, pts: Duration) -> Result<()> {
        ensure!(
            (frame.width, frame.height) == (self.width, self.height),
            "{}x{} frame for a {}x{} master",
            frame.width,
            frame.height,
            self.width,
            self.height
        );
        if let Some(last) = self.last_pts {
            ensure!(pts > last, "master frame at {pts:?} after {last:?}");
        }
        let key = self.last_pts.is_none() || pts.saturating_sub(self.last_key) >= KEY_INTERVAL;
        if key {
            self.last_key = pts;
        }
        self.last_pts = Some(pts);

        let current = Arc::new(frame.data.clone());
        let previous = if key { None } else { self.previous.take() };
        self.previous = Some(current.clone());
        let (done, result) = mpsc::sync_channel(1);
        let jobs = self.jobs.as_ref().expect("jobs live until finish");
        jobs.send(Job {
            previous,
            current,
            done,
        })
        .context("master compression thread exited")?;
        self.pending.push_back(Pending { pts, key, result });
        while self.pending.len() > self.max_pending {
            self.write_oldest()?;
        }
        Ok(())
    }

    fn write_oldest(&mut self) -> Result<()> {
        let Some(pending) = self.pending.pop_front() else {
            return Ok(());
        };
        let payload = pending
            .result
            .recv()
            .context("master compression thread exited")?
            .context("compressing master frame")?;
        let len = u32::try_from(payload.len()).context("master frame too large")?;
        let mut header = [0; RECORD_HEADER_LEN as usize];
        header[..4].copy_from_slice(RECORD_MAGIC);
        header[4] = u8::from(!pending.key);
        header[8..16].copy_from_slice(&(pending.pts.as_nanos() as u64).to_le_bytes());
        header[16..20].copy_from_slice(&len.to_le_bytes());
        self.out.write_all(&header)?;
        self.out.write_all(&payload)?;
        self.index.push(Entry {
            pts: pending.pts,
            offset: self.offset,
            len,
            key: pending.key,
        });
        self.offset += RECORD_HEADER_LEN + u64::from(len);
        Ok(())
    }

    /// Writes the remaining frames and the index. `end` is the recording's
    /// length: the last frame is shown until then.
    pub fn finish(mut self, end: Duration) -> Result<()> {
        if let Some(last) = self.last_pts {
            ensure!(
                end >= last,
                "master ends at {end:?} before its last frame at {last:?}"
            );
        }
        while !self.pending.is_empty() {
            self.write_oldest()?;
        }
        self.jobs = None;
        for worker in self.workers.drain(..) {
            worker.join().ok();
        }

        let mut index = Vec::with_capacity(16 + self.index.len() * INDEX_ENTRY_LEN);
        index.extend_from_slice(INDEX_MAGIC);
        index.extend_from_slice(&(self.index.len() as u32).to_le_bytes());
        index.extend_from_slice(&(end.as_nanos() as u64).to_le_bytes());
        for entry in &self.index {
            index.extend_from_slice(&(entry.pts.as_nanos() as u64).to_le_bytes());
            index.extend_from_slice(&entry.offset.to_le_bytes());
            index.extend_from_slice(&entry.len.to_le_bytes());
            index.extend_from_slice(&[u8::from(!entry.key), 0, 0, 0]);
        }
        index.extend_from_slice(&self.offset.to_le_bytes());
        index.extend_from_slice(TRAILER_MAGIC);
        self.out.write_all(&index)?;
        self.out
            .flush()
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

impl Drop for MasterWriter {
    fn drop(&mut self) {
        // Workers exit once the queue closes; nothing waits for their results.
        self.jobs = None;
    }
}

fn compress_worker(queue: &Mutex<Receiver<Job>>) {
    let mut compressor = None;
    let mut delta = Vec::new();
    loop {
        let Ok(job) = queue
            .lock()
            .map_err(drop)
            .and_then(|q| q.recv().map_err(drop))
        else {
            return;
        };
        let compressor = match &mut compressor {
            Some(c) => Ok(c),
            None => new_compressor().map(|c| compressor.insert(c)),
        };
        let result = compressor.and_then(|c| match &job.previous {
            Some(previous) => {
                xor_into(&mut delta, previous, &job.current);
                c.compress(&delta)
            }
            None => c.compress(&job.current),
        });
        // The writer may be gone (dropped without finish).
        job.done.send(result).ok();
    }
}

fn new_compressor() -> std::io::Result<zstd::bulk::Compressor<'static>> {
    let mut compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL)?;
    compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
    Ok(compressor)
}

/// `out = a ^ b`, eight bytes at a time.
fn xor_into(out: &mut Vec<u8>, a: &[u8], b: &[u8]) {
    out.resize(a.len(), 0);
    let mut out_words = out.chunks_exact_mut(8);
    let mut a_words = a.chunks_exact(8);
    let mut b_words = b.chunks_exact(8);
    for ((o, x), y) in (&mut out_words).zip(&mut a_words).zip(&mut b_words) {
        let v =
            u64::from_ne_bytes(x.try_into().unwrap()) ^ u64::from_ne_bytes(y.try_into().unwrap());
        o.copy_from_slice(&v.to_ne_bytes());
    }
    let tail = out_words.into_remainder();
    for ((o, x), y) in tail
        .iter_mut()
        .zip(a_words.remainder())
        .zip(b_words.remainder())
    {
        *o = x ^ y;
    }
}

/// `frame ^= delta`, eight bytes at a time.
fn xor_in_place(frame: &mut [u8], delta: &[u8]) {
    let mut frame_words = frame.chunks_exact_mut(8);
    let mut delta_words = delta.chunks_exact(8);
    for (f, d) in (&mut frame_words).zip(&mut delta_words) {
        let v = u64::from_ne_bytes((*f).try_into().unwrap())
            ^ u64::from_ne_bytes(d.try_into().unwrap());
        f.copy_from_slice(&v.to_ne_bytes());
    }
    for (f, d) in frame_words
        .into_remainder()
        .iter_mut()
        .zip(delta_words.remainder())
    {
        *f ^= d;
    }
}

pub struct MasterReader {
    file: BufReader<File>,
    info: AnimInfo,
    index: Vec<Entry>,
    end: Duration,
    /// Index of the frame `next_frame` returns.
    next: usize,
    /// The frame before `next`, which a delta applies to.
    frame: Vec<u8>,
    payload: Vec<u8>,
    decompressor: zstd::bulk::Decompressor<'static>,
}

impl MasterReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let file_len = file.metadata()?.len();
        let mut file = BufReader::new(file);
        let mut header = [0; HEADER_LEN as usize];
        file.read_exact(&mut header)
            .context("master header is incomplete")?;
        ensure!(
            has_magic(&header),
            "{} is not a recording master",
            path.display()
        );
        let version = u16::from_le_bytes([header[8], header[9]]);
        ensure!(version == VERSION, "unsupported master version {version}");
        let width = u32::from_le_bytes(header[12..16].try_into().unwrap());
        let height = u32::from_le_bytes(header[16..20].try_into().unwrap());
        ensure!(width > 0 && height > 0, "empty {width}x{height} master");

        // The records alone are enough; the index only saves the scan.
        let (index, end) = match read_index(&mut file, file_len) {
            Ok(Some(found)) => found,
            Ok(None) => {
                log::warn!("{} has no index (cut off?), scanning it", path.display());
                scan(&mut file, file_len)?
            }
            Err(e) => {
                log::warn!("{}: {e:#}, scanning it", path.display());
                scan(&mut file, file_len)?
            }
        };
        ensure!(
            !index.is_empty() && index[0].key,
            "{} has no frames",
            path.display()
        );
        let first = index[0].pts;
        let info = AnimInfo {
            width,
            height,
            frames: index.len() as u64,
            duration: end - first,
        };
        Ok(MasterReader {
            file,
            info,
            index,
            end,
            next: 0,
            frame: vec![0; width as usize * height as usize * 4],
            payload: Vec::new(),
            decompressor: zstd::bulk::Decompressor::new()?,
        })
    }

    /// Positions the reader so that `next_frame` returns the frame showing
    /// at `to` (from the first frame, as [`AnimFrame::pts`]).
    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let at = self.index[0].pts + to;
        let target = self
            .index
            .partition_point(|e| e.pts <= at)
            .saturating_sub(1);
        // Decoding resumes from the current position when that is cheaper
        // than from the keyframe.
        let key = self.index[..=target]
            .iter()
            .rposition(|e| e.key)
            .unwrap_or(0);
        let start = if (key..=target).contains(&self.next) {
            self.next
        } else {
            key
        };
        for i in start..target {
            self.decode(i)?;
        }
        self.next = target;
        Ok(())
    }

    /// Decodes frame `i` into `self.frame` (which must hold frame `i - 1`
    /// unless `i` is a keyframe).
    fn decode(&mut self, i: usize) -> Result<()> {
        let entry = self.index[i];
        self.file
            .seek(SeekFrom::Start(entry.offset + RECORD_HEADER_LEN))?;
        self.payload.resize(entry.len as usize, 0);
        self.file
            .read_exact(&mut self.payload)
            .with_context(|| format!("reading master frame {i}"))?;
        if entry.key {
            let n = self
                .decompressor
                .decompress_to_buffer(&self.payload, &mut self.frame[..])
                .with_context(|| format!("decompressing master frame {i}"))?;
            ensure!(n == self.frame.len(), "master frame {i} is {n} bytes");
        } else {
            let mut delta = std::mem::take(&mut self.payload);
            let packed = delta.len();
            delta.resize(packed + self.frame.len(), 0);
            let (packed, unpacked) = delta.split_at_mut(packed);
            let n = self
                .decompressor
                .decompress_to_buffer(packed, unpacked)
                .with_context(|| format!("decompressing master frame {i}"))?;
            ensure!(n == self.frame.len(), "master frame {i} is {n} bytes");
            xor_in_place(&mut self.frame, unpacked);
            self.payload = delta;
        }
        Ok(())
    }
}

impl AnimDecoder for MasterReader {
    fn info(&self) -> &AnimInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<AnimFrame>> {
        let i = self.next;
        let Some(entry) = self.index.get(i).copied() else {
            return Ok(None);
        };
        self.decode(i)?;
        self.next += 1;
        let until = self.index.get(i + 1).map_or(self.end, |e| e.pts);
        Ok(Some(AnimFrame {
            image: Image::from_bgra(self.info.width, self.info.height, self.frame.clone())?,
            pts: entry.pts - self.index[0].pts,
            duration: until - entry.pts,
        }))
    }

    fn rewind(&mut self) -> Result<()> {
        self.next = 0;
        Ok(())
    }
}

/// Reads the index through the trailer; `None` when there is none (the
/// writer never finished).
fn read_index(file: &mut BufReader<File>, file_len: u64) -> Result<Option<(Vec<Entry>, Duration)>> {
    if file_len < HEADER_LEN + TRAILER_LEN {
        return Ok(None);
    }
    let mut trailer = [0; TRAILER_LEN as usize];
    file.seek(SeekFrom::Start(file_len - TRAILER_LEN))?;
    file.read_exact(&mut trailer)?;
    if &trailer[8..] != TRAILER_MAGIC {
        return Ok(None);
    }
    let offset = u64::from_le_bytes(trailer[..8].try_into().unwrap());
    ensure!(
        (HEADER_LEN..file_len - TRAILER_LEN).contains(&offset),
        "master index offset {offset} out of range"
    );
    let mut index = vec![0; (file_len - TRAILER_LEN - offset) as usize];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut index)?;
    ensure!(
        index.len() >= 16 && &index[..4] == INDEX_MAGIC,
        "corrupt master index"
    );
    let count = u32::from_le_bytes(index[4..8].try_into().unwrap()) as usize;
    let end = Duration::from_nanos(u64::from_le_bytes(index[8..16].try_into().unwrap()));
    let entries = &index[16..];
    ensure!(
        entries.len() == count * INDEX_ENTRY_LEN,
        "corrupt master index"
    );
    let entries = entries
        .chunks_exact(INDEX_ENTRY_LEN)
        .map(|e| Entry {
            pts: Duration::from_nanos(u64::from_le_bytes(e[..8].try_into().unwrap())),
            offset: u64::from_le_bytes(e[8..16].try_into().unwrap()),
            len: u32::from_le_bytes(e[16..20].try_into().unwrap()),
            key: e[20] == 0,
        })
        .collect::<Vec<_>>();
    let ordered = entries.windows(2).all(|w| w[0].pts < w[1].pts);
    let inside = entries
        .iter()
        .all(|e| e.offset + RECORD_HEADER_LEN + u64::from(e.len) <= offset);
    ensure!(ordered && inside, "corrupt master index");
    Ok(Some((entries, end)))
}

/// Rebuilds the index of a master without one from its records, up to the
/// last complete frame. Its end is estimated from the last frame interval.
fn scan(file: &mut BufReader<File>, file_len: u64) -> Result<(Vec<Entry>, Duration)> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut offset = HEADER_LEN;
    let mut payload = Vec::new();
    while offset + RECORD_HEADER_LEN <= file_len {
        let mut header = [0; RECORD_HEADER_LEN as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut header)?;
        if &header[..4] != RECORD_MAGIC {
            break;
        }
        let key = header[4] == 0;
        let pts = Duration::from_nanos(u64::from_le_bytes(header[8..16].try_into().unwrap()));
        let len = u32::from_le_bytes(header[16..20].try_into().unwrap());
        if offset + RECORD_HEADER_LEN + u64::from(len) > file_len
            || entries.last().is_some_and(|last| last.pts >= pts)
            || (entries.is_empty() && !key)
        {
            break;
        }
        // A crash can leave a record whose length made it to disk but not
        // all of its payload; a complete zstd frame proves the payload did.
        payload.resize(len as usize, 0);
        file.read_exact(&mut payload)?;
        if zstd::zstd_safe::find_frame_compressed_size(&payload) != Ok(len as usize) {
            break;
        }
        entries.push(Entry {
            pts,
            offset,
            len,
            key,
        });
        offset += RECORD_HEADER_LEN + u64::from(len);
    }
    let Some(last) = entries.last() else {
        bail!("master has no complete frame");
    };
    let interval = match entries.len() {
        1 => Duration::from_millis(100),
        n => last.pts - entries[n - 2].pts,
    };
    let end = last.pts + interval;
    Ok((entries, end))
}
