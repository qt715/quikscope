//! Save as: the platform's save dialog (the file chooser portal on Linux,
//! the common Save dialog on Windows) for the annotator and the video
//! editor, and where a save then writes.

use std::path::{Path, PathBuf};

use anyhow::Result;
use gpui::{App, Global};

use crate::media::OutputFormat;
use crate::output::OutputFile;

/// Folder of the last file chosen in the dialog, where the next one opens.
/// Kept while the daemon runs, not in the configuration.
struct LastFolder(PathBuf);

impl Global for LastFolder {}

/// Asks where to save. The dialog opens in the folder chosen last, else in
/// `candidate`'s folder (where a plain save would write), with
/// `candidate`'s name filled in. Resolves to `None` when cancelled.
pub fn prompt(candidate: &Path, cx: &App) -> impl Future<Output = Result<Option<PathBuf>>> + use<> {
    let folder = cx
        .try_global::<LastFolder>()
        .map(|last| last.0.clone())
        .filter(|folder| folder.is_dir())
        .unwrap_or_else(|| existing_folder(candidate.parent()));
    let name = candidate
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let answer = cx.prompt_for_new_path(&folder, name.as_deref());
    // A dropped sender (no dialog could be shown) reads as a cancel.
    async move { answer.await.unwrap_or(Ok(None)) }
}

/// Remembers the folder of `chosen` for the next dialog.
pub fn remember(chosen: &Path, cx: &mut App) {
    if let Some(folder) = chosen.parent() {
        cx.set_global(LastFolder(folder.to_owned()));
    }
}

/// The nearest existing folder at or above `folder` (the default save
/// folder may not exist before the first save creates it).
fn existing_folder(folder: Option<&Path>) -> PathBuf {
    folder
        .into_iter()
        .flat_map(Path::ancestors)
        .find(|dir| dir.is_dir())
        .map_or_else(|| crate::config::expand_home("~"), Path::to_owned)
}

/// Where a save writes.
#[derive(Clone, Debug, PartialEq)]
pub enum Destination {
    /// Exactly this file, replacing it: a repeat save, or a name chosen in
    /// the dialog, which already asked before overwriting.
    Replace(PathBuf),
    /// A new file named like this, numbered when the name is taken.
    Reserve(PathBuf),
}

impl Destination {
    pub fn open(&self) -> Result<OutputFile> {
        match self {
            Destination::Replace(path) => OutputFile::replace(path),
            Destination::Reserve(path) => OutputFile::reserve(path),
        }
    }
}

/// Where a recording or animation chosen as `chosen` is written, and in
/// which format: the one its extension names when `allowed`, else
/// `fallback` with its extension added.
pub fn media(
    chosen: PathBuf,
    allowed: impl Fn(OutputFormat) -> bool,
    fallback: OutputFormat,
) -> (Destination, OutputFormat) {
    match OutputFormat::from_path(&chosen).filter(|&f| allowed(f)) {
        Some(format) => (Destination::Replace(chosen), format),
        None => (with_extension(chosen, fallback.extension()), fallback),
    }
}

/// Where a screenshot chosen as `chosen` is written: screenshots are PNG,
/// so any other name gets `.png` added.
pub fn png(chosen: PathBuf) -> Destination {
    let is_png = chosen
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
    if is_png {
        Destination::Replace(chosen)
    } else {
        with_extension(chosen, "png")
    }
}

/// `chosen` plus `.extension`. The dialog only confirmed replacing
/// `chosen` itself, so the longer name never replaces an existing file.
fn with_extension(chosen: PathBuf, extension: &str) -> Destination {
    let mut name = chosen.into_os_string();
    name.push(".");
    name.push(extension);
    Destination::Reserve(name.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_supported_extension_picks_the_format_and_the_exact_file() {
        let any = |_| true;
        assert_eq!(
            media("/v/clip.GIF".into(), any, OutputFormat::Mp4),
            (
                Destination::Replace("/v/clip.GIF".into()),
                OutputFormat::Gif
            )
        );
        assert_eq!(
            media("/v/clip".into(), any, OutputFormat::Mp4),
            (
                Destination::Reserve("/v/clip.mp4".into()),
                OutputFormat::Mp4
            )
        );
        // The annotator writes no video: an .mp4 name gets the animation
        // format's extension instead of a video it cannot make.
        assert_eq!(
            media("/v/clip.mp4".into(), |f| !f.is_video(), OutputFormat::Webp),
            (
                Destination::Reserve("/v/clip.mp4.webp".into()),
                OutputFormat::Webp
            )
        );
        assert_eq!(
            media("/v/notes.v2".into(), any, OutputFormat::Avif).0,
            Destination::Reserve("/v/notes.v2.avif".into())
        );
    }

    #[test]
    fn screenshots_are_always_png() {
        assert_eq!(
            png("/s/a.PNG".into()),
            Destination::Replace("/s/a.PNG".into())
        );
        assert_eq!(png("/s/a".into()), Destination::Reserve("/s/a.png".into()));
        assert_eq!(
            png("/s/a.jpg".into()),
            Destination::Reserve("/s/a.jpg.png".into())
        );
    }
}
