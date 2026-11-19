//! The clip cache: files quikscope hands to other apps by path, such as
//! exports copied to the clipboard and video frames opened in the annotator.
//!
//! Every clip gets a directory of its own, so it keeps a friendly name (the
//! one a chat app shows) without colliding with another clip of that name.
//! Clips older than a day are pruned whenever a new one is made, except
//! anything pinned: a source an editor shows or an export reads, and exports
//! still being written.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};

/// Clips older than this are pruned; the clipboard only needs recent ones.
const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

struct Registry {
    /// One entry per live [`Pin`]; a path pinned twice appears twice.
    pins: Vec<PathBuf>,
    /// Files to remove when their last pin goes.
    doomed: Vec<PathBuf>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    pins: Vec::new(),
    doomed: Vec::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

fn remove_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("removing {}: {e}", path.display());
    }
}

/// Keeps a file, or a clip directory and everything in it, from being pruned
/// or removed until dropped.
#[derive(Debug)]
pub struct Pin(PathBuf);

pub fn pin(path: &Path) -> Pin {
    registry().pins.push(path.to_owned());
    Pin(path.to_owned())
}

impl Drop for Pin {
    fn drop(&mut self) {
        let mut r = registry();
        if let Some(i) = r.pins.iter().position(|p| *p == self.0) {
            r.pins.swap_remove(i);
        }
        if !r.pins.contains(&self.0)
            && let Some(i) = r.doomed.iter().position(|p| *p == self.0)
        {
            r.doomed.swap_remove(i);
            // Under the lock, so nothing pins it again while it goes.
            remove_file(&self.0);
        }
    }
}

/// Removes the file at `path` once nothing pins it: now, or when the last
/// [`Pin`] of it drops (an editor showing it closes, an export reading it
/// finishes).
pub fn remove_when_unused(path: PathBuf) {
    let mut r = registry();
    if !r.pins.contains(&path) {
        remove_file(&path);
    } else if !r.doomed.contains(&path) {
        r.doomed.push(path);
    }
}

/// The directory of a clip being written, pinned until dropped. Dropped
/// without its file (a failed or cancelled export), it leaves nothing behind.
#[derive(Debug)]
pub struct Clip {
    dir: PathBuf,
    _pin: Pin,
}

impl Clip {
    /// Prunes old clips under `root`, then makes a new clip directory there.
    /// Returns it and where a file called `name` goes in it.
    pub fn create(root: &Path, name: &str) -> Result<(Clip, PathBuf)> {
        prune(root, SystemTime::now());
        std::fs::create_dir_all(root).with_context(|| format!("Creating {}", root.display()))?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        for n in 0u32.. {
            let dir = root.join(format!("{stamp}-{}-{n}", std::process::id()));
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    let file = dir.join(name);
                    let pin = pin(&dir);
                    return Ok((Clip { dir, _pin: pin }, file));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("Creating {}", dir.display())),
            }
        }
        unreachable!("unbounded name search")
    }
}

impl Drop for Clip {
    fn drop(&mut self) {
        // Only an empty directory goes: a committed clip stays.
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Removes everything directly under `root` last modified more than
/// [`MAX_AGE`] before `now`, unless something pinned is in it.
fn prune(root: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MAX_AGE);
        if !old {
            continue;
        }
        let path = entry.path();
        // Held while removing, so nothing is pinned inside it meanwhile.
        let r = registry();
        if r.pins.iter().any(|p| p.starts_with(&path)) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                log::warn!("pruning {}: {e}", path.display());
            }
        } else {
            remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later() -> SystemTime {
        SystemTime::now() + MAX_AGE * 2
    }

    #[test]
    fn clips_keep_their_names_in_separate_directories() {
        let root = tempfile::tempdir().unwrap();
        let (a, path_a) = Clip::create(root.path(), "clip.mp4").unwrap();
        let (b, path_b) = Clip::create(root.path(), "clip.mp4").unwrap();
        assert_ne!(path_a, path_b);
        assert_eq!(path_a.file_name(), path_b.file_name());
        std::fs::write(&path_a, b"a").unwrap();
        drop((a, b));
        // The committed clip stays; the abandoned one leaves no directory.
        assert!(path_a.exists());
        assert!(!path_b.parent().unwrap().exists());
    }

    #[test]
    fn pruning_keeps_pinned_sources_and_clips_in_flight() {
        let root = tempfile::tempdir().unwrap();
        let (_writing, in_flight) = Clip::create(root.path(), "new.gif").unwrap();
        let (done, viewed) = Clip::create(root.path(), "viewed.mp4").unwrap();
        std::fs::write(&viewed, b"v").unwrap();
        drop(done);
        let (done, stale) = Clip::create(root.path(), "stale.mp4").unwrap();
        std::fs::write(&stale, b"s").unwrap();
        drop(done);
        let loose = root.path().join("loose.webp");
        std::fs::write(&loose, b"l").unwrap();

        let source = pin(&viewed);
        prune(root.path(), later());
        assert!(
            in_flight.parent().unwrap().exists(),
            "in-flight export pruned"
        );
        assert!(viewed.exists(), "pinned source pruned");
        assert!(!stale.parent().unwrap().exists());
        assert!(!loose.exists());

        drop(source);
        prune(root.path(), later());
        assert!(!viewed.exists());
        // Recent clips survive an ordinary prune.
        let (clip, fresh) = Clip::create(root.path(), "fresh.png").unwrap();
        std::fs::write(&fresh, b"f").unwrap();
        drop(clip);
        prune(root.path(), SystemTime::now());
        assert!(fresh.exists());
    }

    #[test]
    fn removal_waits_for_the_last_pin() {
        let dir = tempfile::tempdir().unwrap();
        let master = dir.path().join("master.qsm");
        std::fs::write(&master, b"m").unwrap();
        let (editor, export) = (pin(&master), pin(&master));
        remove_when_unused(master.clone());
        drop(editor);
        assert!(master.exists());
        drop(export);
        assert!(!master.exists());

        std::fs::write(&master, b"m").unwrap();
        remove_when_unused(master.clone());
        assert!(!master.exists());
    }
}
