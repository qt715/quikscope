//! Native Windows desktop services. All native handles stay on their owning
//! service thread; the daemon sees the same backend traits as on Linux.

mod capture;
mod desktop;
mod shell;

pub use shell::Shell;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW,
};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{FOLDERID_Pictures, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, SetForegroundWindow,
};
use windows::core::{PCWSTR, w};

use super::{Capture, Clipboard, Desktop, EventSink, Notifier, Platform, Surfaces};

pub(super) fn selection_outputs() -> Result<Vec<(String, crate::geom::IRect, u32)>> {
    Ok(desktop::monitors()?
        .into_iter()
        .map(|o| (o.name, o.logical, o.physical.w as u32))
        .collect())
}

pub fn initialize() -> Result<()> {
    // GPUI's Windows manifest requests PerMonitorV2. Verify it at runtime and
    // also support library callers whose executable has no such manifest.
    unsafe {
        if !AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
        .as_bool()
        {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
                .context("enabling per-monitor-v2 DPI awareness")?;
        }
    }
    Ok(())
}

pub(super) fn connect(events: EventSink) -> Platform {
    let capture = capture::WindowsCapture::new()
        .map(|c| Arc::new(c) as Arc<dyn Capture>)
        .map_err(|e| log::error!("screen capture unavailable: {e:#}"))
        .ok();
    let config = crate::config::Config::load().unwrap_or_else(|error| {
        log::error!("loading configuration for Windows shell: {error:#}; using defaults");
        crate::config::Config::default()
    });
    let shell = shell::spawn(events, &config.hotkeys)
        .map_err(|e| log::warn!("Windows shell services unavailable: {e:#}"))
        .ok();
    Platform {
        name: "windows",
        capture,
        desktop: Arc::new(desktop::WindowsDesktop) as Arc<dyn Desktop>,
        clipboard: shell.as_ref().map(|s| s.clone() as Arc<dyn Clipboard>),
        notifier: shell.as_ref().map(|s| s.clone() as Arc<dyn Notifier>),
        shell,
        surfaces: Surfaces::Windows,
        capture_may_prompt: false,
    }
}

pub fn pictures_dir() -> PathBuf {
    // The known folder follows folder redirection and OneDrive, unlike a
    // guessed USERPROFILE\\Pictures path. CoTaskMem ownership is ours.
    unsafe {
        match SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None) {
            Ok(path) => {
                let result = path.to_string().map(PathBuf::from);
                CoTaskMemFree(Some(path.0.cast()));
                if let Ok(path) = result {
                    return path;
                }
            }
            Err(e) => log::warn!("Pictures known folder unavailable: {e}"),
        }
    }
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Pictures")
}

/// Called by the interactive command process, which is allowed to transfer
/// its foreground entitlement to the already-running daemon.
pub fn allow_foreground() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(w!("quickscope-shell"), PCWSTR::null()) {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
        }
    }
}

pub(super) fn overlay_geometry(
    name: &str,
    physical: crate::geom::IRect,
) -> (Option<gpui::DisplayId>, gpui::Bounds<gpui::Pixels>) {
    let monitor = desktop::monitors()
        .ok()
        .and_then(|m| m.into_iter().find(|m| m.name == name));
    let scale = monitor.as_ref().map_or(1., |m| m.scale as f32);
    let display = monitor.map(|m| gpui::DisplayId::new(m.display_id));
    (
        display,
        gpui::Bounds {
            origin: gpui::point(
                gpui::px(physical.x as f32 / scale),
                gpui::px(physical.y as f32 / scale),
            ),
            size: gpui::size(
                gpui::px(physical.w as f32 / scale),
                gpui::px(physical.h as f32 / scale),
            ),
        },
    )
}

pub fn focus_overlay(window: &gpui::Window, physical: crate::geom::IRect, cx: &gpui::App) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::Win32(handle) = handle.as_raw()
    {
        let hwnd = HWND(handle.hwnd.get() as *mut _);
        // Native resizing/activation synchronously calls GPUI's window
        // callbacks. Poll outside App::update/open_window, not while App is
        // mutably borrowed by the window constructor.
        cx.foreground_executor()
            .spawn(async move {
                unsafe {
                    use windows::Win32::Graphics::Dwm::{
                        DWMWA_NCRENDERING_POLICY, DWMWA_TRANSITIONS_FORCEDISABLED,
                        DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute,
                    };
                    use windows::Win32::UI::WindowsAndMessaging::{
                        GWL_STYLE, GetWindowLongW, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE,
                        SWP_SHOWWINDOW, SetWindowLongW, SetWindowPos,
                    };
                    // GPUI's transparent titlebar still keeps WS_THICKFRAME, leaving
                    // invisible resize borders inside these monitor-sized bounds.
                    // Remove the non-client frame before sizing so the viewport and
                    // physical capture lattice agree exactly.
                    SetWindowLongW(
                        hwnd,
                        GWL_STYLE,
                        overlay_style(GetWindowLongW(hwnd, GWL_STYLE)),
                    );
                    let disabled = 1i32;
                    for attribute in [
                        DWMWA_TRANSITIONS_FORCEDISABLED,
                        DWMWA_NCRENDERING_POLICY,
                        DWMWA_WINDOW_CORNER_PREFERENCE,
                    ] {
                        let _ = DwmSetWindowAttribute(
                            hwnd,
                            attribute,
                            (&disabled as *const i32).cast(),
                            4,
                        );
                    }
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        physical.x,
                        physical.y,
                        physical.w,
                        physical.h,
                        SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                    let _ = SetForegroundWindow(hwnd);
                }
            })
            .detach();
    }
}

pub async fn hide_overlay<V: 'static>(
    overlay: gpui::WindowHandle<V>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    let hwnd = overlay.update(cx, |_, window, _| {
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|error| anyhow::anyhow!("selection window handle: {error}"))?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
            _ => anyhow::bail!("Windows overlay has no Win32 handle"),
        }
    })??;
    // Hide while its GPUI window still exists, and outside an App borrow.
    // WM_ACTIVATE queues another foreground task; drain it before removal so
    // that task cannot attempt to update an already-removed window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    cx.foreground_executor().spawn(async {}).await;
    Ok(())
}

fn overlay_style(style: i32) -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
    };
    style & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MAXIMIZEBOX | WS_MINIMIZEBOX).0 as i32
}

pub const PRINT_SCREEN_NOTICE: &str = "To use PrintScreen in quickscope, turn off Settings → Accessibility → Keyboard → \"Use the Print screen key to open screen capture\".";

/// The Windows 11 default applies when this optional per-user override is absent.
pub fn print_screen_capture_enabled() -> Result<bool> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};

    let mut enabled = 0u32;
    let mut size = std::mem::size_of_val(&enabled) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Control Panel\\Keyboard"),
            w!("PrintScreenKeyForSnippingEnabled"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut enabled as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_FILE_NOT_FOUND {
        status
            .ok()
            .context("reading the Windows PrintScreen setting")?;
        return Ok(enabled != 0);
    }
    let mut build = [0u16; 16];
    let mut size = std::mem::size_of_val(&build) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(build.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
    }
    .context("reading the Windows build number")?;
    print_screen_default(&build)
}

fn print_screen_default(build: &[u16]) -> Result<bool> {
    let build = build
        .iter()
        .take_while(|&&c| c != 0)
        .try_fold(0u32, |build, &c| {
            let digit = c.checked_sub(u16::from(b'0'))?;
            (digit < 10).then_some(())?;
            build.checked_mul(10)?.checked_add(u32::from(digit))
        })
        .context("invalid Windows build number")?;
    Ok(build >= 22000)
}

/// Per-user login startup. No elevation, scheduler task or machine-wide key.
pub fn autostart(action: &str) -> Result<bool> {
    anyhow::ensure!(
        matches!(action, "on" | "off" | "status"),
        "usage: quickscope autostart <on|off|status>"
    );
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
    let key = Key(key);
    unsafe {
        match action {
            "on" => {
                let exe = std::env::current_exe()?;
                let command: Vec<u16> = format!("\"{}\" daemon", exe.display())
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let bytes =
                    std::slice::from_raw_parts(command.as_ptr().cast::<u8>(), command.len() * 2);
                RegSetValueExW(key.0, w!("quickscope"), 0, REG_SZ, Some(bytes)).ok()?;
            }
            "off" => {
                let status = RegDeleteValueW(key.0, w!("quickscope"));
                if status != ERROR_FILE_NOT_FOUND {
                    status.ok()?;
                }
            }
            _ => {}
        }
        let mut size = 0;
        let result = RegGetValueW(
            key.0,
            PCWSTR::null(),
            w!("quickscope"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        result.ok()?;
        Ok(size > 2)
    }
}

#[cfg(test)]
mod tests {
    use super::{overlay_style, print_screen_default};
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_CAPTION, WS_CLIPCHILDREN, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
        WS_VISIBLE,
    };

    #[test]
    fn overlay_has_no_nonclient_frame_and_preserves_other_styles() {
        let frame = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MAXIMIZEBOX | WS_MINIMIZEBOX;
        let retained = WS_VISIBLE | WS_CLIPCHILDREN;
        assert_eq!(
            overlay_style((frame | retained).0 as i32),
            retained.0 as i32
        );
        assert_eq!(overlay_style(retained.0 as i32), retained.0 as i32);
    }

    #[test]
    fn print_screen_default_changes_with_windows_11() {
        for (build, expected) in [("19045", false), ("22000", true), ("26300", true)] {
            let build: Vec<_> = build.encode_utf16().chain(Some(0)).collect();
            assert_eq!(print_screen_default(&build).unwrap(), expected);
        }
        assert!(print_screen_default(&[b'x' as u16]).is_err());
    }
}
