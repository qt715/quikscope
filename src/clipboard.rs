//! Clipboard ownership through `wlr-data-control`.
//!
//! The daemon has no focused surface, so the core `wl_data_device` protocol
//! (which needs an input serial) is unusable; data-control lets it own the
//! selection for as long as it runs, like `wl-copy` does, without forking.
//!
//! PNG encoding starts the moment an image is offered but the selection is
//! claimed immediately: a paste that arrives before the encoder finishes
//! simply waits for it.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, anyhow};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use crate::raster::{Compression, Image};

const PNG_MIME: &str = "image/png";

/// Encoded PNG that may still be in progress.
pub struct PendingPng(OnceLock<Result<Vec<u8>, String>>);

impl PendingPng {
    /// Starts encoding on a worker thread and returns immediately.
    pub fn encode(image: Arc<Image>) -> Arc<PendingPng> {
        let png = Arc::new(PendingPng(OnceLock::new()));
        let out = png.clone();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = image
                .encode_png(Compression::Fast)
                .map_err(|e| format!("{e:#}"));
            log::debug!(
                "clipboard PNG encoded in {:.1}ms",
                started.elapsed().as_secs_f64() * 1e3
            );
            let _ = out.0.set(result);
        });
        png
    }

    /// Blocks until the encoder finishes.
    pub fn wait(&self) -> Result<&[u8], &str> {
        self.0.wait().as_deref().map_err(|e| e.as_str())
    }
}

/// Handle to the clipboard thread.
#[derive(Clone)]
pub struct Clipboard {
    tx: mpsc::Sender<Arc<PendingPng>>,
    wake: Arc<OwnedFd>,
}

impl Clipboard {
    pub fn spawn() -> Result<Clipboard> {
        let (tx, rx) = mpsc::channel();
        let (wake_rx, wake_tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("moment-clipboard".into())
            .spawn(move || match Owner::connect() {
                Ok(mut owner) => {
                    let _ = ready_tx.send(Ok(()));
                    if let Err(e) = owner.run(rx, wake_rx) {
                        log::error!("clipboard thread stopped: {e:#}");
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("clipboard thread exited during startup"))??;
        Ok(Clipboard {
            tx,
            wake: Arc::new(wake_tx),
        })
    }

    /// Offers `image` as `image/png` and takes the selection.
    pub fn set_image(&self, image: Arc<Image>) -> Arc<PendingPng> {
        let png = PendingPng::encode(image);
        if self.tx.send(png.clone()).is_ok() {
            let _ = rustix::io::write(&*self.wake, &[1]);
        }
        png
    }
}

struct State {
    manager: zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    current: Option<(
        zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        Arc<PendingPng>,
    )>,
    offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
}

struct Owner {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    device: zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
}

impl Owner {
    fn connect() -> Result<Owner> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let manager = globals
            .bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
            .context("compositor does not support wlr-data-control")?;
        let seat = globals
            .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ())
            .context("compositor has no seat")?;
        let device = manager.get_data_device(&seat, &qh, ());
        let mut state = State {
            manager,
            current: None,
            offer: None,
        };
        queue.roundtrip(&mut state)?;
        Ok(Owner {
            conn,
            queue,
            state,
            device,
        })
    }

    fn run(&mut self, rx: mpsc::Receiver<Arc<PendingPng>>, wake: OwnedFd) -> Result<()> {
        use rustix::event::{PollFd, PollFlags, poll};
        let qh = self.queue.handle();
        loop {
            loop {
                match rx.try_recv() {
                    Ok(png) => self.offer(png, &qh),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            self.queue.flush()?;
            self.queue.dispatch_pending(&mut self.state)?;
            let Some(guard) = self.conn.prepare_read() else {
                continue;
            };
            let (ready_wl, ready_wake) = {
                let conn_fd = guard.connection_fd();
                let mut fds = [
                    PollFd::new(&conn_fd, PollFlags::IN),
                    PollFd::new(&wake, PollFlags::IN),
                ];
                poll(&mut fds, None)?;
                (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
            };
            if ready_wl {
                guard.read()?;
            } else {
                drop(guard);
            }
            if ready_wake {
                let mut buf = [0u8; 64];
                let _ = rustix::io::read(&wake, &mut buf);
            }
            self.queue.dispatch_pending(&mut self.state)?;
        }
    }

    fn offer(&mut self, png: Arc<PendingPng>, qh: &QueueHandle<State>) {
        let source = self.state.manager.create_data_source(qh, ());
        source.offer(PNG_MIME.into());
        self.device.set_selection(Some(&source));
        if let Some((old, _)) = self.state.current.replace((source, png)) {
            old.destroy();
        }
    }
}

fn write_to(fd: OwnedFd, png: Arc<PendingPng>) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        match png.wait() {
            Ok(bytes) => {
                // A reader that hangs up early (EPIPE) is not an error worth logging.
                let _ = file.write_all(bytes);
            }
            Err(e) => log::error!("clipboard PNG encoding failed: {e}"),
        }
    });
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_source_v1::Event;
        match event {
            Event::Send { mime_type, fd } => {
                let png = state
                    .current
                    .as_ref()
                    .filter(|(s, _)| s == source)
                    .map(|(_, p)| p.clone());
                match png {
                    Some(png) if mime_type == PNG_MIME => write_to(fd, png),
                    _ => drop(fd),
                }
            }
            Event::Cancelled => {
                if state.current.as_ref().is_some_and(|(s, _)| s == source) {
                    state.current = None;
                }
                source.destroy();
            }
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
        use zwlr_data_control_device_v1::Event;
        match event {
            // Offers describe other clients' selections; this thread never
            // reads them, so each is released as soon as it is superseded.
            Event::Selection { id } | Event::PrimarySelection { id } => {
                if let Some(old) = state.offer.take() {
                    old.destroy();
                }
                state.offer = id;
            }
            Event::Finished => log::warn!("clipboard device finished"),
            _ => {}
        }
    }

    event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
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

wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
wayland_client::delegate_noop!(State: ignore zwlr_data_control_offer_v1::ZwlrDataControlOfferV1);

// Keep `Proxy` in scope for `==` on proxies and `.destroy()` helpers.
const _: fn(&zwlr_data_control_source_v1::ZwlrDataControlSourceV1) -> u32 = Proxy::version;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_png_waits_for_the_encoder() {
        let img = Arc::new(Image::filled(64, 32, [1, 2, 3, 255]));
        let png = PendingPng::encode(img.clone());
        let bytes = png.wait().unwrap();
        assert_eq!(&Image::decode(bytes).unwrap(), img.as_ref());
    }
}
