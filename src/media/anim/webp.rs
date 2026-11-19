//! WebP: libwebp's animation encoder (webp-animation, static libwebp) for
//! writing, image-webp for reading still and animated files.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use image_webp::WebPDecoder;
use webp_animation::{
    AnimParams, ColorMode, EncoderOptions, EncodingConfig, EncodingType, LossyEncodingConfig,
};

use super::{AnimDecoder, AnimEncoder, AnimFrame, AnimInfo, TickClock, to_bgra};
use crate::raster::Image;

/// libwebp quality for lossy frames; `allow_mixed` still codes a frame
/// losslessly when that is smaller (flat screen content often is).
const QUALITY: f32 = 85.0;

pub struct WebpDecoder {
    path: PathBuf,
    info: AnimInfo,
    decoder: WebPDecoder<BufReader<File>>,
    /// The decoder's canvas (RGB or RGBA); animated frames are drawn onto
    /// the previous canvas.
    canvas: Vec<u8>,
    next: u64,
    pts: Duration,
}

fn open(path: &Path) -> Result<WebPDecoder<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = WebPDecoder::new(BufReader::new(file))
        .with_context(|| format!("reading WebP {}", path.display()))?;
    if decoder.is_animated() {
        // Browsers clear disposed frames to transparent, whatever the
        // file's background hint says.
        decoder.set_background_color([0; 4])?;
    }
    Ok(decoder)
}

impl WebpDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let decoder = open(path)?;
        let (width, height) = decoder.dimensions();
        let (frames, duration) = if decoder.is_animated() {
            let frames = u64::from(decoder.num_frames());
            ensure!(frames > 0, "WebP {} has no frames", path.display());
            (frames, Duration::from_millis(decoder.loop_duration()))
        } else {
            (1, Duration::ZERO)
        };
        let len = decoder
            .output_buffer_size()
            .context("WebP image too large")?;
        Ok(WebpDecoder {
            path: path.to_owned(),
            info: AnimInfo {
                width,
                height,
                frames,
                duration,
            },
            decoder,
            canvas: vec![0; len],
            next: 0,
            pts: Duration::ZERO,
        })
    }
}

impl AnimDecoder for WebpDecoder {
    fn info(&self) -> &AnimInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<AnimFrame>> {
        if self.next == self.info.frames {
            return Ok(None);
        }
        let duration = if self.decoder.is_animated() {
            Duration::from_millis(u64::from(self.decoder.read_frame(&mut self.canvas)?))
        } else {
            self.decoder.read_image(&mut self.canvas)?;
            Duration::ZERO
        };
        self.next += 1;
        let channels = if self.decoder.has_alpha() { 4 } else { 3 };
        let mut image = Image::new(self.info.width, self.info.height);
        to_bgra(&self.canvas, channels, &mut image.data);
        let pts = self.pts;
        self.pts += duration;
        Ok(Some(AnimFrame {
            image,
            pts,
            duration,
        }))
    }

    fn rewind(&mut self) -> Result<()> {
        // A still image reads its pixels only once per decoder.
        self.decoder = open(&self.path)?;
        self.canvas.fill(0);
        self.next = 0;
        self.pts = Duration::ZERO;
        Ok(())
    }
}

/// libwebp's `WebPAnimEncoder` and its picture buffer.
struct SendEncoder(webp_animation::Encoder);

// SAFETY: a libwebp animation encoder has no thread affinity; `&mut self`
// methods keep calls on it sequential.
unsafe impl Send for SendEncoder {}

pub struct WebpEncoder {
    path: PathBuf,
    encoder: SendEncoder,
    width: u32,
    height: u32,
    clock: TickClock,
    last_start: Option<u64>,
}

impl WebpEncoder {
    /// The size is within [`super::check_size`] (as `create_encoder` checks).
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        let options = EncoderOptions {
            anim_params: AnimParams { loop_count: 0 },
            allow_mixed: true,
            color_mode: ColorMode::Bgra,
            encoding_config: Some(EncodingConfig {
                encoding_type: EncodingType::Lossy(LossyEncodingConfig {
                    // Keeps thin colored text from bleeding at 4:2:0.
                    use_sharp_yuv: true,
                    segments: 4,
                    ..LossyEncodingConfig::default()
                }),
                quality: QUALITY,
                method: 4,
            }),
            ..EncoderOptions::default()
        };
        let encoder = webp_animation::Encoder::new_with_options((width, height), options)
            .map_err(|e| anyhow!("starting WebP encoder: {e}"))?;
        Ok(WebpEncoder {
            path: path.to_owned(),
            encoder: SendEncoder(encoder),
            width,
            height,
            clock: TickClock::new(1000),
            last_start: None,
        })
    }
}

impl AnimEncoder for WebpEncoder {
    fn push(&mut self, image: &Image, duration: Duration) -> Result<()> {
        ensure!(
            (image.width, image.height) == (self.width, self.height),
            "{}x{} frame for a {}x{} WebP",
            image.width,
            image.height,
            self.width,
            self.height
        );
        let (start, _) = self.clock.advance(duration);
        // A frame starting on the same millisecond as the one before would
        // give that one no time (libwebp wants increasing timestamps); the
        // earlier frame is kept and this one dropped.
        if self.last_start == Some(start) {
            return Ok(());
        }
        let timestamp = i32::try_from(start).context("WebP animation too long")?;
        self.encoder
            .0
            .add_frame(&image.data, timestamp)
            .map_err(|e| anyhow!("encoding WebP frame: {e}"))?;
        self.last_start = Some(start);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let end = i32::try_from(self.clock.now()).context("WebP animation too long")?;
        let data = self
            .encoder
            .0
            .finalize(end)
            .map_err(|e| anyhow!("finishing WebP: {e}"))?;
        std::fs::write(&self.path, &*data)
            .with_context(|| format!("writing {}", self.path.display()))
    }
}
