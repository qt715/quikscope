//! PipeWire-pulse and PulseAudio share this native source/capture API.

use std::{cell::RefCell, rc::Rc, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};
use anyhow::{Context as _, Result, bail, ensure};
use libpulse_binding as pulse;
use libpulse_simple_binding::Simple;
use crate::media::{AudioChunk, AudioDevice, AudioSource, AudioSpec, DEFAULT_INPUT, DEFAULT_OUTPUT};

struct Source { device: AudioDevice, channels: u8 }
struct Sources { sources: Vec<Source>, input: Option<String>, output: Option<String> }

fn sources() -> Result<Sources> {
    let mut mainloop = pulse::mainloop::standard::Mainloop::new().context("Creating PulseAudio mainloop")?;
    let mut context = pulse::context::Context::new(&mainloop, "quikscope").context("Creating PulseAudio context")?;
    context.connect(None, pulse::context::FlagSet::NOAUTOSPAWN, None)
        .map_err(|e| anyhow::anyhow!("Connecting to PulseAudio/PipeWire-pulse: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match context.get_state() {
            pulse::context::State::Ready => break,
            pulse::context::State::Failed | pulse::context::State::Terminated =>
                bail!("PulseAudio/PipeWire-pulse is unavailable: {}", context.errno()),
            _ => {}
        }
        iterate(&mut mainloop, deadline)?;
    }
    let devices = Rc::new(RefCell::new(Vec::new()));
    let error = Rc::new(RefCell::new(false));
    let found = devices.clone();
    let failed = error.clone();
    let operation = context.introspect().get_source_info_list(move |result| {
        match result {
            pulse::callbacks::ListResult::Item(source) => {
                if let Some(name) = &source.name {
                    found.borrow_mut().push(Source {
                        device: AudioDevice {
                            id: name.to_string(),
                            name: source.description.as_ref().map_or_else(|| name.to_string(), |s| s.to_string()),
                            output: source.monitor_of_sink.is_some(),
                        },
                        channels: source.sample_spec.channels,
                    });
                }
            }
            pulse::callbacks::ListResult::Error => *failed.borrow_mut() = true,
            pulse::callbacks::ListResult::End => {}
        }
    });
    while operation.get_state() == pulse::operation::State::Running { iterate(&mut mainloop, deadline)?; }
    ensure!(!*error.borrow(), "PulseAudio source enumeration failed");
    let defaults = Rc::new(RefCell::new((None, None)));
    let received = defaults.clone();
    let operation = context.introspect().get_server_info(move |info| {
        *received.borrow_mut() = (
            info.default_source_name.as_ref().map(|s| s.to_string()),
            info.default_sink_name.as_ref().map(|s| format!("{s}.monitor")),
        );
    });
    while operation.get_state() == pulse::operation::State::Running { iterate(&mut mainloop, deadline)?; }
    context.disconnect();
    let (input, output) = std::mem::take(&mut *defaults.borrow_mut());
    let sources = std::mem::take(&mut *devices.borrow_mut());
    Ok(Sources { sources, input, output })
}

fn iterate(mainloop: &mut pulse::mainloop::standard::Mainloop, deadline: Instant) -> Result<()> {
    ensure!(Instant::now() < deadline, "PulseAudio/PipeWire-pulse query timed out");
    match mainloop.iterate(false) {
        pulse::mainloop::standard::IterateResult::Success(_) => {}
        _ => bail!("PulseAudio mainloop terminated"),
    }
    thread::sleep(Duration::from_millis(1));
    Ok(())
}

pub(super) fn devices() -> Result<Vec<AudioDevice>> {
    let sources = sources()?;
    let mut devices = vec![
        AudioDevice { id: DEFAULT_OUTPUT.into(), name: "Default desktop audio".into(), output: true },
        AudioDevice { id: DEFAULT_INPUT.into(), name: "Default microphone".into(), output: false },
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
    fn spec(&self) -> AudioSpec { self.spec }
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        match self.receiver.recv_timeout(timeout) {
            Ok(chunk) => chunk.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("PulseAudio recording ended"),
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) { self.stop.store(true, Ordering::Release); }
}

pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let sources = sources()?;
    let id = match id {
        DEFAULT_INPUT => sources.input.as_deref().context("PulseAudio has no default input")?,
        DEFAULT_OUTPUT => sources.output.as_deref().context("PulseAudio has no default output")?,
        id => id,
    };
    let source = sources.sources.iter().find(|s| s.device.id == id)
        .with_context(|| format!("PulseAudio source {id:?} is unavailable"))?;
    let spec = AudioSpec { rate: 48000, channels: source.channels as u16 };
    let pulse_spec = pulse::sample::Spec { format: pulse::sample::Format::F32le, channels: source.channels, rate: spec.rate };
    ensure!(pulse_spec.is_valid(), "Unsupported PulseAudio source channel count");
    let attributes = pulse::def::BufferAttr {
        maxlength: spec.rate / 5 * spec.channels as u32 * 4,
        tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX,
        fragsize: spec.rate / 100 * spec.channels as u32 * 4,
    };
    let capture = Simple::new(None, "quikscope", pulse::stream::Direction::Record, Some(id),
        "Screen recording", &pulse_spec, None, Some(&attributes))
        .map_err(|e| anyhow::anyhow!("Opening PulseAudio source {id}: {e}"))?;
    let (send, receiver) = mpsc::sync_channel(16);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    thread::Builder::new().name("quikscope-pulse".into()).spawn(move || {
        let frames = spec.rate as usize / 100;
        let mut bytes = vec![0_u8; frames * spec.channels as usize * 4];
        while !stopped.load(Ordering::Acquire) {
            if let Err(error) = capture.read(&mut bytes) {
                let _ = send.try_send(Err(anyhow::anyhow!("PulseAudio capture failed: {error}")));
                break;
            }
            let duration = Duration::from_secs_f64(frames as f64 / spec.rate as f64);
            let latency = capture.get_latency().map_or(Duration::ZERO, |us| Duration::from_micros(us.0));
            let now = Instant::now();
            let captured = now.checked_sub(latency + duration).unwrap_or(now);
            let samples = bytes.chunks_exact(4).map(|s| f32::from_le_bytes(s.try_into().unwrap())).collect();
            if let Err(mpsc::TrySendError::Disconnected(_)) = send.try_send(Ok(AudioChunk { samples, captured })) { break; }
        }
        // pa_simple_read has no interrupt API. The worker owns its connection;
        // dropping the consumer never blocks the UI on an unresponsive server.
    })?;
    Ok(Box::new(Capture { spec, receiver, stop }))
}
