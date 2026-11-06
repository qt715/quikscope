//! Persistent X11 CLIPBOARD ownership, independent of the capture backend.
//!
//! PNG requests use the original `PendingPng` bytes. Encoding never blocks the
//! X event loop; INCR transfers retain their content even after ownership changes.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::clipboard::PendingPng;
use crate::platform::Clipboard;
use crate::raster::Image;

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CHUNK: usize = 256 * 1024;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        INCR,
        UTF8_STRING,
        TEXT,
        STRING,
        PNG: b"image/png",
        UTF8_MIME: b"text/plain;charset=utf-8",
        TEXT_MIME: b"text/plain",
    }
}

/// Owns CLIPBOARD on a dedicated connection to the current `DISPLAY`.
/// Dropping this handle ends ownership once outstanding encoder work completes.
pub struct SelectionOwner {
    sender: Sender,
}

#[derive(Clone)]
struct Sender {
    tx: mpsc::Sender<Command>,
    wake: Arc<UnixStream>,
}

impl Sender {
    fn send(&self, command: Command) -> Result<()> {
        self.tx
            .send(command)
            .map_err(|_| anyhow!("X11 clipboard thread stopped"))?;
        // A full nonblocking socket already has a wakeup queued.
        match (&*self.wake).write(&[1]) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

enum Command {
    Offer(Content, mpsc::SyncSender<Result<()>>),
    Encoded(Arc<PendingPng>, bool),
}

#[derive(Clone)]
enum Content {
    Png(Arc<PendingPng>, bool),
    Text(Arc<str>),
}

impl SelectionOwner {
    /// Connects using x11rb's normal DISPLAY/Xauthority handling (including
    /// remote displays); returns connection/setup errors before starting.
    pub fn spawn() -> Result<Self> {
        let owner = Owner::connect()?;
        let (tx, rx) = mpsc::channel();
        let (wake_rx, wake_tx) = UnixStream::pair()?;
        wake_rx.set_nonblocking(true)?;
        wake_tx.set_nonblocking(true)?;
        std::thread::Builder::new()
            .name("quickscope-x11-clipboard".into())
            .spawn(move || {
                if let Err(e) = owner.run(rx, wake_rx) {
                    log::error!("X11 clipboard thread stopped: {e:#}");
                }
            })?;
        Ok(Self {
            sender: Sender {
                tx,
                wake: Arc::new(wake_tx),
            },
        })
    }

    fn offer(&self, content: Content) {
        let (tx, rx) = mpsc::sync_channel(1);
        let result = self
            .sender
            .send(Command::Offer(content, tx))
            .and_then(|()| {
                rx.recv()
                    .map_err(|_| anyhow!("X11 clipboard thread stopped claiming selection"))?
            });
        if let Err(e) = result {
            log::error!("claiming X11 clipboard: {e:#}");
        }
    }
}

impl Clipboard for SelectionOwner {
    fn set_image(&self, image: Arc<Image>) -> Arc<PendingPng> {
        let png = PendingPng::encode(image);
        // Ownership is acknowledged by the server, not held up by PNG encoding.
        self.offer(Content::Png(png.clone(), false));
        let ready = png.clone();
        let sender = self.sender.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("quickscope-x11-png".into())
            .spawn(move || {
                let _ = ready.wait();
                if let Err(e) = sender.send(Command::Encoded(ready, true)) {
                    log::debug!("delivering X11 clipboard PNG: {e:#}");
                }
            })
        {
            log::error!("starting X11 clipboard PNG waiter: {e}");
            let _ = self.sender.send(Command::Encoded(png.clone(), false));
        }
        png
    }

    fn set_text(&self, text: &str) {
        self.offer(Content::Text(text.into()));
    }
}

#[derive(Clone)]
enum Bytes {
    Png(Arc<PendingPng>),
    Text(Arc<str>),
    Latin1(Arc<[u8]>),
}

impl Bytes {
    fn get(&self) -> Result<&[u8]> {
        match self {
            Self::Png(png) => png
                .wait()
                .map_err(|e| anyhow!("encoding clipboard PNG: {e}")),
            Self::Text(text) => Ok(text.as_bytes()),
            Self::Latin1(bytes) => Ok(bytes),
        }
    }
}

struct Transfer {
    bytes: Bytes,
    type_: Atom,
    offset: usize,
    touched: Instant,
}

struct Owner {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    content: Option<Content>,
    pending: Vec<(SelectionRequestEvent, Arc<PendingPng>)>,
    transfers: HashMap<(Window, Atom), Transfer>,
    chunk: usize,
}

impl Owner {
    fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connecting X11 clipboard to DISPLAY")?;
        let atoms = Atoms::new(&conn)?.reply()?;
        let window = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            conn.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        let chunk = conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .clamp(1, MAX_CHUNK);
        conn.flush()?;
        Ok(Self {
            conn,
            window,
            atoms,
            content: None,
            pending: Vec::new(),
            transfers: HashMap::new(),
            chunk,
        })
    }

    fn run(mut self, rx: mpsc::Receiver<Command>, mut wake: UnixStream) -> Result<()> {
        loop {
            let mut buf = [0; 256];
            loop {
                match wake.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            loop {
                match rx.try_recv() {
                    Ok(Command::Offer(content, ack)) => {
                        let result = self.claim(content);
                        let _ = ack.send(result);
                    }
                    Ok(Command::Encoded(png, available)) => self.encoded(png, available),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            while let Some(event) = self.conn.poll_for_event()? {
                if let Err(e) = self.event(event) {
                    // Requestors can disappear at any point; one bad client must
                    // not take down the clipboard connection.
                    log::warn!("handling X11 clipboard event: {e:#}");
                }
            }
            let expired: Vec<_> = self
                .transfers
                .iter()
                .filter(|(_, t)| t.touched.elapsed() >= TRANSFER_TIMEOUT)
                .map(|(key, _)| *key)
                .collect();
            for key in expired {
                self.finish_transfer(key);
            }
            self.conn.flush()?;
            let timeout = Timespec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            let mut fds = [
                PollFd::new(self.conn.stream(), PollFlags::IN),
                PollFd::new(&wake, PollFlags::IN),
            ];
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            if fds[0]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
            {
                return Err(anyhow!("X11 clipboard connection closed"));
            }
        }
    }

    fn claim(&mut self, content: Content) -> Result<()> {
        self.conn
            .set_selection_owner(self.window, self.atoms.CLIPBOARD, CURRENT_TIME)?
            .check()?;
        if self
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)?
            .reply()?
            .owner
            != self.window
        {
            return Err(anyhow!("X server refused CLIPBOARD ownership"));
        }
        self.content = Some(content);
        Ok(())
    }

    fn encoded(&mut self, png: Arc<PendingPng>, available: bool) {
        if let Some(Content::Png(current, ready)) = &mut self.content
            && Arc::ptr_eq(current, &png)
        {
            *ready = available;
            if !available {
                self.content = None;
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for (request, queued) in pending {
            if Arc::ptr_eq(&png, &queued) {
                let result = if available {
                    self.respond(request, Bytes::Png(queued), self.atoms.PNG)
                } else {
                    self.notify(request, NONE)
                };
                if let Err(e) = result {
                    log::warn!("serving pending X11 clipboard PNG: {e:#}");
                    let _ = self.notify(request, NONE);
                }
            } else {
                self.pending.push((request, queued));
            }
        }
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::SelectionRequest(request) if request.owner == self.window => {
                if let Err(e) = self.request(request) {
                    let _ = self.notify(request, NONE);
                    return Err(e);
                }
            }
            Event::SelectionClear(clear) if clear.selection == self.atoms.CLIPBOARD => {
                // Ignore a queued clear from before a later successful claim.
                if self
                    .conn
                    .get_selection_owner(self.atoms.CLIPBOARD)?
                    .reply()?
                    .owner
                    != self.window
                {
                    self.content = None;
                }
            }
            Event::PropertyNotify(property) if property.state == Property::DELETE => {
                self.advance((property.window, property.atom))?;
            }
            Event::DestroyNotify(destroy) => {
                self.transfers
                    .retain(|(window, _), _| *window != destroy.window);
                self.pending
                    .retain(|(request, _)| request.requestor != destroy.window);
            }
            Event::Error(error) => log::warn!("X11 clipboard protocol error: {error:?}"),
            _ => {}
        }
        Ok(())
    }

    fn request(&mut self, request: SelectionRequestEvent) -> Result<()> {
        if request.selection != self.atoms.CLIPBOARD {
            return self.notify(request, NONE);
        }
        let Some(content) = self.content.clone() else {
            return self.notify(request, NONE);
        };
        let property = request_property(&request);
        // Reusing a property during INCR cannot represent two simultaneous
        // transfers; refuse instead of corrupting the existing one.
        if self.transfers.contains_key(&(request.requestor, property)) {
            return self.notify(request, NONE);
        }
        if request.target == self.atoms.TARGETS {
            let targets = self.targets(&content);
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )?
                .check()?;
            return self.notify(request, property);
        }
        match content {
            Content::Png(png, ready) if request.target == self.atoms.PNG => {
                if ready {
                    self.respond(request, Bytes::Png(png), self.atoms.PNG)
                } else {
                    self.pending.push((request, png));
                    Ok(())
                }
            }
            Content::Text(text) if self.text_targets().contains(&request.target) => {
                if request.target == self.atoms.STRING {
                    // ICCCM STRING is ISO-8859-1, not UTF-8. Do not silently
                    // corrupt text that cannot be represented in that encoding.
                    let Some(bytes) = latin1(&text) else {
                        return self.notify(request, NONE);
                    };
                    self.respond(request, Bytes::Latin1(bytes.into()), self.atoms.STRING)
                } else {
                    let type_ = if request.target == self.atoms.TEXT {
                        self.atoms.UTF8_STRING
                    } else {
                        request.target
                    };
                    self.respond(request, Bytes::Text(text), type_)
                }
            }
            _ => self.notify(request, NONE),
        }
    }

    fn text_targets(&self) -> [Atom; 5] {
        [
            self.atoms.UTF8_STRING,
            self.atoms.UTF8_MIME,
            self.atoms.TEXT_MIME,
            self.atoms.TEXT,
            self.atoms.STRING,
        ]
    }

    fn targets(&self, content: &Content) -> Vec<Atom> {
        let mut result = vec![self.atoms.TARGETS];
        match content {
            Content::Png(..) => result.push(self.atoms.PNG),
            Content::Text(_) => result.extend(self.text_targets()),
        }
        result
    }

    fn notify(&self, request: SelectionRequestEvent, property: Atom) -> Result<()> {
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        self.conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)?
            .check()?;
        Ok(())
    }

    fn respond(&mut self, request: SelectionRequestEvent, bytes: Bytes, type_: Atom) -> Result<()> {
        let property = request_property(&request);
        let data = bytes.get()?;
        if data.len() <= self.chunk {
            self.conn
                .change_property8(PropMode::REPLACE, request.requestor, property, type_, data)?
                .check()?;
            return self.notify(request, property);
        }
        let length = u32::try_from(data.len()).context("clipboard exceeds X11 INCR size limit")?;
        // Event selection is per connection, so this does not replace the
        // requestor's own mask. Keep it until its last concurrent transfer ends.
        self.conn
            .change_window_attributes(
                request.requestor,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;
        let key = (request.requestor, property);
        self.transfers.insert(
            key,
            Transfer {
                bytes,
                type_,
                offset: 0,
                touched: Instant::now(),
            },
        );
        let result = (|| {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    self.atoms.INCR,
                    &[length],
                )?
                .check()?;
            self.notify(request, property)
        })();
        if result.is_err() {
            self.finish_transfer(key);
        }
        result
    }

    fn advance(&mut self, key: (Window, Atom)) -> Result<()> {
        let Some(transfer) = self.transfers.get_mut(&key) else {
            return Ok(());
        };
        let data = transfer.bytes.get()?;
        let (end, final_chunk) = chunk_end(transfer.offset, data.len(), self.chunk);
        let result = self
            .conn
            .change_property8(
                PropMode::REPLACE,
                key.0,
                key.1,
                transfer.type_,
                &data[transfer.offset..end],
            )?
            .check();
        if result.is_err() || final_chunk {
            self.finish_transfer(key);
        } else {
            transfer.offset = end;
            transfer.touched = Instant::now();
        }
        result.map_err(Into::into)
    }

    fn finish_transfer(&mut self, key: (Window, Atom)) {
        self.transfers.remove(&key);
        if !self.transfers.keys().any(|(window, _)| *window == key.0) {
            // Ignore BadWindow when a requestor vanished mid-transfer.
            if let Ok(cookie) = self.conn.change_window_attributes(
                key.0,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            ) && let Err(e) = cookie.check()
            {
                log::debug!("removing X11 clipboard requestor watch: {e}");
            }
        }
    }
}

fn request_property(request: &SelectionRequestEvent) -> Atom {
    if request.property == NONE {
        request.target
    } else {
        request.property
    }
}

/// An INCR transfer always sends a separate, final zero-length property.
fn chunk_end(offset: usize, len: usize, chunk: usize) -> (usize, bool) {
    (offset.saturating_add(chunk).min(len), offset == len)
}

fn latin1(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incr_sends_terminator_after_full_or_partial_chunk() {
        assert_eq!(chunk_end(0, 8, 4), (4, false));
        assert_eq!(chunk_end(4, 8, 4), (8, false));
        assert_eq!(chunk_end(8, 8, 4), (8, true));
        assert_eq!(chunk_end(4, 7, 4), (7, false));
        assert_eq!(chunk_end(7, 7, 4), (7, true));
        assert_eq!(chunk_end(0, 0, 4), (0, true));
    }

    #[test]
    fn string_is_latin1_not_utf8() {
        assert_eq!(latin1("café"), Some(vec![b'c', b'a', b'f', 0xe9]));
        assert_eq!(latin1("水"), None);
    }

    #[test]
    fn obsolete_request_without_property_uses_target() {
        let mut request = SelectionRequestEvent {
            target: 42,
            ..Default::default()
        };
        assert_eq!(request_property(&request), 42);
        request.property = 73;
        assert_eq!(request_property(&request), 73);
    }
}
