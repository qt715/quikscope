//! Desktop notifications over `org.freedesktop.Notifications`, with a
//! listener thread that turns notification clicks into "open the editor".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{StructureBuilder, Value};

use crate::platform::{Event, EventSink, Notifier};
use crate::raster::Image;

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// Clicking the notification body invokes this key on most servers.
const DEFAULT_ACTION: &str = "default";
const EDIT_ACTION: &str = "edit";

pub struct Notifications {
    conn: Connection,
    /// Server-assigned notification id → shot id.
    pending: Arc<Mutex<HashMap<u32, u64>>>,
}

impl Notifications {
    /// Connects to the session bus and starts listening for clicks, which
    /// become [`Event::OpenShot`].
    pub fn spawn(events: EventSink) -> Result<Notifications> {
        let conn = Connection::session().context("connecting to the session bus")?;
        let pending: Arc<Mutex<HashMap<u32, u64>>> = Arc::default();
        let notifier = Notifications {
            conn: conn.clone(),
            pending: pending.clone(),
        };
        let proxy = Proxy::new(&conn, DEST, PATH, DEST)?;
        let invoked = proxy.receive_signal("ActionInvoked")?;
        let closed = proxy.receive_signal("NotificationClosed")?;
        std::thread::Builder::new()
            .name("quikscope-notify-closed".into())
            .spawn({
                let pending = pending.clone();
                move || {
                    for msg in closed {
                        if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                            pending.lock().unwrap().remove(&id);
                        }
                    }
                }
            })?;
        std::thread::Builder::new()
            .name("quikscope-notify".into())
            .spawn(move || {
                for msg in invoked {
                    let Ok((id, key)) = msg.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    if key != DEFAULT_ACTION && key != EDIT_ACTION {
                        continue;
                    }
                    let shot = pending.lock().unwrap().get(&id).copied();
                    if let Some(shot) = shot {
                        events(Event::OpenShot(shot));
                    }
                }
            })?;
        Ok(notifier)
    }
}

impl Notifier for Notifications {
    fn shot_captured(&self, shot: u64, image: &Image, body: &str, timeout_ms: i32) -> Result<()> {
        let thumb = image.thumbnail(256);
        let image_data = StructureBuilder::new()
            .add_field(thumb.width as i32)
            .add_field(thumb.height as i32)
            .add_field(thumb.width as i32 * 4)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(thumb.to_rgba())
            .build()?;
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("image-data", Value::Structure(image_data));
        hints.insert("desktop-entry", Value::from("quikscope"));
        hints.insert("transient", Value::from(true));
        let actions = [DEFAULT_ACTION, "Open in editor", EDIT_ACTION, "Edit"];
        let reply = self.conn.call_method(
            Some(DEST),
            PATH,
            Some(DEST),
            "Notify",
            &(
                "quikscope",
                0u32,
                "camera-photo",
                "Screenshot captured",
                body,
                &actions[..],
                hints,
                timeout_ms,
            ),
        )?;
        let id: u32 = reply.body().deserialize()?;
        self.pending.lock().unwrap().insert(id, shot);
        Ok(())
    }

    fn message(&self, summary: &str, body: &str) -> Result<()> {
        let hints: HashMap<&str, Value> = HashMap::new();
        let actions: [&str; 0] = [];
        self.conn.call_method(
            Some(DEST),
            PATH,
            Some(DEST),
            "Notify",
            &(
                "quikscope",
                0u32,
                "camera-photo",
                summary,
                body,
                &actions[..],
                hints,
                5000i32,
            ),
        )?;
        Ok(())
    }
}
