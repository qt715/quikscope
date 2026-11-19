//! Sample-rate and channel conversion of interleaved f32 audio (rubato).
//!
//! Channels are in WAVE/SMPTE order (FL FR FC LFE BL BR SL SR, what WASAPI
//! and PipeWire deliver; [`super::opus::Decoder`] reorders Opus to it).
//! Mono plays on both channels of a wider output; more than two channels
//! downmix to stereo with ITU-R BS.775 gains (centre and surrounds at −3 dB,
//! LFE dropped), scaled so in-phase full-scale input cannot clip.

use std::borrow::Cow;

use anyhow::{Result, ensure};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

use crate::media::AudioSpec;

/// Input frames rubato resamples at a time; output appears in steps of
/// about this many frames (~20 ms at 48 kHz).
const CHUNK: usize = 1024;

/// Streaming converter from one [`AudioSpec`] to another.
pub struct Resampler {
    from: AudioSpec,
    /// Applied before resampling when it reduces the channel count (less to
    /// resample), `upmix` after it otherwise.
    downmix: Option<Matrix>,
    upmix: Option<Matrix>,
    /// `None` when both rates are equal: channel mapping only, no latency.
    stream: Option<Stream>,
}

impl Resampler {
    pub fn new(from: AudioSpec, to: AudioSpec) -> Result<Self> {
        for spec in [from, to] {
            ensure!(
                spec.rate > 0 && spec.channels > 0,
                "Invalid audio format: {} Hz, {} channels",
                spec.rate,
                spec.channels
            );
        }
        let (inputs, outputs) = (usize::from(from.channels), usize::from(to.channels));
        let matrix = Matrix::new(inputs, outputs);
        let (downmix, upmix) = if outputs < inputs {
            (matrix, None)
        } else {
            (None, matrix)
        };
        let stream = if from.rate == to.rate {
            None
        } else {
            let channels = inputs.min(outputs);
            Some(Stream {
                fft: Fft::new(
                    from.rate as usize,
                    to.rate as usize,
                    CHUNK,
                    channels,
                    FixedSync::Input,
                )?,
                channels,
                ratio: (u64::from(to.rate), u64::from(from.rate)),
                pending: Vec::new(),
                fed: 0,
                produced: 0,
            })
        };
        Ok(Resampler {
            from,
            downmix,
            upmix,
            stream,
        })
    }

    /// Converts `input` (whole frames in the source format). Output lags
    /// input by [`delay`](Self::delay) frames plus whatever is still
    /// buffered; [`flush`](Self::flush) returns the rest.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        ensure!(
            input.len().is_multiple_of(usize::from(self.from.channels)),
            "Audio chunk of {} samples is not whole {}-channel frames",
            input.len(),
            self.from.channels
        );
        let samples = match &self.downmix {
            Some(matrix) => Cow::Owned(matrix.apply(input)),
            None => Cow::Borrowed(input),
        };
        let samples = match &mut self.stream {
            Some(stream) => Cow::Owned(stream.process(&samples)?),
            None => samples,
        };
        Ok(match &self.upmix {
            Some(matrix) => matrix.apply(&samples),
            None => samples.into_owned(),
        })
    }

    /// Ends the stream: returns the remaining output so that everything
    /// [`process`](Self::process) and `flush` returned since the start (or
    /// the last flush) is exactly `delay() + ceil(input frames × ratio)`
    /// frames. The resampler is then ready for a new stream.
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        let Some(stream) = &mut self.stream else {
            return Ok(Vec::new());
        };
        let samples = stream.flush()?;
        Ok(match &self.upmix {
            Some(matrix) => matrix.apply(&samples),
            None => samples,
        })
    }

    /// Latency in output frames: the first `delay()` frames of a stream are
    /// filter warm-up; input frame `i` appears at output frame
    /// `delay() + i × ratio`.
    pub fn delay(&self) -> usize {
        self.stream.as_ref().map_or(0, |s| s.fft.output_delay())
    }
}

struct Stream {
    fft: Fft<f32>,
    channels: usize,
    /// Output rate / input rate.
    ratio: (u64, u64),
    /// Input not yet consumed by rubato (it takes fixed-size chunks).
    pending: Vec<f32>,
    /// Frames fed and produced since the stream started.
    fed: u64,
    produced: u64,
}

impl Stream {
    fn process(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        self.pending.extend_from_slice(input);
        self.fed += (input.len() / self.channels) as u64;
        let frames = self.pending.len() / self.channels;
        let mut output = Vec::new();
        let mut offset = 0;
        while frames - offset >= self.fft.input_frames_next() {
            offset += self.run(Indexing::new().input_offset(offset), &mut output)?;
        }
        self.pending.drain(..offset * self.channels);
        Ok(output)
    }

    fn flush(&mut self) -> Result<Vec<f32>> {
        let (num, den) = self.ratio;
        let expected = self.fft.output_delay() as u64 + (self.fed * num).div_ceil(den);
        let mut output = Vec::new();
        let rest = self.pending.len() / self.channels;
        if rest > 0 {
            self.run(Indexing::new().partial_len(rest), &mut output)?;
            self.pending.clear();
        }
        while self.produced < expected {
            self.run(Indexing::new().partial_len(0), &mut output)?;
        }
        output.truncate(output.len() - (self.produced - expected) as usize * self.channels);
        self.fft.reset();
        self.fed = 0;
        self.produced = 0;
        Ok(output)
    }

    /// One rubato call over `pending`, appending to `output`; returns the
    /// input frames consumed.
    fn run(&mut self, indexing: Indexing, output: &mut Vec<f32>) -> Result<usize> {
        let channels = self.channels;
        let input = InterleavedSlice::new(&self.pending, channels, self.pending.len() / channels)?;
        let start = output.len();
        let room = self.fft.output_frames_max();
        output.resize(start + room * channels, 0.0);
        let mut out = InterleavedSlice::new_mut(&mut output[start..], channels, room)?;
        let (read, written) = self
            .fft
            .process_into_buffer(&input, &mut out, Some(&indexing))?;
        output.truncate(start + written * channels);
        self.produced += written as u64;
        Ok(read)
    }
}

/// Gains from every input channel to every output channel.
struct Matrix {
    inputs: usize,
    /// `outputs` rows of `inputs` gains.
    gains: Vec<f32>,
}

impl Matrix {
    /// `None` for the identity.
    fn new(inputs: usize, outputs: usize) -> Option<Matrix> {
        if inputs == outputs {
            return None;
        }
        let mut gains = vec![0.0; inputs * outputs];
        match (inputs, outputs) {
            (1, _) => {
                // Mono feeds the front pair (or the only channel).
                gains[..outputs.min(2)].fill(1.0);
            }
            (_, 1 | 2) => {
                let stereo = stereo_downmix(inputs);
                for (input, [left, right]) in stereo.into_iter().enumerate() {
                    if outputs == 1 {
                        gains[input] = (left + right) / 2.0;
                    } else {
                        gains[input] = left;
                        gains[inputs + input] = right;
                    }
                }
            }
            // Between two multichannel layouts the WAVE order lines the
            // shared positions up.
            _ => {
                for channel in 0..inputs.min(outputs) {
                    gains[channel * inputs + channel] = 1.0;
                }
            }
        }
        Some(Matrix { inputs, gains })
    }

    fn apply(&self, input: &[f32]) -> Vec<f32> {
        let outputs = self.gains.len() / self.inputs;
        let mut output = Vec::with_capacity(input.len() / self.inputs * outputs);
        for frame in input.chunks_exact(self.inputs) {
            for row in self.gains.chunks_exact(self.inputs) {
                output.push(row.iter().zip(frame).map(|(g, s)| g * s).sum());
            }
        }
        output
    }
}

/// `[left, right]` gains per input channel of a WAVE-ordered layout with
/// `channels >= 2`, normalised so the louder side sums to at most 1.
/// Positions past the eighth channel are unknown and dropped.
fn stereo_downmix(channels: usize) -> Vec<[f32; 2]> {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const FL: [f32; 2] = [1.0, 0.0];
    const FR: [f32; 2] = [0.0, 1.0];
    const FC: [f32; 2] = [H, H];
    const LFE: [f32; 2] = [0.0, 0.0];
    const BC: [f32; 2] = [0.5, 0.5];
    const SIDE_L: [f32; 2] = [H, 0.0];
    const SIDE_R: [f32; 2] = [0.0, H];
    let layout: &[[f32; 2]] = match channels {
        2 => &[FL, FR],
        3 => &[FL, FR, FC],
        4 => &[FL, FR, SIDE_L, SIDE_R],
        5 => &[FL, FR, FC, SIDE_L, SIDE_R],
        6 => &[FL, FR, FC, LFE, SIDE_L, SIDE_R],
        7 => &[FL, FR, FC, LFE, BC, SIDE_L, SIDE_R],
        _ => &[FL, FR, FC, LFE, SIDE_L, SIDE_R, SIDE_L, SIDE_R],
    };
    let mut gains = layout.to_vec();
    gains.resize(channels, [0.0; 2]);
    let left: f32 = gains.iter().map(|g| g[0]).sum();
    let right: f32 = gains.iter().map(|g| g[1]).sum();
    let scale = left.max(right).max(1.0);
    for gain in &mut gains {
        gain[0] /= scale;
        gain[1] /= scale;
    }
    gains
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, channels: u16, frequency: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (std::f64::consts::TAU * frequency * i as f64 / f64::from(rate)).sin();
                std::iter::repeat_n(0.5 * s as f32, usize::from(channels))
            })
            .collect()
    }

    /// Frequency from zero crossings of channel 0 over `samples`.
    fn frequency(samples: &[f32], channels: usize, rate: u32) -> f64 {
        let mono: Vec<f32> = samples.iter().step_by(channels).copied().collect();
        let crossings: Vec<usize> = (1..mono.len())
            .filter(|&i| mono[i - 1] < 0.0 && mono[i] >= 0.0)
            .collect();
        let (first, last) = (crossings[0], *crossings.last().unwrap());
        (crossings.len() - 1) as f64 * f64::from(rate) / (last - first) as f64
    }

    #[test]
    fn upsamples_44100_to_48000_streaming() {
        let from = AudioSpec {
            rate: 44_100,
            channels: 2,
        };
        let to = AudioSpec {
            rate: 48_000,
            channels: 2,
        };
        let input = sine(44_100, 2, 1000.0, 44_100);
        let mut resampler = Resampler::new(from, to).unwrap();
        let mut output = Vec::new();
        // Odd, varying chunk sizes as capture devices deliver them.
        let mut rest = &input[..];
        for size in [441, 1, 3000, 17, 4410].into_iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let take = (size * 2).min(rest.len());
            output.extend(resampler.process(&rest[..take]).unwrap());
            rest = &rest[take..];
        }
        output.extend(resampler.flush().unwrap());
        let delay = resampler.delay();
        assert_eq!(output.len() / 2, delay + 48_000);

        let output = &output[delay * 2..];
        let measured = frequency(&output[4800..output.len() - 4800], 2, 48_000);
        assert!((measured - 1000.0).abs() < 0.5, "{measured} Hz");
        // Aligned with the ideal 48 kHz sine once the delay is dropped.
        let ideal = sine(48_000, 2, 1000.0, 48_000);
        let error: f32 = output[2000..90_000]
            .iter()
            .zip(&ideal[2000..90_000])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(error < 0.01, "max error {error}");
    }

    #[test]
    fn mono_to_stereo_duplicates_without_latency() {
        let mut resampler = Resampler::new(
            AudioSpec {
                rate: 48_000,
                channels: 1,
            },
            AudioSpec {
                rate: 48_000,
                channels: 2,
            },
        )
        .unwrap();
        assert_eq!(resampler.delay(), 0);
        let output = resampler.process(&[0.25, -0.5]).unwrap();
        assert_eq!(output, [0.25, 0.25, -0.5, -0.5]);
        assert!(resampler.flush().unwrap().is_empty());
    }

    #[test]
    fn mono_resampled_to_stereo() {
        let mut resampler = Resampler::new(
            AudioSpec {
                rate: 16_000,
                channels: 1,
            },
            AudioSpec {
                rate: 48_000,
                channels: 2,
            },
        )
        .unwrap();
        let mut output = resampler.process(&sine(16_000, 1, 440.0, 16_000)).unwrap();
        output.extend(resampler.flush().unwrap());
        assert_eq!(output.len(), (resampler.delay() + 48_000) * 2);
        assert!(output.chunks_exact(2).all(|f| f[0] == f[1]));
        let measured = frequency(&output[9600..86_400], 2, 48_000);
        assert!((measured - 440.0).abs() < 0.5, "{measured} Hz");
    }

    #[test]
    fn surround_downmix() {
        let mut resampler = Resampler::new(
            AudioSpec {
                rate: 48_000,
                channels: 6,
            },
            AudioSpec {
                rate: 48_000,
                channels: 2,
            },
        )
        .unwrap();
        // FL, FR, FC, LFE, BL, BR
        let output = resampler
            .process(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0])
            .unwrap();
        let norm = 1.0 + 2.0 * std::f32::consts::FRAC_1_SQRT_2;
        let expect = [1.0 / norm, 0.0, std::f32::consts::FRAC_1_SQRT_2 / norm];
        assert!((output[0] - expect[0]).abs() < 1e-6);
        assert_eq!(output[1], expect[1]);
        // Centre at -3 dB on both sides, LFE dropped.
        assert!((output[2] - expect[2]).abs() < 1e-6);
        assert!((output[3] - expect[2]).abs() < 1e-6);
        // In-phase full scale on every channel stays within ±1.
        let loud = resampler.process(&[1.0; 6]).unwrap();
        assert!(loud.iter().all(|s| s.abs() <= 1.0 + 1e-6));
    }

    #[test]
    fn rejects_partial_frames_and_bad_specs() {
        let mut resampler = Resampler::new(
            AudioSpec {
                rate: 44_100,
                channels: 2,
            },
            AudioSpec {
                rate: 48_000,
                channels: 2,
            },
        )
        .unwrap();
        assert!(resampler.process(&[0.0; 3]).is_err());
        assert!(
            Resampler::new(
                AudioSpec {
                    rate: 0,
                    channels: 2
                },
                AudioSpec {
                    rate: 48_000,
                    channels: 2
                }
            )
            .is_err()
        );
    }
}
