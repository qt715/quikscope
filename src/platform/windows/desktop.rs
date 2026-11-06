//! Capture coordinates use the global physical-pixel desktop lattice. GPUI's
//! per-monitor DIPs are converted only at the overlay boundary, avoiding
//! logical gaps and overlaps when neighboring monitors have different DPI.
use std::mem::size_of;

use anyhow::{Result, anyhow};
use windows::Win32::{
    Foundation::{BOOL, HWND, LPARAM, RECT},
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
            MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
        },
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
            MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, IsIconic,
            IsWindowVisible, WS_EX_TOOLWINDOW,
        },
    },
};

use crate::{
    geom::IRect,
    platform::{Desktop, WindowRect},
};

pub struct WindowsDesktop;

/// Native queries must not be virtualized even on a service worker thread.
pub(super) struct PhysicalDpi(DPI_AWARENESS_CONTEXT);
impl PhysicalDpi {
    pub(super) fn enter() -> Self {
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
impl Drop for PhysicalDpi {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Monitor {
    pub name: String,
    pub display_id: u64,
    pub physical: IRect,
    pub logical: IRect,
    pub scale: f64,
}

pub(super) fn monitor_info(handle: HMONITOR) -> Result<Monitor> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO).ok()?;
    }
    let mut x = 0;
    let mut y = 0;
    unsafe {
        GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y)?;
    }
    if x == 0 || x != y {
        return Err(anyhow!("invalid monitor DPI: {x}x{y}"));
    }
    let r = info.monitorInfo.rcMonitor;
    let physical = IRect::new(r.left, r.top, r.right - r.left, r.bottom - r.top);
    let scale = x as f64 / 96.0;
    let end = info
        .szDevice
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(info.szDevice.len());
    Ok(Monitor {
        name: String::from_utf16_lossy(&info.szDevice[..end]),
        display_id: handle.0 as u64,
        physical,
        logical: physical,
        scale,
    })
}

pub(super) fn monitors() -> Result<Vec<Monitor>> {
    unsafe extern "system" fn visit(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<Result<Monitor>>) };
        out.push(monitor_info(h));
        BOOL(1)
    }
    let _dpi = PhysicalDpi::enter();
    let mut out: Vec<Result<Monitor>> = Vec::new();
    unsafe {
        EnumDisplayMonitors(None, None, Some(visit), LPARAM(&mut out as *mut _ as isize)).ok()?;
    }
    let mut out = out.into_iter().collect::<Result<Vec<_>>>()?;
    out.retain(|m| m.physical.w > 0 && m.physical.h > 0);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn window_rect(hwnd: HWND) -> Option<IRect> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as _,
            size_of::<u32>() as u32,
        )
        .ok()?;
        if cloaked != 0 {
            return None;
        }
        let mut r = RECT::default();
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as _,
            size_of::<RECT>() as u32,
        )
        .ok()?;
        if r.right <= r.left || r.bottom <= r.top {
            return None;
        }
        Some(IRect::new(
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
        ))
    }
}

impl Desktop for WindowsDesktop {
    fn focused_output(&self) -> Option<String> {
        let _dpi = PhysicalDpi::enter();
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_invalid() {
                return None;
            }
            monitor_info(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST))
                .ok()
                .map(|m| m.name)
        }
    }
    fn active_window(&self) -> Option<IRect> {
        let _dpi = PhysicalDpi::enter();
        window_rect(unsafe { GetForegroundWindow() })
    }
    fn visible_windows(&self) -> Vec<WindowRect> {
        unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
            if unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
            let out = unsafe { &mut *(data.0 as *mut Vec<WindowRect>) };
            if let Some(rect) = window_rect(hwnd) {
                out.push(WindowRect {
                    rect,
                    floating: true,
                    focus_order: out.len() as i64,
                });
            }
            BOOL(1)
        }
        let _dpi = PhysicalDpi::enter();
        let mut out = Vec::<WindowRect>::new();
        // EnumWindows traverses top-level windows in Z order, topmost first.
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut out as *mut _ as isize)) };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_layout_does_not_overlap_at_mixed_dpi() {
        let left = IRect::new(0, 0, 3840, 2160);
        let right = IRect::new(3840, 0, 2560, 1440);
        assert_eq!(left.right(), right.x);
        assert_eq!(left.union(&right), IRect::new(0, 0, 6400, 2160));
    }
}
