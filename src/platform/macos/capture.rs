//! ScreenCaptureKit streams work on 12.3, unlike the still-image API added in 14.
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, ensure};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::capture::{OutputFrame, Snapshot};
use crate::platform::Capture;
use crate::raster::Image;

pub struct ScreenCapture;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

impl Capture for ScreenCapture {
    fn capture(&self, names: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        // Permission is checked per request so granting access never requires a daemon restart.
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            unsafe { CGRequestScreenCaptureAccess() };
            anyhow::bail!(
                "Screen Recording permission is required. Enable quikscope in System Settings → Privacy & Security → Screen Recording, then retry (macOS may require restarting the app)."
            );
        }
        objc2::rc::autoreleasepool(|_| {
            let (tx, rx) = mpsc::sync_channel(1);
            let handler = RcBlock::new(
                move |content: *mut SCShareableContent, error: *mut NSError| {
                    let result = unsafe {
                        if let Some(error) = error.as_ref() {
                            Err(anyhow!(
                                "ScreenCaptureKit: {}",
                                error.localizedDescription()
                            ))
                        } else if let Some(content) = content.as_ref() {
                            Ok(content.displays())
                        } else {
                            Err(anyhow!("ScreenCaptureKit returned no displays"))
                        }
                    };
                    let _ = tx.send(result);
                },
            );
            unsafe {
                SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false, true, &handler);
            }
            let displays = rx
                .recv_timeout(Duration::from_secs(10))
                .context("enumerating ScreenCaptureKit displays")??;
            let mut outputs = Vec::new();
            for display in displays.iter() {
                let id = unsafe { display.displayID() };
                let name = id.to_string();
                if names.is_some_and(|names| !names.contains(&name)) {
                    continue;
                }
                let logical = super::desktop::display_rect(id);
                let image = capture_display(&display, logical, cursor)?;
                outputs.push(OutputFrame {
                    name,
                    logical,
                    image: Arc::new(image),
                });
            }
            ensure!(!outputs.is_empty(), "no matching macOS displays");
            Ok(Snapshot { outputs })
        })
    }
}

fn capture_display(
    display: &SCDisplay,
    logical: crate::geom::IRect,
    cursor: bool,
) -> Result<Image> {
    let id = unsafe { display.displayID() };
    let mode = core_graphics::display::CGDisplay::new(id)
        .display_mode()
        .context("display disconnected before capture")?;
    let (width, height) = super::desktop::physical_size(logical, mode.width(), mode.pixel_width());
    let (tx, rx) = mpsc::sync_channel(1);
    let output = FrameReceiver::new(tx);
    let queue = DispatchQueue::new("io.github.qt715.quikscope.capture", None);
    unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::<SCWindow>::new(),
        );
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(width as usize);
        configuration.setHeight(height as usize);
        configuration.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
        configuration.setShowsCursor(cursor);
        configuration.setScalesToFit(true);
        configuration.setQueueDepth(1);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            None,
        );
        let protocol = ProtocolObject::from_ref(&*output);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                protocol,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| {
                anyhow!(
                    "adding ScreenCaptureKit output: {}",
                    e.localizedDescription()
                )
            })?;
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let started = RcBlock::new(move |error: *mut NSError| {
            let error = error.as_ref().map(|e| e.localizedDescription().to_string());
            let _ = start_tx.send(error);
        });
        stream.startCaptureWithCompletionHandler(Some(&started));
        let result = (|| {
            if let Some(error) = start_rx
                .recv_timeout(Duration::from_secs(10))
                .context("starting ScreenCaptureKit stream")?
            {
                anyhow::bail!("starting ScreenCaptureKit stream: {error}");
            }
            rx.recv_timeout(Duration::from_secs(10))
                .context("waiting for ScreenCaptureKit frame")?
        })();
        // Always stop, even on timeout. The framework retains callbacks until completion;
        // receivers are owned by the output object, never borrowed stack pointers.
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let stopped = RcBlock::new(move |_error: *mut NSError| {
            let _ = stop_tx.send(());
        });
        stream.stopCaptureWithCompletionHandler(Some(&stopped));
        let _ = stop_rx.recv_timeout(Duration::from_secs(3));
        let _ = stream.removeStreamOutput_type_error(protocol, SCStreamOutputType::Screen);
        result
    }
}

fn complete_frame(status: Option<isize>) -> bool {
    status == Some(SCFrameStatus::Complete.0)
}

fn frame_status(sample: &CMSampleBuffer) -> Option<isize> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // CoreMedia returns CFArray<CFDictionary>, toll-free bridged to Foundation.
    // ScreenCaptureKit documents its status value as an NSNumber.
    let array = unsafe {
        &*(std::ptr::from_ref(&*attachments).cast::<NSArray<NSDictionary<NSString, NSObject>>>())
    };
    let dictionary = array.firstObject()?;
    let value = dictionary.objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    value
        .downcast_ref::<NSNumber>()
        .map(|number| number.integerValue())
}

struct FrameReceiverIvars {
    sender: Mutex<Option<mpsc::SyncSender<Result<Image>>>>,
}

define_class!(
    // NSObject imposes no additional subclass invariants. Callback state is synchronized
    // because ScreenCaptureKit dispatches it on our serial sample queue.
    #[unsafe(super(NSObject))]
    #[ivars = FrameReceiverIvars]
    struct FrameReceiver;
    unsafe impl NSObjectProtocol for FrameReceiver {}
    unsafe impl SCStreamOutput for FrameReceiver {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn receive(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let Some(buffer) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            if !complete_frame(frame_status(sample)) {
                return;
            }
            let mut sender = self.ivars().sender.lock().unwrap();
            let Some(tx) = sender.take() else {
                return;
            };
            let result = unsafe {
                let flags = CVPixelBufferLockFlags::ReadOnly;
                if CVPixelBufferLockBaseAddress(&buffer, flags) != 0 {
                    Err(anyhow!("locking captured pixel buffer"))
                } else {
                    let width = CVPixelBufferGetWidth(&buffer);
                    let height = CVPixelBufferGetHeight(&buffer);
                    let stride = CVPixelBufferGetBytesPerRow(&buffer);
                    let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
                    let result =
                        if base.is_null() || width == 0 || height == 0 || stride < width * 4 {
                            Err(anyhow!("invalid ScreenCaptureKit pixel buffer"))
                        } else {
                            let mut image = Image::new(width as u32, height as u32);
                            for y in 0..height {
                                let row =
                                    std::slice::from_raw_parts(base.add(y * stride), width * 4);
                                image.data[y * width * 4..(y + 1) * width * 4].copy_from_slice(row);
                            }
                            Ok(image)
                        };
                    CVPixelBufferUnlockBaseAddress(&buffer, flags);
                    result
                }
            };
            let _ = tx.send(result);
        }
    }
);

impl FrameReceiver {
    fn new(sender: mpsc::SyncSender<Result<Image>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(FrameReceiverIvars {
            sender: Mutex::new(Some(sender)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_frames_are_accepted() {
        assert!(complete_frame(Some(SCFrameStatus::Complete.0)));
        for status in [
            None,
            Some(SCFrameStatus::Idle.0),
            Some(SCFrameStatus::Blank.0),
            Some(SCFrameStatus::Suspended.0),
            Some(SCFrameStatus::Started.0),
            Some(SCFrameStatus::Stopped.0),
            Some(99),
        ] {
            assert!(!complete_frame(status));
        }
    }
}
