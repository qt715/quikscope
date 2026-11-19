//! ISO BMFF (MP4/MOV) muxing and demuxing on `mp4-atom` boxes.
//!
//! The muxer always writes fragments (`moof`/`mdat`) as packets arrive, so a
//! crash leaves a playable file; classic files are rewritten from those
//! fragments on finish. The demuxer indexes every sample from `moov` sample
//! tables and `moof` runs without reading `mdat`.

use mp4_atom::{Co64, Stbl, Stco, Stsc, StscEntry, Stsd, Stss, Stsz, StszSamples, Stts, SttsEntry};

mod demux;
mod mux;

pub use demux::Mp4Demuxer;
pub use mux::Mp4Muxer;

/// `trun`/`trex` sample flags (ISO/IEC 14496-12 §8.8.3.1): a sync sample
/// depends on nothing; any other depends on earlier ones and is not sync.
const SYNC_FLAGS: u32 = 0x0200_0000;
const NON_SYNC_FLAGS: u32 = 0x0101_0000;
const NON_SYNC_BIT: u32 = 0x0001_0000;

/// Classic (`moov`) sample tables of one track, built sample by sample in
/// file order. Shared with the animated AVIF writer.
#[derive(Debug, Default)]
pub struct SampleTable {
    sizes: Vec<u32>,
    stts: Vec<SttsEntry>,
    /// 1-based numbers of sync samples.
    sync: Vec<u32>,
    /// (offset, samples) per chunk.
    chunks: Vec<(u64, u32)>,
}

impl SampleTable {
    /// Following samples are stored contiguously from `offset`.
    pub fn start_chunk(&mut self, offset: u64) {
        self.chunks.push((offset, 0));
    }

    /// Appends a sample to the current chunk (start one first).
    pub fn push(&mut self, size: u32, duration: u32, sync: bool) {
        self.sizes.push(size);
        if sync {
            self.sync.push(self.sizes.len() as u32);
        }
        match self.stts.last_mut() {
            Some(last) if last.sample_delta == duration => last.sample_count += 1,
            _ => self.stts.push(SttsEntry {
                sample_count: 1,
                sample_delta: duration,
            }),
        }
        if let Some(chunk) = self.chunks.last_mut() {
            chunk.1 += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }

    /// Sum of sample durations in the track's timescale.
    pub fn duration(&self) -> u64 {
        self.stts
            .iter()
            .map(|e| u64::from(e.sample_count) * u64::from(e.sample_delta))
            .sum()
    }

    /// The `stbl`, with every chunk offset moved by `shift` (offsets were
    /// recorded relative to wherever the sample data ends up).
    pub fn stbl(&self, stsd: Stsd, shift: u64) -> Stbl {
        let mut stsc = Vec::<StscEntry>::new();
        for (i, &(_, samples)) in self.chunks.iter().enumerate() {
            if stsc.last().is_none_or(|e| e.samples_per_chunk != samples) {
                stsc.push(StscEntry {
                    first_chunk: i as u32 + 1,
                    samples_per_chunk: samples,
                    sample_description_index: 1,
                });
            }
        }
        let (stco, co64) = chunk_offsets(self.chunks.iter().map(|&(offset, _)| offset + shift));
        let samples = match self.sizes.first() {
            Some(&size) if self.sizes.iter().all(|&s| s == size) => StszSamples::Identical {
                count: self.sizes.len() as u32,
                size,
            },
            _ => StszSamples::Different {
                sizes: self.sizes.clone(),
            },
        };
        Stbl {
            stsd,
            stts: Stts {
                entries: self.stts.clone(),
            },
            // No `stss` means every sample is a sync sample.
            stss: (self.sync.len() != self.sizes.len()).then(|| Stss {
                entries: self.sync.clone(),
            }),
            stsc: Stsc { entries: stsc },
            stsz: Stsz { samples },
            stco,
            co64,
            ..Default::default()
        }
    }
}

/// `stco` while every offset fits 32 bits, else `co64` (files past 4 GiB).
pub fn chunk_offsets(offsets: impl Iterator<Item = u64>) -> (Option<Stco>, Option<Co64>) {
    let offsets: Vec<u64> = offsets.collect();
    match offsets
        .iter()
        .map(|&o| u32::try_from(o))
        .collect::<Result<Vec<u32>, _>>()
    {
        Ok(entries) => (Some(Stco { entries }), None),
        Err(_) => (None, Some(Co64 { entries: offsets })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_offsets_switch_to_co64_past_four_gib() {
        let small = chunk_offsets([0, u64::from(u32::MAX)].into_iter());
        assert_eq!(small.0.unwrap().entries, [0, u32::MAX]);
        assert!(small.1.is_none());
        let big = chunk_offsets([48, u64::from(u32::MAX) + 1].into_iter());
        assert!(big.0.is_none());
        assert_eq!(big.1.unwrap().entries, [48, u64::from(u32::MAX) + 1]);
    }

    #[test]
    fn sample_table_run_lengths() {
        let mut table = SampleTable::default();
        table.start_chunk(100);
        table.push(10, 3000, true);
        table.push(11, 3000, false);
        table.start_chunk(u64::from(u32::MAX));
        table.push(12, 1500, false);
        table.push(13, 1500, true);
        assert_eq!(table.duration(), 9000);
        let stbl = table.stbl(Stsd::default(), 1);
        assert_eq!(stbl.stts.entries.len(), 2);
        assert_eq!(stbl.stss.unwrap().entries, [1, 4]);
        assert_eq!(stbl.stsc.entries.len(), 1);
        // The shifted second chunk no longer fits 32 bits.
        assert_eq!(stbl.co64.unwrap().entries, [101, u64::from(u32::MAX) + 1]);
    }
}
