//! Mixes captured audio sources into one 48 kHz stereo track.
//!
//! Capture timestamps jitter by milliseconds while each device's samples
//! are contiguous, so a source's samples are placed by *count*, not by
//! timestamp: a source is anchored at the session time of its first chunk
//! and every later sample follows the previous one. Timestamps only matter
//! when they disagree with that count by more than [`MAX_DRIFT`] (a dropout,
//! a device whose clock runs fast or slow, a capture that skipped data).
//! Then the source is re-anchored once: the chunk starts a new segment at
//! its own timestamp. If it arrived late, the gap plays as silence; if
//! early (more audio than time passed), the part overlapping what the source
//! already delivered is dropped, because a source never overlaps itself and
//! nothing is summed twice. Either way the source is back in sync.
//!
//! [`Mixer::take`] hands out what every source has delivered. A source that
//! lags more than [`GRACE`] behind (or has not started) stops holding the
//! mix back: its missing stretch plays as silence, and samples it delivers
//! later for already-returned times are dropped (it keeps its timing). Mixed
//! samples are soft-clipped above 0.9 so loud sources sum without wrapping
//! or harsh clipping.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Result;

use super::AudioSpec;
use super::codec::resample::Resampler;

/// Output rate; output is interleaved stereo.
pub const RATE: u32 = 48_000;
const OUTPUT: AudioSpec = AudioSpec {
    rate: RATE,
    channels: 2,
};
/// How far a source's timestamps may stray from its sample count before it
/// is re-anchored: far above capture jitter, below an audible lip-sync error.
pub const MAX_DRIFT: Duration = Duration::from_millis(40);
/// How long [`Mixer::take`] waits for a lagging source.
pub const GRACE: Duration = Duration::from_millis(200);
/// Input this far ahead of the output is dropped instead of buffered.
pub const MAX_AHEAD: Duration = Duration::from_secs(10);

const NANOS: u128 = 1_000_000_000;

pub struct Mixer {
    sources: Vec<Source>,
    /// Output frames returned so far; frame `i` plays at `i / RATE`.
    cursor: u64,
}

impl Mixer {
    pub fn new(specs: &[AudioSpec]) -> Result<Self> {
        let sources = specs
            .iter()
            .map(|&spec| {
                Ok(Source {
                    spec,
                    resampler: Resampler::new(spec, OUTPUT)?,
                    segment: None,
                    buffer: VecDeque::new(),
                    start: 0,
                    end: None,
                    dropping: false,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Mixer { sources, cursor: 0 })
    }

    /// Adds a chunk of `source` (an index into the specs given to
    /// [`new`](Self::new)) in its own format, whose first sample was captured
    /// at session time `at`.
    pub fn add(&mut self, source: usize, samples: &[f32], at: Duration) {
        let cursor = self.cursor;
        let src = &mut self.sources[source];
        let channels = usize::from(src.spec.channels);
        if samples.is_empty() {
            return;
        }
        if !samples.len().is_multiple_of(channels) {
            log::error!(
                "Audio source {source}: dropping a chunk of {} samples, not whole {channels}-channel frames",
                samples.len()
            );
            return;
        }
        if at > frame_time(cursor) + MAX_AHEAD {
            if !src.dropping {
                log::warn!(
                    "Audio source {source} is more than {} s ahead of the mix at {:?}; dropping its input",
                    MAX_AHEAD.as_secs(),
                    frame_time(cursor)
                );
                src.dropping = true;
            }
            return;
        }
        src.dropping = false;

        if let Some(segment) = &src.segment {
            let expected =
                segment.at.as_nanos() + u128::from(segment.fed) * NANOS / u128::from(src.spec.rate);
            let drift = at.as_nanos() as i128 - expected as i128;
            if drift.unsigned_abs() > MAX_DRIFT.as_nanos() {
                let ms = drift.unsigned_abs() / 1_000_000;
                if drift > 0 {
                    log::info!(
                        "Audio source {source} resumed {ms} ms after its samples ran out; padding with silence"
                    );
                } else {
                    log::info!(
                        "Audio source {source} delivered {ms} ms more audio than time passed; dropping the excess"
                    );
                }
                // The new segment starts at the chunk's timestamp; `place`
                // drops whatever of it overlaps what this source already
                // delivered, and a gap reads as silence.
                src.end_segment(cursor);
            }
        }
        let segment = src.segment.get_or_insert(Segment {
            at,
            fed: 0,
            origin: frame_at(at),
            trim: src.resampler.delay(),
            written: 0,
        });
        segment.fed += (samples.len() / channels) as u64;
        match src.resampler.process(samples) {
            Ok(output) => src.write(&output, cursor),
            Err(e) => log::error!("Audio source {source}: dropping a chunk: {e:#}"),
        }
    }

    /// Returns the mix from [`position`](Self::position) up to (excluding)
    /// `until`, or less while a source that is at most [`GRACE`] behind
    /// `until` has not delivered that far yet. Interleaved stereo at
    /// [`RATE`]; never covers time at or after `until`.
    pub fn take(&mut self, until: Duration) -> Vec<f32> {
        let target = frames_before(until);
        let patience = target.saturating_sub(frames_before(GRACE));
        let ready = self
            .sources
            .iter()
            .map(|s| s.end.unwrap_or(0).max(patience))
            .min()
            .unwrap_or(target)
            .min(target);
        self.mix_to(ready)
    }

    /// Ends every source's current stretch (draining resampler latency) and
    /// returns the whole mix up to `until`, silence where a source has
    /// nothing: for the end of a recording. Sources may continue afterwards;
    /// their next chunk is anchored afresh.
    pub fn finish(&mut self, until: Duration) -> Vec<f32> {
        let cursor = self.cursor;
        for source in &mut self.sources {
            source.end_segment(cursor);
        }
        self.mix_to(frames_before(until))
    }

    /// Session time up to which the mix has been returned.
    pub fn position(&self) -> Duration {
        frame_time(self.cursor)
    }

    fn mix_to(&mut self, ready: u64) -> Vec<f32> {
        let Some(frames) = ready.checked_sub(self.cursor).filter(|&n| n > 0) else {
            return Vec::new();
        };
        let mut mix = vec![0.0; frames as usize * 2];
        for source in &mut self.sources {
            if source.buffer.is_empty() || source.start >= ready {
                continue;
            }
            // `write` never keeps samples before the cursor.
            let offset = (source.start - self.cursor) as usize;
            let count = (source.buffer.len() / 2).min(frames as usize - offset);
            for (out, sample) in mix[offset * 2..]
                .iter_mut()
                .zip(source.buffer.drain(..count * 2))
            {
                *out += sample;
            }
            source.start += count as u64;
        }
        for sample in &mut mix {
            *sample = soft_clip(*sample);
        }
        self.cursor = ready;
        mix
    }
}

struct Source {
    spec: AudioSpec,
    resampler: Resampler,
    segment: Option<Segment>,
    /// Output not yet mixed (stereo), first frame at output frame `start`.
    buffer: VecDeque<f32>,
    start: u64,
    /// Output frame this source has delivered up to; `None` before its
    /// first chunk.
    end: Option<u64>,
    /// Input is being dropped for running too far ahead (logged once).
    dropping: bool,
}

/// A stretch of a source's input with contiguous samples.
struct Segment {
    /// Session time of its first input frame.
    at: Duration,
    /// Input frames fed to the resampler since.
    fed: u64,
    /// Output frame of its first input frame.
    origin: u64,
    /// Resampler warm-up frames still to discard.
    trim: usize,
    /// Output frames placed after the warm-up.
    written: u64,
}

impl Source {
    /// Places resampler output after what the current segment placed so far.
    fn write(&mut self, output: &[f32], cursor: u64) {
        let Some(segment) = &mut self.segment else {
            return;
        };
        let trim = segment.trim.min(output.len() / 2);
        segment.trim -= trim;
        let output = &output[trim * 2..];
        let at = segment.origin + segment.written;
        segment.written += (output.len() / 2) as u64;
        self.place(at, output, cursor);
    }

    fn end_segment(&mut self, cursor: u64) {
        if self.segment.is_none() {
            return;
        }
        match self.resampler.flush() {
            Ok(tail) => self.write(&tail, cursor),
            Err(e) => log::error!("Flushing an audio resampler failed: {e:#}"),
        }
        self.segment = None;
    }

    /// Buffers stereo `samples` for output frames from `at`, keeping only
    /// the part at or after `cursor` and not already covered.
    fn place(&mut self, at: u64, samples: &[f32], cursor: u64) {
        let frames = (samples.len() / 2) as u64;
        let end = at + frames;
        self.end = Some(self.end.map_or(end, |e| e.max(end)));
        let mut from = at.max(cursor);
        let buffered = self.start + (self.buffer.len() / 2) as u64;
        if self.buffer.is_empty() {
            self.start = from;
        } else if from > buffered {
            self.buffer
                .resize(self.buffer.len() + (from - buffered) as usize * 2, 0.0);
        } else {
            from = buffered;
        }
        if from < end {
            self.buffer.extend(&samples[(from - at) as usize * 2..]);
        }
    }
}

/// Output frame nearest to session time `at`.
fn frame_at(at: Duration) -> u64 {
    ((at.as_nanos() * u128::from(RATE) + NANOS / 2) / NANOS) as u64
}

/// Number of output frames that start before `until`.
fn frames_before(until: Duration) -> u64 {
    (until.as_nanos() * u128::from(RATE)).div_ceil(NANOS) as u64
}

fn frame_time(frame: u64) -> Duration {
    Duration::from_nanos((u128::from(frame) * NANOS / u128::from(RATE)) as u64)
}

/// Identity up to ±0.9, then a tanh knee that approaches ±1 with matching
/// slope at the knee (no audible corner for occasional peaks).
fn soft_clip(sample: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let level = sample.abs();
    if level <= KNEE {
        return sample;
    }
    (KNEE + (1.0 - KNEE) * ((level - KNEE) / (1.0 - KNEE)).tanh()).copysign(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_48K: AudioSpec = AudioSpec {
        rate: 48_000,
        channels: 2,
    };

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// Deterministic jitter in −3..=3 ms.
    struct Jitter(u64);
    impl Jitter {
        fn next(&mut self) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.0 >> 33) as i64 % 7_000_001 - 3_000_000
        }
    }

    fn offset(base: Duration, nanos: i64) -> Duration {
        Duration::from_nanos((base.as_nanos() as i64 + nanos).max(0) as u64)
    }

    /// `frames` of a sine continuing from frame `from` of its own stream.
    fn tone(
        spec: AudioSpec,
        frequency: f64,
        amplitude: f64,
        from: usize,
        frames: usize,
    ) -> Vec<f32> {
        (from..from + frames)
            .flat_map(|i| {
                let t = i as f64 / f64::from(spec.rate);
                let s = (amplitude * (std::f64::consts::TAU * frequency * t).sin()) as f32;
                std::iter::repeat_n(s, usize::from(spec.channels))
            })
            .collect()
    }

    #[test]
    fn jittery_sources_stay_continuous() {
        let mono_44k = AudioSpec {
            rate: 44_100,
            channels: 1,
        };
        let mut mixer = Mixer::new(&[STEREO_48K, mono_44k]).unwrap();
        let mut jitter = Jitter(7);
        let mut mix = Vec::new();
        // 10 ms chunks for two seconds, each stamped ±3 ms off.
        for chunk in 0..200 {
            let nominal = ms(chunk * 10);
            let a = tone(STEREO_48K, 440.0, 0.3, chunk as usize * 480, 480);
            mixer.add(0, &a, offset(nominal, jitter.next()));
            let b = tone(mono_44k, 660.0, 0.2, chunk as usize * 441, 441);
            mixer.add(1, &b, offset(nominal, jitter.next()));
            let taken = mixer.take(nominal.saturating_sub(ms(30)));
            assert!(mixer.position() <= nominal);
            mix.extend(taken);
        }
        mix.extend(mixer.finish(ms(2000)));
        assert_eq!(mix.len(), 96_000 * 2);

        // Both tones start at a zero crossing within 3 ms of 0; after that a
        // gap or a doubled overlap would jump far beyond the tones' slope
        // (2π·(440·0.3 + 660·0.2)/48000 ≈ 0.035 per sample). The last 10 ms
        // hold the ends of the streams.
        let left: Vec<f32> = mix.iter().step_by(2).copied().collect();
        let body = &left[..left.len() - 480];
        let worst = body
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(worst < 0.045, "discontinuity of {worst}");
        // Never silent once both have started.
        for window in body[480..].chunks(96) {
            assert!(window.iter().any(|s| s.abs() > 0.05));
        }
        assert!(mix.chunks_exact(2).all(|f| f[0] == f[1]));
    }

    #[test]
    fn late_source_lands_at_its_timestamp() {
        let mut mixer = Mixer::new(&[STEREO_48K, STEREO_48K]).unwrap();
        let mut mix = Vec::new();
        for chunk in 0..100 {
            let at = ms(chunk * 10);
            mixer.add(0, &[0.0; 960], at);
            if chunk >= 30 {
                mixer.add(1, &[0.5; 960], at);
            }
            mix.extend(mixer.take(at));
        }
        mix.extend(mixer.finish(ms(1000)));
        let first = mix.iter().position(|&s| s != 0.0).unwrap();
        assert_eq!(first, 14_400 * 2);
        assert!(mix[first..].iter().all(|&s| s == 0.5));
    }

    #[test]
    fn dropout_is_padded_with_silence() {
        let mut mixer = Mixer::new(&[STEREO_48K]).unwrap();
        let mut mix = Vec::new();
        for chunk in (0..100).filter(|c| !(50..60).contains(c)) {
            let at = ms(chunk * 10);
            let samples = tone(STEREO_48K, 440.0, 0.3, chunk as usize * 480, 480);
            mixer.add(0, &samples, at);
            mix.extend(mixer.take(at));
        }
        mix.extend(mixer.finish(ms(1000)));
        let expected = tone(STEREO_48K, 440.0, 0.3, 0, 48_000);
        assert_eq!(mix.len(), expected.len());
        let (before, rest) = mix.split_at(24_000 * 2);
        let (gap, after) = rest.split_at(4_800 * 2);
        assert_eq!(before, &expected[..24_000 * 2]);
        assert!(gap.iter().all(|&s| s == 0.0));
        assert_eq!(after, &expected[28_800 * 2..]);
    }

    #[test]
    fn fast_clock_is_reanchored_once() {
        let mut mixer = Mixer::new(&[STEREO_48K]).unwrap();
        // 5% more samples than time: 504 frames per 10 ms, 40 ms ahead after
        // 800 ms.
        for chunk in 0..100 {
            mixer.add(0, &[0.1; 1008], ms(chunk * 10));
        }
        let mix = mixer.finish(ms(1000));
        assert_eq!(mix.len(), 96_000);
        assert!(
            mix.iter().all(|&s| s == 0.1),
            "no gap where the excess was dropped"
        );
        // The chunk at 810 ms restarted the timing: 19 chunks of 504 later.
        assert_eq!(mixer.sources[0].end, Some(38_880 + 19 * 504));
    }

    #[test]
    fn take_respects_until_and_grace() {
        let mut mixer = Mixer::new(&[STEREO_48K, STEREO_48K]).unwrap();
        mixer.add(0, &[0.25; 9600], ms(0));
        // Source 1 never delivers: the mix waits GRACE for it, no longer.
        assert!(mixer.take(ms(100)).is_empty());
        let mix = mixer.take(ms(250));
        assert_eq!(mix.len(), 2_400 * 2);
        assert!(mix.iter().all(|&s| s == 0.25));
        assert_eq!(mixer.position(), ms(50));
        assert_eq!(mixer.take(ms(280)).len(), 1_440 * 2);
        // Now source 0 lags too: its missing 100..200 ms play as silence.
        let mix = mixer.take(ms(400));
        assert_eq!(mix.len(), 5_760 * 2);
        assert!(mix[..960 * 2].iter().all(|&s| s == 0.25));
        assert!(mix[960 * 2..].iter().all(|&s| s == 0.0));
        assert_eq!(mixer.position(), ms(200));
        // Going backwards returns nothing.
        assert!(mixer.take(ms(10)).is_empty());

        let mut total = mixer.position();
        for until in [201, 230, 229, 280, 600, 1000] {
            mixer.add(0, &[0.25; 960], total);
            let taken = mixer.take(ms(until));
            let allowed = frames_before(ms(until)).saturating_sub(frame_at(total));
            assert!(taken.len() as u64 / 2 <= allowed);
            total = mixer.position();
            assert!(total <= ms(until));
        }
    }

    #[test]
    fn far_ahead_input_is_dropped() {
        let mut mixer = Mixer::new(&[STEREO_48K]).unwrap();
        mixer.add(0, &[0.5; 960], Duration::from_secs(20));
        assert!(mixer.sources[0].buffer.is_empty());
        assert!(mixer.sources[0].end.is_none());
        let mix = mixer.finish(ms(1000));
        assert_eq!(mix.len(), 96_000);
        assert!(mix.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn loud_sums_are_soft_clipped() {
        let mut mixer = Mixer::new(&[STEREO_48K, STEREO_48K]).unwrap();
        mixer.add(0, &[0.6, -0.6], ms(0));
        mixer.add(1, &[0.6, -0.6], ms(0));
        let mix = mixer.finish(ms(1) / 48);
        assert!(mix[0] > 0.95 && mix[0] < 1.0, "{}", mix[0]);
        assert_eq!(mix[1], -mix[0]);
        assert_eq!(soft_clip(0.5), 0.5);
    }
}
