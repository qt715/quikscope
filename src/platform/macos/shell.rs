//! Main-runloop status menu, Carbon global shortcuts, and LaunchAgent control.
use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail, ensure};
use objc2::rc::Retained;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{NSAccessibility, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem};
use objc2_foundation::{NSData, NSObject, NSObjectProtocol, NSSize, NSString};

use crate::config::Hotkeys;
use crate::ipc::{CaptureOptions, Mode, Request, Switch};
use crate::platform::{Event, EventSink};

// Cocoa objects never cross threads. Platform may freely share the handle.
pub struct Shell {
    id: u64,
}
struct NativeShell {
    id: u64,
    item: Retained<NSStatusItem>,
    _target: Retained<MenuTarget>,
    handler: *mut c_void,
    events: Option<Box<EventSink>>,
    bindings: Vec<(u32, u32, u32)>,
    keys: Vec<*mut c_void>,
}
thread_local! { static SHELL: RefCell<Option<NativeShell>> = const { RefCell::new(None) }; }
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = EventSink]
    struct MenuTarget;
    unsafe impl NSObjectProtocol for MenuTarget {}
    impl MenuTarget {
        #[unsafe(method(performMenuAction:))]
        fn perform_menu_action(&self, sender: &NSMenuItem) {
            if let Some(request) = request(sender.tag() as u32) {
                deliver(self.ivars(), Event::Request(request));
            }
        }
    }
);

fn request(id: u32) -> Option<Request> {
    Some(match id {
        1 => Request::Capture(Mode::Region, CaptureOptions::default()),
        2 => Request::Capture(Mode::Screen, CaptureOptions::default()),
        3 => Request::Capture(Mode::Window, CaptureOptions::default()),
        4 => Request::OpenLast,
        5 => Request::Settings,
        6 => Request::Notifications(Switch::Toggle),
        7 => Request::Quit,
        _ => return None,
    })
}
fn deliver(events: &EventSink, event: Event) {
    // A panic must never unwind across an Objective-C or Carbon callback.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| events(event))).is_err() {
        log::error!("macOS shell event callback panicked");
    }
}

impl Shell {
    pub fn new(events: EventSink) -> Result<Self> {
        let mtm =
            MainThreadMarker::new().context("macOS shell must be created on the main thread")?;
        ensure!(
            SHELL.with(|s| s.borrow().is_none()),
            "macOS shell already exists"
        );
        let this = MenuTarget::alloc(mtm).set_ivars(events.clone());
        let target: Retained<MenuTarget> = unsafe { msg_send![super(this), init] };
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        for (id, title) in [
            (1, "Capture region"),
            (2, "Capture screen"),
            (3, "Capture window"),
            (4, "Open last capture"),
            (5, "Settings…"),
            (6, "Toggle notifications"),
            (7, "Quit"),
        ] {
            if id == 4 || id == 7 {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            let entry = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(sel!(performMenuAction:)),
                    &NSString::from_str(""),
                )
            };
            entry.setTag(id);
            unsafe {
                entry.setTarget(Some(&target));
            }
            menu.addItem(&entry);
        }
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        let Some(button) = item.button(mtm) else {
            NSStatusBar::systemStatusBar().removeStatusItem(&item);
            bail!("NSStatusItem has no button");
        };
        let data = NSData::with_bytes(include_bytes!(
            "../../../assets/branding/status-template@2x.png"
        ));
        let image = NSImage::initWithData(NSImage::alloc(), &data)
            .context("decoding bundled macOS status-bar icon")?;
        image.setSize(NSSize::new(18.0, 18.0));
        image.setTemplate(true);
        button.setImage(Some(&image));
        button.setAccessibilityTitle(Some(&NSString::from_str("quikscope")));
        button.setAccessibilityLabel(Some(&NSString::from_str("quikscope")));
        item.setMenu(Some(&menu));
        let mut events = Box::new(events);
        let mut handler = std::ptr::null_mut();
        let event_type = EventTypeSpec {
            class: u32::from_be_bytes(*b"keyb"),
            kind: 6,
        };
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                hotkey_callback,
                1,
                &event_type,
                (&mut *events as *mut EventSink).cast(),
                &mut handler,
            )
        };
        if status != 0 {
            NSStatusBar::systemStatusBar().removeStatusItem(&item);
            bail!("InstallEventHandler failed ({status})");
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        SHELL.with(|s| {
            *s.borrow_mut() = Some(NativeShell {
                id,
                item,
                _target: target,
                handler,
                events: Some(events),
                bindings: Vec::new(),
                keys: Vec::new(),
            })
        });
        Ok(Self { id })
    }
    pub fn set_hotkeys(&self, hotkeys: &Hotkeys) -> Result<()> {
        ensure!(
            MainThreadMarker::new().is_some(),
            "hotkeys must be updated on the main thread"
        );
        let mut bindings = Vec::new();
        for (id, value) in [
            (1, &hotkeys.region),
            (2, &hotkeys.screen),
            (3, &hotkeys.window),
            (4, &hotkeys.open_last),
        ] {
            if let Some((mods, key)) = parse_hotkey(value)? {
                ensure!(
                    !bindings.iter().any(|&(_, m, k)| m == mods && k == key),
                    "duplicate macOS hotkey {value:?}"
                );
                bindings.push((id, mods, key));
            }
        }
        SHELL.with(|s| {
            let mut s = s.borrow_mut();
            let native = s
                .as_mut()
                .filter(|s| s.id == self.id)
                .context("macOS shell stopped")?;
            native.unregister();
            match register(&bindings) {
                Ok(keys) => {
                    native.keys = keys;
                    native.bindings = bindings;
                    Ok(())
                }
                Err(error) => {
                    match register(&native.bindings) {
                        Ok(keys) => native.keys = keys,
                        Err(restore) => log::error!("restoring macOS hotkeys: {restore:#}"),
                    }
                    Err(error)
                }
            }
        })
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        let id = self.id;
        on_main(move || {
            SHELL.with(|s| {
                let mut state = s.borrow_mut();
                if state.as_ref().is_some_and(|s| s.id == id) {
                    state.take();
                }
            })
        });
    }
}
impl NativeShell {
    fn unregister(&mut self) {
        for key in self.keys.drain(..) {
            let status = unsafe { UnregisterEventHotKey(key) };
            if status != 0 {
                log::warn!("UnregisterEventHotKey failed ({status})");
            }
        }
    }
}
impl Drop for NativeShell {
    fn drop(&mut self) {
        self.unregister();
        let status = unsafe { RemoveEventHandler(self.handler) };
        if status != 0 {
            // A handler still owned by Carbon may still call its context. Leak
            // only on this exceptional cleanup failure rather than dangle.
            log::warn!("RemoveEventHandler failed ({status})");
            if let Some(events) = self.events.take() {
                std::mem::forget(events);
            }
        }
        self.item.setMenu(None);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

/// Carbon modifier mask and physical ANSI keycode. Empty disables a binding.
/// Letter shortcuts follow the physical US key positions, as Carbon requires.
pub fn parse_hotkey(value: &str) -> Result<Option<(u32, u32)>> {
    if value.is_empty() {
        return Ok(None);
    }
    let parts: Vec<_> = value.split('+').map(str::trim).collect();
    let (key, modifiers) = parts.split_last().context("missing hotkey key")?;
    let mut flags = 0;
    for modifier in modifiers {
        let flag = match modifier.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" | "win" => 1 << 8,
            "shift" => 1 << 9,
            "alt" | "option" | "opt" => 1 << 11,
            "ctrl" | "control" => 1 << 12,
            _ => bail!("unknown hotkey modifier {modifier:?}"),
        };
        ensure!(flags & flag == 0, "repeated hotkey modifier in {value:?}");
        flags |= flag;
    }
    let key = key.to_ascii_uppercase();
    const LETTERS: &[(u8, u32)] = &[
        (b'A', 0),
        (b'S', 1),
        (b'D', 2),
        (b'F', 3),
        (b'H', 4),
        (b'G', 5),
        (b'Z', 6),
        (b'X', 7),
        (b'C', 8),
        (b'V', 9),
        (b'B', 11),
        (b'Q', 12),
        (b'W', 13),
        (b'E', 14),
        (b'R', 15),
        (b'Y', 16),
        (b'T', 17),
        (b'1', 18),
        (b'2', 19),
        (b'3', 20),
        (b'4', 21),
        (b'6', 22),
        (b'5', 23),
        (b'9', 25),
        (b'7', 26),
        (b'8', 28),
        (b'0', 29),
        (b'O', 31),
        (b'U', 32),
        (b'I', 34),
        (b'P', 35),
        (b'L', 37),
        (b'J', 38),
        (b'K', 40),
        (b'N', 45),
        (b'M', 46),
    ];
    let code = if key.len() == 1 {
        LETTERS
            .iter()
            .find(|(letter, _)| *letter == key.as_bytes()[0])
            .map(|(_, code)| *code)
    } else if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<usize>().ok()) {
        const FUNCTION: [u32; 20] = [
            122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80,
            90,
        ];
        n.checked_sub(1).and_then(|n| FUNCTION.get(n)).copied()
    } else {
        match key.as_str() {
            "SPACE" => Some(49),
            "RETURN" | "ENTER" => Some(36),
            "TAB" => Some(48),
            "ESC" | "ESCAPE" => Some(53),
            "BACKSPACE" => Some(51),
            "DELETE" => Some(117),
            "HOME" => Some(115),
            "END" => Some(119),
            "PAGEUP" => Some(116),
            "PAGEDOWN" => Some(121),
            "LEFT" => Some(123),
            "RIGHT" => Some(124),
            "DOWN" => Some(125),
            "UP" => Some(126),
            _ => None,
        }
    }
    .with_context(|| format!("unsupported macOS hotkey key {key:?}"))?;
    Ok(Some((flags, code)))
}

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}
#[repr(C)]
struct HotKeyId {
    signature: u32,
    id: u32,
}
const SIGNATURE: u32 = u32::from_be_bytes(*b"qksc");
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        callback: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
        count: u32,
        types: *const EventTypeSpec,
        context: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn RemoveEventHandler(handler: *mut c_void) -> i32;
    fn RegisterEventHotKey(
        key: u32,
        modifiers: u32,
        id: HotKeyId,
        target: *mut c_void,
        options: u32,
        out: *mut *mut c_void,
    ) -> i32;
    fn UnregisterEventHotKey(key: *mut c_void) -> i32;
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        kind: u32,
        actual: *mut u32,
        size: u32,
        actual_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}
fn register(bindings: &[(u32, u32, u32)]) -> Result<Vec<*mut c_void>> {
    let mut refs = Vec::new();
    for &(id, modifiers, key) in bindings {
        let mut reference = std::ptr::null_mut();
        let status = unsafe {
            RegisterEventHotKey(
                key,
                modifiers,
                HotKeyId {
                    signature: SIGNATURE,
                    id,
                },
                GetApplicationEventTarget(),
                0,
                &mut reference,
            )
        };
        if status != 0 {
            for key in refs {
                unsafe {
                    UnregisterEventHotKey(key);
                }
            }
            bail!(
                "global shortcut {id} is unavailable (Carbon {status}); change [hotkeys] in config"
            );
        }
        refs.push(reference);
    }
    Ok(refs)
}
unsafe extern "C" fn hotkey_callback(
    _: *mut c_void,
    event: *mut c_void,
    context: *mut c_void,
) -> i32 {
    let mut id = HotKeyId {
        signature: 0,
        id: 0,
    };
    let status = unsafe {
        GetEventParameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            std::ptr::null_mut(),
            std::mem::size_of::<HotKeyId>() as u32,
            std::ptr::null_mut(),
            (&mut id as *mut HotKeyId).cast(),
        )
    };
    if status != 0 || id.signature != SIGNATURE {
        return -9874;
    }
    if let Some(request) = request(id.id) {
        deliver(
            unsafe { &*context.cast::<EventSink>() },
            Event::Request(request),
        );
    }
    0
}

#[link(name = "System")]
unsafe extern "C" {
    // dispatch_get_main_queue is an inline function in Apple's headers.
    #[link_name = "_dispatch_main_q"]
    static MAIN_QUEUE: c_void;
    fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void),
    );
}
pub(super) fn on_main(action: impl FnOnce() + Send + 'static) {
    if MainThreadMarker::new().is_some() {
        action();
        return;
    }
    extern "C" fn run(context: *mut c_void) {
        let action = unsafe { Box::from_raw(context.cast::<Box<dyn FnOnce() + Send>>()) };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).is_err() {
            log::error!("macOS main-queue callback panicked");
        }
    }
    let action: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(action));
    unsafe {
        dispatch_async_f(
            (&raw const MAIN_QUEUE).cast_mut(),
            Box::into_raw(action).cast(),
            run,
        );
    }
}

/// Installs a per-user LaunchAgent for future GUI logins, without starting a
/// second daemon or terminating the current daemon while changing preferences.
pub fn autostart(action: &str) -> Result<bool> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let path =
        std::path::PathBuf::from(home).join("Library/LaunchAgents/io.github.qt715.quikscope.plist");
    match action {
        "status" => Ok(path.try_exists()?),
        "off" => {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("removing LaunchAgent"),
            }
            Ok(false)
        }
        "on" => {
            let executable = std::env::current_exe().context("finding quikscope executable")?;
            let executable = executable
                .to_str()
                .context("LaunchAgent executable path is not UTF-8")?;
            let escaped = executable
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&apos;");
            ensure!(
                !escaped.chars().any(|c| c < ' '),
                "invalid XML character in executable path"
            );
            std::fs::create_dir_all(path.parent().context("missing LaunchAgents directory")?)?;
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>io.github.qt715.quikscope</string><key>ProgramArguments</key><array><string>{escaped}</string><string>daemon</string></array><key>RunAtLoad</key><true/><key>LimitLoadToSessionType</key><string>Aqua</string><key>ProcessType</key><string>Interactive</string></dict></plist>\n"
            );
            let temporary = path.with_extension(format!("plist.{}.tmp", std::process::id()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .context("creating LaunchAgent")?;
            if let Err(error) = file.write_all(xml.as_bytes()) {
                let _ = std::fs::remove_file(&temporary);
                return Err(error).context("writing LaunchAgent");
            }
            drop(file);
            if let Err(error) = std::fs::rename(&temporary, &path) {
                let _ = std::fs::remove_file(&temporary);
                return Err(anyhow!(error)).context("installing LaunchAgent");
            }
            Ok(true)
        }
        _ => bail!("usage: quikscope autostart <on|off|status>"),
    }
}
