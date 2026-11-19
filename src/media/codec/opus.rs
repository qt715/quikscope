//! Opus (libopus): every audio track quikscope writes is 48 kHz stereo Opus
//! in 20 ms packets.

// AudioCodecs owns this file. Contract (see local://media-plan.md):
// pub const RATE: u32 = 48_000;
// pub const FRAME: usize = 960; // samples per channel per packet
// pub struct Encoder; Encoder::new(bitrate) -> Result<Self>; pre_skip() -> u16;
//   head() -> Vec<u8> (OpusHead, RFC 7845 §5.1); encode(&mut self, &[f32; FRAME*2 interleaved]) -> Result<Vec<u8>>
// pub struct Decoder: super::AudioDecoder; Decoder::new(head: &[u8]) -> Result<Self>
