//! Clipboard content shared by every platform backend.

use std::sync::{Arc, OnceLock};

use crate::raster::{Compression, Image};

/// Encoded PNG that may still be in progress.
pub struct PendingPng(OnceLock<Result<Vec<u8>, String>>);

impl PendingPng {
    /// Starts encoding on a worker thread and returns immediately.
    pub fn encode(image: Arc<Image>) -> Arc<PendingPng> {
        let png = Arc::new(PendingPng(OnceLock::new()));
        let out = png.clone();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = image
                .encode_png(Compression::Fast)
                .map_err(|e| format!("{e:#}"));
            log::debug!(
                "clipboard PNG encoded in {:.1}ms",
                started.elapsed().as_secs_f64() * 1e3
            );
            let _ = out.0.set(result);
        });
        png
    }

    /// Blocks until the encoder finishes.
    pub fn wait(&self) -> Result<&[u8], &str> {
        self.0.wait().as_deref().map_err(|e| e.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_png_waits_for_the_encoder() {
        let img = Arc::new(Image::filled(64, 32, [1, 2, 3, 255]));
        let png = PendingPng::encode(img.clone());
        let bytes = png.wait().unwrap();
        assert_eq!(&Image::decode(bytes).unwrap(), img.as_ref());
    }
}
