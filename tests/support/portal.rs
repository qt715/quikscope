//! Fake only the portal transport; the PNG is captured from the private compositor by grim.
use super::*;
use std::io::BufRead as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use zbus::zvariant::{OwnedObjectPath, Value};

struct FakeScreenshot {
    env: Vec<(String, String)>,
    directory: PathBuf,
    response: Arc<AtomicU32>,
    /// Ask through a dialog (the helper's `dialog`) before answering, as
    /// GNOME and KDE may; no Enter within the deadline means denied.
    dialog: Arc<AtomicBool>,
}

/// How long the stand-in dialog waits for the user.
const DIALOG_DEADLINE: Duration = Duration::from_secs(5);

impl FakeScreenshot {
    /// Shows the dialog and returns whether Enter reached it in time.
    fn ask(&self) -> bool {
        let mut dialog = Command::new(HELPER)
            .arg("dialog")
            .env_remove("DISPLAY")
            .envs(self.env.iter().cloned())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = dialog.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line == "accepted" {
                    let _ = tx.send(());
                }
            }
        });
        let accepted = rx.recv_timeout(DIALOG_DEADLINE).is_ok();
        let _ = dialog.kill();
        let _ = dialog.wait();
        accepted
    }
}

#[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
impl FakeScreenshot {
    async fn screenshot(
        &self,
        _parent: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> OwnedObjectPath {
        assert!(!bool::try_from(options.get("interactive").unwrap()).unwrap());
        let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
        let sender = header.sender().unwrap();
        let path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{}/{}",
            sender.as_str().trim_start_matches(':').replace('.', "_"),
            token
        ))
        .unwrap();
        let code = if self.dialog.load(Ordering::Relaxed) && !self.ask() {
            2
        } else {
            self.response.load(Ordering::Relaxed)
        };
        let mut results: HashMap<&str, Value<'_>> = HashMap::new();
        let uri;
        if code == 0 {
            let file = self.directory.join(format!("portal {token}.png"));
            let status = Command::new("grim")
                .env_remove("DISPLAY")
                .envs(self.env.iter().cloned())
                .arg(&file)
                .status()
                .unwrap();
            assert!(status.success());
            uri = url::Url::from_file_path(file).unwrap().to_string();
            results.insert("uri", Value::from(uri.as_str()));
        }
        // Deliberately emit before returning the method reply: a late subscription loses it.
        connection
            .emit_signal(
                Some(sender.as_str()),
                path.as_str(),
                "org.freedesktop.portal.Request",
                "Response",
                &(code, results),
            )
            .await
            .unwrap();
        path
    }
}

impl Session {
    pub fn force_fullscreen(&mut self) {
        self.env
            .push(("QUIKSCOPE_DEBUG_NO_LAYER_SHELL".into(), "1".into()));
    }
    pub fn enable_portal(&mut self) -> Arc<AtomicU32> {
        self.enable_portal_with_dialog(false).0
    }

    /// Like [`Session::enable_portal`]; when `dialog` is set, every
    /// screenshot first waits for the user to press Enter in a
    /// permission-dialog stand-in.
    pub fn enable_portal_with_dialog(&mut self, dialog: bool) -> (Arc<AtomicU32>, Arc<AtomicBool>) {
        let response = Arc::new(AtomicU32::new(0));
        let dialog = Arc::new(AtomicBool::new(dialog));
        self.bus
            .object_server()
            .at(
                "/org/freedesktop/portal/desktop",
                FakeScreenshot {
                    env: self.env.clone(),
                    directory: self.root().to_path_buf(),
                    response: response.clone(),
                    dialog: dialog.clone(),
                },
            )
            .unwrap();
        self.bus
            .request_name("org.freedesktop.portal.Desktop")
            .unwrap();
        self.env.push(("QUIKSCOPE_BACKEND".into(), "portal".into()));
        (response, dialog)
    }
}
