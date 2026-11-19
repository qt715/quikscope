//! StatusNotifierItem service; no compositor or icon-theme dependency.

use std::sync::{LazyLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

use super::{ANIMATION_FPS_CHOICES, Event, EventSink, Tray, TrayState, VIDEO_FPS_CHOICES};
use crate::config::RecordConfig;
use crate::ipc::{CaptureOptions, Mode, RecordAction, RecordOptions, Request};
use crate::media::Kind;

static IDLE_ICON: LazyLock<ksni::Icon> =
    LazyLock::new(|| icon(include_bytes!("../../assets/branding/tray-idle.png")));
static RECORDING_ICON: LazyLock<ksni::Icon> =
    LazyLock::new(|| icon(include_bytes!("../../assets/branding/tray-recording.png")));

fn icon(bytes: &[u8]) -> ksni::Icon {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("embedded tray PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    let mut data = image.into_raw();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

enum Command {
    Update(TrayState),
    Shutdown,
}

pub(super) struct StatusNotifier {
    commands: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl StatusNotifier {
    pub(super) fn spawn(events: EventSink, config: &RecordConfig) -> anyhow::Result<Self> {
        let state = TrayState {
            recording: None,
            audio: Vec::new(),
            video_fps: config.video_fps,
            animation_fps: config.animation_fps,
        };
        // Keep the bus service alive before a panel starts; ksni watches for a
        // returning watcher and re-registers rather than requiring a daemon restart.
        let handle = Item {
            events,
            state,
            now: Instant::now(),
        }
        .assume_sni_available(true)
        .spawn()?;
        let (commands, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("quikscope-tray".into())
            .spawn(move || {
                let mut ticking = false;
                loop {
                    let command = if ticking {
                        match receiver.recv_timeout(Duration::from_secs(1)) {
                            Ok(command) => Some(command),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match receiver.recv() {
                            Ok(command) => Some(command),
                            Err(_) => break,
                        }
                    };
                    match command {
                        Some(Command::Update(state)) => {
                            ticking = state
                                .recording
                                .as_ref()
                                .is_some_and(|r| !r.paused && !r.saving);
                            if handle
                                .update(|item| {
                                    item.state = state;
                                    item.now = Instant::now();
                                })
                                .is_none()
                            {
                                break;
                            }
                        }
                        Some(Command::Shutdown) => break,
                        None => {
                            if handle.update(|item| item.now = Instant::now()).is_none() {
                                break;
                            }
                        }
                    }
                }
                handle.shutdown().wait();
            })?;
        Ok(Self {
            commands,
            worker: Some(worker),
        })
    }
}

impl Tray for StatusNotifier {
    fn update(&self, state: &TrayState) {
        let _ = self.commands.send(Command::Update(state.clone()));
    }
}

impl Drop for StatusNotifier {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Item {
    events: EventSink,
    state: TrayState,
    now: Instant,
}

impl Item {
    fn recording_label(&self) -> Option<String> {
        let recording = self.state.recording.as_ref()?;
        if recording.saving {
            return Some(recording.saving_label());
        }
        let elapsed = recording.elapsed
            + if recording.paused {
                Duration::ZERO
            } else {
                self.now.saturating_duration_since(recording.at)
            };
        let seconds = elapsed.as_secs();
        Some(format!(
            "● {} {}:{:02} ({})",
            if recording.paused {
                "Paused"
            } else {
                "Recording"
            },
            seconds / 60,
            seconds % 60,
            recording.kind.as_str()
        ))
    }

    fn action(label: &str, request: Request) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            activate: Box::new(move |item: &mut Self| {
                (item.events)(Event::Request(request.clone()))
            }),
            ..Default::default()
        }
        .into()
    }

    fn fps(&self, label: &str, kind: Kind, choices: [u32; 4], selected: u32) -> MenuItem<Self> {
        SubMenu {
            label: label.into(),
            submenu: vec![
                RadioGroup {
                    // Custom configured rates intentionally leave every preset unchecked.
                    selected: choices
                        .iter()
                        .position(|&fps| fps == selected)
                        .unwrap_or(usize::MAX),
                    options: choices
                        .iter()
                        .map(|fps| RadioItem {
                            label: format!("{fps} fps"),
                            ..Default::default()
                        })
                        .collect(),
                    select: Box::new(move |item: &mut Self, index| {
                        if let Some(&fps) = choices.get(index) {
                            (item.events)(Event::SetFps(kind, fps));
                        }
                    }),
                }
                .into(),
            ],
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "quikscope".into()
    }

    fn title(&self) -> String {
        self.recording_label().map_or_else(
            || "quikscope".into(),
            |label| format!("quikscope — {label}"),
        )
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![
            if self.state.recording.is_some() {
                &*RECORDING_ICON
            } else {
                &*IDLE_ICON
            }
            .clone(),
        ]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title(),
            description: match &self.state.recording {
                Some(r) if r.saving => "",
                Some(_) => "Click to stop and save",
                None => "Click to open last capture",
            }
            .into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.events)(Event::Request(match &self.state.recording {
            // Stopping again would only report that it is still saving.
            Some(r) if r.saving => return,
            Some(_) => Request::RecordControl(RecordAction::Stop),
            None => Request::OpenLast,
        }));
    }

    fn menu_about_to_show(&mut self) {
        (self.events)(Event::RefreshAudioDevices);
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::info!(
            "system tray host unavailable ({reason:?}); waiting for a StatusNotifierWatcher"
        );
        true
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let disabled = |label| -> MenuItem<Self> {
            StandardItem {
                label,
                enabled: false,
                ..Default::default()
            }
            .into()
        };
        let mut menu = match (&self.state.recording, self.recording_label()) {
            (Some(r), Some(label)) if r.saving => {
                let mut items = vec![disabled(label)];
                if r.save_cancellable() {
                    items.push(Self::action(
                        "Cancel saving",
                        Request::RecordControl(RecordAction::Cancel),
                    ));
                }
                items
            }
            (Some(r), Some(label)) => vec![
                disabled(label),
                Self::action(
                    if r.paused { "Resume" } else { "Pause" },
                    Request::RecordControl(RecordAction::TogglePause),
                ),
                Self::action("Stop and save", Request::RecordControl(RecordAction::Stop)),
                Self::action("Discard", Request::RecordControl(RecordAction::Cancel)),
            ],
            _ => vec![
                Self::action(
                    "Record video",
                    Request::Record(RecordOptions::new(Kind::Video)),
                ),
                Self::action(
                    "Record animation",
                    Request::Record(RecordOptions::new(Kind::Animation)),
                ),
            ],
        };
        menu.push(MenuItem::Separator);
        menu.push(
            SubMenu {
                label: "Audio".into(),
                submenu: self
                    .state
                    .audio
                    .iter()
                    .map(|(device, checked)| {
                        let id = device.id.clone();
                        let checked = *checked;
                        CheckmarkItem {
                            label: device.name.replace('_', "__"),
                            checked,
                            activate: Box::new(move |item: &mut Self| {
                                (item.events)(Event::SetAudio(id.clone(), !checked))
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
        );
        menu.push(self.fps(
            "Video frame rate",
            Kind::Video,
            VIDEO_FPS_CHOICES,
            self.state.video_fps,
        ));
        menu.push(self.fps(
            "Animation frame rate",
            Kind::Animation,
            ANIMATION_FPS_CHOICES,
            self.state.animation_fps,
        ));
        menu.extend([
            MenuItem::Separator,
            Self::action(
                "Capture region",
                Request::Capture(Mode::Region, CaptureOptions::default()),
            ),
            Self::action("Open last capture", Request::OpenLast),
            Self::action("Pick color", Request::Pick),
            Self::action("Settings…", Request::Settings),
            // Quit always stops and saves a recording first; say so.
            Self::action(
                if self.state.recording.is_some() {
                    "Save recording and quit"
                } else {
                    "Quit"
                },
                Request::Quit,
            ),
        ]);
        menu
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::OwnedValue;

    type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

    fn children(menu: &Proxy<'_>, parent: i32) -> Vec<Node> {
        let layout: zbus::Result<(u32, Node)> =
            menu.call("GetLayout", &(parent, 1_i32, Vec::<String>::new()));
        match layout {
            Ok((_, root)) => root
                .2
                .into_iter()
                .map(|child| child.try_into().unwrap())
                .collect(),
            // A concurrent structural update invalidates submenu ids; the
            // polling caller must fetch the new root before trying again.
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs" =>
            {
                Vec::new()
            }
            Err(error) => panic!("GetLayout failed: {error}"),
        }
    }

    fn label(node: &Node) -> &str {
        node.1
            .get("label")
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or("")
    }

    fn find(menu: &Proxy<'_>, parent: i32, name: &str) -> Node {
        children(menu, parent)
            .into_iter()
            .find(|node| label(node) == name)
            .unwrap_or_else(|| panic!("missing menu item {name}"))
    }

    fn click(menu: &Proxy<'_>, node: &Node) {
        let _: () = menu
            .call(
                "Event",
                &(node.0, "clicked", OwnedValue::from(0_i32), 0_u32),
            )
            .unwrap();
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "tray update timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn private_bus_menu() {
        if std::env::var_os("QUIKSCOPE_TRAY_TEST_BUS").is_none() {
            let status = std::process::Command::new("dbus-run-session")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::sni::tests::private_bus_menu",
                    "--nocapture",
                ])
                .env("QUIKSCOPE_TRAY_TEST_BUS", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let (sender, events) = mpsc::channel();
        // Deliberately no watcher: service startup must still succeed and expose
        // its real DBusMenu, not an in-memory imitation of the exported tree.
        let tray = StatusNotifier::spawn(
            Arc::new(move |event| {
                sender.send(event).unwrap();
            }),
            &RecordConfig::default(),
        )
        .unwrap();
        let connection = Connection::session().unwrap();
        let bus = zbus::blocking::fdo::DBusProxy::new(&connection).unwrap();
        let destination = bus
            .list_names()
            .unwrap()
            .into_iter()
            .find(|name| name.as_str().starts_with("org.kde.StatusNotifierItem-"))
            .unwrap();
        let menu = Proxy::new(
            &connection,
            destination.as_str(),
            "/MenuBar",
            "com.canonical.dbusmenu",
        )
        .unwrap();
        let item: Proxy<'_> = zbus::blocking::proxy::Builder::new(&connection)
            .destination(destination.as_str())
            .unwrap()
            .path("/StatusNotifierItem")
            .unwrap()
            .interface("org.kde.StatusNotifierItem")
            .unwrap()
            // SNI uses NewTitle, not standard PropertiesChanged.
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .unwrap();
        assert_eq!(item.get_property::<String>("Id").unwrap(), "quikscope");
        assert_eq!(item.get_property::<String>("Status").unwrap(), "Active");
        assert!(
            !item
                .get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap")
                .unwrap()
                .is_empty()
        );
        let root = children(&menu, 0);
        assert_eq!(
            root.iter().map(label).collect::<Vec<_>>(),
            [
                "Record video",
                "Record animation",
                "",
                "Audio",
                "Video frame rate",
                "Animation frame rate",
                "",
                "Capture region",
                "Open last capture",
                "Pick color",
                "Settings…",
                "Quit",
            ]
        );
        assert_eq!(<&str>::try_from(&root[2].1["type"]).unwrap(), "separator");
        let receive = || events.recv_timeout(Duration::from_secs(2)).unwrap();
        for (name, request) in [
            (
                "Record video",
                Request::Record(RecordOptions::new(Kind::Video)),
            ),
            (
                "Record animation",
                Request::Record(RecordOptions::new(Kind::Animation)),
            ),
            (
                "Capture region",
                Request::Capture(Mode::Region, CaptureOptions::default()),
            ),
            ("Open last capture", Request::OpenLast),
            ("Pick color", Request::Pick),
            ("Settings…", Request::Settings),
            ("Quit", Request::Quit),
        ] {
            click(&menu, &find(&menu, 0, name));
            assert_eq!(receive(), Event::Request(request));
        }
        let _: () = item.call("Activate", &(0_i32, 0_i32)).unwrap();
        assert_eq!(receive(), Event::Request(Request::OpenLast));

        let mut state = TrayState {
            recording: None,
            audio: vec![(
                crate::media::AudioDevice {
                    id: "monitor".into(),
                    name: "Desktop audio".into(),
                    output: true,
                },
                true,
            )],
            video_fps: 60,
            animation_fps: 30,
        };
        tray.update(&state);
        wait_for(|| !children(&menu, find(&menu, 0, "Audio").0).is_empty());
        let audio = find(&menu, find(&menu, 0, "Audio").0, "Desktop audio");
        assert_eq!(i32::try_from(&audio.1["toggle-state"]).unwrap(), 1);
        click(&menu, &audio);
        assert_eq!(receive(), Event::SetAudio("monitor".into(), false));
        for (title, kind, choices) in [
            ("Video frame rate", Kind::Video, VIDEO_FPS_CHOICES),
            (
                "Animation frame rate",
                Kind::Animation,
                ANIMATION_FPS_CHOICES,
            ),
        ] {
            let parent = find(&menu, 0, title).0;
            for fps in choices {
                let option = find(&menu, parent, &format!("{fps} fps"));
                assert_eq!(<&str>::try_from(&option.1["toggle-type"]).unwrap(), "radio");
                click(&menu, &option);
                assert_eq!(receive(), Event::SetFps(kind, fps));
            }
        }
        state.audio[0].1 = false;
        state.video_fps = 24;
        state.animation_fps = 10;
        state.recording = Some(super::super::RecordingStatus {
            kind: Kind::Video,
            elapsed: Duration::from_secs(12),
            at: Instant::now(),
            paused: false,
            saving: false,
            progress: None,
        });
        tray.update(&state);
        wait_for(|| label(&children(&menu, 0)[0]).starts_with("● Recording 0:12"));
        assert!(!bool::try_from(&children(&menu, 0)[0].1["enabled"]).unwrap());
        assert_eq!(
            i32::try_from(
                &find(&menu, find(&menu, 0, "Audio").0, "Desktop audio").1["toggle-state"]
            )
            .unwrap(),
            0
        );
        for (title, rate) in [
            ("Video frame rate", "24 fps"),
            ("Animation frame rate", "10 fps"),
        ] {
            assert_eq!(
                i32::try_from(&find(&menu, find(&menu, 0, title).0, rate).1["toggle-state"])
                    .unwrap(),
                1
            );
        }
        for (name, action) in [
            ("Pause", RecordAction::TogglePause),
            ("Stop and save", RecordAction::Stop),
            ("Discard", RecordAction::Cancel),
        ] {
            click(&menu, &find(&menu, 0, name));
            assert_eq!(receive(), Event::Request(Request::RecordControl(action)));
        }
        click(&menu, &find(&menu, 0, "Save recording and quit"));
        assert_eq!(receive(), Event::Request(Request::Quit));
        let _: bool = menu.call("AboutToShow", &(0_i32,)).unwrap();
        assert_eq!(receive(), Event::RefreshAudioDevices);
        let _: () = item.call("Activate", &(0_i32, 0_i32)).unwrap();
        assert_eq!(
            receive(),
            Event::Request(Request::RecordControl(RecordAction::Stop))
        );
        wait_for(|| {
            item.get_property::<String>("Title")
                .unwrap()
                .contains("0:13")
        });
        state.recording.as_mut().unwrap().paused = true;
        tray.update(&state);
        wait_for(|| {
            children(&menu, 0)
                .iter()
                .any(|node| label(node) == "Resume")
        });
        assert!(
            item.get_property::<String>("Title")
                .unwrap()
                .contains("Paused 0:12")
        );
        click(&menu, &find(&menu, 0, "Resume"));
        assert_eq!(
            receive(),
            Event::Request(Request::RecordControl(RecordAction::TogglePause))
        );
        thread::sleep(Duration::from_millis(1100));
        assert!(
            item.get_property::<String>("Title")
                .unwrap()
                .contains("Paused 0:12")
        );
        // Saving: no recording controls until the file is written, and the
        // icon click no longer asks to stop again.
        state.recording.as_mut().unwrap().saving = true;
        tray.update(&state);
        wait_for(|| label(&children(&menu, 0)[0]) == "Saving recording…");
        let root = children(&menu, 0);
        assert!(!bool::try_from(&root[0].1["enabled"]).unwrap());
        for gone in [
            "Resume",
            "Pause",
            "Stop and save",
            "Discard",
            "Record video",
        ] {
            assert!(root.iter().all(|node| label(node) != gone), "{gone}");
        }
        assert!(
            root.iter()
                .any(|node| label(node) == "Save recording and quit")
        );
        let _: () = item.call("Activate", &(0_i32, 0_i32)).unwrap();
        assert!(
            events.recv_timeout(Duration::from_millis(300)).is_err(),
            "activating while saving sends nothing"
        );
        // Saving a stopped recording shows its progress and can be cancelled.
        state.recording.as_mut().unwrap().progress = Some(42);
        tray.update(&state);
        wait_for(|| label(&children(&menu, 0)[0]) == "Saving recording… 42%");
        click(&menu, &find(&menu, 0, "Cancel saving"));
        assert_eq!(
            receive(),
            Event::Request(Request::RecordControl(RecordAction::Cancel))
        );
        state.recording = None;
        tray.update(&state);
        wait_for(|| label(&children(&menu, 0)[0]) == "Record video");
        assert_eq!(item.get_property::<String>("Title").unwrap(), "quikscope");
        drop(tray);
    }
}
