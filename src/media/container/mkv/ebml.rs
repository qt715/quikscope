//! Just enough raw EBML reading to find what `matroska-demuxer` does not
//! expose: cue positions, the first and last cluster's block times.

use std::io::{self, Read, Seek, SeekFrom};

pub const SEGMENT: u32 = 0x1853_8067;
pub const SEEK_HEAD: u32 = 0x114D_9B74;
pub const SEEK: u32 = 0x4DBB;
pub const SEEK_ID: u32 = 0x53AB;
pub const SEEK_POSITION: u32 = 0x53AC;
pub const CUES: u32 = 0x1C53_BB6B;
pub const CUE_POINT: u32 = 0xBB;
pub const CUE_TIME: u32 = 0xB3;
pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
pub const CUE_RELATIVE_POSITION: u32 = 0xF0;
pub const CLUSTER: u32 = 0x1F43_B675;
pub const TIMESTAMP: u32 = 0xE7;
pub const SIMPLE_BLOCK: u32 = 0xA3;
pub const BLOCK_GROUP: u32 = 0xA0;
pub const BLOCK: u32 = 0xA1;
pub const BLOCK_DURATION: u32 = 0x9B;
pub const VOID: u32 = 0xEC;
/// Segment children that end an unknown-size cluster.
const TOP_LEVEL: [u32; 8] = [
    CLUSTER,
    CUES,
    SEEK_HEAD,
    0x1549_A966,
    0x1654_AE6B,
    0x1254_C367,
    0x1043_A770,
    0x1941_A469,
];

#[derive(Clone, Copy, Debug)]
pub struct Element {
    pub id: u32,
    /// Position of the ID's first byte.
    pub start: u64,
    /// Position of the payload.
    pub data: u64,
    /// `None` for an unknown size (runs until a parent-level element).
    pub size: Option<u64>,
}

impl Element {
    pub fn end(&self) -> Option<u64> {
        self.size.map(|s| self.data + s)
    }
}

/// The element starting at `pos`.
pub fn element_at<R: Read + Seek>(r: &mut R, pos: u64) -> io::Result<Element> {
    r.seek(SeekFrom::Start(pos))?;
    let (id, id_len) = vint(r)?;
    let (size, size_len) = vint(r)?;
    let id_len_bits = 7 * id_len as u32;
    let marker = 1u64 << id_len_bits;
    let unknown = size == (1u64 << (7 * size_len as u32)) - 1;
    Ok(Element {
        id: u32::try_from(id | marker).map_err(|_| invalid("EBML ID over 4 bytes"))?,
        start: pos,
        data: pos + (id_len + size_len) as u64,
        size: (!unknown).then_some(size),
    })
}

/// A variable-length integer without its length marker, and its length.
fn vint(r: &mut impl Read) -> io::Result<(u64, usize)> {
    let mut first = [0u8];
    r.read_exact(&mut first)?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 8 {
        return Err(invalid("Invalid EBML variable-length integer"));
    }
    let mut value = u64::from(first[0]) & (0xFF >> len);
    let mut rest = [0u8; 7];
    r.read_exact(&mut rest[..len - 1])?;
    for &b in &rest[..len - 1] {
        value = value << 8 | u64::from(b);
    }
    Ok((value, len))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn read_uint<R: Read + Seek>(r: &mut R, el: &Element) -> io::Result<u64> {
    let size = el.size.unwrap_or(0);
    if size > 8 {
        return Err(invalid("Integer element over 8 bytes"));
    }
    r.seek(SeekFrom::Start(el.data))?;
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf[..size as usize])?;
    Ok(buf[..size as usize]
        .iter()
        .fold(0, |n, &b| n << 8 | u64::from(b)))
}

pub fn read_binary<R: Read + Seek>(r: &mut R, el: &Element, max: u64) -> io::Result<Vec<u8>> {
    let size = el.size.unwrap_or(0).min(max);
    r.seek(SeekFrom::Start(el.data))?;
    let mut buf = vec![0; size as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// The children of a known-size element, stopping early at a read error
/// (truncated file).
pub fn children<R: Read + Seek>(r: &mut R, parent: &Element) -> Vec<Element> {
    let Some(end) = parent.end() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut pos = parent.data;
    while pos < end {
        let Ok(child) = element_at(r, pos) else { break };
        let Some(child_end) = child.end() else { break };
        out.push(child);
        pos = child_end;
    }
    out
}

/// A block's track number and time relative to its cluster.
pub struct BlockTime {
    pub track: u64,
    pub relative: i16,
    /// From a BlockGroup's BlockDuration, in ticks.
    pub duration: Option<u64>,
}

/// The cluster's timestamp and its blocks' times, and where the cluster
/// ends (for unknown sizes, the next parent-level element or end of file).
pub fn cluster_blocks<R: Read + Seek>(
    r: &mut R,
    cluster: &Element,
    file_len: u64,
) -> (Option<u64>, Vec<BlockTime>, u64) {
    let end = cluster.end().unwrap_or(file_len).min(file_len);
    let mut timestamp = None;
    let mut blocks = Vec::new();
    let mut pos = cluster.data;
    while pos < end {
        let Ok(child) = element_at(r, pos) else { break };
        if cluster.size.is_none() && TOP_LEVEL.contains(&child.id) {
            return (timestamp, blocks, pos);
        }
        match child.id {
            TIMESTAMP => timestamp = read_uint(r, &child).ok(),
            SIMPLE_BLOCK => blocks.extend(block_time(r, &child, None)),
            BLOCK_GROUP => {
                let parts = children(r, &child);
                let duration = parts
                    .iter()
                    .find(|e| e.id == BLOCK_DURATION)
                    .and_then(|e| read_uint(r, e).ok());
                if let Some(block) = parts.iter().find(|e| e.id == BLOCK) {
                    blocks.extend(block_time(r, block, duration));
                }
            }
            _ => {}
        }
        let Some(next) = child.end() else { break };
        pos = next;
    }
    (timestamp, blocks, end)
}

fn block_time<R: Read + Seek>(
    r: &mut R,
    block: &Element,
    duration: Option<u64>,
) -> Option<BlockTime> {
    r.seek(SeekFrom::Start(block.data)).ok()?;
    let (track, _) = vint(r).ok()?;
    let mut time = [0u8; 2];
    r.read_exact(&mut time).ok()?;
    Some(BlockTime {
        track,
        relative: i16::from_be_bytes(time),
        duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_ids_sizes_and_unknown_sizes() {
        let mut data = Cursor::new(vec![
            0x1A, 0x45, 0xDF, 0xA3, 0x84, 0, 0, 0, 0, 0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        let ebml = element_at(&mut data, 0).unwrap();
        assert_eq!((ebml.id, ebml.data, ebml.size), (0x1A45_DFA3, 5, Some(4)));
        let segment = element_at(&mut data, 9).unwrap();
        assert_eq!(
            (segment.id, segment.data, segment.size),
            (SEGMENT, 21, None)
        );
    }
}
