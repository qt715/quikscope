//! Test-only Wayland client for the e2e suite. It plays the user inside a
//! headless wlroots compositor:
//!
//! ```text
//! quikscope-e2e-helper pattern [PHASE_FILE]         # resident wallpaper; repaint when phase file changes
//! quikscope-e2e-helper drag X1 Y1 X2 Y2 [right]     # press, move, release (virtual pointer)
//! quikscope-e2e-helper drag-hold X1 Y1 X2 Y2       # drag; print ready, release on stdin
//! quikscope-e2e-helper click X Y [right]
//! quikscope-e2e-helper move X Y
//! quikscope-e2e-helper key [ctrl+][shift+]NAME...   # press keys (virtual keyboard), e.g. `key r ctrl+s`
//! quikscope-e2e-helper type TEXT                    # type ASCII text
//! quikscope-e2e-helper ime                          # resident input method: steps on stdin, reports activation
//! quikscope-e2e-helper dialog                       # permission-dialog stand-in: a toplevel; prints "accepted" on Enter
//! quikscope-e2e-helper toplevels                    # print "app_id\ttitle" per window
//! quikscope-e2e-helper close APP_ID               # request native close of exactly one toplevel
//! quikscope-e2e-helper paste MIME                   # write the clipboard contents to stdout
//! quikscope-e2e-helper output NAME TRANSFORM SCALE [WxH]  # reconfigure an output
//! ```
//!
//! Coordinates are global logical pixels.

use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::time::{Duration, Instant};

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2, zwp_input_method_v2,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
    zwlr_output_manager_v1, zwlr_output_mode_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// `xkbcli compile-keymap --layout us`, uploaded with every virtual keyboard.
const KEYMAP: &str = include_str!("us.xkb");
const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 4;

/// Linux evdev key code for a key name.
fn keycode(name: &str) -> Option<u32> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return match c {
            'a'..='z' => Some(LETTERS[(c as u8 - b'a') as usize]),
            '1'..='9' => Some(2 + (c as u8 - b'1') as u32),
            '0' => Some(11),
            ' ' => Some(57),
            '-' => Some(12),
            '=' => Some(13),
            '[' => Some(26),
            ']' => Some(27),
            '.' => Some(52),
            ',' => Some(51),
            _ => None,
        };
    }
    Some(match name {
        "escape" => 1,
        "enter" => 28,
        "backspace" => 14,
        "delete" => 111,
        "space" => 57,
        "left" => 105,
        "right" => 106,
        "up" => 103,
        "down" => 108,
        _ => return None,
    })
}

fn virtual_keyboard(
    globals: &GlobalList,
    qh: &QueueHandle<State>,
) -> zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 {
    let mgr: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 =
        globals.bind(qh, 1..=1, ()).expect("virtual keyboard");
    let seat: wl_seat::WlSeat = globals.bind(qh, 1..=1, ()).expect("seat");
    let kb = mgr.create_virtual_keyboard(&seat, qh, ());
    let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(KEYMAP.as_bytes()).unwrap();
    file.write_all(&[0]).unwrap();
    kb.keymap(
        wl_keyboard::KeymapFormat::XkbV1.into(),
        file.as_fd(),
        KEYMAP.len() as u32 + 1,
    );
    kb
}

/// Presses each `[ctrl+][shift+]name` chord in turn.
fn press_keys(
    kb: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    queue: &mut EventQueue<State>,
    state: &mut State,
    chords: &[String],
) {
    let t0 = Instant::now();
    for chord in chords {
        let mut mods = 0;
        let mut name = chord.as_str();
        loop {
            if let Some(rest) = name.strip_prefix("ctrl+") {
                mods |= MOD_CTRL;
                name = rest;
            } else if let Some(rest) = name.strip_prefix("shift+") {
                mods |= MOD_SHIFT;
                name = rest;
            } else {
                break;
            }
        }
        let (code, shifted) = match keycode(name) {
            Some(c) => (c, false),
            None => {
                let lower: String = name.to_lowercase();
                match keycode(&lower) {
                    Some(c) if name.len() == 1 => (c, true),
                    _ => panic!("unknown key {name:?}"),
                }
            }
        };
        if shifted {
            mods |= MOD_SHIFT;
        }
        let ms = t0.elapsed().as_millis() as u32;
        kb.modifiers(mods, 0, 0, 0);
        kb.key(ms, code, wl_keyboard::KeyState::Pressed.into());
        kb.key(ms + 1, code, wl_keyboard::KeyState::Released.into());
        kb.modifiers(0, 0, 0, 0);
        queue.roundtrip(state).unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[derive(Default)]
struct Output {
    logical: (i32, i32, i32, i32),
}

#[derive(Default)]
struct Toplevel {
    id: Option<ObjectId>,
    app_id: String,
    title: String,
}

#[derive(Default)]
struct Head {
    proxy: Option<zwlr_output_head_v1::ZwlrOutputHeadV1>,
    name: String,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    toplevels: Vec<Toplevel>,
    layer_configured: Vec<Option<(u32, u32)>>,
    offer_mimes: Vec<String>,
    selection: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
    selection_seen: bool,
    heads: Vec<Head>,
    head_serial: Option<u32>,
    config_result: Option<bool>,
    /// Input method: activation requested by the compositor, applied at
    /// the next `done`.
    ime_pending: bool,
    ime_active: bool,
    /// Number of `done` events, which commits must quote.
    ime_serial: u32,
    ime_unavailable: bool,
    /// Dialog: its toplevel was configured, and Enter reached it.
    dialog_configured: bool,
    dialog_accepted: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
    let qh = queue.handle();
    let mut state = State::default();

    let outputs: Vec<wl_output::WlOutput> = globals.contents().with_list(|l| {
        l.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| globals.registry().bind(g.name, g.version.min(4), &qh, ()))
            .collect()
    });
    if let Ok(xdg) =
        globals.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(&qh, 2..=3, ())
    {
        for (i, o) in outputs.iter().enumerate() {
            state.outputs.push(Output::default());
            xdg.get_xdg_output(o, &qh, i);
        }
    }
    queue.roundtrip(&mut state).unwrap();

    let cmd = args.first().map(String::as_str).unwrap_or("");
    let num = |i: usize| -> f64 { args[i].parse().expect("number") };
    match cmd {
        "move" | "click" | "scroll" | "drag" | "drag-hold" => {
            let mgr: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 =
                globals.bind(&qh, 1..=2, ()).expect("virtual pointer");
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("seat");
            let ptr = mgr.create_virtual_pointer(Some(&seat), &qh, ());
            let (ext_w, ext_h) = layout_extent(&state);
            let t0 = Instant::now();
            let ms = || t0.elapsed().as_millis() as u32;
            let goto = |x: f64, y: f64| {
                ptr.motion_absolute(ms(), x as u32, y as u32, ext_w, ext_h);
                ptr.frame();
            };
            let button = |b: u32, pressed: bool| {
                let s = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                ptr.button(ms(), b, s);
                ptr.frame();
            };
            let pause = |queue: &mut wayland_client::EventQueue<State>, state: &mut State| {
                queue.roundtrip(state).unwrap();
                std::thread::sleep(Duration::from_millis(40));
            };
            match cmd {
                "move" => {
                    goto(num(1), num(2));
                    pause(&mut queue, &mut state);
                }
                "scroll" => {
                    goto(num(1), num(2));
                    pause(&mut queue, &mut state);
                    // GPUI consumes continuous deltas only for touchpad/finger
                    // sources; wheel sources require discrete axis events.
                    ptr.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Finger);
                    ptr.axis(
                        ms(),
                        wayland_client::protocol::wl_pointer::Axis::VerticalScroll,
                        num(3),
                    );
                    ptr.frame();
                    pause(&mut queue, &mut state);
                }
                "click" => {
                    let b = if args.get(3).map(String::as_str) == Some("right") {
                        BTN_RIGHT
                    } else {
                        BTN_LEFT
                    };
                    goto(num(1), num(2));
                    pause(&mut queue, &mut state);
                    button(b, true);
                    pause(&mut queue, &mut state);
                    button(b, false);
                    pause(&mut queue, &mut state);
                }
                _ => {
                    let b = if args.get(5).map(String::as_str) == Some("right") {
                        BTN_RIGHT
                    } else {
                        BTN_LEFT
                    };
                    let (x1, y1, x2, y2) = (num(1), num(2), num(3), num(4));
                    goto(x1, y1);
                    pause(&mut queue, &mut state);
                    button(b, true);
                    pause(&mut queue, &mut state);
                    for i in 1..=8 {
                        let t = i as f64 / 8.0;
                        goto(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
                        pause(&mut queue, &mut state);
                    }
                    if cmd == "drag-hold" {
                        println!("ready");
                        std::io::stdout().flush().unwrap();
                        std::io::stdin().read_exact(&mut [0u8; 1]).unwrap();
                    }
                    button(b, false);
                    pause(&mut queue, &mut state);
                }
            }
            ptr.destroy();
            queue.roundtrip(&mut state).unwrap();
        }
        "toplevels" | "close" => {
            let _mgr: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 =
                globals.bind(&qh, 1..=3, ()).expect("foreign toplevel");
            queue.roundtrip(&mut state).unwrap();
            queue.roundtrip(&mut state).unwrap();
            if cmd == "close" {
                let app_id = args.get(1).expect("app id");
                let matching: Vec<_> = state
                    .toplevels
                    .iter()
                    .filter(|t| &t.app_id == app_id)
                    .collect();
                assert_eq!(
                    matching.len(),
                    1,
                    "close requires exactly one matching toplevel"
                );
                let handle = zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1::from_id(
                    &conn,
                    matching[0].id.clone().unwrap(),
                )
                .unwrap();
                handle.close();
                queue.roundtrip(&mut state).unwrap();
            } else {
                for t in &state.toplevels {
                    println!("{}\t{}", t.app_id, t.title);
                }
            }
        }
        "paste" => {
            let mime = args.get(1).expect("mime");
            let mgr: zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 =
                globals.bind(&qh, 1..=2, ()).expect("data control");
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("seat");
            let _dev = mgr.get_data_device(&seat, &qh, ());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !state.selection_seen && Instant::now() < deadline {
                queue.roundtrip(&mut state).unwrap();
            }
            let Some(offer) = state.selection.take() else {
                eprintln!("no selection");
                std::process::exit(2);
            };
            if !state.offer_mimes.iter().any(|m| m == mime) {
                eprintln!("selection offers {:?}", state.offer_mimes);
                std::process::exit(3);
            }
            let (r, w) = rustix::pipe::pipe().unwrap();
            offer.receive(mime.clone(), w.as_fd());
            drop(w);
            queue.roundtrip(&mut state).unwrap();
            let mut data = Vec::new();
            std::fs::File::from(r).read_to_end(&mut data).unwrap();
            std::io::stdout().write_all(&data).unwrap();
        }
        "output" => {
            let (name, transform, scale) = (args[1].clone(), num(2) as u32, num(3));
            let _mgr: zwlr_output_manager_v1::ZwlrOutputManagerV1 =
                globals.bind(&qh, 1..=4, ()).expect("output management");
            queue.roundtrip(&mut state).unwrap();
            queue.roundtrip(&mut state).unwrap();
            let serial = state.head_serial.expect("output manager done");
            let mgr = _mgr;
            let config = mgr.create_configuration(serial, &qh, ());
            for head in &state.heads {
                let proxy = head.proxy.as_ref().unwrap();
                let ch = config.enable_head(proxy, &qh, ());
                if head.name == name {
                    ch.set_transform(match transform {
                        1 => wl_output::Transform::_90,
                        2 => wl_output::Transform::_180,
                        3 => wl_output::Transform::_270,
                        4 => wl_output::Transform::Flipped,
                        5 => wl_output::Transform::Flipped90,
                        6 => wl_output::Transform::Flipped180,
                        7 => wl_output::Transform::Flipped270,
                        _ => wl_output::Transform::Normal,
                    });
                    ch.set_scale(scale);
                    // Optional 5th argument: custom mode WxH.
                    if let Some((w, h)) = args.get(4).and_then(|m| m.split_once('x')) {
                        ch.set_custom_mode(w.parse().unwrap(), h.parse().unwrap(), 0);
                    }
                }
            }
            config.apply();
            while state.config_result.is_none() {
                queue.blocking_dispatch(&mut state).unwrap();
            }
            if state.config_result != Some(true) {
                eprintln!("output configuration failed");
                std::process::exit(1);
            }
        }
        "key" => {
            let kb = virtual_keyboard(&globals, &qh);
            queue.roundtrip(&mut state).unwrap();
            // wlroots drops keys a client receives in the same burst as the
            // keymap switch to a just-created keyboard; let the switch land.
            std::thread::sleep(Duration::from_millis(300));
            press_keys(&kb, &mut queue, &mut state, &args[1..]);
            kb.destroy();
            queue.roundtrip(&mut state).unwrap();
        }
        "type" => {
            let kb = virtual_keyboard(&globals, &qh);
            queue.roundtrip(&mut state).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            let chords: Vec<String> = args[1]
                .chars()
                .map(|c| {
                    if c == ' ' {
                        "space".to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            press_keys(&kb, &mut queue, &mut state, &chords);
            kb.destroy();
            queue.roundtrip(&mut state).unwrap();
        }
        "ime" => run_ime(&globals, &mut queue, &mut state),
        "pattern" => run_pattern(
            &globals,
            &mut queue,
            &mut state,
            &outputs,
            args.get(1).map(String::as_str),
        ),
        "dialog" => run_dialog(&globals, &mut queue, &mut state),
        _ => {
            eprintln!("unknown command {cmd:?}");
            std::process::exit(2);
        }
    }
}

fn layout_extent(state: &State) -> (u32, u32) {
    let w = state
        .outputs
        .iter()
        .map(|o| o.logical.0 + o.logical.2)
        .max()
        .unwrap_or(1);
    let h = state
        .outputs
        .iter()
        .map(|o| o.logical.1 + o.logical.3)
        .max()
        .unwrap_or(1);
    (w.max(1) as u32, h.max(1) as u32)
}

/// Deterministic wallpaper: horizontal red ramp, vertical green ramp, blue
/// 255 in the top-left 64×64 block. Pixel (x, y) of a 1x output is
/// `(r, g, b) = (x % 256, y % 256, 0)` outside that block.
fn pattern_pixel(x: u32, y: u32) -> [u8; 4] {
    let b = if x < 64 && y < 64 { 255 } else { 0 };
    [b, (y % 256) as u8, (x % 256) as u8, 255]
}

/// Plays a resident input method (what fcitx5 or IBus are to the
/// compositor). Prints `active` / `inactive` whenever the compositor
/// (de)activates it for a focused text input, and applies one step per
/// stdin line: `pre=TEXT` (pre-edit) or `commit=TEXT`. Exits on EOF.
fn run_ime(
    globals: &wayland_client::globals::GlobalList,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
) {
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("seat");
    let mgr: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        globals.bind(&qh, 1..=1, ()).expect("input method manager");
    let im = mgr.get_input_method(&seat, &qh, ());
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            tx.send(line).unwrap();
        }
    });
    queue.roundtrip(state).unwrap();
    println!("ready");
    let mut reported = false;
    loop {
        queue.roundtrip(state).unwrap();
        if state.ime_unavailable {
            eprintln!("another input method is registered");
            std::process::exit(1);
        }
        if state.ime_active != reported {
            reported = state.ime_active;
            println!("{}", if reported { "active" } else { "inactive" });
        }
        std::io::stdout().flush().unwrap();
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(step) => {
                if !state.ime_active {
                    eprintln!("step {step:?} while no text input is active");
                    std::process::exit(1);
                }
                let (kind, value) = step.split_once('=').expect("STEP is kind=value");
                match kind {
                    "pre" => {
                        let end = value.len() as i32;
                        im.set_preedit_string(value.to_string(), end, end);
                    }
                    "commit" => im.commit_string(value.to_string()),
                    other => panic!("unknown ime step {other:?}"),
                }
                im.commit(state.ime_serial);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    im.destroy();
    queue.roundtrip(state).unwrap();
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_input_method_v2::Event;
        match event {
            Event::Activate => state.ime_pending = true,
            Event::Deactivate => state.ime_pending = false,
            Event::Done => {
                state.ime_serial += 1;
                state.ime_active = state.ime_pending;
            }
            Event::Unavailable => state.ime_unavailable = true,
            _ => {}
        }
    }
}

/// Stands in for a desktop permission dialog (the Screenshot portal's):
/// an ordinary toplevel that the compositor focuses, accepting on Enter.
/// Anything holding the keyboard meanwhile keeps the user from answering.
fn run_dialog(
    globals: &wayland_client::globals::GlobalList,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
) {
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=5, ()).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("seat");
    let _keyboard = seat.get_keyboard(&qh, ());
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("quikscope-e2e-dialog".into());
    toplevel.set_title("Allow screenshot?".into());
    surface.commit();
    while !state.dialog_configured {
        queue.blocking_dispatch(state).unwrap();
    }
    let (w, h) = (320u32, 160u32);
    let size = (w * h * 4) as usize;
    let fd = rustix::fs::memfd_create("dialog", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&fd, size as u64).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(&[0x60; 320 * 160 * 4]).unwrap();
    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        (w * 4) as i32,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    queue.roundtrip(state).unwrap();
    println!("ready");
    std::io::stdout().flush().unwrap();
    while !state.dialog_accepted {
        queue.blocking_dispatch(state).unwrap();
    }
    println!("accepted");
    std::io::stdout().flush().unwrap();
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            state.dialog_configured = true;
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
            && Some(key) == keycode("enter")
        {
            state.dialog_accepted = true;
        }
    }
}

fn run_pattern(
    globals: &wayland_client::globals::GlobalList,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    outputs: &[wl_output::WlOutput],
    phase_file: Option<&str>,
) {
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let mut surfaces = Vec::new();
    for (i, o) in outputs.iter().enumerate() {
        let surface = compositor.create_surface(&qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            Some(o),
            zwlr_layer_shell_v1::Layer::Background,
            "quikscope-e2e-pattern".into(),
            &qh,
            i,
        );
        layer.set_anchor(zwlr_layer_surface_v1::Anchor::all());
        layer.set_exclusive_zone(-1);
        surface.commit();
        state.layer_configured.push(None);
        surfaces.push((surface, layer));
    }
    while state.layer_configured.iter().any(Option::is_none) {
        queue.blocking_dispatch(state).unwrap();
    }
    let paint = |queue: &mut EventQueue<State>, state: &mut State, phase: u8| {
        for (i, (surface, _)) in surfaces.iter().enumerate() {
            let (w, h) = state.layer_configured[i].unwrap();
            let stride = w * 4;
            let size = (stride * h) as usize;
            let fd = rustix::fs::memfd_create("pattern", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
            rustix::fs::ftruncate(&fd, size as u64).unwrap();
            let mut file = std::fs::File::from(fd);
            let mut data = Vec::with_capacity(size);
            for y in 0..h {
                for x in 0..w {
                    let mut pixel = pattern_pixel(x, y);
                    if phase % 2 == 1 {
                        pixel[..3].iter_mut().for_each(|c| *c = 255 - *c);
                    }
                    data.extend_from_slice(&pixel);
                }
            }
            file.write_all(&data).unwrap();
            let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                w as i32,
                h as i32,
                stride as i32,
                wl_shm::Format::Xrgb8888,
                &qh,
                (),
            );
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
            // Keep buffers alive until this private helper exits; destroying
            // one before the compositor releases it makes its contents undefined.
            pool.destroy();
        }
        queue.roundtrip(state).unwrap();
    };
    let mut phase = 0;
    paint(queue, state, phase);
    if let Some(path) = phase_file {
        std::fs::write(format!("{path}.ready"), "0").unwrap();
    }
    // A seat without a keyboard has no wl_keyboard capability; clients
    // would only bind one once a short-lived virtual keyboard appears, and
    // miss its first keys. Keep one attached for the whole session.
    let _keyboard = virtual_keyboard(globals, &qh);
    queue.roundtrip(state).unwrap();
    println!("ready");
    std::io::stdout().flush().unwrap();
    loop {
        if let Some(path) = phase_file {
            let next = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .unwrap_or(phase);
            if next != phase {
                paint(queue, state, next);
                phase = next;
                std::fs::write(format!("{path}.ready"), phase.to_string()).unwrap();
            }
            queue.roundtrip(state).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        } else {
            queue.blocking_dispatch(state).unwrap();
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer.ack_configure(serial);
            state.layer_configured[*i] = Some((width, height));
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = &mut state.outputs[*i].logical;
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => (o.0, o.1) = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => (o.2, o.3) = (width, height),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(Toplevel {
                id: Some(toplevel.id()),
                ..Default::default()
            });
        }
    }

    event_created_child!(State, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(t) = state
            .toplevels
            .iter_mut()
            .find(|t| t.id.as_ref() == Some(&handle.id()))
        else {
            return;
        };
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => t.title = title,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { .. } => state.offer_mimes.clear(),
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.selection = id;
                state.selection_seen = true;
            }
            _ => {}
        }
    }

    event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mimes.push(mime_type);
        }
    }
}

impl Dispatch<zwlr_output_manager_v1::ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(Head {
                proxy: Some(head),
                name: String::new(),
            }),
            zwlr_output_manager_v1::Event::Done { serial } => state.head_serial = Some(serial),
            _ => {}
        }
    }

    event_created_child!(State, zwlr_output_manager_v1::ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (zwlr_output_head_v1::ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<zwlr_output_head_v1::ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        head: &zwlr_output_head_v1::ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_head_v1::Event::Name { name } = event
            && let Some(h) = state
                .heads
                .iter_mut()
                .find(|h| h.proxy.as_ref() == Some(head))
        {
            h.name = name;
        }
    }

    event_created_child!(State, zwlr_output_head_v1::ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (zwlr_output_mode_v1::ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => state.config_result = Some(true),
            zwlr_output_configuration_v1::Event::Failed
            | zwlr_output_configuration_v1::Event::Cancelled => state.config_result = Some(false),
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
wayland_client::delegate_noop!(State: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(State: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
wayland_client::delegate_noop!(State: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
wayland_client::delegate_noop!(State: ignore zwlr_output_mode_v1::ZwlrOutputModeV1);
wayland_client::delegate_noop!(State: ignore zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1);

wayland_client::delegate_noop!(State: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(State: ignore zwp_input_method_manager_v2::ZwpInputMethodManagerV2);
