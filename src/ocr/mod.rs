//! Text extraction from screenshots.
//!
//! Tesseract (see [`tess`]) reads the whole region with the primary model
//! (English unless configured otherwise), which also finds the text lines.
//! Lines it is unsure of are re-read by every other installed model, in
//! parallel, and the most confident reading wins: one screenshot can mix
//! English with Japanese, Korean, Chinese, Arabic, Thai or Vietnamese and
//! each line is read in its own language. Models load on first use, each
//! on its own thread, and are dropped again after a few idle minutes.

pub mod tess;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use unicode_script::{Script, UnicodeScript};

use crate::geom::IRect;
use crate::raster::Image;
use tess::{Engine, Gray, Line, Psm, Word};

/// Below this confidence a line's language is in doubt and the other
/// models re-read it. Clean English reads at about 92; English-model
/// readings of Vietnamese or German (diacritics lost) land around 80–87.
const REREAD_BELOW: f32 = 90.0;
/// A re-read replaces a line only when it is this much more confident:
/// Latin-script models agree on English within a point or two.
const WIN_MARGIN: f32 = 5.0;
/// A re-read this confident (in its own script) settles a line: models
/// that have not reached it yet skip it. Slow models (Korean, Chinese)
/// trail the fast ones, so they mostly skip lines already settled.
const SETTLE: f32 = 93.0;
/// Re-reading models unused this long are unloaded (reloading costs
/// 5–200 ms; all of them together take a few hundred MB). The primary
/// model, once warmed, stays.
const IDLE: Duration = Duration::from_secs(120);
/// A Hangul word break is a gap wider than this fraction of the line
/// height; syllables within a word sit about 0.2 apart, words about 0.5.
const WORD_GAP: f32 = 0.36;

/// Where Tesseract's `*.traineddata` live: `$TESSDATA_PREFIX` (the
/// directory itself or its parent), then the distributions' locations.
pub fn tessdata_dir() -> Option<PathBuf> {
    let env = std::env::var_os("TESSDATA_PREFIX").map(PathBuf::from);
    let candidates = env
        .iter()
        .flat_map(|p| [p.clone(), p.join("tessdata")])
        .chain(
            [
                "/usr/share/tessdata",
                "/usr/share/tesseract-ocr/5/tessdata",
                "/usr/share/tesseract-ocr/4.00/tessdata",
                "/usr/local/share/tessdata",
            ]
            .map(PathBuf::from),
        );
    candidates
        .into_iter()
        .find(|d| !installed_languages(d).is_empty())
}

/// Language models in `dir`, by Tesseract name (`eng`, `jpn`, …), sorted.
/// Orientation/script and equation data are not languages.
pub fn installed_languages(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut langs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".traineddata")
                .map(str::to_string)
        })
        .filter(|l| !matches!(l.as_str(), "osd" | "equ"))
        .collect();
    langs.sort();
    langs
}

/// Converts `region` of a screenshot to what Tesseract reads best: gray,
/// dark text on light, and screen-sized text enlarged `upscale` times with
/// bilinear filtering. Polarity is decided per pixel from its surroundings,
/// so a light window on a dark desktop (or a dark sidebar in a light app)
/// all ends up dark-on-light.
pub fn prepare(image: &Image, region: IRect, upscale: u32) -> Gray {
    let Some(r) = region.intersect(&image.bounds()) else {
        return Gray {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
    };
    let (w, h) = (r.w as usize, r.h as usize);
    let mut luma = Vec::with_capacity(w * h);
    for y in r.y..r.bottom() {
        let row = &image.row(y as u32)[r.x as usize * 4..r.right() as usize * 4];
        for p in row.chunks_exact(4) {
            // BGRA, Rec. 601 weights in fixed point.
            luma.push(((p[2] as u32 * 77 + p[1] as u32 * 150 + p[0] as u32 * 29) >> 8) as u8);
        }
    }
    invert_dark_backgrounds(&mut luma, w, h);
    let s = upscale.max(1) as usize;
    if s == 1 {
        return Gray {
            width: w as u32,
            height: h as u32,
            data: luma,
        };
    }
    let (ow, oh) = (w * s, h * s);
    let mut out = Vec::with_capacity(ow * oh);
    for oy in 0..oh {
        // Sample at pixel centres.
        let fy = ((oy as f32 + 0.5) / s as f32 - 0.5).max(0.0);
        let (y0, ty) = (fy as usize, fy.fract());
        let y1 = (y0 + 1).min(h - 1);
        for ox in 0..ow {
            let fx = ((ox as f32 + 0.5) / s as f32 - 0.5).max(0.0);
            let (x0, tx) = (fx as usize, fx.fract());
            let x1 = (x0 + 1).min(w - 1);
            let at = |x: usize, y: usize| luma[y * w + x] as f32;
            let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
            let bottom = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
            out.push((top * (1.0 - ty) + bottom * ty + 0.5) as u8);
        }
    }
    Gray {
        width: ow as u32,
        height: oh as u32,
        data: out,
    }
}

/// One line of extracted text.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    /// Where the line is, in the screenshot's pixels.
    pub rect: IRect,
    pub text: String,
    /// The model that read it, e.g. "jpn".
    pub lang: String,
    pub conf: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extracted {
    pub lines: Vec<TextLine>,
    /// The lines in reading order, one per line. (Tesseract's block
    /// boundaries are too noisy on screen layouts to mark with blank lines.)
    pub text: String,
}

/// Which models read: `primary` the whole region, `rereaders` the lines
/// the primary is unsure of.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub primary: String,
    pub rereaders: Vec<String>,
}

/// The reading plan for the configured languages (empty: automatic) given
/// the installed models.
pub fn plan(configured: &[String], installed: &[String]) -> Result<Plan> {
    if installed.is_empty() {
        bail!(
            "no Tesseract language data found: install a language pack such as \
             `tesseract-data-eng` (Arch) or `tesseract-ocr-eng` (Debian/Ubuntu)"
        );
    }
    if let Some((primary, rest)) = configured.split_first() {
        if let Some(missing) = configured.iter().find(|l| !installed.contains(l)) {
            bail!(
                "ocr.languages: {missing:?} is not installed (installed: {})",
                installed.join(", ")
            );
        }
        return Ok(Plan {
            primary: primary.clone(),
            rereaders: rest.to_vec(),
        });
    }
    let primary = if installed.iter().any(|l| l == "eng") {
        "eng".to_string()
    } else {
        installed[0].clone()
    };
    let rereaders = installed
        .iter()
        // Vertical-text models read rotated columns, not screen lines.
        .filter(|l| **l != primary && !l.ends_with("_vert"))
        .cloned()
        .collect();
    Ok(Plan { primary, rereaders })
}

enum Job {
    /// Loads the model ahead of the first request and keeps it loaded.
    Pin,
    Read {
        image: Arc<Gray>,
        psm: Psm,
        reply: Sender<Result<Vec<Line>>>,
    },
    /// Reads each line image in turn, skipping lines another model has
    /// settled, and replies per line, tagged with the model.
    Reread {
        lines: Arc<[Gray]>,
        settled: Arc<[AtomicBool]>,
        reply: Sender<(String, Result<(usize, Line)>)>,
    },
}

/// One thread per model; Tesseract instances are single-threaded, and
/// separate threads let all re-reads of a line run at once.
struct Pool {
    dir: PathBuf,
    workers: Mutex<HashMap<String, Sender<Job>>>,
}

impl Pool {
    fn get() -> Result<&'static Arc<Pool>> {
        static POOL: OnceLock<Option<Arc<Pool>>> = OnceLock::new();
        POOL.get_or_init(|| {
            tessdata_dir().map(|dir| {
                Arc::new(Pool {
                    dir,
                    workers: Mutex::default(),
                })
            })
        })
        .as_ref()
        .ok_or_else(|| {
            anyhow!(
                "no Tesseract language data found: install `tesseract` and a language pack \
                 such as `tesseract-data-eng` (Arch) or `tesseract-ocr-eng` (Debian/Ubuntu)"
            )
        })
    }

    fn send(self: &Arc<Pool>, lang: &str, job: Job) {
        let mut workers = self.workers.lock().unwrap();
        let tx = workers
            .entry(lang.to_string())
            .or_insert_with(|| self.spawn(lang));
        // A worker removes itself under this lock before exiting, so a
        // sender in the map always reaches a live thread.
        let _ = tx.send(job);
    }

    fn spawn(self: &Arc<Pool>, lang: &str) -> Sender<Job> {
        let (tx, rx) = mpsc::channel::<Job>();
        let pool = self.clone();
        let lang = lang.to_string();
        std::thread::Builder::new()
            .name(format!("quickscope-ocr-{lang}"))
            .spawn(move || {
                let mut engine: Option<Engine> = None;
                let mut pinned = false;
                let run = |job: Job, engine: &mut Option<Engine>| {
                    let ready = match engine {
                        Some(e) => Ok(e),
                        None => {
                            let started = Instant::now();
                            let e = Engine::new(&pool.dir, &lang);
                            log::debug!(
                                "loaded OCR model {lang} in {:.0}ms",
                                started.elapsed().as_secs_f64() * 1e3
                            );
                            e.map(|e| engine.insert(e))
                        }
                    };
                    match job {
                        Job::Read { image, psm, reply } => {
                            let _ = reply.send(ready.and_then(|e| e.recognize(&image, psm)));
                        }
                        Job::Pin => {
                            if let Err(e) = ready {
                                log::debug!("OCR unavailable: {e:#}");
                            }
                        }
                        Job::Reread {
                            lines,
                            settled,
                            reply,
                        } => {
                            let send = |r| {
                                let _ = reply.send((lang.clone(), r));
                            };
                            let engine = match ready {
                                Ok(e) => e,
                                Err(e) => return send(Err(e)),
                            };
                            for (i, image) in lines.iter().enumerate() {
                                if settled[i].load(Ordering::Relaxed) {
                                    continue;
                                }
                                match engine.recognize(image, Psm::SingleLine) {
                                    Ok(found) => {
                                        if let Some(line) = found.into_iter().next() {
                                            if line.conf >= SETTLE && own_script(&lang, &line.text)
                                            {
                                                settled[i].store(true, Ordering::Relaxed);
                                            }
                                            send(Ok((i, line)));
                                        }
                                    }
                                    Err(e) => send(Err(e)),
                                }
                            }
                        }
                    }
                };
                loop {
                    let timeout = if pinned { Duration::MAX } else { IDLE };
                    match rx.recv_timeout(timeout) {
                        Ok(job) => {
                            pinned |= matches!(job, Job::Pin);
                            run(job, &mut engine)
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            pool.workers.lock().unwrap().remove(&lang);
                            // Jobs sent before the removal still get answers.
                            while let Ok(job) = rx.try_recv() {
                                run(job, &mut engine);
                            }
                            log::debug!("unloaded idle OCR model {lang}");
                            return;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawning an OCR thread");
        tx
    }

    fn read(
        self: &Arc<Pool>,
        lang: &str,
        image: Arc<Gray>,
        psm: Psm,
    ) -> mpsc::Receiver<Result<Vec<Line>>> {
        let (reply, rx) = mpsc::channel();
        self.send(lang, Job::Read { image, psm, reply });
        rx
    }
}

/// Loads the primary model in the background so the first extraction
/// does not wait for it. Silently does nothing without Tesseract.
pub fn warm(configured: &[String]) {
    let Ok(pool) = Pool::get() else { return };
    if let Ok(plan) = plan(configured, &installed_languages(&pool.dir)) {
        pool.send(&plan.primary, Job::Pin);
    }
}

/// How much to enlarge a capture with `scale` physical pixels per logical
/// pixel: ordinary screen text is too small for Tesseract, HiDPI text is not.
pub fn upscale_for(scale: f32) -> u32 {
    if scale >= 1.75 { 1 } else { 2 }
}

/// Reads the text in `region` of `image`. `upscale` enlarges the pixels
/// first (2 for ordinary screens, 1 for HiDPI captures).
pub fn extract(
    image: &Image,
    region: IRect,
    upscale: u32,
    configured: &[String],
) -> Result<Extracted> {
    let started = Instant::now();
    let pool = Pool::get()?;
    let plan = plan(configured, &installed_languages(&pool.dir))?;
    let up = upscale.max(1) as i32;
    let Some(region) = region.intersect(&image.bounds()) else {
        return Ok(Extracted::default());
    };
    let gray = Arc::new(prepare(image, region, upscale));
    let mut lines = pool
        .read(&plan.primary, gray.clone(), Psm::Auto)
        .recv()
        .context("OCR thread stopped")??;
    let mut langs = vec![plan.primary.clone(); lines.len()];
    let primary_done = started.elapsed();

    // Every doubtful line goes to every other model at once; per line the
    // most confident re-read in its own script is the candidate.
    let page = IRect::new(0, 0, gray.width as i32, gray.height as i32);
    let doubtful: Vec<usize> = if plan.rereaders.is_empty() {
        Vec::new()
    } else {
        (0..lines.len())
            .filter(|&i| lines[i].conf < REREAD_BELOW)
            .collect()
    };
    if !doubtful.is_empty() {
        let crops: Arc<[Gray]> = doubtful
            .iter()
            .map(|&i| {
                let (l, pad) = (lines[i].rect, 6 * up);
                let r = IRect::new(l.x - pad, l.y - pad, l.w + 2 * pad, l.h + 2 * pad)
                    .intersect(&page)
                    .unwrap_or(l);
                crop(&gray, r)
            })
            .collect();
        let settled: Arc<[AtomicBool]> = doubtful.iter().map(|_| AtomicBool::new(false)).collect();
        let (reply, results) = mpsc::channel();
        for lang in &plan.rereaders {
            pool.send(
                lang,
                Job::Reread {
                    lines: crops.clone(),
                    settled: settled.clone(),
                    reply: reply.clone(),
                },
            );
        }
        // Results end once every model has finished and dropped its sender.
        drop(reply);
        let mut best: Vec<Option<(String, Line)>> = vec![None; doubtful.len()];
        for (lang, result) in results {
            match result {
                Ok((k, line)) => {
                    log::trace!("line {k}: {lang} {:.1} {:?}", line.conf, line.text);
                    let better = best[k].as_ref().is_none_or(|(_, b)| line.conf > b.conf);
                    if better && own_script(&lang, &line.text) {
                        best[k] = Some((lang, line));
                    }
                }
                Err(e) => log::debug!("re-reading with {lang} failed: {e:#}"),
            }
        }
        for (k, candidate) in best.into_iter().enumerate() {
            let i = doubtful[k];
            if let Some((lang, line)) = candidate
                && wins(&lang, &line, &lines[i])
            {
                lines[i] = Line {
                    rect: lines[i].rect,
                    ..line
                };
                langs[i] = lang;
            }
        }
    }
    let reread = doubtful.len();

    let mut out = Extracted::default();
    for (line, lang) in lines.into_iter().zip(langs) {
        let text = normalize(&line_text(&line.text, &line.words, line.rect.h as f32));
        if !out.text.is_empty() {
            out.text.push('\n');
        }
        out.text.push_str(&text);
        let r = line.rect;
        out.lines.push(TextLine {
            rect: IRect::new(
                region.x + r.x / up,
                region.y + r.y / up,
                (r.w + up - 1) / up,
                (r.h + up - 1) / up,
            ),
            text,
            lang,
            conf: line.conf,
        });
    }
    log::debug!(
        "extracted {} lines ({} re-read) from {}x{} in {:.0}ms ({:.0}ms page)",
        out.lines.len(),
        reread,
        region.w,
        region.h,
        started.elapsed().as_secs_f64() * 1e3,
        primary_done.as_secs_f64() * 1e3
    );
    Ok(out)
}

/// Models for scripts other than Latin. Tesseract names Latin-script
/// languages by ISO 639-2 code too, so this list is what tells them apart.
const NON_LATIN: [&str; 30] = [
    "jpn", "chi_sim", "chi_tra", "kor", "ara", "fas", "urd", "pus", "heb", "yid", "tha", "lao",
    "khm", "mya", "hin", "mar", "nep", "ben", "tam", "tel", "kan", "mal", "guj", "pan", "rus",
    "ukr", "bel", "bul", "srp", "ell",
];

/// Whether `lang`'s re-reading should replace the primary reading of a
/// line: clearly more confident, and not a non-Latin model mis-reading
/// Latin text (CJK models read English with full-width punctuation and
/// high confidence, but find none of their own letters in it).
fn wins(lang: &str, reread: &Line, primary: &Line) -> bool {
    own_script(lang, &reread.text)
        && reread.conf > primary.conf + WIN_MARGIN
        && !reread.text.trim().is_empty()
}

/// Whether `text` could be `lang`'s own reading: Latin-script models read
/// anything, a non-Latin model must have found letters of its script.
fn own_script(lang: &str, text: &str) -> bool {
    !NON_LATIN.contains(&lang.trim_end_matches("_vert"))
        || text.chars().any(|c| {
            c.is_alphabetic()
                && !matches!(
                    c.script(),
                    Script::Latin | Script::Common | Script::Inherited
                )
        })
}

/// Half-size of the window whose mean decides a pixel's background: wider
/// than any screen text stroke, narrower than most panels.
const BACKGROUND_RADIUS: usize = 16;

/// Inverts every pixel whose surroundings average dark, using a summed-area
/// table so the cost does not depend on the window size.
///
/// The table is `u32` with wrapping arithmetic: prefix sums of a large
/// bright screen pass 2³² (5K: ~3.8e9 each, 8K: ~8.5e9), but every box sum
/// is below 33² × 255, so sums taken modulo 2³² still give exact boxes. That
/// keeps the table at 4 bytes per pixel instead of 8 for `u64`.
fn invert_dark_backgrounds(luma: &mut [u8], w: usize, h: usize) {
    let stride = w + 1;
    let mut sum = vec![0u32; stride * (h + 1)];
    for y in 0..h {
        let mut row = 0u32;
        for x in 0..w {
            row = row.wrapping_add(luma[y * w + x] as u32);
            sum[(y + 1) * stride + x + 1] = sum[y * stride + x + 1].wrapping_add(row);
        }
    }
    let r = BACKGROUND_RADIUS;
    // The table holds the original values, so pixels can flip in place.
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(r), (y + r + 1).min(h));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(r), (x + r + 1).min(w));
            let total = sum[y1 * stride + x1]
                .wrapping_sub(sum[y0 * stride + x1])
                .wrapping_sub(sum[y1 * stride + x0])
                .wrapping_add(sum[y0 * stride + x0]);
            if total < 128 * ((x1 - x0) * (y1 - y0)) as u32 {
                let l = &mut luma[y * w + x];
                *l = 255 - *l;
            }
        }
    }
}

fn crop(g: &Gray, r: IRect) -> Gray {
    let mut data = Vec::with_capacity(r.w as usize * r.h as usize);
    for y in r.y..r.bottom() {
        let start = y as usize * g.width as usize + r.x as usize;
        data.extend_from_slice(&g.data[start..start + r.w as usize]);
    }
    Gray {
        width: r.w as u32,
        height: r.h as u32,
        data,
    }
}

/// Written without spaces between characters: Han, kana, Thai, and CJK
/// or full-width punctuation.
fn unspaced(c: char) -> bool {
    matches!(
        c.script(),
        Script::Han | Script::Hiragana | Script::Katakana | Script::Thai | Script::Bopomofo
    ) || matches!(c as u32, 0x3000..=0x303F | 0xFF00..=0xFFEF | 0xFE30..=0xFE6F | 0x30FC)
}

/// Replaces the CJK models' "small form" punctuation (U+FE50…FE6B, a
/// compatibility block no one types) with the full-width forms screens
/// actually show: `何﹖` becomes `何？`.
pub fn normalize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{FE50}' => '，',
            '\u{FE51}' => '、',
            '\u{FE52}' => '．',
            '\u{FE54}' => '；',
            '\u{FE55}' => '：',
            '\u{FE56}' => '？',
            '\u{FE57}' => '！',
            '\u{FE58}' => '—',
            '\u{FE59}' => '（',
            '\u{FE5A}' => '）',
            '\u{FE5B}' => '｛',
            '\u{FE5C}' => '｝',
            '\u{FE5D}' => '〔',
            '\u{FE5E}' => '〕',
            '\u{FE5F}' => '＃',
            '\u{FE60}' => '＆',
            '\u{FE61}' => '＊',
            '\u{FE62}' => '＋',
            '\u{FE63}' => '－',
            '\u{FE64}' => '＜',
            '\u{FE65}' => '＞',
            '\u{FE66}' => '＝',
            '\u{FE68}' => '＼',
            '\u{FE69}' => '＄',
            '\u{FE6A}' => '％',
            '\u{FE6B}' => '＠',
            c => c,
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Unspaced,
    Hangul,
    Punct,
    Other,
}

fn class(c: char) -> Class {
    if unspaced(c) {
        Class::Unspaced
    } else if c.script() == Script::Hangul {
        Class::Hangul
    } else if c.is_ascii_punctuation() {
        Class::Punct
    } else {
        Class::Other
    }
}

/// A line's text with spacing fixed for scripts Tesseract splits into one
/// "word" per character: none between Han, kana or Thai characters,
/// Hangul words split where the gap says so, and spaces elsewhere as read.
pub fn line_text(text: &str, words: &[Word], height: f32) -> String {
    let needs_fix = text
        .chars()
        .any(|c| unspaced(c) || c.script() == Script::Hangul);
    if !needs_fix || words.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            let prev = &words[i - 1];
            let (a, b) = (
                prev.text.chars().last().unwrap_or(' '),
                w.text.chars().next().unwrap_or(' '),
            );
            let gap = (w.rect.x - prev.rect.right()) as f32 / height.max(1.0);
            let space = match (class(a), class(b)) {
                // Full-width and ASCII punctuation hug CJK text: "何？？？".
                (Class::Unspaced | Class::Punct, Class::Unspaced | Class::Punct) => false,
                (Class::Other, Class::Other) => true,
                // Hangul, and Latin next to CJK: follow the layout.
                _ => gap > WORD_GAP,
            };
            if space {
                out.push(' ');
            }
        }
        out.push_str(&w.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn automatic_plan_reads_english_first_and_rereads_with_the_rest() {
        let installed = langs(&["ara", "eng", "jpn", "jpn_vert", "kor"]);
        assert_eq!(
            plan(&[], &installed).unwrap(),
            Plan {
                primary: "eng".into(),
                rereaders: langs(&["ara", "jpn", "kor"]),
            }
        );
        // Without English the first installed model leads.
        assert_eq!(plan(&[], &langs(&["deu", "fra"])).unwrap().primary, "deu");
        assert!(plan(&[], &[]).is_err());
    }

    #[test]
    fn configured_plan_uses_exactly_those_models() {
        let installed = langs(&["eng", "jpn", "kor"]);
        assert_eq!(
            plan(&langs(&["jpn", "eng"]), &installed).unwrap(),
            Plan {
                primary: "jpn".into(),
                rereaders: langs(&["eng"]),
            }
        );
        let err = plan(&langs(&["eng", "tha"]), &installed)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"tha\" is not installed"), "{err}");
    }

    fn line(text: &str, conf: f32) -> Line {
        Line {
            rect: IRect::new(0, 0, 10, 10),
            text: text.into(),
            conf,
            words: Vec::new(),
        }
    }

    #[test]
    fn rereads_win_only_when_clearly_better_and_in_their_own_script() {
        let english = line("Tiéng Viét cé dau", 81.0);
        assert!(wins("vie", &line("Tiếng Việt có dấu", 96.8), &english));
        assert!(
            !wins("vie", &line("Tiếng Việt có dấu", 84.0), &english),
            "within the margin"
        );
        let primary = line("BABOTFRAMZ", 14.0);
        assert!(wins("jpn", &line("日本語のテキスト", 95.6), &primary));
        assert!(wins("jpn", &line("AVIFはGPUで", 92.0), &primary));
        // A CJK model "reading" an English line finds no CJK in it.
        let en = line("The quick brown fox jumps over 13 lazy dogs.", 80.0);
        assert!(!wins(
            "chi_tra",
            &line("The quick brown fox jumps over 13 |azy dogs﹒", 89.5),
            &en
        ));
        assert!(!wins("kor_vert", &line("lazy dogs", 99.0), &en));
        assert!(!wins("eng", &line("  ", 99.0), &en));
    }

    #[test]
    fn installed_languages_skip_non_language_data() {
        let dir = tempfile::tempdir().unwrap();
        for f in [
            "eng.traineddata",
            "osd.traineddata",
            "jpn.traineddata",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(f), b"").unwrap();
        }
        assert_eq!(installed_languages(dir.path()), langs(&["eng", "jpn"]));
    }

    fn word(x: i32, w: i32, text: &str) -> Word {
        Word {
            rect: IRect::new(x, 0, w, 30),
            text: text.into(),
        }
    }

    #[test]
    fn cjk_and_thai_lose_tesseracts_per_character_spaces() {
        let ja = [
            word(0, 28, "日"),
            word(44, 28, "本"),
            word(80, 28, "語"),
            word(120, 20, "、"),
            word(150, 28, "か"),
        ];
        assert_eq!(line_text("日 本 語 、 か", &ja, 30.0), "日本語、か");
        let th = [word(0, 20, "ภา"), word(24, 20, "ษา"), word(48, 20, "ไท")];
        assert_eq!(line_text("ภา ษา ไท", &th, 30.0), "ภาษาไท");
        let q = [word(0, 28, "何"), word(45, 10, "?"), word(75, 10, "?")];
        assert_eq!(line_text("何 ? ?", &q, 30.0), "何??");
    }

    #[test]
    fn hangul_and_mixed_lines_keep_spaces_where_the_gap_is_wide() {
        // 한국어 텍스트: syllables 7 px apart, words 15 px apart (height 30).
        let ko = [
            word(0, 28, "한"),
            word(35, 28, "국"),
            word(70, 28, "어"),
            word(113, 28, "텍"),
            word(148, 28, "스"),
        ];
        assert_eq!(line_text("한 국 어 텍 스", &ko, 30.0), "한국어 텍스");
        // Latin words inside a Japanese line: a real space stays, a tight
        // Latin-to-kana join does not.
        let mixed = [
            word(0, 40, "AVIF"),
            word(45, 28, "は"),
            word(90, 40, "GPU"),
            word(140, 28, "で"),
        ];
        assert_eq!(line_text("AVIF は GPU で", &mixed, 30.0), "AVIFは GPUで");
    }

    #[test]
    fn small_form_punctuation_becomes_full_width() {
        assert_eq!(normalize("何﹖﹖﹗ (a﹐b)"), "何？？！ (a，b)");
        assert_eq!(normalize("plain? text!"), "plain? text!");
    }

    #[test]
    fn latin_and_arabic_lines_are_left_as_read() {
        let en = [word(0, 30, "Hello"), word(32, 30, "world")];
        assert_eq!(line_text("Hello world", &en, 30.0), "Hello world");
        assert_eq!(line_text("مرحبا بالعالم", &[], 30.0), "مرحبا بالعالم");
    }

    #[test]
    fn prepare_inverts_dark_themes_and_upscales() {
        // Light text (200) on a dark background (20).
        let mut img = Image::filled(4, 2, [20, 20, 20, 255]);
        img.data[0..4].copy_from_slice(&[200, 200, 200, 255]);
        let g = prepare(&img, img.bounds(), 1);
        assert_eq!((g.width, g.height), (4, 2));
        assert!(g.data[0] < 60 && g.data[1] > 200, "{:?}", g.data);
        let g2 = prepare(&img, IRect::new(1, 0, 3, 2), 2);
        assert_eq!((g2.width, g2.height), (6, 4));
        assert!(
            g2.data.iter().all(|&v| v > 200),
            "region excludes the text pixel"
        );
        // Light themes stay as they are.
        let light = Image::filled(2, 2, [240, 240, 240, 255]);
        assert!(
            prepare(&light, light.bounds(), 1)
                .data
                .iter()
                .all(|&v| v > 200)
        );
    }

    #[test]
    fn bright_5k_screens_keep_correct_polarity_where_prefix_sums_are_largest() {
        // A white 5120x2880 page (prefix sums near 3.8e9, past what two of
        // them added in u32 can hold) with a dark panel and a light text
        // pixel in the far corner, where the sums are largest.
        let (w, h) = (5120usize, 2880usize);
        let mut img = Image::filled(w as u32, h as u32, [255, 255, 255, 255]);
        for y in h - 100..h {
            for x in w - 100..w {
                let i = (y * w + x) * 4;
                img.data[i..i + 3].copy_from_slice(&[20, 20, 20]);
            }
        }
        let i = ((h - 50) * w + (w - 50)) * 4;
        img.data[i..i + 3].copy_from_slice(&[230, 230, 230]);
        let g = prepare(&img, img.bounds(), 1);
        let at = |x: usize, y: usize| g.data[y * w + x];
        assert!(
            at(10, 10) > 200 && at(w - 200, h - 50) > 200,
            "page stays light"
        );
        assert!(at(w - 10, h - 10) > 200, "dark panel inverted to light");
        assert!(at(w - 50, h - 50) < 60, "its light text inverted to dark");
    }

    #[test]
    fn mixed_themes_become_dark_text_on_light_everywhere() {
        // Left half a dark panel with light text, right half a light page
        // with dark text, each 100 px wide.
        let mut img = Image::filled(200, 60, [245, 245, 245, 255]);
        for y in 0..60 {
            for x in 0..100 {
                let i = (y * 200 + x) * 4;
                img.data[i..i + 3].copy_from_slice(&[25, 25, 25]);
            }
        }
        let text = |img: &mut Image, x: usize, v: u8| {
            let i = (30 * 200 + x) * 4;
            img.data[i..i + 3].copy_from_slice(&[v, v, v]);
        };
        text(&mut img, 50, 230);
        text(&mut img, 150, 20);
        let g = prepare(&img, img.bounds(), 1);
        let at = |x: usize, y: usize| g.data[y * 200 + x];
        assert!(
            at(10, 10) > 200 && at(190, 10) > 200,
            "both backgrounds light"
        );
        assert!(at(50, 30) < 60 && at(150, 30) < 60, "both texts dark");
    }
}
