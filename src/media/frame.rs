//! One captured video frame: an FFmpeg frame (CPU or hardware) plus the
//! moment it was captured.

use std::time::Instant;

use ffmpeg_next as ffmpeg;

use crate::raster::Image;

/// A captured frame. CPU frames are BGRA (`AV_PIX_FMT_BGRA`, or `BGR0` when
/// opaque); hardware frames (DRM PRIME, D3D11, VideoToolbox, CUDA, VAAPI)
/// carry their `hw_frames_ctx` so the encoder can map or download them.
pub struct Frame {
    pub(crate) av: ffmpeg::frame::Video,
    /// When the pixels were captured; the session derives timestamps from it.
    pub captured: Instant,
}

// SAFETY: an AVFrame is reference-counted data with no thread affinity; the
// session moves each frame to its encoder thread exactly once.
unsafe impl Send for Frame {}

impl Frame {
    /// Wraps a frame built by a platform source (hardware frames included).
    pub fn from_av(av: ffmpeg::frame::Video, captured: Instant) -> Frame {
        Frame { av, captured }
    }

    /// Copies BGRA rows (`stride` bytes apart) into a new CPU frame. `opaque`
    /// frames are tagged `BGR0` so encoders ignore the alpha byte.
    pub fn from_bgra(
        width: u32,
        height: u32,
        stride: usize,
        data: &[u8],
        opaque: bool,
        captured: Instant,
    ) -> Frame {
        let format = if opaque {
            ffmpeg::format::Pixel::BGRZ
        } else {
            ffmpeg::format::Pixel::BGRA
        };
        let mut av = ffmpeg::frame::Video::new(format, width, height);
        let dst_stride = av.stride(0);
        let row = width as usize * 4;
        let dst = av.data_mut(0);
        for y in 0..height as usize {
            dst[y * dst_stride..y * dst_stride + row]
                .copy_from_slice(&data[y * stride..y * stride + row]);
        }
        Frame { av, captured }
    }

    pub fn from_image(image: &Image, captured: Instant) -> Frame {
        Frame::from_bgra(
            image.width,
            image.height,
            image.stride(),
            &image.data,
            image.is_opaque(),
            captured,
        )
    }

    pub fn width(&self) -> u32 {
        self.av.width()
    }

    pub fn height(&self) -> u32 {
        self.av.height()
    }

    pub fn av(&self) -> &ffmpeg::frame::Video {
        &self.av
    }

    pub fn into_av(self) -> ffmpeg::frame::Video {
        self.av
    }
}
