//! Fake only the portal transport; the PNG is captured from the private compositor by grim.
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use zbus::zvariant::{OwnedObjectPath, Value};

struct FakeScreenshot {
    env: Vec<(String, String)>,
    directory: PathBuf,
    response: Arc<AtomicU32>,
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
        let code = self.response.load(Ordering::Relaxed);
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
    pub fn enable_portal(&mut self) -> Arc<AtomicU32> {
        let response = Arc::new(AtomicU32::new(0));
        self.bus
            .object_server()
            .at(
                "/org/freedesktop/portal/desktop",
                FakeScreenshot {
                    env: self.env.clone(),
                    directory: self.root().to_path_buf(),
                    response: response.clone(),
                },
            )
            .unwrap();
        self.bus
            .request_name("org.freedesktop.portal.Desktop")
            .unwrap();
        self.env
            .push(("QUICKSCOPE_BACKEND".into(), "portal".into()));
        response
    }
}
