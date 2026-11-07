//! Resident Win32 clipboard owner, tray, notifications, and global hotkeys.

use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::clipboard::PendingPng;
use crate::config::{Hotkeys, parse_hotkey};
use crate::ipc::{CaptureOptions, Mode, Request, Switch};
use crate::platform::{Clipboard, Event, EventSink, Notifier};
use crate::raster::Image;

const WAKE: u32 = WM_APP + 1;
const TRAY: u32 = WM_APP + 2;
const CLIPBOARD_TIMER: usize = 1;
const DIB: u32 = 8;
const UNICODE_TEXT: u32 = 13;
const DIB_V5: u32 = 17;

/// The handle is an integer here so the public shell is Send + Sync. Only its
/// owning thread uses native resources; callers merely post a wakeup message.
pub struct Shell {
    hwnd: usize,
    commands: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

enum Content {
    Image {
        image: Arc<Image>,
        png: Arc<PendingPng>,
    },
    Text(String),
}

enum Command {
    Clipboard(Content),
    Notify {
        shot: Option<u64>,
        summary: String,
        body: String,
        timeout: u32,
        reply: mpsc::SyncSender<std::result::Result<(), String>>,
    },
    Hotkeys(Vec<(i32, u32, u32, String)>),
    Quit,
}

fn hotkey_bindings(hotkeys: &Hotkeys) -> Result<Vec<(i32, u32, u32, String)>> {
    let mut bindings = Vec::new();
    for (id, value) in [
        (1, &hotkeys.region),
        (2, &hotkeys.screen),
        (3, &hotkeys.window),
        (4, &hotkeys.open_last),
    ] {
        if let Some((modifiers, key)) = parse_hotkey(value)? {
            bindings.push((id, modifiers, key, value.clone()));
        }
    }
    Ok(bindings)
}

/// A reserved shortcut must not take clipboard/tray services down with it;
/// registration failures are reported separately with the config remedy.
pub fn spawn(events: EventSink, hotkeys: &Hotkeys) -> Result<Arc<Shell>> {
    let bindings = hotkey_bindings(hotkeys)?;
    let (commands, receive) = mpsc::channel();
    let (ready, started) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("quickscope-win32".into())
        .spawn(move || {
            // All Win32 resources, including cleanup on failed initialization, stay
            // on this thread. The box pins the address stored in GWLP_USERDATA.
            let result = unsafe { run(events, receive, bindings, &ready) };
            if let Err(error) = result {
                let message = format!("{error:#}");
                let _ = ready.send(Err(message.clone()));
                log::error!("Windows shell: {message}");
            }
        })
        .context("starting Windows shell thread")?;
    match started.recv() {
        Ok(Ok(hwnd)) => Ok(Arc::new(Shell {
            hwnd,
            commands,
            thread: Some(thread),
        })),
        result => {
            let _ = thread.join();
            match result {
                Ok(Err(error)) => Err(anyhow!(error)),
                _ => bail!("Windows shell thread exited during startup"),
            }
        }
    }
}

impl Shell {
    pub fn update_hotkeys(&self, hotkeys: &Hotkeys) {
        let result =
            hotkey_bindings(hotkeys).and_then(|bindings| self.send(Command::Hotkeys(bindings)));
        if let Err(error) = result {
            log::error!("updating Windows global hotkeys: {error:#}");
        }
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .context("Windows shell stopped")?;
        unsafe { PostMessageW(HWND(self.hwnd as _), WAKE, WPARAM(0), LPARAM(0)) }
            .context("waking Windows shell")
    }

    fn notify(&self, shot: Option<u64>, summary: &str, body: &str, timeout: u32) -> Result<()> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.send(Command::Notify {
            shot,
            summary: summary.into(),
            body: body.into(),
            timeout,
            reply,
        })?;
        receive
            .recv()
            .context("Windows shell stopped while notifying")?
            .map_err(|error| anyhow!(error))
    }
}

impl Clipboard for Shell {
    fn set_image(&self, image: Arc<Image>) -> Arc<PendingPng> {
        let png = PendingPng::encode(image.clone());
        if let Err(error) = self.send(Command::Clipboard(Content::Image {
            image,
            png: png.clone(),
        })) {
            log::error!("offering clipboard image: {error:#}");
        }
        png
    }

    fn set_text(&self, text: &str) {
        if let Err(error) = self.send(Command::Clipboard(Content::Text(text.into()))) {
            log::error!("offering clipboard text: {error:#}");
        }
    }
}

impl Notifier for Shell {
    fn shot_captured(&self, shot: u64, _image: &Image, body: &str, timeout_ms: i32) -> Result<()> {
        self.notify(
            Some(shot),
            "Screenshot captured",
            body,
            timeout_ms.max(0) as u32,
        )
    }

    fn message(&self, summary: &str, body: &str) -> Result<()> {
        self.notify(None, summary, body, 5000)
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            // Event sinks may release their last Shell reference on this thread.
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

struct WindowClass {
    name: Vec<u16>,
    instance: HINSTANCE,
}

impl Drop for WindowClass {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterClassW(PCWSTR(self.name.as_ptr()), self.instance);
        }
    }
}

struct State {
    hwnd: Cell<HWND>,
    icon: Cell<HICON>,
    tray_present: Cell<bool>,
    hotkeys: RefCell<Vec<i32>>,
    commands: RefCell<mpsc::Receiver<Command>>,
    events: EventSink,
    png_format: u32,
    taskbar_created: u32,
    content: RefCell<Option<Content>>,
    pending: RefCell<Option<Content>>,
    retries: Cell<u32>,
    balloon_shot: Cell<Option<u64>>,
}

unsafe fn run(
    events: EventSink,
    commands: mpsc::Receiver<Command>,
    bindings: Vec<(i32, u32, u32, String)>,
    ready: &mpsc::SyncSender<std::result::Result<usize, String>>,
) -> Result<()> {
    let name: Vec<u16> = "quickscope-shell".encode_utf16().chain(Some(0)).collect();
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_win32()).context("registering shell window class");
    }
    let class = WindowClass { name, instance };
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if png_format == 0 || taskbar_created == 0 {
        return Err(windows::core::Error::from_win32())
            .context("registering shell messages/formats");
    }
    let state = Box::new(State {
        hwnd: Cell::new(HWND::default()),
        icon: Cell::new(HICON::default()),
        tray_present: Cell::new(false),
        hotkeys: RefCell::new(Vec::new()),
        commands: RefCell::new(commands),
        events,
        png_format,
        taskbar_created,
        content: RefCell::new(None),
        pending: RefCell::new(None),
        retries: Cell::new(0),
        balloon_shot: Cell::new(None),
    });
    // A hidden top-level window (not HWND_MESSAGE) receives Explorer restart
    // broadcasts as well as clipboard and tray callbacks. It is never shown.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class.name.as_ptr()),
            w!("quickscope"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            Some((&*state as *const State).cast()),
        )
    }
    .context("creating shell window")?;
    state.hwnd.set(hwnd);
    state
        .icon
        .set(unsafe { CopyIcon(LoadIconW(None, IDI_APPLICATION)?)? });
    unsafe {
        state.add_tray()?;
    }
    unsafe {
        state.update_hotkeys(bindings);
    }
    match super::print_screen_capture_enabled() {
        Ok(true) => {
            log::warn!(
                "Windows intercepts PrintScreen. {}",
                super::PRINT_SCREEN_NOTICE
            );
            if let Err(error) = unsafe {
                state.notify(
                    None,
                    "PrintScreen opens Windows screen capture",
                    super::PRINT_SCREEN_NOTICE,
                    10000,
                )
            } {
                log::warn!("showing Windows PrintScreen warning: {error:#}");
            }
        }
        Ok(false) => {}
        Err(error) => log::warn!("checking the Windows PrintScreen setting: {error:#}"),
    }
    let _ = ready.send(Ok(hwnd.0 as usize));
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if result == -1 {
            return Err(windows::core::Error::from_win32()).context("reading shell messages");
        }
        if result == 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // Drop the state (and destroy its window) before unregistering its class.
    drop(state);
    drop(class);
    Ok(())
}

impl State {
    unsafe fn update_hotkeys(&self, bindings: Vec<(i32, u32, u32, String)>) {
        let hwnd = self.hwnd.get();
        for id in self.hotkeys.borrow_mut().drain(..) {
            if let Err(error) = unsafe { UnregisterHotKey(hwnd, id) } {
                log::error!("unregistering global hotkey {id}: {error}");
            }
        }
        for (id, modifiers, key, name) in bindings {
            match unsafe {
                RegisterHotKey(hwnd, id, HOT_KEY_MODIFIERS(modifiers) | MOD_NOREPEAT, key)
            } {
                Ok(()) => self.hotkeys.borrow_mut().push(id),
                Err(error) => log::error!(
                    "global hotkey {name:?} unavailable (already in use or reserved by Windows): {error}; \
                     choose another binding in [hotkeys]"
                ),
            }
        }
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd.get(),
            uID: 1,
            ..Default::default()
        }
    }

    unsafe fn add_tray(&self) -> Result<()> {
        let mut data = self.tray_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = TRAY;
        data.hIcon = self.icon.get();
        wide_copy(&mut data.szTip, "quickscope");
        unsafe { Shell_NotifyIconW(NIM_ADD, &data).ok() }.context("adding tray icon")?;
        self.tray_present.set(true);
        Ok(())
    }

    unsafe fn notify(
        &self,
        shot: Option<u64>,
        summary: &str,
        body: &str,
        timeout: u32,
    ) -> Result<()> {
        let mut data = self.tray_data();
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        data.Anonymous.uTimeout = timeout;
        wide_copy(&mut data.szInfoTitle, summary);
        wide_copy(&mut data.szInfo, body);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data).ok() }.context("showing notification")?;
        self.balloon_shot.set(shot);
        Ok(())
    }

    fn request(&self, id: usize) {
        let request = match id {
            1 => Request::Capture(Mode::Region, CaptureOptions::default()),
            2 => Request::Capture(Mode::Screen, CaptureOptions::default()),
            3 => Request::Capture(Mode::Window, CaptureOptions::default()),
            4 => Request::OpenLast,
            5 => Request::Notifications(Switch::Toggle),
            6 => Request::Quit,
            7 => Request::Settings,
            _ => return,
        };
        (self.events)(Event::Request(request));
    }

    unsafe fn menu(&self) -> Result<()> {
        struct Menu(HMENU);
        impl Drop for Menu {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyMenu(self.0);
                }
            }
        }
        let menu = Menu(unsafe { CreatePopupMenu()? });
        let foreground = unsafe { GetForegroundWindow() };
        for (id, text) in [
            (1, "Capture region"),
            (2, "Capture screen"),
            (3, "Capture window"),
            (4, "Open last"),
            (5, "Toggle notifications"),
            (7, "Settings…"),
            (6, "Quit"),
        ] {
            let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            unsafe {
                AppendMenuW(menu.0, MF_STRING, id, PCWSTR(text.as_ptr()))?;
            }
        }
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point)?;
        }
        // Required for a tray popup to dismiss when clicking elsewhere.
        unsafe {
            let _ = SetForegroundWindow(self.hwnd.get());
        }
        let selected = unsafe {
            TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd.get(),
                None,
            )
        }
        .0;
        unsafe {
            let _ = PostMessageW(self.hwnd.get(), WM_NULL, WPARAM(0), LPARAM(0));
        }
        if selected != 0 {
            if !foreground.is_invalid() {
                unsafe {
                    let _ = SetForegroundWindow(foreground);
                }
            }
            self.request(selected as usize);
        }
        Ok(())
    }

    unsafe fn drain(&self) {
        loop {
            let command = self.commands.borrow_mut().try_recv();
            match command {
                Ok(Command::Clipboard(content)) => {
                    self.pending.replace(Some(content));
                    self.retries.set(0);
                    unsafe {
                        self.offer();
                    }
                }
                Ok(Command::Notify {
                    shot,
                    summary,
                    body,
                    timeout,
                    reply,
                }) => {
                    let result = unsafe { self.notify(shot, &summary, &body, timeout) }
                        .map_err(|error| format!("{error:#}"));
                    let _ = reply.send(result);
                }
                Ok(Command::Hotkeys(bindings)) => unsafe {
                    self.update_hotkeys(bindings);
                },
                Ok(Command::Quit) => {
                    unsafe {
                        PostQuitMessage(0);
                    }
                    break;
                }
                Err(_) => break,
            }
        }
    }

    unsafe fn offer(&self) {
        if self.pending.borrow().is_none() {
            return;
        }
        if let Err(error) = unsafe { OpenClipboard(self.hwnd.get()) } {
            let attempts = self.retries.get() + 1;
            self.retries.set(attempts);
            if attempts <= 20
                && unsafe { SetTimer(self.hwnd.get(), CLIPBOARD_TIMER, 25, None) } != 0
            {
                return;
            }
            self.pending.take();
            unsafe {
                let _ = KillTimer(self.hwnd.get(), CLIPBOARD_TIMER);
            }
            log::error!("opening clipboard after {attempts} attempts: {error}");
            return;
        }
        let _open = OpenClipboardGuard;
        unsafe {
            let _ = KillTimer(self.hwnd.get(), CLIPBOARD_TIMER);
        }
        if let Err(error) = unsafe { EmptyClipboard() } {
            self.pending.take();
            log::error!("emptying clipboard: {error}");
            return;
        }
        self.content.replace(self.pending.take());
        let content = self.content.borrow();
        let formats: &[u32] = match content.as_ref() {
            Some(Content::Image { .. }) => &[self.png_format, DIB_V5, DIB],
            Some(Content::Text(_)) => &[UNICODE_TEXT],
            None => &[],
        };
        for &format in formats {
            // Delayed rendering intentionally passes NULL. Win32 returns NULL
            // on success too, which windows-rs represents as Err; ignore it.
            unsafe {
                let _ = SetClipboardData(format, HANDLE::default());
            }
        }
    }

    unsafe fn render(&self, format: u32) -> Result<()> {
        let content = self.content.borrow();
        let Some(content) = content.as_ref() else {
            return Ok(());
        };
        match content {
            Content::Image { png, .. } if format == self.png_format => {
                let bytes = png
                    .wait()
                    .map_err(|error| anyhow!("encoding clipboard PNG: {error}"))?;
                unsafe { publish(format, bytes) }
            }
            Content::Image { image, .. } if format == DIB || format == DIB_V5 => unsafe {
                publish(format, &dib(image, format == DIB_V5)?)
            },
            Content::Text(text) if format == UNICODE_TEXT => {
                let bytes: Vec<u8> = text
                    .encode_utf16()
                    .chain(Some(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                unsafe { publish(format, &bytes) }
            }
            _ => Ok(()),
        }
    }

    unsafe fn render_all(&self) {
        if unsafe { OpenClipboard(self.hwnd.get()) }.is_err() {
            return;
        }
        let _open = OpenClipboardGuard;
        if unsafe { GetClipboardOwner() }.ok() != Some(self.hwnd.get()) {
            return;
        }
        for format in [self.png_format, DIB_V5, DIB, UNICODE_TEXT] {
            if let Err(error) = unsafe { self.render(format) } {
                log::error!("rendering clipboard format {format}: {error:#}");
            }
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            let hwnd = self.hwnd.get();
            if !hwnd.is_invalid() {
                let _ = KillTimer(hwnd, CLIPBOARD_TIMER);
                self.render_all();
                for &id in self.hotkeys.borrow().iter() {
                    let _ = UnregisterHotKey(hwnd, id);
                }
                if self.tray_present.get() {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &self.tray_data());
                }
                let _ = DestroyWindow(hwnd);
            }
            if !self.icon.get().is_invalid() {
                let _ = DestroyIcon(self.icon.get());
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const State;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let state = unsafe { &*pointer };
    match message {
        WAKE => unsafe {
            state.drain();
        },
        WM_HOTKEY => state.request(wparam.0),
        WM_TIMER if wparam.0 == CLIPBOARD_TIMER => unsafe {
            state.offer();
        },
        WM_RENDERFORMAT => {
            // Clipboard is already open by the requesting application.
            if let Err(error) = unsafe { state.render(wparam.0 as u32) } {
                log::error!("rendering clipboard: {error:#}");
            }
        }
        WM_RENDERALLFORMATS => unsafe {
            state.render_all();
        },
        TRAY => match lparam.0 as u32 {
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                if let Err(error) = unsafe { state.menu() } {
                    log::error!("tray menu: {error:#}");
                }
            }
            WM_LBUTTONDBLCLK => state.request(4),
            NIN_BALLOONUSERCLICK => {
                if let Some(shot) = state.balloon_shot.take() {
                    (state.events)(Event::OpenShot(shot));
                }
            }
            NIN_BALLOONTIMEOUT => {
                state.balloon_shot.set(None);
            }
            _ => {}
        },
        WM_CLOSE => unsafe {
            PostQuitMessage(0);
        },
        WM_NCDESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        _ if message == state.taskbar_created => {
            state.tray_present.set(false);
            if let Err(error) = unsafe { state.add_tray() } {
                log::error!("restoring tray icon: {error:#}");
            }
        }
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
    LRESULT(0)
}

struct OpenClipboardGuard;
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

unsafe fn publish(format: u32, bytes: &[u8]) -> Result<()> {
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len())? };
    let destination = unsafe { GlobalLock(memory) };
    if destination.is_null() {
        let error = windows::core::Error::from_win32();
        unsafe {
            let _ = GlobalFree(memory);
        }
        return Err(error.into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast(), bytes.len());
        let _ = GlobalUnlock(memory);
    }
    if let Err(error) = unsafe { SetClipboardData(format, HANDLE(memory.0)) } {
        unsafe {
            let _ = GlobalFree(memory);
        }
        return Err(error.into());
    }
    // The clipboard now owns the movable allocation.
    Ok(())
}

/// Clipboard DIBs omit BITMAPFILEHEADER. Negative height gives top-down BGRA
/// rows; V5 explicitly describes straight alpha and the sRGB color space.
fn dib(image: &Image, v5: bool) -> Result<Vec<u8>> {
    let width = i32::try_from(image.width).context("clipboard bitmap width overflow")?;
    let height = i32::try_from(image.height).context("clipboard bitmap height overflow")?;
    let bytes = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("clipboard bitmap size overflow")?;
    if width == 0 || height == 0 || image.data.len() != bytes {
        bail!("invalid clipboard image dimensions");
    }
    let size = u32::try_from(bytes).context("clipboard bitmap exceeds DIB size limit")?;
    let header = if v5 { 124 } else { 40 };
    let mut out = vec![0; header + bytes];
    out[0..4].copy_from_slice(&(header as u32).to_le_bytes());
    out[4..8].copy_from_slice(&width.to_le_bytes());
    out[8..12].copy_from_slice(&(-height).to_le_bytes());
    out[12..14].copy_from_slice(&1u16.to_le_bytes());
    out[14..16].copy_from_slice(&32u16.to_le_bytes());
    out[20..24].copy_from_slice(&size.to_le_bytes());
    if v5 {
        out[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        for (offset, mask) in [
            (40, 0x00ff0000u32),
            (44, 0x0000ff00),
            (48, 0x000000ff),
            (52, 0xff000000),
        ] {
            out[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
        }
        out[56..60].copy_from_slice(&0x73524742u32.to_le_bytes()); // LCS_sRGB
        out[108..112].copy_from_slice(&4u32.to_le_bytes()); // LCS_GM_IMAGES
    }
    out[header..].copy_from_slice(&image.data);
    Ok(out)
}

fn wide_copy<const N: usize>(destination: &mut [u16; N], text: &str) {
    let mut used = 0;
    for character in text.chars() {
        let mut units = [0; 2];
        let encoded = character.encode_utf16(&mut units);
        if used + encoded.len() >= N {
            break;
        }
        destination[used..used + encoded.len()].copy_from_slice(encoded);
        used += encoded.len();
    }
    destination[used] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_dibs_are_top_down_bgra_with_v5_channel_masks() {
        let mut image = Image::new(1, 2);
        image
            .data
            .copy_from_slice(&[10, 20, 30, 255, 40, 50, 60, 128]);
        for (v5, header) in [(false, 40), (true, 124)] {
            let bytes = dib(&image, v5).unwrap();
            assert_eq!(i32::from_le_bytes(bytes[8..12].try_into().unwrap()), -2);
            assert_eq!(&bytes[header..], image.data.as_slice());
            if v5 {
                assert_eq!(
                    u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
                    0x00ff0000
                );
                assert_eq!(
                    u32::from_le_bytes(bytes[52..56].try_into().unwrap()),
                    0xff000000
                );
            }
        }
    }

    #[test]
    fn tray_text_truncation_keeps_surrogate_pairs_and_terminator() {
        let mut output = [0xffff; 4];
        wide_copy(&mut output, "A😀B");
        assert_eq!(String::from_utf16(&output[..3]).unwrap(), "A😀");
        assert_eq!(output[3], 0);
        let mut short = [0xffff; 3];
        wide_copy(&mut short, "A😀");
        assert_eq!(short[0], b'A' as u16);
        assert_eq!(short[1], 0);
    }
}
