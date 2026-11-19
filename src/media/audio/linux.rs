//! PipeWire-pulse and PulseAudio share this native source/capture API.
//! A named device that goes away ends its own track; the default ids follow
//! the server's current default sink or source.

use crate::media::{
    AudioChunk, AudioDevice, AudioSource, AudioSpec, DEFAULT_INPUT, DEFAULT_OUTPUT,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use libpulse_binding as pulse;
use pulse::context::Context;
use pulse::mainloop::standard::Mainloop;
use pulse::stream::{PeekResult, Stream};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// How long connecting to the server, listing devices or opening a stream
/// may take.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Capture runs at this rate whatever the device's; the server resamples.
const RATE: u32 = 48_000;
/// Pause between attempts to reach a default device after losing it.
const REOPEN: Duration = Duration::from_millis(500);

struct Source {
    device: AudioDevice,
    channels: u8,
}
struct Sources {
    sources: Vec<Source>,
    input: Option<String>,
    output: Option<String>,
}

/// Runs `mainloop` once, waiting at most until `deadline` for events.
fn iterate(mainloop: &mut Mainloop, deadline: Instant) -> Result<()> {
    let left = deadline.saturating_duration_since(Instant::now());
    ensure!(!left.is_zero(), "PulseAudio/PipeWire-pulse timed out");
    wait(mainloop, left)
}

/// Waits up to `timeout` for events and dispatches them.
fn wait(mainloop: &mut Mainloop, timeout: Duration) -> Result<()> {
    let micros = pulse::time::MicroSeconds(timeout.as_micros().min(i32::MAX as u128) as u64);
    mainloop
        .prepare(Some(micros))
        .and_then(|()| mainloop.poll())
        .and_then(|_| mainloop.dispatch())
        .map_err(|e| anyhow!("PulseAudio mainloop: {e}"))?;
    Ok(())
}

/// A ready connection to the server.
fn connect() -> Result<(Mainloop, Context)> {
    let mut mainloop = Mainloop::new().context("Creating PulseAudio mainloop")?;
    let mut context =
        Context::new(&mainloop, "quikscope").context("Creating PulseAudio context")?;
    context
        .connect(None, pulse::context::FlagSet::NOAUTOSPAWN, None)
        .map_err(|e| anyhow!("Connecting to PulseAudio/PipeWire-pulse: {e}"))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match context.get_state() {
            pulse::context::State::Ready => return Ok((mainloop, context)),
            pulse::context::State::Failed | pulse::context::State::Terminated => bail!(
                "PulseAudio/PipeWire-pulse is unavailable: {}",
                context.errno()
            ),
            _ => {}
        }
        iterate(&mut mainloop, deadline)?;
    }
}

fn sources() -> Result<Sources> {
    let (mut mainloop, mut context) = connect()?;
    let deadline = Instant::now() + TIMEOUT;
    let devices = Rc::new(RefCell::new(Vec::new()));
    let error = Rc::new(RefCell::new(false));
    let found = devices.clone();
    let failed = error.clone();
    let operation = context
        .introspect()
        .get_source_info_list(move |result| match result {
            pulse::callbacks::ListResult::Item(source) => {
                if let Some(name) = &source.name {
                    found.borrow_mut().push(Source {
                        device: AudioDevice {
                            id: name.to_string(),
                            name: source
                                .description
                                .as_ref()
                                .map_or_else(|| name.to_string(), |s| s.to_string()),
                            output: source.monitor_of_sink.is_some(),
                        },
                        channels: source.sample_spec.channels,
                    });
                }
            }
            pulse::callbacks::ListResult::Error => *failed.borrow_mut() = true,
            pulse::callbacks::ListResult::End => {}
        });
    while operation.get_state() == pulse::operation::State::Running {
        iterate(&mut mainloop, deadline)?;
    }
    ensure!(!*error.borrow(), "PulseAudio source enumeration failed");
    let defaults = Rc::new(RefCell::new((None, None)));
    let received = defaults.clone();
    let operation = context.introspect().get_server_info(move |info| {
        *received.borrow_mut() = (default_device(info, false), default_device(info, true));
    });
    while operation.get_state() == pulse::operation::State::Running {
        iterate(&mut mainloop, deadline)?;
    }
    context.disconnect();
    let (input, output) = std::mem::take(&mut *defaults.borrow_mut());
    let sources = std::mem::take(&mut *devices.borrow_mut());
    Ok(Sources {
        sources,
        input,
        output,
    })
}

pub(super) fn devices() -> Result<Vec<AudioDevice>> {
    let sources = sources()?;
    let mut devices = vec![
        AudioDevice {
            id: DEFAULT_OUTPUT.into(),
            name: "Default desktop audio".into(),
            output: true,
        },
        AudioDevice {
            id: DEFAULT_INPUT.into(),
            name: "Default microphone".into(),
            output: false,
        },
    ];
    devices.extend(sources.sources.into_iter().map(|s| s.device));
    Ok(devices)
}

struct Capture {
    spec: AudioSpec,
    receiver: mpsc::Receiver<Result<AudioChunk>>,
    stop: Arc<AtomicBool>,
}
impl AudioSource for Capture {
    fn spec(&self) -> AudioSpec {
        self.spec
    }
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        match self.receiver.recv_timeout(timeout) {
            Ok(chunk) => chunk.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("PulseAudio recording ended"),
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // The worker notices within one poll interval; dropping never waits
        // for an unresponsive server.
        self.stop.store(true, Ordering::Release);
    }
}
/// The source the server's default sink (`output`) or default source stands
/// for.
fn default_device(
    info: &pulse::context::introspect::ServerInfo<'_>,
    output: bool,
) -> Option<String> {
    if output {
        info.default_sink_name
            .as_ref()
            .map(|sink| format!("{sink}.monitor"))
    } else {
        info.default_source_name.as_ref().map(|s| s.to_string())
    }
}

/// The device a default-following capture switches to, given the one it
/// records (`None` after losing it) and the server's current default.
fn switch_to<'a>(current: Option<&str>, default: Option<&'a str>) -> Option<&'a str> {
    default.filter(|default| current != Some(*default))
}

/// Opens capture of device `id`. A named device going away (unplugged,
/// sink removed) ends the stream: `read` returns an error and the source
/// delivers nothing more. The default ids instead move to the server's new
/// default whenever it changes, and wait for one while there is none.
pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let sources = sources()?;
    let follow = match id {
        DEFAULT_OUTPUT => Some(true),
        DEFAULT_INPUT => Some(false),
        _ => None,
    };
    let device = match follow {
        Some(true) => sources
            .output
            .as_deref()
            .context("PulseAudio has no default output")?,
        Some(false) => sources
            .input
            .as_deref()
            .context("PulseAudio has no default input")?,
        None => id,
    };
    let source = sources
        .sources
        .iter()
        .find(|s| s.device.id == device)
        .with_context(|| format!("PulseAudio source {device:?} is unavailable"))?;
    // A later default may have another channel count; the server remaps
    // it to this spec.
    let spec = AudioSpec {
        rate: RATE,
        channels: source.channels as u16,
    };
    let device = device.to_string();
    let (chunks, receiver) = mpsc::sync_channel(32);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let name = device.clone();
    thread::Builder::new()
        .name("quikscope-pulse".into())
        .spawn(move || {
            if let Err(e) = capture(name, follow, spec, &stopped, &ready_tx, &chunks) {
                let text = format!("{e:#}");
                let _ = ready_tx.try_send(Err(anyhow!(text.clone())));
                // Blocks at most until the source is dropped, which drops
                // the receiver: dropping never waits for this thread.
                let _ = chunks.send(Err(anyhow!(text)));
            }
        })?;
    let capture = Capture {
        spec,
        receiver,
        stop,
    };
    ready
        .recv_timeout(TIMEOUT + Duration::from_secs(1))
        .map_err(|_| anyhow!("PulseAudio did not open source {device} in time"))??;
    Ok(Box::new(capture))
}

/// One record stream of one device.
struct Recording {
    device: String,
    stream: Stream,
}

impl Recording {
    /// Connects a stream to `device` and waits until it records.
    fn open(
        mainloop: &mut Mainloop,
        context: &mut Context,
        device: &str,
        spec: AudioSpec,
    ) -> Result<Self> {
        let sample = pulse::sample::Spec {
            format: pulse::sample::Format::F32le,
            channels: spec.channels as u8,
            rate: spec.rate,
        };
        ensure!(
            sample.is_valid(),
            "Unsupported PulseAudio source channel count"
        );
        let mut stream = Stream::new(context, "Screen recording", &sample, None)
            .context("Creating PulseAudio stream")?;
        let frame_bytes = spec.channels as u32 * 4;
        let attributes = pulse::def::BufferAttr {
            // Up to a second may queue on the server while this thread is late.
            maxlength: spec.rate * frame_bytes,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: spec.rate / 100 * frame_bytes,
        };
        // DONT_MOVE: a vanished device fails the stream instead of silently
        // recording another one (a default capture picks the next device
        // itself). The timing flags keep `get_latency` current without
        // round trips.
        use pulse::stream::FlagSet as F;
        stream
            .connect_record(
                Some(device),
                Some(&attributes),
                F::DONT_MOVE | F::ADJUST_LATENCY | F::AUTO_TIMING_UPDATE | F::INTERPOLATE_TIMING,
            )
            .map_err(|e| anyhow!("Opening PulseAudio source {device}: {e}"))?;
        let recording = Recording {
            device: device.to_string(),
            stream,
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match recording.stream.get_state() {
                pulse::stream::State::Ready => return Ok(recording),
                pulse::stream::State::Failed | pulse::stream::State::Terminated => {
                    bail!("Opening PulseAudio source {device}: {}", context.errno())
                }
                _ => iterate(mainloop, deadline)?,
            }
        }
    }

    /// Sends every fragment read so far with the capture time of its first
    /// sample; `false` once the receiver is gone.
    fn drain(
        &mut self,
        context: &Context,
        spec: AudioSpec,
        chunks: &mpsc::SyncSender<Result<AudioChunk>>,
    ) -> Result<bool> {
        let device = &self.device;
        match self.stream.get_state() {
            pulse::stream::State::Ready => {}
            pulse::stream::State::Failed | pulse::stream::State::Terminated => {
                bail!(
                    "PulseAudio source {device} disconnected: {}",
                    context.errno()
                )
            }
            _ => return Ok(true),
        }
        loop {
            // The age of the next unread sample, i.e. the fragment's first.
            let latency = self.stream.get_latency();
            let samples: Vec<f32> = match self
                .stream
                .peek()
                .map_err(|e| anyhow!("Reading PulseAudio source {device}: {e}"))?
            {
                PeekResult::Empty => return Ok(true),
                // Samples the server lost: silence keeps the sample count
                // equal to elapsed time.
                PeekResult::Hole(bytes) => vec![0.0; bytes / 4],
                PeekResult::Data(bytes) => bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&s| f32::from_le_bytes(s))
                    .collect(),
            };
            self.stream
                .discard()
                .map_err(|e| anyhow!("Reading PulseAudio source {device}: {e}"))?;
            let now = Instant::now();
            let duration = Duration::from_secs_f64(
                samples.len() as f64 / f64::from(spec.channels) / f64::from(spec.rate),
            );
            let captured = now.checked_sub(age(latency, duration)).unwrap_or(now);
            if chunks.send(Ok(AudioChunk { samples, captured })).is_err() {
                return Ok(false);
            }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
    }
}

/// The capture thread: records `device` until `stop`. With `follow` (the
/// default sink's monitor when `true`, the default source when `false`), it
/// moves to the server's new default whenever that changes.
fn capture(
    device: String,
    follow: Option<bool>,
    spec: AudioSpec,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<()>>,
    chunks: &mpsc::SyncSender<Result<AudioChunk>>,
) -> Result<()> {
    let (mut mainloop, mut context) = connect()?;
    // Set by the server's change events: the defaults may have moved.
    let changed = Rc::new(Cell::new(false));
    let _subscription = follow.map(|_| {
        use pulse::context::subscribe::{Facility, InterestMaskSet};
        let flag = changed.clone();
        context.set_subscribe_callback(Some(Box::new(move |facility, _, _| {
            if facility == Some(Facility::Server) {
                flag.set(true);
            }
        })));
        context.subscribe(InterestMaskSet::SERVER, |_| {})
    });
    let mut recording = Some(Recording::open(&mut mainloop, &mut context, &device, spec)?);
    let _ = ready.send(Ok(()));
    // The pending server-info query and its answer, the current default.
    let mut query = None;
    let answer = Rc::new(RefCell::new(None::<String>));
    let mut asked = Instant::now();
    while !stop.load(Ordering::Acquire) {
        wait(&mut mainloop, Duration::from_millis(50))?;
        if let Some(open) = &mut recording {
            match open.drain(&context, spec, chunks) {
                Ok(true) => {}
                Ok(false) => return Ok(()), // the recording dropped its source
                Err(error) if follow.is_some() => {
                    log::warn!("default audio device lost: {error:#}");
                    recording = None;
                }
                Err(error) => return Err(error),
            }
        }
        let Some(output) = follow else {
            continue;
        };
        if query.is_none()
            && (changed.replace(false) || (recording.is_none() && asked.elapsed() >= REOPEN))
        {
            asked = Instant::now();
            let answer = answer.clone();
            query = Some(context.introspect().get_server_info(move |info| {
                *answer.borrow_mut() = default_device(info, output);
            }));
        }
        if query
            .as_ref()
            .is_some_and(|q| q.get_state() != pulse::operation::State::Running)
        {
            query = None;
            let default = answer.borrow_mut().take();
            let current = recording.as_ref().map(|r| r.device.as_str());
            if let Some(next) = switch_to(current, default.as_deref()) {
                match Recording::open(&mut mainloop, &mut context, next, spec) {
                    Ok(open) => {
                        log::info!("default audio device is now {next}");
                        recording = Some(open);
                    }
                    Err(error) if recording.is_none() => {
                        log::debug!("default audio device {next} unavailable: {error:#}");
                    }
                    Err(error) => {
                        log::warn!("cannot follow the default audio device: {error:#}");
                    }
                }
            }
        }
    }
    Ok(())
}

/// How long before now the first sample of a just-read fragment of
/// `duration` was captured, from the stream's latency (which may not be
/// known before the server's first timing update).
fn age(
    latency: Result<pulse::stream::Latency, pulse::error::PAErr>,
    duration: Duration,
) -> Duration {
    match latency {
        Ok(pulse::stream::Latency::Positive(us)) => Duration::from_micros(us.0),
        // Before any timing information, the fragment is at least as old as
        // its own length.
        Err(_) => duration,
        Ok(_) => Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse::stream::Latency;

    #[test]
    fn fragment_age_comes_from_latency() {
        let ten = Duration::from_millis(10);
        assert_eq!(
            age(
                Ok(Latency::Positive(pulse::time::MicroSeconds(25_000))),
                ten
            ),
            Duration::from_millis(25)
        );
        assert_eq!(age(Ok(Latency::None), ten), Duration::ZERO);
        assert_eq!(
            age(Ok(Latency::Negative(pulse::time::MicroSeconds(5))), ten),
            Duration::ZERO
        );
        assert_eq!(age(Err(pulse::error::PAErr(-1)), ten), ten);
    }

    #[test]
    fn defaults_name_the_sink_monitor_or_the_source() {
        let info = pulse::context::introspect::ServerInfo {
            user_name: None,
            host_name: None,
            server_version: None,
            server_name: None,
            sample_spec: pulse::sample::Spec {
                format: pulse::sample::Format::F32le,
                channels: 2,
                rate: RATE,
            },
            default_sink_name: Some("headset".into()),
            default_source_name: Some("mic".into()),
            cookie: 0,
            channel_map: Default::default(),
        };
        assert_eq!(
            default_device(&info, true).as_deref(),
            Some("headset.monitor")
        );
        assert_eq!(default_device(&info, false).as_deref(), Some("mic"));
        let none = pulse::context::introspect::ServerInfo {
            default_sink_name: None,
            default_source_name: None,
            ..info
        };
        assert_eq!(default_device(&none, true), None);
        assert_eq!(default_device(&none, false), None);
    }

    #[test]
    fn default_capture_moves_only_to_a_new_default() {
        // Unchanged default: keep recording.
        assert_eq!(switch_to(Some("a.monitor"), Some("a.monitor")), None);
        // The user picked another output: move there.
        assert_eq!(
            switch_to(Some("a.monitor"), Some("b.monitor")),
            Some("b.monitor")
        );
        // The default vanished with no replacement: keep what still records.
        assert_eq!(switch_to(Some("a.monitor"), None), None);
        // Lost the device: reopen the default, even the same name (it may
        // be back), and wait while there is none.
        assert_eq!(switch_to(None, Some("a.monitor")), Some("a.monitor"));
        assert_eq!(switch_to(None, None), None);
    }

    /// Requires pipewire, pipewire-pulse, WirePlumber and pactl. The services
    /// get a private runtime, state and bus. Run alone: it points this
    /// process's PulseAudio client at the private server.
    #[test]
    #[ignore = "requires private PipeWire services; run alone"]
    fn private_default_output_follows_the_default_sink() {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let wait_for = |what: &str, mut ready: Box<dyn FnMut() -> bool + '_>| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(20));
            }
        };
        let home = tempfile::tempdir().unwrap();
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        std::io::BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let _bus = Child(bus);
        let spawn = |name: &str| {
            Child(
                Command::new(name)
                    .env("HOME", home.path())
                    .env("XDG_RUNTIME_DIR", home.path())
                    .env("PIPEWIRE_RUNTIME_DIR", home.path())
                    .env("XDG_STATE_HOME", home.path().join("state"))
                    .env("XDG_CONFIG_HOME", home.path().join("config"))
                    .env("DBUS_SESSION_BUS_ADDRESS", address.trim())
                    // No gvfs activation on the private bus.
                    .env("GIO_USE_VFS", "local")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        };
        let _pipewire = spawn("pipewire");
        wait_for(
            "pipewire",
            Box::new(|| home.path().join("pipewire-0").exists()),
        );
        let _manager = spawn("wireplumber");
        let _pulse = spawn("pipewire-pulse");
        let socket = home.path().join("pulse/native");
        wait_for("pipewire-pulse", Box::new(|| socket.exists()));
        let server = format!("unix:{}", socket.display());
        // SAFETY: this ignored test runs alone; no other thread reads the
        // environment meanwhile.
        unsafe { std::env::set_var("PULSE_SERVER", &server) };
        let run = |args: &[&str]| -> Option<String> {
            let output = Command::new("pactl")
                .args(args)
                .env("PULSE_SERVER", &server)
                .stderr(Stdio::null())
                .output()
                .unwrap();
            output
                .status
                .success()
                .then(|| String::from_utf8(output.stdout).unwrap())
        };
        let pactl = |args: &[&str]| -> String {
            run(args).unwrap_or_else(|| panic!("pactl {args:?} failed"))
        };
        let json = |what: &str| -> Vec<serde_json::Value> {
            serde_json::from_str(&pactl(&["-f", "json", "list", what])).unwrap()
        };
        let modules: Vec<String> = ["qa", "qb"]
            .iter()
            .map(|name| {
                pactl(&[
                    "load-module",
                    "module-null-sink",
                    &format!("sink_name={name}"),
                ])
                .trim()
                .to_owned()
            })
            .collect();
        // The sinks appear once WirePlumber has handled them.
        wait_for(
            "the test sinks",
            Box::new(|| run(&["set-default-sink", "qa"]).is_some()),
        );
        wait_for(
            "the default sink",
            Box::new(|| sources().is_ok_and(|s| s.output.as_deref() == Some("qa.monitor"))),
        );
        let mut source = open(DEFAULT_OUTPUT).unwrap();
        // The source our stream records, by name.
        let recorded = || -> Option<String> {
            let index = json("source-outputs").first()?["source"].as_u64()?;
            json("sources")
                .iter()
                .find(|s| s["index"].as_u64() == Some(index))
                .and_then(|s| s["name"].as_str().map(str::to_owned))
        };
        // Keeps reading (the capture thread blocks on a full queue) until
        // the stream records `name`.
        let mut follows = |name: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while recorded().as_deref() != Some(name) {
                assert!(
                    Instant::now() < deadline,
                    "still recording {:?}, not {name}",
                    recorded()
                );
                source.read(Duration::from_millis(50)).unwrap();
            }
            let mut chunks = 0;
            while chunks < 5 {
                chunks += usize::from(source.read(Duration::from_secs(1)).unwrap().is_some());
            }
        };
        follows("qa.monitor");
        pactl(&["set-default-sink", "qb"]);
        follows("qb.monitor");
        // The recorded default sink goes away: the stream fails, and the
        // capture moves to the new default instead of ending.
        pactl(&["unload-module", &modules[1]]);
        follows("qa.monitor");
    }
}
