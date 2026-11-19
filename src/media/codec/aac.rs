//! AAC-LC decoding (symphonia) for opening third-party videos.
//!
//! Only plain AAC-LC with one or two channels is supported (what phones,
//! screen recorders and most web video use). HE-AAC signalled implicitly
//! (an LC config with SBR data in the stream) decodes as its LC core at half
//! the rate: duller but correctly timed.

use anyhow::{Context, Result, bail, ensure};
use symphonia_codec_aac::AacDecoder;
use symphonia_core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder as _, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration as Ticks, Timestamp};

use super::AudioDecoder;
use crate::media::AudioSpec;

/// Samples per channel of an AAC-LC frame.
const FRAME: u64 = 1024;

pub struct Decoder {
    inner: AacDecoder,
    spec: AudioSpec,
}

impl Decoder {
    /// `audio_specific_config` as MP4 `esds` / Matroska `CodecPrivate` store
    /// it; when empty (raw Matroska tracks without one), `spec` describes
    /// the stream instead.
    pub fn new(audio_specific_config: &[u8], spec: AudioSpec) -> Result<Self> {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC);
        let channels = if audio_specific_config.is_empty() {
            params.with_sample_rate(spec.rate);
            spec.channels
        } else {
            let config = Config::parse(audio_specific_config)?;
            params.with_extra_data(audio_specific_config.into());
            config.channels
        };
        params.with_channels(match channels {
            1 => CHANNEL_LAYOUT_MONO,
            2 => CHANNEL_LAYOUT_STEREO,
            n => bail!("AAC with {n} channels is not supported (only mono and stereo)"),
        });
        let inner = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
            .context("Unsupported AAC stream")?;
        let decoded = inner.codec_params();
        let spec = AudioSpec {
            rate: decoded
                .sample_rate
                .context("AAC stream without a sample rate")?,
            channels: decoded
                .channels
                .as_ref()
                .map_or(channels, |c| c.count() as u16),
        };
        Ok(Decoder { inner, spec })
    }
}

impl AudioDecoder for Decoder {
    fn spec(&self) -> AudioSpec {
        self.spec
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        let packet = PacketRef::new(0, Timestamp::new(0), Ticks::new(FRAME), packet);
        let mut samples = Vec::new();
        self.inner
            .decode_ref(&packet)
            .context("Corrupt AAC packet")?
            .copy_to_vec_interleaved(&mut samples);
        Ok(samples)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// The leading fields of an AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1),
/// read to explain what symphonia would only call "too complex".
struct Config {
    channels: u16,
}

impl Config {
    fn parse(data: &[u8]) -> Result<Config> {
        let mut bits = Bits { data, at: 0 };
        let mut object_type = bits.read(5)?;
        if object_type == 31 {
            object_type = 32 + bits.read(6)?;
        }
        if bits.read(4)? == 15 {
            bits.read(24)?; // explicit sampling frequency
        }
        let channel_config = bits.read(4)?;
        match object_type {
            2 => {}
            5 | 29 => bail!("HE-AAC is not supported (only AAC-LC)"),
            other => bail!("AAC object type {other} is not supported (only AAC-LC)"),
        }
        ensure!(
            matches!(channel_config, 1 | 2),
            "AAC channel configuration {channel_config} is not supported (only mono and stereo)"
        );
        Ok(Config {
            channels: channel_config as u16,
        })
    }
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = self
                .data
                .get(self.at / 8)
                .context("Truncated AAC AudioSpecificConfig")?;
            value = value << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn rejects_unsupported_configs() {
        // object type 2 (LC), 44.1 kHz (index 4), stereo
        let lc = [0x12, 0x10];
        let decoder = Decoder::new(
            &lc,
            AudioSpec {
                rate: 0,
                channels: 0,
            },
        )
        .unwrap();
        assert_eq!(
            decoder.spec(),
            AudioSpec {
                rate: 44_100,
                channels: 2
            }
        );

        // object type 5 (SBR), 24 kHz core, stereo, 48 kHz extension, LC
        let he = [0x2b, 0x11, 0x88, 0x00];
        let error = Decoder::new(
            &he,
            AudioSpec {
                rate: 48_000,
                channels: 2,
            },
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("HE-AAC"), "{error:#}");

        // LC 48 kHz (index 3), 5.1 (configuration 6)
        let surround = [0x11, 0xb0];
        let error = Decoder::new(
            &surround,
            AudioSpec {
                rate: 48_000,
                channels: 6,
            },
        )
        .err()
        .unwrap();
        assert!(
            format!("{error:#}").contains("configuration 6"),
            "{error:#}"
        );

        assert!(
            Decoder::new(
                &[0x12],
                AudioSpec {
                    rate: 44_100,
                    channels: 2
                }
            )
            .is_err()
        );
    }

    /// Decodes AAC-LC made by the ffmpeg CLI, when installed (an external
    /// encoder, so the test checks interoperability, not a round trip).
    #[test]
    fn decodes_ffmpeg_aac() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.aac");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:sample_rate=44100:duration=1")
            .args(["-ac", "2", "-c:a", "aac", "-b:a", "128k", "-f", "adts"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        }
        let adts = std::fs::read(&path).unwrap();
        // ADTS header: profile (object type − 1), frequency index, channels.
        let profile = adts[2] >> 6;
        let index = adts[2] >> 2 & 0xf;
        let channels = (adts[2] & 1) << 2 | adts[3] >> 6;
        let config = [
            (profile + 1) << 3 | index >> 1,
            (index & 1) << 7 | channels << 3,
        ];
        let mut decoder = Decoder::new(
            &config,
            AudioSpec {
                rate: 0,
                channels: 0,
            },
        )
        .unwrap();
        assert_eq!(
            decoder.spec(),
            AudioSpec {
                rate: 44_100,
                channels: 2
            }
        );

        let mut samples = Vec::new();
        let mut rest = &adts[..];
        while rest.len() > 7 {
            let length = usize::from(rest[3] & 3) << 11
                | usize::from(rest[4]) << 3
                | usize::from(rest[5] >> 5);
            let header = if rest[1] & 1 == 0 { 9 } else { 7 };
            samples.extend(decoder.decode(&rest[header..length]).unwrap());
            rest = &rest[length..];
        }
        // ffmpeg's encoder adds 1024 frames of priming.
        assert!(samples.len() / 2 >= 44_100, "{} frames", samples.len() / 2);
        let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let middle = &left[4410..39_690];
        let crossings = middle
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        // 0.8 s of 440 Hz.
        assert!((350..=354).contains(&crossings), "{crossings} crossings");
        let peak = middle.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05, "peak {peak}");
    }
}
