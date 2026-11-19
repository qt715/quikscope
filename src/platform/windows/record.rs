//! Screen recording through the capture worker's Desktop Duplication (see
//! `capture.rs`): DXGI allows one duplication per output and process, so the
//! recorder shares it with screenshots instead of opening its own.
use super::capture::{Request, WindowsCapture};
use crate::{
    geom::IRect,
    media::{Frame, VideoSource},
    platform::{RecordTarget, ScreenRecorder},
};
use anyhow::{Result, bail};
use std::{
    sync::{LazyLock, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::CreateSolidBrush,
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::{PCWSTR, w},
};

/// Longest single wait inside the capture worker, so screenshots taken
/// during a recording of a static screen are not held up.
const POLL: Duration = Duration::from_millis(20);

pub struct WindowsRecorder {
    capture: WindowsCapture,
}
impl WindowsRecorder {
    pub fn new(capture: WindowsCapture) -> Self {
        Self { capture }
    }
}
impl ScreenRecorder for WindowsRecorder {
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        anyhow::ensure!(fps > 0, "recording frame rate must be positive");
        // Windows' logical desktop coordinates are its physical pixels.
        let region = target.region;
        let stream = self.capture.call(|reply| Request::Open {
            output: target.output.clone(),
            region,
            cursor,
            reply,
        })?;
        Ok(Box::new(Source {
            capture: self.capture.clone(),
            stream,
            size: (region.w as u32, region.h as u32),
            period: Duration::from_secs_f64(1.0 / fps as f64),
            next: Instant::now(),
            paused: false,
            fresh: true,
        }))
    }
    fn outline(&self, region: IRect) -> Result<Box<dyn Send>> {
        Ok(Box::new(Outline::open(region)?))
    }
}

struct Source {
    capture: WindowsCapture,
    stream: u64,
    size: (u32, u32),
    period: Duration,
    /// Earliest moment of the next frame.
    next: Instant,
    paused: bool,
    /// The next frame is delivered even if nothing changed (after opening
    /// and resuming).
    fresh: bool,
}
impl VideoSource for Source {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        if self.paused || self.next > deadline {
            thread::sleep(timeout);
            return Ok(None);
        }
        thread::sleep(self.next.saturating_duration_since(Instant::now()));
        loop {
            let wait = deadline.saturating_duration_since(Instant::now()).min(POLL);
            let frame = self.capture.call(|reply| Request::Frame {
                stream: self.stream,
                wait,
                fresh: self.fresh,
                reply,
            })?;
            if let Some(frame) = frame {
                self.fresh = false;
                // Keep the frame grid while on time; after waiting for a
                // change of a static screen, start a new grid.
                self.next += self.period;
                if self.next < frame.captured {
                    self.next = frame.captured + self.period;
                }
                return Ok(Some(frame));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
    fn set_paused(&mut self, paused: bool) {
        if self.paused && !paused {
            self.fresh = true;
            self.next = Instant::now();
        }
        self.paused = paused;
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.capture.close(self.stream);
    }
}

/// The app accent, as the outline on the other platforms.
const OUTLINE_COLOR: COLORREF = COLORREF(0x00f2_8c8f); // 0x00BBGGRR of #8f8cf2
/// Opacity of the outline strips (of 255).
const OUTLINE_ALPHA: u8 = 220;
/// Strip thickness in physical pixels.
const OUTLINE_WIDTH: i32 = 2;
const OUTLINE_CLASS: PCWSTR = w!("quikscope-recording-outline");
const OUTLINE_QUIT: u32 = WM_APP + 20;

/// Registers the outline window class once per process: plain windows whose
/// class background brush paints them in the outline color.
fn outline_class() -> Result<()> {
    unsafe extern "system" fn proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
    static REGISTERED: LazyLock<std::result::Result<(), String>> = LazyLock::new(|| unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: GetModuleHandleW(None).map_err(|e| e.to_string())?.into(),
            lpszClassName: OUTLINE_CLASS,
            hbrBackground: CreateSolidBrush(OUTLINE_COLOR),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_win32().to_string());
        }
        Ok(())
    });
    REGISTERED
        .clone()
        .map_err(|e| anyhow::anyhow!("registering the outline window class: {e}"))
}

struct Outline {
    hwnd: usize,
    thread: Option<thread::JoinHandle<()>>,
}
impl Outline {
    fn open(r: IRect) -> Result<Self> {
        outline_class()?;
        let (ready, receive) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("quikscope-outline".into())
            .spawn(move || {
                let _dpi = super::desktop::PhysicalDpi::enter();
                let mut windows = Vec::new();
                let result: Result<()> = (|| {
                    let t = OUTLINE_WIDTH;
                    for (x, y, w, h) in [
                        (r.x - t, r.y - t, r.w + 2 * t, t),
                        (r.x - t, r.y + r.h, r.w + 2 * t, t),
                        (r.x - t, r.y, t, r.h),
                        (r.x + r.w, r.y, t, r.h),
                    ] {
                        let hwnd = unsafe {
                            CreateWindowExW(
                                WS_EX_LAYERED
                                    | WS_EX_TRANSPARENT
                                    | WS_EX_TOOLWINDOW
                                    | WS_EX_NOACTIVATE,
                                OUTLINE_CLASS,
                                w!("quikscope recording outline"),
                                WS_POPUP,
                                x,
                                y,
                                w,
                                h,
                                None,
                                None,
                                None,
                                None,
                            )?
                        };
                        windows.push(hwnd);
                        unsafe {
                            SetLayeredWindowAttributes(
                                hwnd,
                                COLORREF(0),
                                OUTLINE_ALPHA,
                                LWA_ALPHA,
                            )?;
                            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
                            SetWindowPos(
                                hwnd,
                                HWND_TOPMOST,
                                x,
                                y,
                                w,
                                h,
                                SWP_NOACTIVATE | SWP_SHOWWINDOW,
                            )?;
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    let _ = ready.send(Err(format!("{e:#}")));
                } else {
                    let _ = ready.send(Ok(windows[0].0 as usize));
                    let mut message = MSG::default();
                    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                        if message.message == OUTLINE_QUIT {
                            break;
                        }
                        unsafe {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                }
                for hwnd in windows {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
            })?;
        match receive.recv()? {
            Ok(hwnd) => Ok(Self {
                hwnd,
                thread: Some(thread),
            }),
            Err(e) => {
                let _ = thread.join();
                bail!(e)
            }
        }
    }
}
impl Drop for Outline {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(HWND(self.hwnd as _), OUTLINE_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Capture as _;
    use windows::Win32::Graphics::Gdi::{GetPixel, GetWindowDC, ReleaseDC};

    /// Native acceptance: DXGI allows one duplication per output and
    /// process, so this covers recording after a screenshot (whose
    /// duplication stays resident), a screenshot during the recording,
    /// pausing, resuming and a second recording of the same output.
    #[test]
    #[ignore = "requires an unlocked native Windows desktop and DDA"]
    fn native_recording_shares_duplication_with_screenshots() -> Result<()> {
        let monitor = super::super::desktop::monitors()?.remove(0);
        let capture = WindowsCapture::new()?;
        let before = capture.capture(Some(std::slice::from_ref(&monitor.name)), false)?;
        let region = IRect::new(monitor.physical.x + 32, monitor.physical.y + 48, 320, 240);
        let outline = Outline::open(region)?;
        unsafe {
            let hwnd = HWND(outline.hwnd as _);
            let mut affinity = 0;
            GetWindowDisplayAffinity(hwnd, &mut affinity)?;
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            assert!(IsWindowVisible(hwnd).as_bool());
            let dc = GetWindowDC(hwnd);
            assert_eq!(GetPixel(dc, 0, 0), OUTLINE_COLOR);
            ReleaseDC(hwnd, dc);
        }
        let recorder = WindowsRecorder::new(capture.clone());
        let target = RecordTarget {
            output: monitor.name.clone(),
            output_rect: monitor.logical,
            region,
        };
        let mut source = recorder.open(&target, 30, true)?;
        assert_eq!(source.size(), (320, 240));
        // The first frame arrives even on a static screen.
        let first = source.next_frame(Duration::from_secs(2))?;
        let first = first.expect("no first DDA frame");
        assert_eq!((first.width(), first.height()), (320, 240));
        let during = capture.capture(Some(std::slice::from_ref(&monitor.name)), false)?;
        assert_eq!(
            (
                during.outputs[0].image.width,
                during.outputs[0].image.height
            ),
            (
                before.outputs[0].image.width,
                before.outputs[0].image.height
            )
        );
        source.set_paused(true);
        assert!(source.next_frame(Duration::from_millis(100))?.is_none());
        source.set_paused(false);
        assert!(
            source.next_frame(Duration::from_secs(1))?.is_some(),
            "no frame right after resuming"
        );
        drop(source);
        let mut again = recorder.open(&target, 60, false)?;
        assert!(again.next_frame(Duration::from_secs(2))?.is_some());
        drop(outline);
        Ok(())
    }
}
