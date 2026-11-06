//! Native Windows desktop services. All native handles stay on their owning
//! service thread; the daemon sees the same backend traits as on Linux.

mod capture;
mod desktop;
mod shell;

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
    let shell = crate::config::Config::load()
        .and_then(|config| shell::spawn(events, &config.hotkeys))
        .map_err(|e| log::warn!("Windows shell services unavailable: {e:#}"))
        .ok();
    Platform {
        name: "windows",
        capture,
        desktop: Arc::new(desktop::WindowsDesktop) as Arc<dyn Desktop>,
        clipboard: shell.as_ref().map(|s| s.clone() as Arc<dyn Clipboard>),
        notifier: shell.map(|s| s as Arc<dyn Notifier>),
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

pub fn focus_overlay(window: &gpui::Window, physical: crate::geom::IRect) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::Win32(handle) = handle.as_raw()
    {
        unsafe {
            use windows::Win32::Graphics::Dwm::{
                DWMWA_NCRENDERING_POLICY, DWMWA_TRANSITIONS_FORCEDISABLED,
                DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute,
            };
            use windows::Win32::UI::WindowsAndMessaging::{
                HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos,
            };
            let hwnd = HWND(handle.hwnd.get() as *mut _);
            let disabled = 1i32;
            for attribute in [
                DWMWA_TRANSITIONS_FORCEDISABLED,
                DWMWA_NCRENDERING_POLICY,
                DWMWA_WINDOW_CORNER_PREFERENCE,
            ] {
                let _ = DwmSetWindowAttribute(hwnd, attribute, (&disabled as *const i32).cast(), 4);
            }
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                physical.x,
                physical.y,
                physical.w,
                physical.h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(hwnd);
        }
    }
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
