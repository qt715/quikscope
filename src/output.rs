//! Files written by more than one writer at a time: recordings, exports,
//! clipboard copies and screenshots.
//!
//! Every writer streams into its own partial file and publishes it with one
//! rename, so a reader never sees half a file and two writers never share a
//! partial. New files are additionally *reserved*: the final name is created
//! exclusively as an empty placeholder before writing starts, so concurrent
//! writers that derive the same name (two saves in the same second, a
//! recording and an editor export) end up with distinct files instead of
//! replacing each other.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};

/// One file being written. Dropping it without [`OutputFile::commit`] removes
/// the partial file and, for reservations, the placeholder.
#[derive(Debug)]
pub struct OutputFile {
    path: PathBuf,
    part: PathBuf,
    reserved: bool,
    done: bool,
}

impl OutputFile {
    /// Writes to exactly `path`; [`commit`](Self::commit) replaces whatever is
    /// there. For destinations the user chose explicitly.
    pub fn replace(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("Creating {}", dir.display()))?;
        }
        Self::open(path.to_owned(), false)
    }

    /// Reserves a new file named like `candidate`. When that name is taken
    /// (on disk or by another writer's reservation) it tries `name-1.ext`,
    /// `name-2.ext`, … and claims the first free one atomically.
    pub fn reserve(candidate: &Path) -> Result<Self> {
        let dir = candidate.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("Creating {}", dir.display()))?;
        let name = candidate
            .file_name()
            .context("Output path has no file name")?
            .to_string_lossy()
            .into_owned();
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        for n in 0u32.. {
            let path = if n == 0 {
                candidate.to_owned()
            } else {
                dir.join(format!("{stem}-{n}{ext}"))
            };
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Self::open(path, true),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("Reserving {}", path.display())),
            }
        }
        unreachable!("unbounded suffix search")
    }

    fn open(path: PathBuf, reserved: bool) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.{n}.part", std::process::id()));
        let part = path.with_file_name(name);
        let file = Self {
            path,
            part,
            reserved,
            done: false,
        };
        // Create it now so a writer that never writes still owns a file.
        File::create(&file.part).with_context(|| format!("Creating {}", file.part.display()))?;
        Ok(file)
    }

    /// The final path, valid once committed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the writer writes until [`commit`](Self::commit).
    pub fn part(&self) -> &Path {
        &self.part
    }

    /// Makes the writer write the reserved final file itself, for formats
    /// that are playable at every point they are flushed (fragmented MP4,
    /// Matroska): a crash then leaves a playable file under its real name
    /// instead of an oddly named partial. Dropping without commit still
    /// deletes it. Only for [`reserve`](Self::reserve)d files: writing in
    /// place over an existing file would destroy it before the new one is
    /// complete.
    pub fn write_in_place(&mut self) -> Result<()> {
        anyhow::ensure!(self.reserved, "Only reserved files can be written in place");
        if self.part != self.path {
            std::fs::remove_file(&self.part)
                .with_context(|| format!("Removing {}", self.part.display()))?;
            self.part = self.path.clone();
        }
        Ok(())
    }

    /// Publishes the partial file under the final name and returns it. If
    /// the rename fails, the partial file is kept (and named in the error)
    /// rather than deleted, so finished work is never lost.
    pub fn commit(mut self) -> Result<PathBuf> {
        if self.part != self.path
            && let Err(e) = std::fs::rename(&self.part, &self.path)
        {
            self.done = true;
            return Err(e).with_context(|| {
                format!(
                    "Saving {} (the finished file was kept as {})",
                    self.path.display(),
                    self.part.display()
                )
            });
        }
        self.done = true;
        Ok(std::mem::take(&mut self.path))
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let _ = std::fs::remove_file(&self.part);
        if self.reserved {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_reservations_get_distinct_names_and_drop_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("clip.mp4");
        let a = OutputFile::reserve(&candidate).unwrap();
        let b = OutputFile::reserve(&candidate).unwrap();
        assert_eq!(a.path(), candidate);
        assert_eq!(b.path(), dir.path().join("clip-1.mp4"));
        assert_ne!(a.part(), b.part());
        std::fs::write(a.part(), b"a").unwrap();
        let saved = a.commit().unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), b"a");
        drop(b);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["clip.mp4"]);
    }

    #[test]
    fn replace_keeps_the_old_file_until_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.gif");
        std::fs::write(&path, b"old").unwrap();
        let file = OutputFile::replace(&path).unwrap();
        std::fs::write(file.part(), b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let file = OutputFile::replace(&path).unwrap();
        std::fs::write(file.part(), b"new").unwrap();
        file.commit().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn in_place_writes_the_final_name_and_drop_still_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = OutputFile::reserve(&dir.path().join("rec.mp4")).unwrap();
        file.write_in_place().unwrap();
        assert_eq!(file.part(), file.path());
        std::fs::write(file.part(), b"frag").unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["rec.mp4"], "no partial file beside it");
        drop(file);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let mut file = OutputFile::reserve(&dir.path().join("rec.mp4")).unwrap();
        file.write_in_place().unwrap();
        std::fs::write(file.part(), b"frag").unwrap();
        assert_eq!(std::fs::read(file.commit().unwrap()).unwrap(), b"frag");
        assert!(
            OutputFile::replace(&dir.path().join("x.mp4"))
                .unwrap()
                .write_in_place()
                .is_err()
        );
    }
}
