//! Resident Win32 clipboard owner, tray, notifications, and global hotkeys.

use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::clipboard::PendingPng;
use crate::config::{Hotkeys, parse_hotkey};
use crate::ipc::{CaptureOptions, Mode, RecordAction, RecordOptions, Request, Switch};
use crate::media::Kind;
use crate::platform::{
    ANIMATION_FPS_CHOICES, Clipboard, Event, EventSink, Notifier, Tray, TrayState,
    VIDEO_FPS_CHOICES,
};
use crate::raster::Image;

const WAKE: u32 = WM_APP + 1;
const TRAY: u32 = WM_APP + 2;
const RECORDING_TIMER: usize = 2;
const CLIPBOARD_TIMER: usize = 1;
const SESSION_TIMER: usize = 3;
/// How long logout or shutdown waits for the daemon to save and quit.
/// Windows shows the user its "preventing shutdown" screen after a few
/// seconds anyway; this bounds an unattended one when a save is stuck.
const SESSION_END_LIMIT_MS: u32 = 60_000;
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
    Files(Vec<PathBuf>),
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
    Tray(TrayState),
    Quit,
}

fn hotkey_bindings(hotkeys: &Hotkeys) -> Result<Vec<(i32, u32, u32, String)>> {
    let mut bindings = Vec::new();
    for (id, value) in [
        (1, &hotkeys.region),
        (2, &hotkeys.screen),
        (3, &hotkeys.window),
        (4, &hotkeys.open_last),
        (9, &hotkeys.record_video),
        (10, &hotkeys.record_animation),
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
        .name("quikscope-win32".into())
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

    fn set_files(&self, paths: &[std::path::PathBuf]) {
        if let Err(error) = self.send(Command::Clipboard(Content::Files(paths.to_vec()))) {
            log::error!("offering clipboard files: {error:#}");
        }
    }
}

impl Tray for Shell {
    fn update(&self, state: &TrayState) {
        if let Err(error) = self.send(Command::Tray(state.clone())) {
            log::error!("updating Windows tray: {error:#}");
        }
    }
}

impl Notifier for Shell {
    fn shot_captured(
        &self,
        shot: u64,
        _image: &Image,
        summary: &str,
        body: &str,
        timeout_ms: i32,
    ) -> Result<()> {
        self.notify(Some(shot), summary, body, timeout_ms.max(0) as u32)
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
    tray: RefCell<TrayState>,
    drop_effect_format: u32,
}

unsafe fn run(
    events: EventSink,
    commands: mpsc::Receiver<Command>,
    bindings: Vec<(i32, u32, u32, String)>,
    ready: &mpsc::SyncSender<std::result::Result<usize, String>>,
) -> Result<()> {
    let name: Vec<u16> = "quikscope-shell".encode_utf16().chain(Some(0)).collect();
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
        tray: RefCell::new(TrayState {
            recording: None,
            audio: Vec::new(),
            video_fps: 60,
            animation_fps: 30,
        }),
        drop_effect_format: unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) },
    });
    // A hidden top-level window (not HWND_MESSAGE) receives Explorer restart
    // broadcasts as well as clipboard and tray callbacks. It is never shown.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class.name.as_ptr()),
            w!("quikscope"),
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
    state.icon.set(unsafe {
        // Library hosts (including libtest) need not embed our executable's
        // icon resource; a native application icon keeps shell services usable.
        CopyIcon(
            LoadIconW(instance, PCWSTR(std::ptr::without_provenance(1)))
                .or_else(|_| LoadIconW(None, IDI_APPLICATION))?,
        )?
    });
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
    /// A stopped recording is still being written.
    fn saving(&self) -> bool {
        self.tray
            .borrow()
            .recording
            .as_ref()
            .is_some_and(|r| r.saving)
    }

    /// Logout or shutdown asks to proceed. A recording is saved here rather
    /// than in WM_ENDSESSION: GPUI's window exits the process as soon as it
    /// receives WM_ENDSESSION, and Windows sends no WM_ENDSESSION until every
    /// window has answered WM_QUERYENDSESSION.
    unsafe fn query_end_session(&self) {
        // Sent messages overtake posted ones: apply a tray update still
        // queued behind this message, such as a recording that just started.
        unsafe {
            self.drain();
        }
        if self.tray.borrow().recording.is_none() {
            return;
        }
        let hwnd = self.hwnd.get();
        unsafe {
            // Shown on Windows' "apps are preventing shutdown" screen while
            // the save runs, where the user may still end the session.
            let _ = ShutdownBlockReasonCreate(hwnd, w!("Saving the screen recording"));
            self.quit_daemon();
            let _ = ShutdownBlockReasonDestroy(hwnd);
        }
    }

    /// Requests the daemon's quit, which stops and saves a recording first,
    /// and runs this thread's messages (the daemon's notifications, clipboard
    /// rendering, tray updates and the shell's final `Command::Quit`) until it
    /// has quit or [`SESSION_END_LIMIT_MS`] passes. A canceled logout leaves
    /// quikscope quit; keeping the recording running is the worse outcome.
    unsafe fn quit_daemon(&self) {
        let hwnd = self.hwnd.get();
        (self.events)(Event::Request(Request::Quit));
        unsafe {
            SetTimer(hwnd, SESSION_TIMER, SESSION_END_LIMIT_MS, None);
        }
        let mut message = MSG::default();
        loop {
            match unsafe { GetMessageW(&mut message, None, 0, 0) }.0 {
                0 => {
                    // This loop consumed WM_QUIT; `run`'s loop must see one too.
                    unsafe {
                        PostQuitMessage(0);
                    }
                    break;
                }
                -1 => break,
                _ if message.hwnd == hwnd
                    && message.message == WM_TIMER
                    && message.wParam.0 == SESSION_TIMER =>
                {
                    log::warn!(
                        "quikscope did not quit within {} s of the session ending",
                        SESSION_END_LIMIT_MS / 1000
                    );
                    break;
                }
                _ => unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }
        unsafe {
            let _ = KillTimer(hwnd, SESSION_TIMER);
        }
    }

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
        wide_copy(&mut data.szTip, "quikscope");
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
        for (base, kind, choices) in [
            (100, Kind::Video, &VIDEO_FPS_CHOICES),
            (200, Kind::Animation, &ANIMATION_FPS_CHOICES),
        ] {
            if let Some(fps) = id.checked_sub(base).and_then(|i| choices.get(i)) {
                (self.events)(Event::SetFps(kind, *fps));
                return;
            }
        }
        let request = match id {
            1 => Request::Capture(Mode::Region, CaptureOptions::default()),
            2 => Request::Capture(Mode::Screen, CaptureOptions::default()),
            3 => Request::Capture(Mode::Window, CaptureOptions::default()),
            4 => Request::OpenLast,
            5 => Request::Notifications(Switch::Toggle),
            6 => Request::Quit,
            7 => Request::Settings,
            8 => Request::Pick,
            9 => Request::Record(RecordOptions::new(Kind::Video)),
            10 => Request::Record(RecordOptions::new(Kind::Animation)),
            11 => Request::RecordControl(RecordAction::TogglePause),
            12 => Request::RecordControl(RecordAction::Stop),
            13 => Request::RecordControl(RecordAction::Cancel),
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
        // Lists devices plugged in since start-up next time; this menu is
        // built from the current state.
        (self.events)(Event::RefreshAudioDevices);
        let state = self.tray.borrow().clone();
        for entry in menu_entries(&state) {
            unsafe {
                match entry {
                    Entry::Action(id, text) => append_item(menu.0, id, &text, MF_STRING)?,
                    Entry::Label(text) => append_item(menu.0, 0, &text, MF_DISABLED)?,
                    Entry::Separator => AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?,
                    Entry::Submenu(text, items) => {
                        let submenu = CreatePopupMenu()?;
                        append_item(menu.0, submenu.0 as usize, &text, MF_POPUP)?;
                        for (id, text, checked) in items {
                            append_item(
                                submenu,
                                id,
                                &text,
                                if checked { MF_CHECKED } else { MF_STRING },
                            )?;
                        }
                    }
                }
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
            // The modal menu loop may already have applied a newer tray
            // state; audio rows refer to the device list this menu showed.
            match (selected as usize)
                .checked_sub(1000)
                .and_then(|i| state.audio.get(i))
            {
                Some((device, enabled)) => {
                    (self.events)(Event::SetAudio(device.id.clone(), !enabled))
                }
                None => self.request(selected as usize),
            }
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
                Ok(Command::Tray(state)) => {
                    self.tray.replace(state);
                    unsafe {
                        self.update_tip();
                        // The label is static while paused or saving.
                        if (self.tray.borrow().recording.as_ref())
                            .is_some_and(|r| !r.paused && !r.saving)
                        {
                            SetTimer(self.hwnd.get(), RECORDING_TIMER, 1000, None);
                        } else {
                            let _ = KillTimer(self.hwnd.get(), RECORDING_TIMER);
                        }
                    }
                }
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
            Some(Content::Files(_)) => &[15, self.drop_effect_format],
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
            Content::Files(paths) if format == 15 => unsafe { publish(format, &file_drop(paths)?) },
            Content::Files(_) if format == self.drop_effect_format => unsafe {
                publish(format, &1u32.to_le_bytes())
            },
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
        for format in [
            self.png_format,
            DIB_V5,
            DIB,
            UNICODE_TEXT,
            15,
            self.drop_effect_format,
        ] {
            if let Err(error) = unsafe { self.render(format) } {
                log::error!("rendering clipboard format {format}: {error:#}");
            }
        }
    }

    unsafe fn update_tip(&self) {
        let mut data = self.tray_data();
        data.uFlags = NIF_TIP;
        let text = (self.tray.borrow().recording.as_ref())
            .map(elapsed_label)
            .unwrap_or_else(|| "quikscope".into());
        wide_copy(&mut data.szTip, &text);
        // No borrow across the call: waiting on Explorer dispatches sent
        // messages, and WM_QUERYENDSESSION applies tray updates.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
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
        WM_TIMER if wparam.0 == RECORDING_TIMER => unsafe {
            state.update_tip();
        },
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
            // Opening captures waits until the recording is written.
            WM_LBUTTONDBLCLK if !state.saving() => state.request(4),
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
        WM_QUERYENDSESSION => {
            unsafe {
                state.query_end_session();
            }
            return LRESULT(1);
        }
        // Flushes the open-last cache if this window hears it before GPUI's,
        // which exits the process.
        WM_ENDSESSION if wparam.0 != 0 => unsafe {
            state.quit_daemon();
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

unsafe fn append_item(menu: HMENU, id: usize, text: &str, flags: MENU_ITEM_FLAGS) -> Result<()> {
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()))?;
    }
    Ok(())
}

/// One tray menu row; ids are those `State::request` handles.
#[derive(Debug, PartialEq)]
enum Entry {
    Action(usize, String),
    Label(String),
    Separator,
    /// Checkable `(id, text, checked)` items.
    Submenu(String, Vec<(usize, String, bool)>),
}

/// The tray menu in the Linux tray's order: recording controls first, Quit
/// last.
fn menu_entries(state: &TrayState) -> Vec<Entry> {
    let action = |id, text: &str| Entry::Action(id, text.into());
    let mut entries = match &state.recording {
        // Only a cancel until it is written; stopping again would only
        // report that it is still saving.
        Some(recording) if recording.saving => {
            let mut entries = vec![Entry::Label(recording.saving_label())];
            if recording.save_cancellable() {
                entries.push(action(13, "Cancel saving"));
            }
            entries
        }
        Some(recording) => vec![
            Entry::Label(elapsed_label(recording)),
            action(11, if recording.paused { "Resume" } else { "Pause" }),
            action(12, "Stop and save"),
            action(13, "Discard"),
        ],
        None => vec![action(9, "Record video"), action(10, "Record animation")],
    };
    entries.push(Entry::Separator);
    entries.push(Entry::Submenu(
        "Audio".into(),
        state
            .audio
            .iter()
            .enumerate()
            .map(|(i, (device, enabled))| (1000 + i, device.name.clone(), *enabled))
            .collect(),
    ));
    for (label, base, selected, choices) in [
        ("Video frame rate", 100, state.video_fps, &VIDEO_FPS_CHOICES),
        (
            "Animation frame rate",
            200,
            state.animation_fps,
            &ANIMATION_FPS_CHOICES,
        ),
    ] {
        entries.push(Entry::Submenu(
            label.into(),
            choices
                .iter()
                .enumerate()
                .map(|(i, fps)| (base + i, format!("{fps} fps"), *fps == selected))
                .collect(),
        ));
    }
    entries.extend([
        Entry::Separator,
        action(1, "Capture region"),
        action(2, "Capture screen"),
        action(3, "Capture window"),
        action(4, "Open last"),
        action(8, "Pick color"),
        action(5, "Toggle notifications"),
        Entry::Separator,
        action(7, "Settings…"),
        // The daemon's quit stops and saves a recording first.
        action(
            6,
            if state.recording.is_some() {
                "Save recording and quit"
            } else {
                "Quit"
            },
        ),
    ]);
    entries
}

fn elapsed_label(recording: &crate::platform::RecordingStatus) -> String {
    if recording.saving {
        return format!("quikscope — {}", recording.saving_label());
    }
    let elapsed = recording.elapsed
        + if recording.paused {
            std::time::Duration::ZERO
        } else {
            recording.at.elapsed()
        };
    let seconds = elapsed.as_secs();
    format!(
        "quikscope — {:02}:{:02}{}",
        seconds / 60,
        seconds % 60,
        if recording.paused { " (paused)" } else { "" }
    )
}

fn file_drop(paths: &[PathBuf]) -> Result<Vec<u8>> {
    // DROPFILES is a fixed 20-byte header, independent of pointer width.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 12]);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    for path in paths {
        anyhow::ensure!(path.is_absolute(), "clipboard file path must be absolute");
        for word in path.as_os_str().encode_wide() {
            anyhow::ensure!(word != 0, "clipboard file path contains NUL");
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    Ok(bytes)
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
    fn file_drop_has_wide_absolute_paths_and_double_terminator() {
        let bytes = file_drop(&[
            PathBuf::from(r"C:\日本語\movie.mp4"),
            PathBuf::from(r"C:\animation.gif"),
        ])
        .unwrap();
        assert_eq!(&bytes[..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes());
        let words: Vec<u16> = bytes[20..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect();
        assert_eq!(
            String::from_utf16(&words).unwrap(),
            "C:\\日本語\\movie.mp4\0C:\\animation.gif\0\0"
        );
        assert!(file_drop(&[PathBuf::from("relative.mp4")]).is_err());
    }

    #[test]
    fn recording_hotkeys_emit_distinct_ids() {
        let bindings = hotkey_bindings(&Hotkeys::default()).unwrap();
        assert!(bindings.iter().any(|(id, _, _, _)| *id == 9));
        assert!(bindings.iter().any(|(id, _, _, _)| *id == 10));
    }

    #[test]
    fn tray_recording_controls_come_before_quit() {
        let status = |paused, saving, progress| crate::platform::RecordingStatus {
            kind: Kind::Video,
            elapsed: std::time::Duration::from_secs(75),
            at: std::time::Instant::now(),
            paused,
            saving,
            progress,
        };
        let ids = |recording| {
            menu_entries(&TrayState {
                recording,
                audio: Vec::new(),
                video_fps: 60,
                animation_fps: 30,
            })
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Action(id, _) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        for (recording, controls) in [
            (None, vec![9, 10]),
            (Some(status(false, false, None)), vec![11, 12, 13]),
            (Some(status(true, false, None)), vec![11, 12, 13]),
            (Some(status(false, true, None)), vec![]),
            (Some(status(true, true, Some(42))), vec![13]),
        ] {
            let ids = ids(recording);
            assert_eq!(&ids[..controls.len()], controls.as_slice());
            assert_eq!(ids.last(), Some(&6), "Quit is last");
            assert!(!ids[controls.len()..].iter().any(|id| (9..=13).contains(id)));
        }
        let paused = menu_entries(&TrayState {
            recording: Some(status(true, false, None)),
            audio: Vec::new(),
            video_fps: 60,
            animation_fps: 30,
        });
        assert_eq!(paused[0], Entry::Label("quikscope — 01:15 (paused)".into()));
        assert_eq!(paused[1], Entry::Action(11, "Resume".into()));
        assert_eq!(
            paused.last(),
            Some(&Entry::Action(6, "Save recording and quit".into()))
        );
        let saving = menu_entries(&TrayState {
            recording: Some(status(true, true, None)),
            audio: Vec::new(),
            video_fps: 60,
            animation_fps: 30,
        });
        assert_eq!(saving[0], Entry::Label("Saving recording…".into()));
        let saving = menu_entries(&TrayState {
            recording: Some(status(true, true, Some(42))),
            audio: Vec::new(),
            video_fps: 60,
            animation_fps: 30,
        });
        assert_eq!(saving[0], Entry::Label("Saving recording… 42%".into()));
        assert_eq!(saving[1], Entry::Action(13, "Cancel saving".into()));
        let idle = menu_entries(&TrayState {
            recording: None,
            audio: Vec::new(),
            video_fps: 60,
            animation_fps: 30,
        });
        assert_eq!(idle.last(), Some(&Entry::Action(6, "Quit".into())));
    }

    #[test]
    #[ignore = "requires interactive Windows clipboard and tray"]
    fn native_shell_file_clipboard_and_tray() -> Result<()> {
        let shell = spawn(Arc::new(|_| {}), &Hotkeys::default())?;
        let path = std::env::temp_dir().join("quikscope-file-drop-test.txt");
        std::fs::write(&path, "quikscope recording clipboard verification")?;
        shell.set_files(std::slice::from_ref(&path));
        shell.update(&TrayState {
            recording: None,
            audio: crate::media::audio::devices()?
                .into_iter()
                .map(|d| (d, true))
                .collect(),
            video_fps: 60,
            animation_fps: 30,
        });
        thread::sleep(std::time::Duration::from_millis(100));
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-Clipboard -Format FileDropList).FullName",
            ])
            .output()?;
        assert!(String::from_utf8_lossy(&output.stdout).contains(path.to_string_lossy().as_ref()));
        unsafe {
            PostMessageW(
                HWND(shell.hwnd as _),
                TRAY,
                WPARAM(1),
                LPARAM(WM_RBUTTONUP as isize),
            )?;
        }
        // The native acceptance runner uses this interval to inspect the actual
        // notification-area popup with UIA, without starting a production daemon.
        thread::sleep(std::time::Duration::from_secs(20));
        unsafe {
            PostMessageW(HWND(shell.hwnd as _), WM_CANCELMODE, WPARAM(0), LPARAM(0))?;
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows session with a tray"]
    fn session_end_waits_for_the_daemon_to_save_and_quit() -> Result<()> {
        let (sender, events) = mpsc::channel();
        let shell = spawn(
            Arc::new(move |event| {
                let _ = sender.send(event);
            }),
            &Hotkeys::default(),
        )?;
        let hwnd = shell.hwnd;
        // Stands in for Windows asking to end the session.
        let query = move || {
            thread::spawn(move || unsafe {
                SendMessageW(HWND(hwnd as _), WM_QUERYENDSESSION, WPARAM(0), LPARAM(0)).0
            })
        };
        assert_eq!(query().join().unwrap(), 1);
        assert!(events.try_recv().is_err(), "idle: nothing to save, no quit");
        shell.update(&TrayState {
            recording: Some(crate::platform::RecordingStatus {
                kind: Kind::Video,
                elapsed: std::time::Duration::ZERO,
                at: std::time::Instant::now(),
                paused: false,
                saving: false,
                progress: None,
            }),
            audio: Vec::new(),
            video_fps: 60,
            animation_fps: 30,
        });
        let answer = query();
        assert_eq!(
            events.recv_timeout(std::time::Duration::from_secs(5))?,
            Event::Request(Request::Quit)
        );
        thread::sleep(std::time::Duration::from_millis(200));
        assert!(!answer.is_finished(), "the session waits for the save");
        // What the daemon's exit does once the recording is saved.
        drop(shell);
        assert_eq!(answer.join().unwrap(), 1);
        Ok(())
    }

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
