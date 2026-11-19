//! AVIF: rav1e for writing image sequences, rav1d (through its dav1d C API)
//! for reading still images and sequences; ISOBMFF boxes with mp4-atom.
//!
//! Written files follow libavif's layout for animations: an `avis` file
//! whose `meta` box makes the first frame the primary (still) image and
//! whose `moov` track holds every frame, set to loop forever. Frames are
//! 8-bit 4:2:0 BT.709 (limited range). Transparency is not written: frames
//! are flattened onto black (AVIF alpha needs a second, auxiliary track).
//!
//! Reading covers the primary item of still images (single or grid, with
//! an alpha auxiliary item) and the track of sequences (with an alpha
//! track), 8 to 12 bits (rounded to 8), any chroma layout, and the matrix
//! coefficients of the stream or `colr` box. Image rotation and mirroring
//! properties are ignored.

use std::borrow::Cow;
use std::ffi::c_int;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::mem::MaybeUninit;
use std::path::Path;
use std::ptr::NonNull;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use mp4_atom::{
    Any, Atom, Av01, Av1c, Ccst, Co64, Colr, DecodeAtom, DecodeMaybe, Dinf, Dref, Edts, Elst,
    ElstEntry, Encode, FourCC, Ftyp, Hdlr, Header, Iinf, Iloc, Ipco, Ipma, Iprp, Iref, Ispe,
    ItemInfoEntry, ItemLocation, ItemLocationExtent, Matrix, Mdhd, Mdia, Meta, Minf, Moov, Mvhd,
    Pitm, Pixi, PropertyAssociation, PropertyAssociations, Stbl, Stco, Stsc, StscEntry, Stsd, Stss,
    Stsz, StszSamples, Stts, SttsEntry, Tkhd, Trak, Url, Visual, Vmhd,
};
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
    DAV1D_PIXEL_LAYOUT_I444,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};
use rav1e::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, Config, Context as Rav1eContext,
    EncoderConfig, EncoderStatus, FrameType, MatrixCoefficients, PixelRange, Rational,
    TransferCharacteristics,
};
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvGrayImage, YuvPlanarImage, YuvPlanarImageMut,
    YuvRange, YuvStandardMatrix,
};

use super::{AnimDecoder, AnimEncoder, AnimFrame, AnimInfo, TickClock};
use crate::raster::Image;

/// rav1e speed preset: 0 (slowest, smallest) to 10.
const SPEED: u8 = 8;
/// rav1e quantizer (0-255); low enough to keep text crisp.
const QUANTIZER: usize = 80;

// H.273 code points (as in `colr` boxes and the AV1 color config).
const MC_IDENTITY: u32 = 0;
const MC_BT709: u32 = 1;
const MC_UNSPECIFIED: u32 = 2;
const MC_FCC: u32 = 4;
const MC_BT470BG: u32 = 5;
const MC_BT601: u32 = 6;
const MC_SMPTE240: u32 = 7;
const MC_BT2020_NCL: u32 = 9;

/// Where the coded bytes of one frame (or one image item) are.
#[derive(Clone, Debug)]
enum Coded {
    /// `(offset, length)` ranges of the file, concatenated.
    File(Vec<(usize, usize)>),
    /// Bytes stored in the `idat` box.
    Inline(Vec<u8>),
}

impl Coded {
    fn bytes(&self, file: &[u8]) -> Result<Vec<u8>> {
        match self {
            Coded::Inline(data) => Ok(data.clone()),
            Coded::File(ranges) => {
                let mut out = Vec::new();
                for &(offset, len) in ranges {
                    let range = file
                        .get(offset..offset.checked_add(len).context("AVIF extent overflows")?)
                        .context("AVIF data outside the file")?;
                    out.extend_from_slice(range);
                }
                Ok(out)
            }
        }
    }
}

/// An image made of AV1 frames: one coded frame, or a grid of tiles.
#[derive(Clone, Debug)]
enum Picture {
    Single(Coded),
    Grid {
        rows: usize,
        columns: usize,
        width: u32,
        height: u32,
        tiles: Vec<Coded>,
    },
}

/// What a file shows, frame by frame.
struct Source {
    width: u32,
    height: u32,
    color: Vec<Picture>,
    alpha: Option<Vec<Picture>>,
    durations: Vec<Duration>,
    /// Matrix coefficients and full-range flag from the `colr` box, used
    /// when the AV1 stream leaves them unspecified.
    colr: Option<(u32, bool)>,
}

pub struct AvifDecoder {
    data: Vec<u8>,
    source: Source,
    info: AnimInfo,
    color: Av1Decoder,
    alpha: Option<Av1Decoder>,
    next: usize,
    pts: Duration,
}

impl AvifDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let source = parse(&data).with_context(|| format!("reading AVIF {}", path.display()))?;
        ensure!(
            !source.color.is_empty(),
            "AVIF {} has no frames",
            path.display()
        );
        let info = AnimInfo {
            width: source.width,
            height: source.height,
            frames: source.color.len() as u64,
            duration: source.durations.iter().sum(),
        };
        Ok(AvifDecoder {
            color: Av1Decoder::new()?,
            alpha: source
                .alpha
                .as_ref()
                .map(|_| Av1Decoder::new())
                .transpose()?,
            data,
            source,
            info,
            next: 0,
            pts: Duration::ZERO,
        })
    }
}

impl AnimDecoder for AvifDecoder {
    fn info(&self) -> &AnimInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<AnimFrame>> {
        let i = self.next;
        let Some(picture) = self.source.color.get(i) else {
            return Ok(None);
        };
        // Opaque unless an alpha plane says otherwise.
        let mut image = Image::filled(self.info.width, self.info.height, [0, 0, 0, 255]);
        let colr = self.source.colr;
        decode_picture(
            &mut self.color,
            &self.data,
            picture,
            &mut image,
            |pic, out, x, y| pic.color_into(out, x, y, colr),
        )
        .with_context(|| format!("decoding AVIF frame {i}"))?;
        if let (Some(decoder), Some(alpha)) = (&mut self.alpha, &self.source.alpha)
            && let Some(picture) = alpha.get(i)
        {
            decode_picture(
                decoder,
                &self.data,
                picture,
                &mut image,
                |pic, out, x, y| pic.alpha_into(out, x, y),
            )
            .with_context(|| format!("decoding AVIF alpha {i}"))?;
        }
        self.next += 1;
        let duration = self.source.durations[i];
        let pts = self.pts;
        self.pts += duration;
        Ok(Some(AnimFrame {
            image,
            pts,
            duration,
        }))
    }

    fn rewind(&mut self) -> Result<()> {
        // Sequence frames reference earlier ones; start a fresh decoder.
        self.color = Av1Decoder::new()?;
        if self.alpha.is_some() {
            self.alpha = Some(Av1Decoder::new()?);
        }
        self.next = 0;
        self.pts = Duration::ZERO;
        Ok(())
    }
}

/// Decodes `picture` and hands each decoded frame (one, or every grid tile)
/// to `put` with its position in `out`.
fn decode_picture(
    decoder: &mut Av1Decoder,
    file: &[u8],
    picture: &Picture,
    out: &mut Image,
    put: impl Fn(&DecodedPicture, &mut Image, u32, u32) -> Result<()>,
) -> Result<()> {
    match picture {
        Picture::Single(coded) => {
            let decoded = decoder.decode(&coded.bytes(file)?)?;
            put(&decoded, out, 0, 0)
        }
        Picture::Grid {
            rows,
            columns,
            width,
            height,
            tiles,
        } => {
            ensure!(
                tiles.len() == rows * columns,
                "AVIF grid of {} tiles",
                tiles.len()
            );
            ensure!(
                (*width, *height) == (out.width, out.height),
                "AVIF grid size mismatch"
            );
            for (i, tile) in tiles.iter().enumerate() {
                let decoded = decoder.decode(&tile.bytes(file)?)?;
                let (w, h) = decoded.size();
                put(
                    &decoded,
                    out,
                    (i % columns) as u32 * w,
                    (i / columns) as u32 * h,
                )?;
            }
            Ok(())
        }
    }
}

fn parse(file: &[u8]) -> Result<Source> {
    let mut buf = file;
    let (mut ftyp, mut meta, mut moov) = (None, None, None);
    while let Some(header) = mp4_atom::Header::decode_maybe(&mut buf)? {
        let size = header.size.unwrap_or(buf.len());
        ensure!(size <= buf.len(), "truncated {:?} box", header.kind);
        let mut body = &buf[..size];
        match header.kind {
            k if k == Ftyp::KIND => ftyp = Some(Ftyp::decode_atom(&header, &mut body)?),
            k if k == Meta::KIND => meta = Some(Meta::decode_atom(&header, &mut body)?),
            k if k == Moov::KIND => moov = Some(Moov::decode_atom(&header, &mut body)?),
            _ => {}
        }
        buf = &buf[size..];
    }
    let ftyp = ftyp.context("no ftyp box")?;
    let sequence = ftyp.major_brand == FourCC::new(b"avis");
    // A sequence plays its track; anything else shows the primary item.
    if let Some(moov) = moov.filter(|_| sequence || meta.is_none()) {
        return parse_track(&moov);
    }
    parse_items(&meta.context("no meta box")?, file)
}

fn parse_track(moov: &Moov) -> Result<Source> {
    let is_av1 = |trak: &&Trak| {
        matches!(
            trak.mdia.minf.stbl.stsd.codecs.first(),
            Some(mp4_atom::Codec::Av01(_))
        )
    };
    let aux_of = |trak: &Trak| {
        trak.tref.as_ref().and_then(|tref| {
            tref.track_reference_type_boxes
                .iter()
                .find(|r| r.reference_type == FourCC::new(b"auxl"))
                .and_then(|r| r.track_ids.first().copied())
        })
    };
    let color = moov
        .trak
        .iter()
        .filter(is_av1)
        .find(|t| aux_of(t).is_none())
        .context("no AV1 track")?;
    let alpha = moov
        .trak
        .iter()
        .filter(is_av1)
        .find(|t| aux_of(t) == Some(color.tkhd.track_id));
    let Some(mp4_atom::Codec::Av01(av01)) = color.mdia.minf.stbl.stsd.codecs.first() else {
        unreachable!("filtered on AV1")
    };
    let (color_samples, durations) = samples(color)?;
    let alpha = alpha
        .map(|t| samples(t).map(|(s, _)| s))
        .transpose()?
        .filter(|a| a.len() == color_samples.len());
    let wrap = |s: Vec<Coded>| s.into_iter().map(Picture::Single).collect::<Vec<_>>();
    let durations = if durations.len() == 1 {
        vec![Duration::ZERO]
    } else {
        durations
    };
    Ok(Source {
        width: u32::from(av01.visual.width),
        height: u32::from(av01.visual.height),
        color: wrap(color_samples),
        alpha: alpha.map(wrap),
        durations,
        colr: av01.colr.iter().find_map(nclx),
    })
}

fn nclx(colr: &Colr) -> Option<(u32, bool)> {
    match *colr {
        Colr::Nclx {
            matrix_coefficients,
            full_range_flag,
            ..
        } => Some((u32::from(matrix_coefficients), full_range_flag)),
        _ => None,
    }
}

/// Every sample of a track (by its chunk offsets and sizes) and its display
/// time.
fn samples(trak: &Trak) -> Result<(Vec<Coded>, Vec<Duration>)> {
    let stbl = &trak.mdia.minf.stbl;
    let timescale = u64::from(trak.mdia.mdhd.timescale.max(1));
    let sizes: Vec<u32> = match &stbl.stsz.samples {
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
        StszSamples::Different { sizes } => sizes.clone(),
    };
    let chunks: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (Some(stco), _) => stco.entries.iter().map(|&o| u64::from(o)).collect(),
        (None, Some(co64)) => co64.entries.clone(),
        (None, None) => bail!("track without chunk offsets"),
    };
    let mut coded = Vec::with_capacity(sizes.len());
    let mut sample = 0;
    for (i, entry) in stbl.stsc.entries.iter().enumerate() {
        let first = entry.first_chunk.max(1) as usize - 1;
        let last = stbl.stsc.entries.get(i + 1).map_or(chunks.len(), |next| {
            (next.first_chunk.max(1) as usize - 1).min(chunks.len())
        });
        for &chunk in chunks.get(first..last).unwrap_or_default() {
            let mut offset = chunk as usize;
            for _ in 0..entry.samples_per_chunk {
                let Some(&size) = sizes.get(sample) else {
                    break;
                };
                coded.push(Coded::File(vec![(offset, size as usize)]));
                offset += size as usize;
                sample += 1;
            }
        }
    }
    ensure!(
        coded.len() == sizes.len(),
        "sample table covers {} of {} samples",
        coded.len(),
        sizes.len()
    );
    let mut durations = Vec::with_capacity(coded.len());
    for entry in &stbl.stts.entries {
        let d = Duration::from_nanos(u64::from(entry.sample_delta) * 1_000_000_000 / timescale);
        durations.extend(std::iter::repeat_n(d, entry.sample_count as usize));
    }
    durations.resize(
        coded.len(),
        durations.last().copied().unwrap_or(Duration::ZERO),
    );
    Ok((coded, durations))
}

fn parse_items(meta: &Meta, file: &[u8]) -> Result<Source> {
    let mut pitm = None;
    let (mut iloc, mut iinf, mut iprp, mut iref, mut idat) = (None, None, None, None, None);
    for item in &meta.items {
        match item {
            Any::Pitm(b) => pitm = Some(b),
            Any::Iloc(b) => iloc = Some(b),
            Any::Iinf(b) => iinf = Some(b),
            Any::Iprp(b) => iprp = Some(b),
            Any::Iref(b) => iref = Some(b),
            Any::Idat(b) => idat = Some(b),
            _ => {}
        }
    }
    let primary = pitm.context("no primary item")?.item_id;
    let iloc: &Iloc = iloc.context("no iloc box")?;
    let iinf: &Iinf = iinf.context("no iinf box")?;
    let iprp: &Iprp = iprp.context("no iprp box")?;
    let refs = |kind: &'static [u8; 4]| {
        let kind = FourCC::new(kind);
        iref.into_iter()
            .flat_map(|r: &Iref| &r.references)
            .filter(move |r| r.reference_type == kind)
    };
    let item_type = |id: u32| {
        iinf.item_infos
            .iter()
            .find(|e| e.item_id == id)
            .and_then(|e| e.item_type)
    };
    let properties = |id: u32| {
        iprp.ipma
            .iter()
            .flat_map(|ipma| &ipma.item_properties)
            .filter(move |p| p.item_id == id)
            .flat_map(|p| &p.associations)
            .filter_map(|a| {
                iprp.ipco
                    .properties
                    .get(usize::from(a.property_index).checked_sub(1)?)
            })
    };
    let coded = |id: u32| -> Result<Coded> {
        let loc = iloc
            .item_locations
            .iter()
            .find(|l| l.item_id == id)
            .with_context(|| format!("item {id} has no location"))?;
        let ranges = loc
            .extents
            .iter()
            .map(|e| ((loc.base_offset + e.offset) as usize, e.length as usize));
        match loc.construction_method {
            0 => Ok(Coded::File(ranges.collect())),
            1 => {
                let idat = &idat.context("item in a missing idat box")?.data;
                let mut out = Vec::new();
                for (offset, len) in ranges {
                    out.extend_from_slice(
                        idat.get(offset..offset + len)
                            .context("idat extent outside the box")?,
                    );
                }
                Ok(Coded::Inline(out))
            }
            m => bail!("item construction method {m}"),
        }
    };
    let picture = |id: u32| -> Result<(Picture, u32, u32)> {
        let (width, height) = properties(id)
            .find_map(|p| match p {
                Any::Ispe(ispe) => Some((ispe.width, ispe.height)),
                _ => None,
            })
            .with_context(|| format!("item {id} has no size"))?;
        match item_type(id) {
            Some(t) if t == FourCC::new(b"av01") => {
                Ok((Picture::Single(coded(id)?), width, height))
            }
            Some(t) if t == FourCC::new(b"grid") => {
                let data = coded(id)?.bytes(file)?;
                let tiles = refs(b"dimg")
                    .find(|r| r.from_item_id == id)
                    .context("grid without tiles")?
                    .to_item_ids
                    .iter()
                    .map(|&tile| coded(tile))
                    .collect::<Result<_>>()?;
                let (rows, columns, w, h) = grid(&data)?;
                ensure!((w, h) == (width, height), "grid output size mismatch");
                Ok((
                    Picture::Grid {
                        rows,
                        columns,
                        width,
                        height,
                        tiles,
                    },
                    width,
                    height,
                ))
            }
            other => bail!("unsupported AVIF item type {other:?}"),
        }
    };
    let (color, width, height) = picture(primary)?;
    let alpha_id = refs(b"auxl")
        .filter(|r| r.to_item_ids.contains(&primary))
        .map(|r| r.from_item_id)
        .find(|&id| {
            properties(id).any(|p| {
                matches!(p, Any::Auxc(auxc) if auxc.aux_type == "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha"
                    || auxc.aux_type == "urn:mpeg:hevc:2015:auxid:1")
            })
        });
    let alpha = alpha_id
        .map(picture)
        .transpose()?
        .filter(|(_, w, h)| (*w, *h) == (width, height))
        .map(|(p, _, _)| vec![p]);
    Ok(Source {
        width,
        height,
        color: vec![color],
        alpha,
        durations: vec![Duration::ZERO],
        colr: properties(primary).find_map(|p| match p {
            Any::Colr(colr) => nclx(colr),
            _ => None,
        }),
    })
}

/// Reads an `ImageGrid` item.
fn grid(data: &[u8]) -> Result<(usize, usize, u32, u32)> {
    ensure!(data.len() >= 8, "short grid item");
    let rows = usize::from(data[2]) + 1;
    let columns = usize::from(data[3]) + 1;
    let (w, h) = if data[1] & 1 != 0 {
        ensure!(data.len() >= 12, "short grid item");
        (
            u32::from_be_bytes(data[4..8].try_into().unwrap()),
            u32::from_be_bytes(data[8..12].try_into().unwrap()),
        )
    } else {
        (
            u32::from(u16::from_be_bytes([data[4], data[5]])),
            u32::from(u16::from_be_bytes([data[6], data[7]])),
        )
    };
    Ok((rows, columns, w, h))
}

/// rav1d's error for "send more data / take a picture first".
const EAGAIN: c_int = 11;

/// A dav1d decoder context (rav1d's C API; it has no Rust API yet).
struct Av1Decoder(Dav1dContext);

// SAFETY: a dav1d context has no thread affinity; `&mut self` methods keep
// calls on it sequential.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    fn new() -> Result<Self> {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: the pointer is valid for a write of the settings.
        unsafe { dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr())) };
        // SAFETY: initialized just above.
        let mut settings = unsafe { settings.assume_init() };
        // One picture out per temporal unit in, so frames come back in step.
        settings.max_frame_delay = 1;
        // Its default (0) is a thread per core, for each decoder (color and
        // alpha); a few threads already decode small frames quickly.
        settings.n_threads =
            std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as c_int;
        let mut context = None;
        // SAFETY: both pointers are valid; `context` receives the decoder.
        let result = unsafe {
            dav1d_open(
                Some(NonNull::from(&mut context)),
                Some(NonNull::from(&mut settings)),
            )
        };
        ensure!(result.0 == 0, "starting AV1 decoder: error {}", result.0);
        Ok(Av1Decoder(context.context("AV1 decoder did not start")?))
    }

    /// Decodes one temporal unit (one frame).
    fn decode(&mut self, data: &[u8]) -> Result<DecodedPicture> {
        ensure!(!data.is_empty(), "empty AV1 sample");
        let mut input = Dav1dData::default();
        // SAFETY: `input` is valid; the returned buffer holds `data.len()` bytes.
        let buffer = unsafe { dav1d_data_create(Some(NonNull::from(&mut input)), data.len()) };
        ensure!(!buffer.is_null(), "out of memory for AV1 data");
        // SAFETY: `buffer` was just allocated with room for `data`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len()) };
        let result = self.drive(&mut input);
        // SAFETY: `input` came from `dav1d_data_create` (unref is a no-op
        // once fully consumed).
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut input))) };
        result
    }

    fn drive(&mut self, input: &mut Dav1dData) -> Result<DecodedPicture> {
        loop {
            if input.sz > 0 {
                // SAFETY: the context is open and `input` is valid.
                let sent =
                    unsafe { dav1d_send_data(Some(self.0), Some(NonNull::from(&mut *input))) };
                ensure!(
                    sent.0 == 0 || sent.0 == -EAGAIN,
                    "AV1 decoding failed: error {}",
                    sent.0
                );
            }
            let mut picture = Dav1dPicture::default();
            // SAFETY: the context is open and `picture` is valid to write.
            let got = unsafe { dav1d_get_picture(Some(self.0), Some(NonNull::from(&mut picture))) };
            match got.0 {
                0 => return DecodedPicture::new(picture),
                e if e == -EAGAIN && input.sz > 0 => continue,
                e if e == -EAGAIN => bail!("AV1 sample decoded to no picture"),
                e => bail!("AV1 decoding failed: error {e}"),
            }
        }
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        let mut context = Some(self.0);
        // SAFETY: the context came from `dav1d_open` and is closed once.
        unsafe { dav1d_close(Some(NonNull::from(&mut context))) };
    }
}

/// A decoded picture, released on drop.
struct DecodedPicture(Dav1dPicture);

impl DecodedPicture {
    fn new(picture: Dav1dPicture) -> Result<Self> {
        let picture = DecodedPicture(picture);
        let bpc = picture.0.p.bpc;
        ensure!(
            matches!(bpc, 8 | 10 | 12),
            "{bpc}-bit AVIF is not supported"
        );
        Ok(picture)
    }

    fn size(&self) -> (u32, u32) {
        (self.0.p.w as u32, self.0.p.h as u32)
    }

    /// Plane `i` as 8-bit samples, and its stride. Deeper pictures are
    /// rounded down to 8 bits (our frames are 8-bit BGRA anyway).
    fn plane(&self, i: usize) -> (Cow<'_, [u8]>, usize) {
        let (w, h) = self.size();
        let layout = self.0.p.layout;
        let ss_x = u32::from(layout != DAV1D_PIXEL_LAYOUT_I444);
        let ss_y = u32::from(layout == DAV1D_PIXEL_LAYOUT_I420);
        let (cols, rows) = if i == 0 {
            (w, h)
        } else {
            ((w + ss_x) >> ss_x, (h + ss_y) >> ss_y)
        };
        let (cols, rows) = (cols as usize, rows as usize);
        let stride = self.0.stride[usize::from(i > 0)] as usize;
        let Some(ptr) = self.0.data[i] else {
            return (Cow::Borrowed(&[]), stride);
        };
        // SAFETY: dav1d planes hold `rows` lines of `stride` bytes while the
        // picture is referenced.
        let plane = unsafe { std::slice::from_raw_parts(ptr.as_ptr().cast::<u8>(), stride * rows) };
        let bpc = self.0.p.bpc as u32;
        if bpc == 8 {
            return (Cow::Borrowed(plane), stride);
        }
        let shift = bpc - 8;
        let mut narrow = Vec::with_capacity(cols * rows);
        for row in plane.chunks_exact(stride) {
            narrow.extend(row[..cols * 2].as_chunks::<2>().0.iter().map(|&s| {
                let v = u32::from(u16::from_ne_bytes(s));
                ((v + (1 << (shift - 1))) >> shift).min(255) as u8
            }));
        }
        (Cow::Owned(narrow), cols)
    }

    /// Matrix coefficients and full range of the stream, falling back to
    /// the container's `colr` and then libavif's default (BT.601).
    fn color_config(&self, colr: Option<(u32, bool)>) -> (u32, bool) {
        // SAFETY: the sequence header lives as long as the picture.
        let seq = self.0.seq_hdr.map(|s| unsafe { s.as_ref() });
        let (matrix, full) = seq.map_or((MC_UNSPECIFIED, false), |s| (s.mtrx, s.color_range != 0));
        if matrix != MC_UNSPECIFIED {
            (matrix, full)
        } else {
            colr.unwrap_or((MC_BT601, full))
        }
    }

    /// Converts to BGRA at (`x`, `y`) in `out`, keeping `out`'s alpha.
    fn color_into(&self, out: &mut Image, x: u32, y: u32, colr: Option<(u32, bool)>) -> Result<()> {
        let (w, h) = self.size();
        ensure!(
            x + w <= out.width && y + h <= out.height,
            "AVIF tile outside the image"
        );
        let (matrix_code, full) = self.color_config(colr);
        let range = if full {
            YuvRange::Full
        } else {
            YuvRange::Limited
        };
        let mut bgra = vec![255u8; w as usize * h as usize * 4];
        let stride = w * 4;
        let (y_plane, y_stride) = self.plane(0);
        let y_plane = &*y_plane;
        let layout = self.0.p.layout;
        if layout == DAV1D_PIXEL_LAYOUT_I400 {
            let gray = YuvGrayImage {
                y_plane,
                y_stride: y_stride as u32,
                width: w,
                height: h,
            };
            yuv::yuv400_to_bgra(&gray, &mut bgra, stride, range, YuvStandardMatrix::Bt709)?;
        } else {
            let (u_plane, u_stride) = self.plane(1);
            let (v_plane, v_stride) = self.plane(2);
            let (u_plane, v_plane) = (&*u_plane, &*v_plane);
            if matrix_code == MC_IDENTITY {
                ensure!(
                    layout == DAV1D_PIXEL_LAYOUT_I444,
                    "identity matrix needs 4:4:4"
                );
                identity_to_bgra(
                    &mut bgra,
                    w,
                    h,
                    [
                        (y_plane, y_stride),
                        (u_plane, u_stride),
                        (v_plane, v_stride),
                    ],
                    full,
                );
            } else {
                let image = YuvPlanarImage {
                    y_plane,
                    y_stride: y_stride as u32,
                    u_plane,
                    u_stride: u_stride as u32,
                    v_plane,
                    v_stride: v_stride as u32,
                    width: w,
                    height: h,
                };
                let matrix = match matrix_code {
                    MC_BT709 => YuvStandardMatrix::Bt709,
                    MC_BT2020_NCL => YuvStandardMatrix::Bt2020,
                    MC_SMPTE240 => YuvStandardMatrix::Smpte240,
                    MC_FCC => YuvStandardMatrix::Fcc,
                    MC_BT470BG | MC_BT601 => YuvStandardMatrix::Bt601,
                    _ => YuvStandardMatrix::Bt601,
                };
                match layout {
                    DAV1D_PIXEL_LAYOUT_I420 => {
                        yuv::yuv420_to_bgra(&image, &mut bgra, stride, range, matrix)?
                    }
                    DAV1D_PIXEL_LAYOUT_I422 => {
                        yuv::yuv422_to_bgra(&image, &mut bgra, stride, range, matrix)?
                    }
                    DAV1D_PIXEL_LAYOUT_I444 => {
                        yuv::yuv444_to_bgra(&image, &mut bgra, stride, range, matrix)?
                    }
                    other => bail!("AV1 pixel layout {other}"),
                }
            }
        }
        let out_stride = out.stride();
        for (row, src) in bgra.chunks_exact(stride as usize).enumerate() {
            let start = (y as usize + row) * out_stride + x as usize * 4;
            for (d, s) in out.data[start..start + src.len()]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<4>().0)
            {
                d[..3].copy_from_slice(&s[..3]);
            }
        }
        Ok(())
    }

    /// Writes the luma plane as alpha at (`x`, `y`) in `out`.
    fn alpha_into(&self, out: &mut Image, x: u32, y: u32) -> Result<()> {
        let (w, h) = self.size();
        ensure!(
            x + w <= out.width && y + h <= out.height,
            "AVIF alpha tile outside the image"
        );
        let (_, full) = self.color_config(None);
        let (plane, stride) = self.plane(0);
        let out_stride = out.stride();
        for row in 0..h as usize {
            let src = &plane[row * stride..row * stride + w as usize];
            let start = (y as usize + row) * out_stride + x as usize * 4;
            for (d, &a) in out.data[start..start + w as usize * 4]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src)
            {
                d[3] = if full {
                    a
                } else {
                    ((u32::from(a.clamp(16, 235)) - 16) * 255 / 219) as u8
                };
            }
        }
        Ok(())
    }
}

impl Drop for DecodedPicture {
    fn drop(&mut self) {
        // SAFETY: the picture came from `dav1d_get_picture` and is released once.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.0))) };
    }
}

/// GBR planes (identity matrix, as lossless AVIF uses) to BGRA.
fn identity_to_bgra(bgra: &mut [u8], w: u32, h: u32, planes: [(&[u8], usize); 3], full: bool) {
    let expand = |v: u8| {
        if full {
            v
        } else {
            ((u32::from(v.clamp(16, 235)) - 16) * 255 / 219) as u8
        }
    };
    let [(g, gs), (b, bs), (r, rs)] = planes;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let px = &mut bgra[(y * w as usize + x) * 4..][..4];
            px[0] = expand(b[y * bs + x]);
            px[1] = expand(g[y * gs + x]);
            px[2] = expand(r[y * rs + x]);
        }
    }
}

struct Sample {
    data: Vec<u8>,
    key: bool,
}

pub struct AvifEncoder {
    path: std::path::PathBuf,
    context: Rav1eContext<u8>,
    width: u32,
    height: u32,
    clock: TickClock,
    /// Display time of each frame sent, in milliseconds.
    durations: Vec<u32>,
    samples: Vec<Sample>,
    yuv: YuvPlanarImageMut<'static, u8>,
    flat: Vec<u8>,
}

impl AvifEncoder {
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        let mut config = EncoderConfig::with_speed_preset(SPEED);
        config.width = width as usize;
        config.height = height as usize;
        config.bit_depth = 8;
        config.chroma_sampling = ChromaSampling::Cs420;
        config.pixel_range = PixelRange::Limited;
        config.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::SRGB,
            matrix_coefficients: MatrixCoefficients::BT709,
        });
        config.quantizer = QUANTIZER;
        config.time_base = Rational::new(1, 1000);
        // rav1e only works in parallel across tiles (and is built without
        // assembly): with a tile per thread, speed 8 encoded a synthetic
        // desktop ~5x faster than speed 7 in one tile, for ~3% more bytes.
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
        config.tiles = threads;
        let context = Config::new()
            .with_encoder_config(config)
            .with_threads(threads)
            .new_context()
            .map_err(|e| anyhow!("starting AV1 encoder: {e}"))?;
        // Fails early instead of after encoding everything.
        File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(AvifEncoder {
            path: path.to_owned(),
            context,
            width,
            height,
            clock: TickClock::new(1000),
            durations: Vec::new(),
            samples: Vec::new(),
            yuv: YuvPlanarImageMut::alloc(width, height, YuvChromaSubsampling::Yuv420),
            flat: Vec::new(),
        })
    }

    /// Collects every packet rav1e has ready.
    fn drain(&mut self) -> Result<()> {
        loop {
            match self.context.receive_packet() {
                Ok(packet) => {
                    ensure!(
                        packet.input_frameno == self.samples.len() as u64,
                        "AV1 packet {} out of order",
                        packet.input_frameno
                    );
                    // Temporal delimiters are left out of ISOBMFF samples.
                    let mut data = packet.data;
                    if data.starts_with(&[0x12, 0x00]) {
                        data.drain(..2);
                    }
                    self.samples.push(Sample {
                        data,
                        key: packet.frame_type == FrameType::KEY,
                    });
                }
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(e) => bail!("AV1 encoding failed: {e}"),
            }
        }
    }
}

impl AnimEncoder for AvifEncoder {
    fn push(&mut self, image: &Image, duration: Duration) -> Result<()> {
        ensure!(
            (image.width, image.height) == (self.width, self.height),
            "{}x{} frame for a {}x{} AVIF",
            image.width,
            image.height,
            self.width,
            self.height
        );
        let (start, end) = self.clock.advance(duration);
        self.durations
            .push(u32::try_from(end - start).context("AVIF frame too long")?);
        // Straight alpha over black.
        self.flat.clear();
        self.flat
            .extend(image.data.as_chunks::<4>().0.iter().flat_map(|px| {
                let a = u16::from(px[3]);
                let f = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
                [f(px[0]), f(px[1]), f(px[2]), 255]
            }));
        yuv::bgra_to_yuv420(
            &mut self.yuv,
            &self.flat,
            self.width * 4,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )?;
        let mut frame = self.context.new_frame();
        let planes = [
            (self.yuv.y_plane.borrow(), self.yuv.y_stride),
            (self.yuv.u_plane.borrow(), self.yuv.u_stride),
            (self.yuv.v_plane.borrow(), self.yuv.v_stride),
        ];
        for (plane, (data, stride)) in frame.planes.iter_mut().zip(planes) {
            plane.copy_from_raw_u8(data, stride as usize, 1);
        }
        match self.context.send_frame(frame) {
            Ok(()) | Err(EncoderStatus::EnoughData) => {}
            Err(e) => bail!("AV1 encoding failed: {e}"),
        }
        self.drain()
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.context.flush();
        self.drain()?;
        ensure!(
            !self.samples.is_empty() && self.samples.len() == self.durations.len(),
            "AV1 encoder returned {} of {} frames",
            self.samples.len(),
            self.durations.len()
        );
        ensure!(self.samples[0].key, "first AV1 frame is not a keyframe");
        let av1c = av1_config(
            &self.context.container_sequence_header(),
            &self.samples[0].data,
        )?;
        let file = avis(
            self.width,
            self.height,
            &av1c,
            &self.samples,
            &self.durations,
        )?;
        let out = File::create(&self.path)
            .with_context(|| format!("creating {}", self.path.display()))?;
        let mut out = BufWriter::new(out);
        out.write_all(&file)?;
        out.flush()?;
        Ok(())
    }
}

/// The `av1C` record: rav1e's header bytes plus the sequence header OBU
/// from the first keyframe as config OBU.
fn av1_config(header: &[u8], keyframe: &[u8]) -> Result<Av1c> {
    let mut av1c = Av1c::decode_body(&mut &header[..]).map_err(|e| anyhow!("parsing av1C: {e}"))?;
    let mut rest = keyframe;
    while let Some((&first, after)) = rest.split_first() {
        let kind = (first >> 3) & 0xf;
        let extension = usize::from((first >> 2) & 1);
        ensure!(first & 2 != 0, "AV1 OBU without size");
        let mut size = 0usize;
        let mut read = extension;
        for (i, &byte) in after[extension..].iter().enumerate().take(8) {
            size |= usize::from(byte & 0x7f) << (7 * i);
            read += 1;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let total = 1 + read + size;
        ensure!(total <= rest.len(), "truncated AV1 OBU");
        // OBU_SEQUENCE_HEADER
        if kind == 1 {
            av1c.config_obus = rest[..total].to_vec();
            return Ok(av1c);
        }
        rest = &rest[total..];
    }
    bail!("AV1 keyframe without sequence header")
}

/// Lays out a libavif-style animated AVIF: `ftyp`, `meta` (first frame as
/// the primary image), `moov` (all frames, looping) and `mdat`.
fn avis(
    width: u32,
    height: u32,
    av1c: &Av1c,
    samples: &[Sample],
    durations: &[u32],
) -> Result<Vec<u8>> {
    let total: u64 = durations.iter().map(|&d| u64::from(d)).sum();
    let payload: usize = samples.iter().map(|s| s.data.len()).sum();
    let colr = Colr::Nclx {
        colour_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: MC_BT709 as u16,
        full_range_flag: false,
    };
    let (w16, h16) = (
        u16::try_from(width).context("AVIF too wide")?,
        u16::try_from(height).context("AVIF too high")?,
    );
    let ftyp = Ftyp {
        major_brand: FourCC::new(b"avis"),
        minor_version: 0,
        compatible_brands: [
            b"avif", b"avis", b"msf1", b"iso8", b"mif1", b"miaf", b"MA1B",
        ]
        .into_iter()
        .map(FourCC::new)
        .collect(),
    };
    let mut keyframes = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        if s.key {
            keyframes.push(i as u32 + 1);
        }
    }
    let mut stts: Vec<SttsEntry> = Vec::new();
    for &d in durations {
        match stts.last_mut() {
            Some(e) if e.sample_delta == d => e.sample_count += 1,
            _ => stts.push(SttsEntry {
                sample_count: 1,
                sample_delta: d,
            }),
        }
    }

    let build = |mdat_start: u64| -> Result<Vec<u8>> {
        let meta = Meta {
            hdlr: Hdlr {
                handler: FourCC::new(b"pict"),
                name: String::new(),
            },
            items: vec![
                Pitm { item_id: 1 }.into(),
                Iloc {
                    item_locations: vec![ItemLocation {
                        item_id: 1,
                        construction_method: 0,
                        data_reference_index: 0,
                        base_offset: 0,
                        extents: vec![ItemLocationExtent {
                            item_reference_index: 0,
                            offset: mdat_start,
                            length: samples[0].data.len() as u64,
                        }],
                    }],
                }
                .into(),
                Iinf {
                    item_infos: vec![ItemInfoEntry {
                        item_id: 1,
                        item_protection_index: 0,
                        item_type: Some(FourCC::new(b"av01")),
                        item_name: "Color".into(),
                        content_type: None,
                        content_encoding: None,
                        item_uri_type: None,
                        item_not_in_presentation: false,
                    }],
                }
                .into(),
                Iprp {
                    ipco: Ipco {
                        properties: vec![
                            Ispe { width, height }.into(),
                            Pixi {
                                bits_per_channel: vec![8, 8, 8],
                            }
                            .into(),
                            av1c.clone().into(),
                            colr.clone().into(),
                        ],
                    },
                    ipma: vec![Ipma {
                        item_properties: vec![PropertyAssociations {
                            item_id: 1,
                            associations: [(false, 1), (false, 2), (true, 3), (false, 4)]
                                .into_iter()
                                .map(|(essential, property_index)| PropertyAssociation {
                                    essential,
                                    property_index,
                                })
                                .collect(),
                        }],
                    }],
                }
                .into(),
            ],
        };
        let chunk = u32::try_from(mdat_start).context("AVIF too large");
        let (stco, co64) = match chunk {
            Ok(offset) => (
                Some(Stco {
                    entries: vec![offset],
                }),
                None,
            ),
            Err(_) => (
                None,
                Some(Co64 {
                    entries: vec![mdat_start],
                }),
            ),
        };
        // An indefinite duration plus a repeating edit list is how libavif
        // says "loop forever".
        let moov = Moov {
            mvhd: Mvhd {
                creation_time: 0,
                modification_time: 0,
                timescale: 1000,
                duration: u64::MAX,
                rate: 1.into(),
                volume: 1.into(),
                matrix: Matrix::default(),
                next_track_id: 2,
            },
            trak: vec![Trak {
                tkhd: Tkhd {
                    track_id: 1,
                    duration: u64::MAX,
                    enabled: true,
                    in_movie: true,
                    width: w16.into(),
                    height: h16.into(),
                    matrix: Matrix::default(),
                    ..Tkhd::default()
                },
                edts: Some(Edts {
                    elst: Some(Elst {
                        entries: vec![ElstEntry {
                            segment_duration: total,
                            media_time: Some(0),
                            media_rate: 1.into(),
                        }],
                    }),
                }),
                mdia: Mdia {
                    mdhd: Mdhd {
                        creation_time: 0,
                        modification_time: 0,
                        timescale: 1000,
                        duration: total,
                        language: "und".into(),
                    },
                    hdlr: Hdlr {
                        handler: FourCC::new(b"pict"),
                        name: String::new(),
                    },
                    minf: Minf {
                        vmhd: Some(Vmhd::default()),
                        dinf: Dinf {
                            dref: Dref {
                                entries: vec![
                                    Url {
                                        location: String::new(),
                                    }
                                    .into(),
                                ],
                            },
                        },
                        stbl: Stbl {
                            stsd: Stsd {
                                codecs: vec![
                                    Av01 {
                                        visual: Visual {
                                            data_reference_index: 1,
                                            width: w16,
                                            height: h16,
                                            horizresolution: 72.into(),
                                            vertresolution: 72.into(),
                                            frame_count: 1,
                                            compressor: "".into(),
                                            depth: 24,
                                        },
                                        av1c: av1c.clone(),
                                        ccst: Some(Ccst {
                                            all_ref_pics_intra: false,
                                            intra_pred_used: true,
                                            max_ref_per_pic: 15,
                                        }),
                                        colr: vec![colr.clone()],
                                        ..Av01::default()
                                    }
                                    .into(),
                                ],
                            },
                            stts: Stts {
                                entries: stts.clone(),
                            },
                            stss: Some(Stss {
                                entries: keyframes.clone(),
                            }),
                            stsc: Stsc {
                                entries: vec![StscEntry {
                                    first_chunk: 1,
                                    samples_per_chunk: samples.len() as u32,
                                    sample_description_index: 1,
                                }],
                            },
                            stsz: Stsz {
                                samples: StszSamples::Different {
                                    sizes: samples.iter().map(|s| s.data.len() as u32).collect(),
                                },
                            },
                            stco,
                            co64,
                            ..Stbl::default()
                        },
                        ..Minf::default()
                    },
                },
                ..Trak::default()
            }],
            ..Moov::default()
        };
        let mut out = Vec::new();
        ftyp.encode(&mut out)?;
        meta.encode(&mut out)?;
        let moov_start = out.len();
        moov.encode(&mut out)?;
        set_elst_repeat(&mut out[moov_start..])?;
        Header {
            kind: FourCC::new(b"mdat"),
            size: Some(payload),
        }
        .encode(&mut out)?;
        Ok(out)
    };

    // Box sizes depend on the offsets they hold; settle them first.
    let mut start = 0;
    let mut head = build(start)?;
    while head.len() as u64 != start {
        start = head.len() as u64;
        head = build(start)?;
    }
    head.reserve(payload);
    for sample in samples {
        head.extend_from_slice(&sample.data);
    }
    Ok(head)
}

/// Sets the `elst` box's flags to 1 ("repeat the edit list"), which
/// mp4-atom has no field for.
fn set_elst_repeat(moov: &mut [u8]) -> Result<()> {
    let at = moov
        .windows(4)
        .position(|w| w == b"elst")
        .context("no elst box written")?;
    moov[at + 5..at + 8].copy_from_slice(&[0, 0, 1]);
    Ok(())
}
