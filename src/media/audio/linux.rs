//! PipeWire-pulse and PulseAudio share this native source/capture API.

use crate::media::{
    AudioChunk, AudioDevice, AudioSource, AudioSpec, DEFAULT_INPUT, DEFAULT_OUTPUT,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use libpulse_binding as pulse;
use pulse::context::Context;
use pulse::mainloop::standard::Mainloop;
use pulse::stream::{PeekResult, Stream};
use std::{
    cell::RefCell,
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
        *received.borrow_mut() = (
            info.default_source_name.as_ref().map(|s| s.to_string()),
            info.default_sink_name
                .as_ref()
                .map(|s| format!("{s}.monitor")),
        );
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

/// Opens capture of device `id` (or a default). Once open, the device
/// going away (unplugged, sink removed) ends the stream: `read` returns an
/// error and the source delivers nothing more. The stream never moves to
/// another device on its own.
pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let sources = sources()?;
    let id = match id {
        DEFAULT_INPUT => sources
            .input
            .as_deref()
            .context("PulseAudio has no default input")?,
        DEFAULT_OUTPUT => sources
            .output
            .as_deref()
            .context("PulseAudio has no default output")?,
        id => id,
    };
    let source = sources
        .sources
        .iter()
        .find(|s| s.device.id == id)
        .with_context(|| format!("PulseAudio source {id:?} is unavailable"))?;
    let spec = AudioSpec {
        rate: RATE,
        channels: source.channels as u16,
    };
    let device = id.to_string();
    let (chunks, receiver) = mpsc::sync_channel(32);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    thread::Builder::new()
        .name("quikscope-pulse".into())
        .spawn(move || {
            if let Err(e) = capture(&device, spec, &stopped, &ready_tx, &chunks) {
                let text = format!("{e:#}");
                let _ = ready_tx.try_send(Err(anyhow!(text.clone())));
                let _ = chunks.try_send(Err(anyhow!(text)));
            }
        })?;
    let capture = Capture {
        spec,
        receiver,
        stop,
    };
    ready
        .recv_timeout(TIMEOUT + Duration::from_secs(1))
        .map_err(|_| anyhow!("PulseAudio did not open source {id} in time"))??;
    Ok(Box::new(capture))
}

/// The capture thread: records `device` until `stop`, sending each fragment
/// with the capture time of its first sample.
fn capture(
    device: &str,
    spec: AudioSpec,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<()>>,
    chunks: &mpsc::SyncSender<Result<AudioChunk>>,
) -> Result<()> {
    let (mut mainloop, mut context) = connect()?;
    let sample = pulse::sample::Spec {
        format: pulse::sample::Format::F32le,
        channels: spec.channels as u8,
        rate: spec.rate,
    };
    ensure!(
        sample.is_valid(),
        "Unsupported PulseAudio source channel count"
    );
    let mut stream = Stream::new(&mut context, "Screen recording", &sample, None)
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
    // recording another one. The timing flags keep `get_latency` current
    // without round trips.
    use pulse::stream::FlagSet as F;
    stream
        .connect_record(
            Some(device),
            Some(&attributes),
            F::DONT_MOVE | F::ADJUST_LATENCY | F::AUTO_TIMING_UPDATE | F::INTERPOLATE_TIMING,
        )
        .map_err(|e| anyhow!("Opening PulseAudio source {device}: {e}"))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match stream.get_state() {
            pulse::stream::State::Ready => break,
            pulse::stream::State::Failed | pulse::stream::State::Terminated => {
                bail!("Opening PulseAudio source {device}: {}", context.errno())
            }
            _ => iterate(&mut mainloop, deadline)?,
        }
    }
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Acquire) {
        wait(&mut mainloop, Duration::from_millis(50))?;
        match stream.get_state() {
            pulse::stream::State::Ready => {}
            pulse::stream::State::Failed | pulse::stream::State::Terminated => {
                bail!(
                    "PulseAudio source {device} disconnected: {}",
                    context.errno()
                )
            }
            _ => continue,
        }
        loop {
            // The age of the next unread sample, i.e. the fragment's first.
            let latency = stream.get_latency();
            let samples: Vec<f32> = match stream
                .peek()
                .map_err(|e| anyhow!("Reading PulseAudio source {device}: {e}"))?
            {
                PeekResult::Empty => break,
                // Samples the server lost: silence keeps the sample count
                // equal to elapsed time.
                PeekResult::Hole(bytes) => vec![0.0; bytes / 4],
                PeekResult::Data(bytes) => bytes
                    .chunks_exact(4)
                    .map(|s| f32::from_le_bytes(s.try_into().unwrap()))
                    .collect(),
            };
            stream
                .discard()
                .map_err(|e| anyhow!("Reading PulseAudio source {device}: {e}"))?;
            let now = Instant::now();
            let duration = Duration::from_secs_f64(
                samples.len() as f64 / f64::from(spec.channels) / f64::from(spec.rate),
            );
            let captured = now.checked_sub(age(latency, duration)).unwrap_or(now);
            if chunks.send(Ok(AudioChunk { samples, captured })).is_err() {
                return Ok(());
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
}
