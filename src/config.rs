//! User configuration (`$XDG_CONFIG_HOME/moment/config.toml`) and the
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
    /// How long the notification stays up, in milliseconds (-1 = server default).
    pub notification_timeout_ms: i32,
    pub editor: EditorConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    /// Initial annotation color as `#rrggbb`.
    pub color: String,
    /// Initial stroke size: 0 = thin, 1 = medium, 2 = thick.
    pub stroke: u8,
    /// Close the editor after a successful Ctrl+S.
    pub close_on_save: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            save_dir: "~/Pictures/Screenshots".into(),
            filename: "screenshot_%Y%m%d_%H%M%S.png".into(),
            copy_to_clipboard: true,
            notify: true,
            open_editor: false,
            cursor: false,
            notification_timeout_ms: 8000,
            editor: EditorConfig::default(),
        }
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        EditorConfig {
            color: "#e03131".into(),
            stroke: 1,
            close_on_save: false,
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
        parse_hex_color(&config.editor.color)
            .with_context(|| format!("editor.color {:?} is not #rrggbb", config.editor.color))?;
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

pub fn parse_hex_color(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
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

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

pub fn config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("moment/config.toml")
}

pub fn cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache").join("moment")
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

/// IPC socket. `MOMENT_SOCKET` overrides it (used by tests and to run
/// several instances side by side).
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MOMENT_SOCKET") {
        return PathBuf::from(p);
    }
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    // Keyed by display so a nested compositor gets its own daemon.
    let display = Path::new(&display)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(display);
    runtime_dir().join(format!("moment-{display}.sock"))
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
    fn defaults_when_file_is_empty() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
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
        let path = dir.path().join("moment/config.toml");
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
    fn hex_colors() {
        assert_eq!(parse_hex_color("#e03131"), Some(0xe03131));
        assert_eq!(parse_hex_color("e03131"), None);
        assert_eq!(parse_hex_color("#e0313"), None);
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
