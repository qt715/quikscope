//! Synchronize overlay destruction with the display server before a live capture.
use anyhow::{Context as _, Result, bail};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

#[cfg(target_os = "linux")]
use x11rb::{
    connection::Connection as _,
    protocol::{present::ConnectionExt as _, xproto::ConnectionExt as _},
    rust_connection::RustConnection,
};

pub(crate) enum SelectionBarrier {
    #[cfg(target_os = "linux")]
    Wayland(std::ptr::NonNull<std::ffi::c_void>),
    #[cfg(target_os = "linux")]
    X11 {
        connection: Box<RustConnection>,
        windows: Vec<u32>,
        root: u32,
        event_id: u32,
    },
    #[cfg(windows)]
    Windows(Vec<windows::Win32::Foundation::HWND>),
}

impl SelectionBarrier {
    /// Subscribe before closing: DestroyNotify must not race the observer.
    pub(crate) fn new(handles: &[(RawWindowHandle, RawDisplayHandle)]) -> Result<Self> {
        let Some((_, display)) = handles.first() else {
            bail!("no live selection surfaces to synchronize");
        };
        match display {
            #[cfg(target_os = "linux")]
            RawDisplayHandle::Wayland(display) => Ok(Self::Wayland(display.display)),
            #[cfg(target_os = "linux")]
            RawDisplayHandle::Xcb(_) | RawDisplayHandle::Xlib(_) => {
                use x11rb::protocol::{present, xproto};
                let (connection, screen) = x11rb::connect(None)?;
                let root = connection.setup().roots[screen].root;
                let event_id = connection.generate_id()?;
                connection
                    .present_select_input(event_id, root, present::EventMask::COMPLETE_NOTIFY)?
                    .check()?;
                let mut windows = Vec::with_capacity(handles.len());
                for (window, _) in handles {
                    let id = match window {
                        RawWindowHandle::Xcb(handle) => handle.window.get(),
                        RawWindowHandle::Xlib(handle) => handle.window as u32,
                        _ => bail!("mixed live selection surface kinds"),
                    };
                    connection
                        .change_window_attributes(
                            id,
                            &xproto::ChangeWindowAttributesAux::new()
                                .event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
                        )?
                        .check()?;
                    windows.push(id);
                }
                connection.flush()?;
                Ok(Self::X11 {
                    connection: Box::new(connection),
                    windows,
                    root,
                    event_id,
                })
            }
            #[cfg(windows)]
            RawDisplayHandle::Windows(_) => {
                let windows: Result<Vec<_>> = handles
                    .iter()
                    .map(|(window, _)| {
                        let RawWindowHandle::Win32(window) = window else {
                            bail!("mixed live selection surface kinds");
                        };
                        Ok(windows::Win32::Foundation::HWND(window.hwnd.get() as *mut _))
                    })
                    .collect();
                Ok(Self::Windows(windows?))
            }
            _ => bail!("this surface kind cannot synchronize transparent selection"),
        }
    }

    /// Call on the UI thread while App (and its display) is alive, after GPUI
    /// has dropped the windows, not just marked them removed.
    pub(crate) fn wait(self) -> Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(display) => {
                // SAFETY: wait runs synchronously inside App::update, so GPUI's
                // display cannot be destroyed until this borrowed backend drops.
                // Its private queue never dispatches GPUI's event handlers.
                let backend = unsafe {
                    wayland_client::backend::Backend::from_foreign_display(display.as_ptr().cast())
                };
                let connection = wayland_client::Connection::from_backend(backend);
                // The sync request follows wl_surface.destroy on GPUI's SAME
                // display socket. An unrelated connection's roundtrip cannot
                // establish that ordering. Native screencopy then requests the
                // next rendered frame; portal capture starts only after the sync.
                connection
                    .roundtrip()
                    .context("waiting for Wayland overlay destruction")?;
            }
            #[cfg(target_os = "linux")]
            Self::X11 {
                connection,
                mut windows,
                root,
                event_id,
            } => {
                use x11rb::protocol::Event;
                while !windows.is_empty() {
                    if let Event::DestroyNotify(event) = connection.wait_for_event()? {
                        windows.retain(|&window| window != event.window);
                    }
                }
                // X11 compositing is asynchronous to DestroyWindow. Present's
                // MSC notifications wait for actual display refreshes rather
                // than guessing a compositor delay with a timer.
                let mut target = 0;
                for serial in 1..=2 {
                    connection
                        .present_notify_msc(root, serial, target, 0, 0)?
                        .check()?;
                    connection.flush()?;
                    loop {
                        if let Event::PresentCompleteNotify(event) = connection.wait_for_event()?
                            && event.event == event_id
                            && event.serial == serial
                        {
                            target = event.msc + 1;
                            break;
                        }
                    }
                }
            }
            #[cfg(windows)]
            Self::Windows(handles) => {
                use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SW_HIDE, ShowWindow};
                // GPUI queues ShowWindowAsync/DestroyWindow from Drop. Explicitly
                // hide on the owning UI thread before flushing DWM; a deferred
                // destroy task need not run before this capture to keep it clean.
                for hwnd in handles {
                    unsafe {
                        if IsWindow(hwnd).as_bool() {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                }
                unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }
                    .context("waiting for Windows overlay composition")?;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn x11_live_unavailable() -> Option<String> {
    let check = || -> Result<()> {
        use x11rb::protocol::xproto::VisualClass;
        let (connection, screen_index) = x11rb::connect(None)?;
        let screen = &connection.setup().roots[screen_index];
        let atom = connection
            .intern_atom(false, format!("_NET_WM_CM_S{screen_index}").as_bytes())?
            .reply()?
            .atom;
        if connection.get_selection_owner(atom)?.reply()?.owner == 0 {
            bail!("X11 has no compositing manager");
        }
        if !screen.allowed_depths.iter().any(|depth| {
            depth.depth == 32
                && depth.visuals.iter().any(|v| {
                    v.class == VisualClass::TRUE_COLOR
                        && (v.red_mask, v.green_mask, v.blue_mask) == (0xff0000, 0xff00, 0xff)
                })
        }) {
            bail!("X11 has no alpha visual");
        }
        if !connection.query_extension(b"Present")?.reply()?.present {
            bail!("X11 has no Present refresh synchronization");
        }
        Ok(())
    };
    check().err().map(|e| format!("{e:#}"))
}
