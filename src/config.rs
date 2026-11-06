//! User configuration (`$XDG_CONFIG_HOME/quickscope/config.toml`) and the
//! filesystem locations derived from it.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where Ctrl+S in the editor writes screenshots. `~` expands to `$HOME`.
    pub save_dir: String,
    /// strftime-style file name: `%Y %m %d %H %M %S` and `%%` are supported.
    pub filename: String,
    /// Put every capture on the clipboard as `image/png`.
    pub copy_to_clipboard: bool,
    /// Show a desktop notification after each capture; clicking it opens the editor.
    pub notify: bool,
    /// Open the editor right after capturing instead of waiting for a
    /// notification click.
    pub open_editor: bool,
    /// Include the pointer in captures.
    pub cursor: bool,
    /// Freeze the desktop before region selection; false selects over the live desktop.
    pub freeze: bool,
    /// How long the notification stays up, in milliseconds (-1 = server default).
    pub notification_timeout_ms: i32,
    pub editor: EditorConfig,
    pub ocr: OcrConfig,
    /// Windows-only global shortcuts. Empty strings disable a binding.
    pub hotkeys: Hotkeys,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Hotkeys {
    pub region: String,
    pub screen: String,
    pub window: String,
    pub open_last: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            region: "PrintScreen".into(),
            screen: "Ctrl+PrintScreen".into(),
            window: "Alt+PrintScreen".into(),
            open_last: "Shift+PrintScreen".into(),
        }
    }
}

/// Win32 modifier flags and virtual-key code, also validated on Linux so a
/// shared configuration never silently changes meaning between platforms.
pub fn parse_hotkey(value: &str) -> Result<Option<(u32, u32)>> {
    if value.is_empty() {
        return Ok(None);
    }
    let parts: Vec<_> = value.split('+').map(str::trim).collect();
    let (key, modifiers) = parts.split_last().expect("split always has one element");
    let mut flags = 0;
    for modifier in modifiers {
        let flag = match modifier.to_ascii_lowercase().as_str() {
            "alt" => 1,
            "ctrl" | "control" => 2,
            "shift" => 4,
            "win" => 8,
            _ => anyhow::bail!("unknown hotkey modifier {modifier:?} in {value:?}"),
        };
        anyhow::ensure!(flags & flag == 0, "repeated hotkey modifier in {value:?}");
        flags |= flag;
    }
    let key = key.to_ascii_uppercase();
    let vk = match key.as_str() {
        "PRINTSCREEN" => 0x2c,
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => k.as_bytes()[0] as u32,
        k => match k.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
            Some(n @ 1..=24) => 0x70 + n - 1,
            _ => {
                anyhow::bail!("unsupported hotkey key {key:?}: use PrintScreen, F1–F24, A–Z or 0–9")
            }
        },
    };
    Ok(Some((flags, vk)))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OcrConfig {
    /// Tesseract models to read with, e.g. `["eng", "jpn"]`: the first
    /// reads every line, the rest re-read lines it is unsure of. Empty
    /// (the default): English first, then every installed model.
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    /// Initial annotation color as `#rrggbb` or `#rrggbbaa`.
    pub color: String,
    /// Initial stroke size: 0 = thin, 1 = medium, 2 = thick.
    pub stroke: u8,
    /// Close the editor after a successful Ctrl+S.
    pub close_on_save: bool,
    /// Default font for new text: any installed family, or the bundled "Inter".
    pub font_family: String,
    /// 100 (thin) … 900 (black).
    pub font_weight: u16,
    /// Logical pixels.
    pub font_size: f32,
    pub font_italic: bool,
}

impl EditorConfig {
    pub fn font(&self) -> crate::fonts::FontSpec {
        crate::fonts::FontSpec {
            family: self.font_family.as_str().into(),
            weight: self.font_weight,
            italic: self.font_italic,
            size: self.font_size,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            save_dir: default_save_dir(),
            filename: "screenshot_%Y%m%d_%H%M%S.png".into(),
            copy_to_clipboard: true,
            notify: true,
            open_editor: false,
            cursor: false,
            freeze: true,
            notification_timeout_ms: 8000,
            editor: EditorConfig::default(),
            ocr: OcrConfig::default(),
            hotkeys: Hotkeys::default(),
        }
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        EditorConfig {
            color: "#e03131".into(),
            stroke: 1,
            close_on_save: false,
            font_family: crate::fonts::DEFAULT_FAMILY.into(),
            font_weight: crate::fonts::DEFAULT_WEIGHT,
            font_size: crate::fonts::DEFAULT_SIZE,
            font_italic: false,
        }
    }
}

impl Config {
    /// Loads the config file, falling back to defaults when it does not exist.
    pub fn load() -> Result<Config> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        let e = &config.editor;
        crate::color::parse_hex(&e.color)
            .with_context(|| format!("editor.color {:?} is not #rrggbb or #rrggbbaa", e.color))?;
        anyhow::ensure!(
            (1..=1000).contains(&e.font_weight),
            "editor.font_weight {} is outside 1–1000",
            e.font_weight
        );
        anyhow::ensure!(
            (crate::fonts::MIN_SIZE..=crate::fonts::MAX_SIZE).contains(&e.font_size),
            "editor.font_size {} is outside {}–{}",
            e.font_size,
            crate::fonts::MIN_SIZE,
            crate::fonts::MAX_SIZE
        );
        for lang in &config.ocr.languages {
            anyhow::ensure!(
                !lang.is_empty()
                    && lang
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "ocr.languages: {lang:?} is not a Tesseract model name like \"eng\" or \"chi_sim\""
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (name, binding) in [
            ("region", &config.hotkeys.region),
            ("screen", &config.hotkeys.screen),
            ("window", &config.hotkeys.window),
            ("open_last", &config.hotkeys.open_last),
        ] {
            if let Some(key) = parse_hotkey(binding).with_context(|| format!("hotkeys.{name}"))? {
                anyhow::ensure!(
                    seen.insert(key),
                    "hotkeys.{name}: duplicate global shortcut {binding:?}"
                );
            }
        }
        Ok(config)
    }

    pub fn save_dir(&self) -> PathBuf {
        expand_home(&self.save_dir)
    }

    /// Path for a new screenshot taken at `time`. Never returns an existing
    /// file: a numeric suffix is added when two saves land in the same second.
    pub fn new_save_path(&self, time: SystemTime) -> PathBuf {
        let dir = self.save_dir();
        let name = format_time(&self.filename, time);
        let candidate = dir.join(&name);
        if !candidate.exists() {
            return candidate;
        }
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), format!(".{e}")),
            None => (name.clone(), String::new()),
        };
        (1..)
            .map(|n| dir.join(format!("{stem}-{n}{ext}")))
            .find(|p| !p.exists())
            .expect("unbounded suffix search")
    }
}

/// Sets top-level `key = value` in the config file at `path`, keeping every
/// other key, comment and the file's layout; creates the file if missing.
/// The result is validated before it replaces the old file.
pub fn persist_bool(path: &Path, key: &str, value: bool) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    doc[key] = toml_edit::value(value);
    let out = doc.to_string();
    Config::parse(&out)
        .with_context(|| format!("updating {} would make it invalid", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("toml.{}.part", std::process::id()));
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn default_save_dir() -> String {
    #[cfg(windows)]
    {
        crate::platform::windows::pictures_dir()
            .join("Screenshots")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(windows))]
    {
        "~/Pictures/Screenshots".into()
    }
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

#[cfg(not(windows))]
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    return home_env("APPDATA", "AppData/Roaming").join("quickscope/config.toml");
    #[cfg(not(windows))]
    xdg_dir("XDG_CONFIG_HOME", ".config").join("quickscope/config.toml")
}

pub fn cache_dir() -> PathBuf {
    #[cfg(windows)]
    return home_env("LOCALAPPDATA", "AppData/Local").join("quickscope");
    #[cfg(not(windows))]
    xdg_dir("XDG_CACHE_HOME", ".cache").join("quickscope")
}

#[cfg(windows)]
fn home_env(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// Uncompressed copy of the latest capture, read by `open-last` after a
/// daemon restart.
pub fn last_capture_path() -> PathBuf {
    cache_dir().join("last.raw")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// IPC socket. `QUICKSCOPE_SOCKET` overrides it (used by tests and to run
/// several instances side by side).
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("QUICKSCOPE_SOCKET") {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    return cache_dir().join("ipc.sock");
    #[cfg(not(windows))]
    {
        // Keyed by display so a nested compositor or X server gets its own
        // daemon.
        let display = match (std::env::var("WAYLAND_DISPLAY"), std::env::var("DISPLAY")) {
            (Ok(w), _) if !w.is_empty() => Path::new(&w)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(w),
            (_, Ok(x)) if !x.is_empty() => format!("x11-{}", x.replace([':', '/'], "")),
            _ => "wayland-0".into(),
        };
        runtime_dir().join(format!("quickscope-{display}.sock"))
    }
}

/// Local broken-down time via libc, so filenames follow the user's timezone
/// without pulling in a date crate.
pub fn format_time(pattern: &str, time: SystemTime) -> String {
    let secs = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let tm = local_tm(secs);
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", tm.year)),
            Some('m') => out.push_str(&format!("{:02}", tm.month)),
            Some('d') => out.push_str(&format!("{:02}", tm.day)),
            Some('H') => out.push_str(&format!("{:02}", tm.hour)),
            Some('M') => out.push_str(&format!("{:02}", tm.minute)),
            Some('S') => out.push_str(&format!("{:02}", tm.second)),
            Some('s') => out.push_str(&secs.to_string()),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

#[cfg(windows)]
pub fn local_tm(secs: i64) -> LocalTime {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ticks = secs
        .saturating_add(11_644_473_600)
        .saturating_mul(10_000_000) as u64;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // Null timezone uses Windows' current timezone and daylight-saving rules.
    if unsafe { FileTimeToSystemTime(&file, &mut utc) }.is_err()
        || unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_err()
    {
        return LocalTime {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
    }
    LocalTime {
        year: local.wYear as i32,
        month: local.wMonth as u32,
        day: local.wDay as u32,
        hour: local.wHour as u32,
        minute: local.wMinute as u32,
        second: local.wSecond as u32,
    }
}

#[cfg(not(windows))]
pub fn local_tm(secs: i64) -> LocalTime {
    let t: libc::time_t = secs;
    // SAFETY: zeroed `tm` is a valid out-parameter; both pointers outlive the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return LocalTime {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
    }
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_validate_modifiers_keys_and_collisions() {
        assert_eq!(parse_hotkey("Ctrl+Shift+F24").unwrap(), Some((6, 0x87)));
        assert_eq!(parse_hotkey("").unwrap(), None);
        assert!(parse_hotkey("Ctrl+Ctrl+A").is_err());
        assert!(parse_hotkey("Super+PrintScreen").is_err());
        assert!(parse_hotkey("F25").is_err());
        assert!(Config::parse("[hotkeys]\nregion = 'Ctrl+PrintScreen'").is_err());
        assert!(Config::parse("[hotkeys]\nregion = ''\nscreen = 'Win+S'").is_ok());
    }

    #[test]
    fn defaults_when_file_is_empty() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn freeze_defaults_true_and_accepts_boolean_overrides() {
        assert!(Config::default().freeze);
        assert!(Config::parse("").unwrap().freeze);
        assert!(Config::parse("freeze = true").unwrap().freeze);
        assert!(!Config::parse("freeze = false").unwrap().freeze);
        assert!(Config::parse("freeze = 'false'").is_err());
        assert!(Config::parse("freeze = 0").is_err());
    }

    #[test]
    fn partial_file_overrides_only_given_keys() {
        let c = Config::parse(
            "open_editor = true\nsave_dir = \"/tmp/shots\"\n[editor]\ncolor = \"#00ff00\"\n",
        )
        .unwrap();
        assert!(c.open_editor);
        assert_eq!(c.save_dir(), PathBuf::from("/tmp/shots"));
        assert_eq!(c.editor.color, "#00ff00");
        assert_eq!(c.editor.stroke, 1);
        assert!(c.copy_to_clipboard);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_colors() {
        assert!(Config::parse("save_directory = \"/x\"").is_err());
        assert!(Config::parse("[editor]\ncolor = \"red\"").is_err());
    }

    #[test]
    fn persisting_a_toggle_keeps_comments_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quickscope/config.toml");
        persist_bool(&path, "notify", false).unwrap();
        assert!(
            !Config::parse(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .notify
        );

        let original = "# my settings\nsave_dir = \"/tmp/x\" # inline\nnotify = false\n\n[editor]\nstroke = 2\n";
        std::fs::write(&path, original).unwrap();
        persist_bool(&path, "notify", true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# my settings") && text.contains("# inline"),
            "{text}"
        );
        let c = Config::parse(&text).unwrap();
        assert!(c.notify);
        assert_eq!(c.editor.stroke, 2);
        assert_eq!(c.save_dir, "/tmp/x");
    }

    #[test]
    fn persisting_never_writes_an_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "bogus_key = 1\n").unwrap();
        assert!(persist_bool(&path, "notify", true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bogus_key = 1\n");
    }

    #[test]
    fn editor_font_defaults_and_validation() {
        let c = Config::parse(
            "[editor]\nfont_family = \"Berkeley Mono\"\nfont_weight = 300\nfont_italic = true\nfont_size = 18\ncolor = \"#11223380\"\n",
        )
        .unwrap();
        let f = c.editor.font();
        assert_eq!(
            (&*f.family, f.weight, f.italic, f.size),
            ("Berkeley Mono", 300, true, 18.0)
        );
        assert_eq!(
            Config::default().editor.font(),
            crate::fonts::FontSpec::default()
        );
        assert!(Config::parse("[editor]\nfont_weight = 0").is_err());
        assert!(Config::parse("[editor]\nfont_size = 2000").is_err());
    }

    #[test]
    fn filename_pattern_expands_known_fields() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let tm = local_tm(1_700_000_000);
        let name = format_time("shot_%Y%m%d_%H%M%S_%%_%q.png", t);
        assert_eq!(
            name,
            format!(
                "shot_{:04}{:02}{:02}_{:02}{:02}{:02}_%_%q.png",
                tm.year, tm.month, tm.day, tm.hour, tm.minute, tm.second
            )
        );
    }

    #[test]
    fn save_path_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            save_dir: dir.path().to_string_lossy().into_owned(),
            filename: "fixed.png".into(),
            ..Config::default()
        };
        let t = SystemTime::now();
        let first = config.new_save_path(t);
        assert_eq!(first, dir.path().join("fixed.png"));
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(config.new_save_path(t), dir.path().join("fixed-1.png"));
    }

    #[test]
    fn home_expansion() {
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
        assert!(expand_home("~/Pictures").ends_with("Pictures"));
        assert!(!expand_home("~/Pictures").starts_with("~"));
    }
}
