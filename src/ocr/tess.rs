//! Tesseract's C API, loaded at run time.
//!
//! `libtesseract` is opened with `dlopen` the first time text is extracted,
//! so quikscope builds and runs without it and only text extraction needs the
//! package. One [`Engine`] holds one initialised model set; Tesseract
//! instances are not thread-safe, so each stays on the thread that made it.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::geom::IRect;

type Api = c_void;
type ResultIter = c_void;
type PageIter = c_void;

const RIL_TEXTLINE: c_int = 2;
const RIL_WORD: c_int = 3;

/// Page segmentation modes used here (Tesseract's `PSM_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Psm {
    /// Find blocks and lines on a whole page.
    Auto = 3,
    /// The image is exactly one line of text.
    SingleLine = 7,
}

struct Lib {
    _lib: libloading::Library,
    version: unsafe extern "C" fn() -> *const c_char,
    create: unsafe extern "C" fn() -> *mut Api,
    delete: unsafe extern "C" fn(*mut Api),
    init2: unsafe extern "C" fn(*mut Api, *const c_char, *const c_char, c_int) -> c_int,
    set_variable: unsafe extern "C" fn(*mut Api, *const c_char, *const c_char) -> c_int,
    set_psm: unsafe extern "C" fn(*mut Api, c_int),
    set_image: unsafe extern "C" fn(*mut Api, *const u8, c_int, c_int, c_int, c_int),
    set_resolution: unsafe extern "C" fn(*mut Api, c_int),
    recognize: unsafe extern "C" fn(*mut Api, *mut c_void) -> c_int,
    clear: unsafe extern "C" fn(*mut Api),
    get_iterator: unsafe extern "C" fn(*mut Api) -> *mut ResultIter,
    iter_delete: unsafe extern "C" fn(*mut ResultIter),
    iter_page: unsafe extern "C" fn(*mut ResultIter) -> *mut PageIter,
    iter_text: unsafe extern "C" fn(*mut ResultIter, c_int) -> *mut c_char,
    iter_confidence: unsafe extern "C" fn(*mut ResultIter, c_int) -> f32,
    iter_next: unsafe extern "C" fn(*mut ResultIter, c_int) -> c_int,
    page_bbox: unsafe extern "C" fn(
        *mut PageIter,
        c_int,
        *mut c_int,
        *mut c_int,
        *mut c_int,
        *mut c_int,
    ) -> c_int,
    page_begins: unsafe extern "C" fn(*mut PageIter, c_int) -> c_int,
    delete_text: unsafe extern "C" fn(*const c_char),
}

impl Lib {
    fn open() -> Result<Lib> {
        let mut last = None;
        #[cfg(windows)]
        let names = ["libtesseract-5.dll", "tesseract55.dll", "libtesseract.dll"];
        #[cfg(not(windows))]
        let names = ["libtesseract.so.5", "libtesseract.so"];
        for name in names {
            // SAFETY: loading a shared library runs its initialisers;
            // libtesseract's are ordinary C++ static constructors.
            match unsafe { libloading::Library::new(name) } {
                Ok(lib) => return Lib::bind(lib),
                Err(e) => last = Some(e),
            }
        }
        #[cfg(not(windows))]
        let message = "text extraction needs Tesseract (libtesseract.so.5): install `tesseract` \
                       and a language pack such as `tesseract-data-eng`";
        #[cfg(windows)]
        let message = "text extraction needs Tesseract (libtesseract-5.dll): install Tesseract \
                       and eng.traineddata, and add its DLL directory to PATH";
        Err(anyhow!(
            "{message} ({})",
            last.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    fn bind(lib: libloading::Library) -> Result<Lib> {
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: each symbol is declared with the signature of the
                // function of that name in Tesseract's capi.h.
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .with_context(|| format!("libtesseract lacks {}", $name))?
            };
        }
        Ok(Lib {
            version: sym!("TessVersion"),
            create: sym!("TessBaseAPICreate"),
            delete: sym!("TessBaseAPIDelete"),
            init2: sym!("TessBaseAPIInit2"),
            set_variable: sym!("TessBaseAPISetVariable"),
            set_psm: sym!("TessBaseAPISetPageSegMode"),
            set_image: sym!("TessBaseAPISetImage"),
            set_resolution: sym!("TessBaseAPISetSourceResolution"),
            recognize: sym!("TessBaseAPIRecognize"),
            clear: sym!("TessBaseAPIClear"),
            get_iterator: sym!("TessBaseAPIGetIterator"),
            iter_delete: sym!("TessResultIteratorDelete"),
            iter_page: sym!("TessResultIteratorGetPageIterator"),
            iter_text: sym!("TessResultIteratorGetUTF8Text"),
            iter_confidence: sym!("TessResultIteratorConfidence"),
            iter_next: sym!("TessResultIteratorNext"),
            page_bbox: sym!("TessPageIteratorBoundingBox"),
            page_begins: sym!("TessPageIteratorIsAtBeginningOf"),
            delete_text: sym!("TessDeleteText"),
            _lib: lib,
        })
    }
}

static LIB: OnceLock<Result<Arc<Lib>, String>> = OnceLock::new();

fn lib() -> Result<Arc<Lib>> {
    LIB.get_or_init(|| Lib::open().map(Arc::new).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow!(e))
}

/// The loaded Tesseract's version, e.g. "5.5.1".
pub fn version() -> Result<String> {
    let lib = lib()?;
    // SAFETY: TessVersion returns a static NUL-terminated string.
    Ok(unsafe { CStr::from_ptr((lib.version)()) }
        .to_string_lossy()
        .into_owned())
}

/// An 8-bit grayscale image, dark text on a light background.
#[derive(Clone, Debug, PartialEq)]
pub struct Gray {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// One recognised text line.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// In the pixels of the image that was recognised.
    pub rect: IRect,
    pub text: String,
    /// Tesseract's confidence, 0–100.
    pub conf: f32,
    /// The line's words with their boxes, for rebuilding spacing.
    pub words: Vec<Word>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub rect: IRect,
    pub text: String,
}

/// One Tesseract instance with a model set loaded, e.g. `"eng"` or
/// `"jpn"`.
pub struct Engine {
    lib: Arc<Lib>,
    api: *mut Api,
    pub lang: String,
}

// SAFETY: a TessBaseAPI may move between threads; it is only ever used by
// one thread at a time (`&mut self`).
unsafe impl Send for Engine {}

impl Engine {
    pub fn new(tessdata: &Path, lang: &str) -> Result<Engine> {
        let lib = lib()?;
        let dir = CString::new(tessdata.as_os_str().as_encoded_bytes())?;
        let lang_c = CString::new(lang)?;
        // SAFETY: plain constructor; checked for NULL below.
        let api = unsafe { (lib.create)() };
        if api.is_null() {
            bail!("TessBaseAPICreate failed");
        }
        let engine = Engine {
            lib,
            api,
            lang: lang.to_string(),
        };
        // LSTM only: distribution models also carry the legacy recogniser,
        // which would more than double each model's memory for no gain on
        // screen text.
        const OEM_LSTM_ONLY: c_int = 1;
        // SAFETY: `api` is live, strings are NUL-terminated and outlive the call.
        let rc = unsafe { (engine.lib.init2)(api, dir.as_ptr(), lang_c.as_ptr(), OEM_LSTM_ONLY) };
        if rc != 0 {
            bail!(
                "Tesseract could not load {lang:?} from {}",
                tessdata.display()
            );
        }
        for (k, v) in [
            // Images arrive already as dark-on-light; skipping Tesseract's
            // own inverted retry of weak lines halves the time on them.
            ("tessedit_do_invert", "0"),
            // Keep its diagnostics ("Detected N diacritics") off stderr.
            ("debug_file", "/dev/null"),
        ] {
            let (k, v) = (CString::new(k)?, CString::new(v)?);
            // SAFETY: as above.
            unsafe { (engine.lib.set_variable)(api, k.as_ptr(), v.as_ptr()) };
        }
        Ok(engine)
    }

    /// Recognises `image` and returns its text lines in reading order.
    pub fn recognize(&mut self, image: &Gray, psm: Psm) -> Result<Vec<Line>> {
        let lib = &self.lib;
        let (w, h) = (image.width as c_int, image.height as c_int);
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `image.data` holds w*h bytes, one per pixel, and outlives
        // the recognition, which finishes before `clear` below.
        unsafe {
            (lib.set_psm)(self.api, psm as c_int);
            (lib.set_image)(self.api, image.data.as_ptr(), w, h, 1, w);
            // Screen text after preprocessing is about 2x a 96 dpi screen.
            (lib.set_resolution)(self.api, 200);
            if (lib.recognize)(self.api, std::ptr::null_mut()) != 0 {
                (lib.clear)(self.api);
                bail!("Tesseract recognition failed");
            }
        }
        let mut lines = Vec::new();
        // SAFETY: the iterator belongs to `api`'s current results and is
        // deleted before they are cleared; every returned string is freed
        // with TessDeleteText.
        unsafe {
            let it = (lib.get_iterator)(self.api);
            if !it.is_null() {
                let text_at = |level| {
                    let p = (lib.iter_text)(it, level);
                    if p.is_null() {
                        return None;
                    }
                    let s = CStr::from_ptr(p).to_string_lossy().trim_end().to_string();
                    (lib.delete_text)(p);
                    Some(s)
                };
                let bbox_at = |page, level| {
                    let (mut l, mut t, mut r, mut b) = (0, 0, 0, 0);
                    (lib.page_bbox)(page, level, &mut l, &mut t, &mut r, &mut b);
                    IRect::new(l, t, (r - l).max(1), (b - t).max(1))
                };
                // Walk words; a word that starts a line opens a new `Line`.
                loop {
                    let page = (lib.iter_page)(it);
                    if (lib.page_begins)(page, RIL_TEXTLINE) != 0 || lines.is_empty() {
                        lines.push(Line {
                            rect: bbox_at(page, RIL_TEXTLINE),
                            text: text_at(RIL_TEXTLINE).unwrap_or_default(),
                            conf: (lib.iter_confidence)(it, RIL_TEXTLINE),
                            words: Vec::new(),
                        });
                    }
                    if let Some(w) = text_at(RIL_WORD).filter(|w| !w.trim().is_empty()) {
                        let line = lines.last_mut().expect("pushed above");
                        line.words.push(Word {
                            rect: bbox_at(page, RIL_WORD),
                            text: w,
                        });
                    }
                    if (lib.iter_next)(it, RIL_WORD) == 0 {
                        break;
                    }
                }
                (lib.iter_delete)(it);
            }
            (lib.clear)(self.api);
        }
        lines.retain(|l| !l.text.trim().is_empty());
        Ok(lines)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: `api` came from TessBaseAPICreate and is dropped once.
        unsafe { (self.lib.delete)(self.api) };
    }
}
