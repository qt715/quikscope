//! Sample-rate and channel conversion of interleaved f32 audio (rubato).

// AudioCodecs owns this file. Contract:
// pub struct Resampler; Resampler::new(from: AudioSpec, to: AudioSpec) -> Result<Self>
//   process(&mut self, input: &[f32]) -> Result<Vec<f32>>  (any length, streaming)
//   flush(&mut self) -> Result<Vec<f32>>; delay(&self) -> usize (output frames of latency)
// Channel mapping: mono -> both channels; more than two -> standard downmix to stereo.
