//! H.264 bitstream plumbing between the Annex-B form at the container API
//! and the length-prefixed samples plus `avcC` record files store.

use anyhow::{Context, Result, bail};
use mp4_atom::{Avcc, AvccExt};

const SPS: u8 = 7;
const PPS: u8 = 8;
const AUD: u8 = 9;
const IDR: u8 = 5;

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

/// The NAL units of an Annex-B stream, without start codes or the trailing
/// zero bytes that pad before the next start code.
pub fn annexb_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = match find_start(data) {
        Some((_, payload)) => &data[payload..],
        None => &data[data.len()..],
    };
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (nal, next) = match find_start(rest) {
            Some((code, payload)) => (&rest[..code], &rest[payload..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = next;
        let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        Some(&nal[..end])
    })
    .filter(|nal| !nal.is_empty())
}

/// Position of the first `00 00 01` and of the byte after it.
fn find_start(data: &[u8]) -> Option<(usize, usize)> {
    data.windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|i| (i, i + 3))
}

/// The length-prefixed NAL units of a sample (`length_size` 1–4 bytes).
pub fn sample_nals(sample: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> {
    let mut rest = sample;
    std::iter::from_fn(move || {
        if rest.len() < length_size || length_size == 0 {
            return None;
        }
        let len = rest[..length_size]
            .iter()
            .fold(0usize, |n, &b| n << 8 | usize::from(b));
        let nal = rest.get(length_size..length_size + len)?;
        rest = &rest[length_size + len..];
        Some(nal)
    })
}

/// `avcC` from the first SPS and PPS of an Annex-B keyframe. High profiles
/// get the extension fields for the 8-bit 4:2:0 our encoders produce.
pub fn avcc_from_keyframe(annexb: &[u8]) -> Result<Avcc> {
    let sps = annexb_nals(annexb)
        .find(|n| nal_type(n) == SPS)
        .context("The first H.264 keyframe has no SPS")?;
    let pps = annexb_nals(annexb)
        .find(|n| nal_type(n) == PPS)
        .context("The first H.264 keyframe has no PPS")?;
    let mut avcc = Avcc::new(sps, pps)?;
    if matches!(avcc.avc_profile_indication, 100 | 110 | 122 | 144) {
        avcc.ext = Some(AvccExt::default());
    }
    Ok(avcc)
}

/// Annex-B to a sample with 4-byte lengths. Access unit delimiters and
/// parameter sets already in `avcc` are dropped; a parameter set that
/// differs stays in band so a mid-stream change still decodes.
pub fn annexb_to_sample(annexb: &[u8], avcc: &Avcc, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    out.reserve(annexb.len() + 8);
    for nal in annexb_nals(annexb) {
        let redundant = match nal_type(nal) {
            AUD => true,
            SPS => avcc.sequence_parameter_sets.iter().any(|s| s == nal),
            PPS => avcc.picture_parameter_sets.iter().any(|p| p == nal),
            _ => false,
        };
        if !redundant {
            let Ok(len) = u32::try_from(nal.len()) else {
                bail!("H.264 NAL unit of {} bytes", nal.len());
            };
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(nal);
        }
    }
    Ok(())
}

/// The `avcC` parameter sets as Annex-B, to put before keyframes.
pub fn parameter_sets(avcc: &Avcc) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in avcc
        .sequence_parameter_sets
        .iter()
        .chain(&avcc.picture_parameter_sets)
    {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// A stored sample back to Annex-B, with `params` (see [`parameter_sets`])
/// in front of keyframes that do not carry their own SPS.
pub fn sample_to_annexb(
    sample: &[u8],
    length_size: usize,
    keyframe: bool,
    params: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.len() + params.len() + 16);
    if keyframe && !sample_nals(sample, length_size).any(|n| nal_type(n) == SPS) {
        out.extend_from_slice(params);
    }
    for nal in sample_nals(sample, length_size) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// Whether a stored sample holds an IDR slice (for containers that do not
/// flag keyframes themselves).
pub fn sample_is_idr(sample: &[u8], length_size: usize) -> bool {
    sample_nals(sample, length_size).any(|n| nal_type(n) == IDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes_and_strips_padding() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let nals: Vec<&[u8]> = annexb_nals(&stream).collect();
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn sample_round_trip_moves_parameter_sets_to_avcc() {
        let stream = [
            0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 0x64, 0, 0x1F, 0, 0, 1, 0x68, 3, 0, 0, 1,
            0x65, 4,
        ];
        let avcc = avcc_from_keyframe(&stream).unwrap();
        assert_eq!(avcc.avc_profile_indication, 100);
        assert!(avcc.ext.is_some());
        let mut sample = Vec::new();
        annexb_to_sample(&stream, &avcc, &mut sample).unwrap();
        assert_eq!(sample, [0, 0, 0, 2, 0x65, 4]);
        assert!(sample_is_idr(&sample, 4));
        let back = sample_to_annexb(&sample, 4, true, &parameter_sets(&avcc));
        assert_eq!(
            back,
            [
                0, 0, 0, 1, 0x67, 0x64, 0, 0x1F, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4
            ]
        );
        // Truncated samples stop at the last complete NAL instead of panicking.
        assert_eq!(sample_nals(&sample[..5], 4).count(), 0);
    }
}
