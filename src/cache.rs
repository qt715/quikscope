//! The on-disk copy of the latest capture that `open-last` reads after a
//! daemon restart.
//!
//! One writer thread owns the file. Captures are queued in the order they
//! were taken; when several are waiting, only the newest is written, so a
//! slow write of an older shot can never land after (and replace) a newer
//! one, and two writes never overlap.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use crate::raster::Image;

enum Msg {
    Store(Arc<Image>, f32),
    Flush(mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct LastCapture {
    tx: mpsc::Sender<Msg>,
}

impl LastCapture {
    pub fn spawn(path: PathBuf) -> LastCapture {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("moment-cache".into())
            .spawn(move || run(path, rx))
            .expect("spawning the cache writer");
        LastCapture { tx }
    }

    /// Queues `image` as the latest capture. Never blocks.
    pub fn store(&self, image: Arc<Image>, scale: f32) {
        let _ = self.tx.send(Msg::Store(image, scale));
    }

    /// Blocks until every capture queued before this call is on disk (or
    /// superseded by a newer one that is).
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        if self.tx.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

fn run(path: PathBuf, rx: mpsc::Receiver<Msg>) {
    while let Ok(first) = rx.recv() {
        let mut latest = None;
        let mut waiters = Vec::new();
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                Msg::Store(image, scale) => latest = Some((image, scale)),
                Msg::Flush(done) => waiters.push(done),
            }
        }
        if let Some((image, scale)) = latest
            && let Err(e) = image.write_raw(&path, scale)
        {
            log::warn!("caching the capture failed: {e:#}");
        }
        for done in waiters {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_stores_leave_the_newest_capture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last.raw");
        let cache = LastCapture::spawn(path.clone());
        // Large enough that writes are still running while newer ones queue.
        for i in 1..=40u32 {
            let img = Image::filled(400 + i, 300, [i as u8, 0, 0, 255]);
            cache.store(Arc::new(img), i as f32);
        }
        cache.flush();
        let (img, scale) = Image::read_raw(&path).unwrap();
        assert_eq!((img.width, scale), (440, 40.0));
        assert_eq!(img.pixel(0, 0)[0], 40);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "last.raw")
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn flush_waits_for_a_store_made_just_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last.raw");
        let cache = LastCapture::spawn(path.clone());
        for round in 1..=5u32 {
            cache.store(Arc::new(Image::filled(round, 1, [0, 0, 0, 255])), 1.0);
            cache.flush();
            assert_eq!(Image::read_raw(&path).unwrap().0.width, round);
        }
    }
}
