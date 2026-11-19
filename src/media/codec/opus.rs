//! Opus (libopus): every audio track quikscope writes is 48 kHz stereo Opus
//! in 20 ms packets.
//!
//! Timing convention shared with the containers: a packet's pts is the media
//! time of its first *decoded* sample, counting the encoder's pre-skip as
//! part of the stream (Ogg granule position / 48 kHz, Matroska block time,
//! MP4 media time before any edit list). [`Decoder`] discards the pre-skip
//! itself after creation and after every [`reset`](super::AudioDecoder::reset),
//! so the first sample it returns after a (re)start belongs at that packet's
//! pts and later output follows contiguously.

use anyhow::{Result, bail, ensure};
use opus::{Application, Bitrate, Channels};

use super::AudioDecoder;
use crate::media::AudioSpec;

pub const RATE: u32 = 48_000;
/// Samples per channel per packet (20 ms).
pub const FRAME: usize = 960;
/// Bitrate for recordings: transparent for desktop audio and voice.
pub const BITRATE: u32 = 160_000;

/// Largest packet libopus can produce (RFC 6716 §3.4: 120 ms of 1275-byte
/// frames plus framing).
const MAX_PACKET: usize = 4000;
/// Samples per channel of the longest packet (120 ms).
const MAX_PACKET_FRAMES: usize = 5760;

/// Stereo 48 kHz encoder, `Application::Audio`, VBR.
pub struct Encoder {
    inner: opus::Encoder,
    pre_skip: u16,
}

impl Encoder {
    pub fn new(bitrate: u32) -> Result<Self> {
        let mut inner = opus::Encoder::new(RATE, Channels::Stereo, Application::Audio)?;
        inner.set_bitrate(Bitrate::Bits(i32::try_from(bitrate)?))?;
        inner.set_vbr(true)?;
        let pre_skip = u16::try_from(inner.get_lookahead()?)?;
        Ok(Encoder { inner, pre_skip })
    }

    /// Samples per channel of encoder delay at the start of the stream, which
    /// players discard (OpusHead pre-skip, MP4 edit list, Matroska
    /// `CodecDelay`).
    pub fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    /// The OpusHead describing this stream (`codec_private` of the track).
    pub fn head(&self) -> Vec<u8> {
        Head {
            channels: 2,
            pre_skip: self.pre_skip,
            input_rate: RATE,
            gain: 0,
            mapping: None,
        }
        .to_bytes()
    }

    /// Encodes one packet from exactly [`FRAME`] interleaved stereo frames.
    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        ensure!(
            samples.len() == FRAME * 2,
            "Opus packets take {} samples, got {}",
            FRAME * 2,
            samples.len()
        );
        Ok(self.inner.encode_vec_float(samples, MAX_PACKET)?)
    }
}

/// Identification header, RFC 7845 §5.1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub channels: u8,
    pub pre_skip: u16,
    /// Rate of the original input; informational, Opus always decodes at
    /// 48 kHz.
    pub input_rate: u32,
    /// Output gain in Q7.8 dB.
    pub gain: i16,
    /// `None` for mapping family 0 (one mono or stereo stream).
    pub mapping: Option<ChannelMapping>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelMapping {
    /// 1 = Vorbis channel order, 255 = undefined order; others are unknown.
    pub family: u8,
    pub streams: u8,
    pub coupled: u8,
    /// Decoded channel index per output channel (255 = silent).
    pub table: Vec<u8>,
}

impl Head {
    const MAGIC: &[u8; 8] = b"OpusHead";

    pub fn parse(data: &[u8]) -> Result<Head> {
        ensure!(
            data.len() >= 19 && data.starts_with(Self::MAGIC),
            "Not an OpusHead"
        );
        // Minor versions stay compatible; a new major version would not be.
        ensure!(
            data[8] >> 4 == 0,
            "Unsupported OpusHead version {}",
            data[8]
        );
        let channels = data[9];
        ensure!(channels > 0, "OpusHead with no channels");
        let mapping = match data[18] {
            0 => {
                ensure!(
                    channels <= 2,
                    "OpusHead mapping family 0 with {channels} channels"
                );
                None
            }
            family => {
                let table = data
                    .get(21..21 + usize::from(channels))
                    .ok_or_else(|| anyhow::anyhow!("Truncated OpusHead channel mapping"))?;
                let (streams, coupled) = (data[19], data[20]);
                ensure!(
                    streams > 0 && coupled <= streams,
                    "Invalid OpusHead stream counts {streams}/{coupled}"
                );
                let decoded = u16::from(streams) + u16::from(coupled);
                ensure!(
                    table.iter().all(|&c| c == 255 || u16::from(c) < decoded),
                    "Invalid OpusHead channel mapping"
                );
                Some(ChannelMapping {
                    family,
                    streams,
                    coupled,
                    table: table.to_vec(),
                })
            }
        };
        Ok(Head {
            channels,
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            input_rate: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            gain: i16::from_le_bytes([data[16], data[17]]),
            mapping,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(21 + usize::from(self.channels));
        out.extend_from_slice(Self::MAGIC);
        out.push(1);
        out.push(self.channels);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_rate.to_le_bytes());
        out.extend_from_slice(&self.gain.to_le_bytes());
        match &self.mapping {
            None => out.push(0),
            Some(mapping) => {
                out.extend_from_slice(&[mapping.family, mapping.streams, mapping.coupled]);
                out.extend_from_slice(&mapping.table);
            }
        }
        out
    }
}

/// Decodes any OpusHead layout at 48 kHz. Channels come out in WAVE order
/// (mapping family 1 is reordered from Vorbis order) so
/// [`super::resample`] can downmix them; the pre-skip is discarded as
/// described in the module docs.
pub struct Decoder {
    inner: opus::MSDecoder,
    channels: usize,
    pre_skip: usize,
    /// Pre-skip frames still to discard.
    skip: usize,
    /// WAVE channel `i` is decoded channel `order[i]`.
    order: Option<&'static [usize]>,
    buffer: Vec<f32>,
}

impl Decoder {
    pub fn new(head: &[u8]) -> Result<Self> {
        let head = Head::parse(head)?;
        let channels = usize::from(head.channels);
        let (streams, coupled, table, order) = match head.mapping {
            None => (1, head.channels - 1, (0..head.channels).collect(), None),
            Some(mapping) => {
                let order = match mapping.family {
                    1 => vorbis_to_wave(channels),
                    255 => None,
                    family => bail!("Unsupported Opus channel mapping family {family}"),
                };
                (mapping.streams, mapping.coupled, mapping.table, order)
            }
        };
        let mut inner = opus::MSDecoder::new(RATE, streams, coupled, &table)?;
        if head.gain != 0 {
            inner.set_gain(i32::from(head.gain))?;
        }
        Ok(Decoder {
            inner,
            channels,
            pre_skip: usize::from(head.pre_skip),
            skip: usize::from(head.pre_skip),
            order,
            buffer: vec![0.0; MAX_PACKET_FRAMES * channels],
        })
    }
}

impl AudioDecoder for Decoder {
    fn spec(&self) -> AudioSpec {
        AudioSpec {
            rate: RATE,
            channels: self.channels as u16,
        }
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        // An empty packet would ask libopus to conceal a loss of unknown
        // length; demuxers never produce one for real data.
        if packet.is_empty() {
            return Ok(Vec::new());
        }
        let frames = self.inner.decode_float(packet, &mut self.buffer, false)?;
        let skipped = self.skip.min(frames);
        self.skip -= skipped;
        let decoded = &self.buffer[skipped * self.channels..frames * self.channels];
        Ok(match self.order {
            None => decoded.to_vec(),
            Some(order) => decoded
                .chunks_exact(self.channels)
                .flat_map(|frame| order.iter().map(|&c| frame[c]))
                .collect(),
        })
    }

    fn reset(&mut self) {
        // OPUS_RESET_STATE cannot fail on a valid decoder.
        if let Err(e) = self.inner.reset_state() {
            log::warn!("Resetting the Opus decoder failed: {e}");
        }
        self.skip = self.pre_skip;
    }
}

/// Vorbis channel order (RFC 7845 §5.1.1.2) to WAVE order.
fn vorbis_to_wave(channels: usize) -> Option<&'static [usize]> {
    Some(match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 5, 3, 4],
        7 => &[0, 2, 1, 6, 5, 3, 4],
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / RATE as f32;
                let s = 0.5 * (std::f32::consts::TAU * 440.0 * t).sin();
                [s, s]
            })
            .collect()
    }

    fn snr_db(reference: &[f32], decoded: &[f32]) -> f64 {
        let (signal, noise) = reference
            .iter()
            .zip(decoded)
            .fold((0.0, 0.0), |(s, n), (&r, &d)| {
                (s + f64::from(r).powi(2), n + f64::from(r - d).powi(2))
            });
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn head_round_trip() {
        let encoder = Encoder::new(BITRATE).unwrap();
        let bytes = encoder.head();
        let pre_skip = encoder.pre_skip().to_le_bytes();
        assert_eq!(
            bytes,
            [
                b'O',
                b'p',
                b'u',
                b's',
                b'H',
                b'e',
                b'a',
                b'd',
                1,
                2,
                pre_skip[0],
                pre_skip[1],
                0x80,
                0xbb,
                0,
                0,
                0,
                0,
                0
            ]
        );
        let head = Head::parse(&bytes).unwrap();
        assert_eq!(head.channels, 2);
        assert_eq!(head.pre_skip, encoder.pre_skip());
        assert_eq!(head.input_rate, 48_000);
        assert_eq!(head.to_bytes(), bytes);

        let surround = Head {
            channels: 6,
            pre_skip: 312,
            input_rate: 44_100,
            gain: -256,
            mapping: Some(ChannelMapping {
                family: 1,
                streams: 4,
                coupled: 2,
                table: vec![0, 4, 1, 2, 3, 5],
            }),
        };
        assert_eq!(Head::parse(&surround.to_bytes()).unwrap(), surround);
        assert!(Decoder::new(&surround.to_bytes()).unwrap().spec().channels == 6);

        assert!(Head::parse(b"OpusTags\x01\x02\0\0\0\0\0\0\0\0\0").is_err());
        let mut future = bytes.clone();
        future[8] = 0x10;
        assert!(Head::parse(&future).is_err());
        let mut three = bytes;
        three[9] = 3;
        assert!(Head::parse(&three).is_err(), "family 0 is mono or stereo");
    }

    #[test]
    fn encode_decode_round_trip() {
        const PACKETS: usize = 100;
        let input = sine(FRAME * PACKETS);
        let mut encoder = Encoder::new(BITRATE).unwrap();
        let pre_skip = usize::from(encoder.pre_skip());
        assert!(pre_skip > 0 && pre_skip < FRAME);
        let packets: Vec<Vec<u8>> = input
            .chunks_exact(FRAME * 2)
            .map(|frame| encoder.encode(frame).unwrap())
            .collect();
        assert!(encoder.encode(&input[..FRAME]).is_err());

        // VBR around 160 kb/s: 400 bytes per 20 ms on average.
        let total: usize = packets.iter().map(Vec::len).sum();
        assert!(packets.iter().all(|p| !p.is_empty() && p.len() <= 1275));
        assert!(
            (200 * PACKETS..=600 * PACKETS).contains(&total),
            "{total} bytes"
        );

        let mut decoder = Decoder::new(&encoder.head()).unwrap();
        assert_eq!(
            decoder.spec(),
            AudioSpec {
                rate: 48_000,
                channels: 2
            }
        );
        let mut decoded = Vec::new();
        for (i, packet) in packets.iter().enumerate() {
            let samples = decoder.decode(packet).unwrap();
            let expect = if i == 0 { FRAME - pre_skip } else { FRAME };
            assert_eq!(samples.len(), expect * 2, "packet {i}");
            decoded.extend(samples);
        }
        // Pre-skip dropped: decoded sample k is input sample k.
        let snr = snr_db(&input[2 * FRAME..decoded.len()], &decoded[2 * FRAME..]);
        assert!(snr > 25.0, "SNR {snr:.1} dB");

        // After a reset (seek) the pre-skip is dropped again from the first
        // packet decoded.
        decoder.reset();
        assert_eq!(
            decoder.decode(&packets[50]).unwrap().len(),
            (FRAME - pre_skip) * 2
        );
        assert!(decoder.decode(&[]).unwrap().is_empty());
    }

    #[test]
    fn surround_is_reordered_to_wave() {
        // Six channels as three mono streams (identity mapping isn't
        // possible for coupled pairs) with a marker on the Vorbis centre.
        let head = Head {
            channels: 6,
            pre_skip: 0,
            input_rate: RATE,
            gain: 0,
            mapping: Some(ChannelMapping {
                family: 1,
                streams: 6,
                coupled: 0,
                table: vec![0, 1, 2, 3, 4, 5],
            }),
        };
        let mut encoder =
            opus::MSEncoder::new(RATE, 6, 0, &[0, 1, 2, 3, 4, 5], Application::Audio).unwrap();
        let mut decoder = Decoder::new(&head.to_bytes()).unwrap();
        let mut centre = Vec::new();
        let mut front_right = Vec::new();
        for packet in 0..10 {
            let input: Vec<f32> = (0..FRAME)
                .flat_map(|i| {
                    let t = (packet * FRAME + i) as f32 / RATE as f32;
                    let s = 0.5 * (std::f32::consts::TAU * 440.0 * t).sin();
                    // Vorbis order: FL C FR RL RR LFE; tone on C only.
                    [0.0, s, 0.0, 0.0, 0.0, 0.0]
                })
                .collect();
            let mut out = vec![0; MAX_PACKET * 6];
            let len = encoder.encode_float(&input, &mut out).unwrap();
            for frame in decoder.decode(&out[..len]).unwrap().chunks_exact(6) {
                centre.push(frame[2]);
                front_right.push(frame[1]);
            }
        }
        let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>();
        assert!(energy(&centre) > 100.0 * energy(&front_right));
    }
}
