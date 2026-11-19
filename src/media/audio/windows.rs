//! WASAPI clients never leave their COM apartment. A bounded queue prevents
//! a paused or stalled encoder from retaining minutes of microphone data.
//! A lost endpoint (unplugged, disabled) only silences its own track while
//! it is retried; the default ids follow Windows' current default endpoint.
//! Every endpoint is captured as 48 kHz stereo float, the mixer's own
//! format: the shared-mode engine converts rate and channels (with the
//! device's real channel mask, so 5.1/7.1 headsets downmix correctly).
use crate::media::{
    AudioChunk, AudioDevice, AudioSource, AudioSpec, DEFAULT_INPUT, DEFAULT_OUTPUT,
};
use anyhow::{Context, Result, anyhow};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Media::{Audio::*, Multimedia::WAVE_FORMAT_IEEE_FLOAT},
        System::{Com::*, Performance::*},
    },
    core::{Interface, PCWSTR},
};

struct Apartment;
impl Apartment {
    fn enter() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}
fn endpoint_id(device: &IMMDevice) -> Result<String> {
    let id = unsafe { device.GetId()? };
    let result = unsafe { id.to_string() };
    unsafe {
        CoTaskMemFree(Some(id.0.cast()));
    }
    Ok(result?)
}
pub(super) fn devices() -> Result<Vec<AudioDevice>> {
    // A dedicated apartment also works when the caller is GPUI's STA thread.
    thread::spawn(|| -> Result<_> {
        let _com = Apartment::enter()?;
        let enumeration = enumerator()?;
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
        for (flow, output) in [(eRender, true), (eCapture, false)] {
            let endpoints = unsafe { enumeration.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
            for i in 0..unsafe { endpoints.GetCount()? } {
                let device = unsafe { endpoints.Item(i)? };
                let id = endpoint_id(&device)?;
                let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
                let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName)? };
                let name = value.to_string();
                devices.push(AudioDevice {
                    id,
                    name: if output {
                        format!("{name} (desktop audio)")
                    } else {
                        name
                    },
                    output,
                });
            }
        }
        Ok(devices)
    })
    .join()
    .map_err(|_| anyhow!("WASAPI enumeration thread panicked"))?
}

/// Pause between attempts to reach a lost endpoint, and between checks for
/// a new default endpoint.
const REOPEN: Duration = Duration::from_millis(500);
/// What every stream delivers, whatever the endpoint's mix format.
const SPEC: AudioSpec = AudioSpec {
    rate: 48_000,
    channels: 2,
};

/// The capture format asked of WASAPI: [`SPEC`] as 32-bit float. A plain
/// `WAVEFORMATEX` describes one or two channels only, which is all this
/// asks for; AUTOCONVERTPCM makes the engine convert to it.
fn wave_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: SPEC.channels,
        nSamplesPerSec: SPEC.rate,
        nAvgBytesPerSec: SPEC.rate * u32::from(SPEC.channels) * 4,
        nBlockAlign: SPEC.channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

struct Source {
    receive: mpsc::Receiver<AudioChunk>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl AudioSource for Source {
    fn spec(&self) -> AudioSpec {
        SPEC
    }
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        match self.receive.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            // The capture thread already logged why it stopped; one lost
            // device ends its own track, not the whole recording.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(timeout);
                Ok(None)
            }
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
pub(super) fn open(id: &str) -> Result<Box<dyn AudioSource>> {
    let id = id.to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let (ready, started) = mpsc::sync_channel(1);
    let (send, receive) = mpsc::sync_channel(16);
    let thread = thread::Builder::new()
        .name("quikscope-wasapi".into())
        .spawn(move || {
            // Only startup fails; later losses are retried in `capture`.
            if let Err(error) = capture(&id, &stopping, &ready, &send) {
                let _ = ready.send(Err(format!("{error:#}")));
            }
        })?;
    match started
        .recv()
        .context("WASAPI thread stopped during startup")?
    {
        Ok(()) => Ok(Box::new(Source {
            receive,
            stop,
            thread: Some(thread),
        })),
        Err(error) => {
            let _ = thread.join();
            Err(anyhow!(error))
        }
    }
}

fn capture(
    id: &str,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<std::result::Result<(), String>>,
    send: &mpsc::SyncSender<AudioChunk>,
) -> Result<()> {
    let _com = Apartment::enter()?;
    let enumeration = enumerator()?;
    let stream = Stream::open(&endpoint(&enumeration, id)?)?;
    let clock = Clock::new()?;
    let _ = ready.send(Ok(()));
    // From here on a lost endpoint is retried instead of ending the
    // recording.
    let mut stream = Some(stream);
    let mut checked = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Some(open) = &mut stream {
            match open.drain(&clock, send) {
                Ok(true) => {}
                Ok(false) => return Ok(()), // the recording dropped its source
                Err(error) => {
                    // AUDCLNT_E_DEVICE_INVALIDATED when unplugged, disabled
                    // or its format changed.
                    log::warn!("audio device {id} lost: {error:#}");
                    stream = None;
                }
            }
        }
        if checked.elapsed() >= REOPEN {
            checked = Instant::now();
            let current = stream.as_ref().map(|s| s.endpoint.as_str());
            match endpoint(&enumeration, id) {
                // The default endpoint changed, or the lost one is back.
                Ok(device) if current != endpoint_id(&device).ok().as_deref() => {
                    match Stream::open(&device) {
                        Ok(open) => {
                            log::info!("audio device {id} now captures {}", open.endpoint);
                            stream = Some(open);
                        }
                        Err(error) if stream.is_none() => {
                            log::debug!("audio device {id} still unavailable: {error:#}");
                        }
                        Err(error) => {
                            log::warn!("audio device {id} cannot follow the default: {error:#}");
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => log::debug!("audio device {id} has no endpoint: {error:#}"),
            }
        }
        thread::sleep(Duration::from_millis(3));
    }
    Ok(())
}

/// The endpoint `id` stands for right now.
fn endpoint(enumeration: &IMMDeviceEnumerator, id: &str) -> Result<IMMDevice> {
    unsafe {
        Ok(match id {
            DEFAULT_OUTPUT => enumeration.GetDefaultAudioEndpoint(eRender, eConsole)?,
            DEFAULT_INPUT => enumeration.GetDefaultAudioEndpoint(eCapture, eConsole)?,
            _ => {
                let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                enumeration.GetDevice(PCWSTR(wide.as_ptr()))?
            }
        })
    }
}

/// One started WASAPI client of one endpoint.
struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    endpoint: String,
    /// Capture time just past the previous packet, for packets whose
    /// timestamp WASAPI flags as wrong.
    next: Option<Instant>,
}
impl Stream {
    /// Captures `device` in [`SPEC`].
    fn open(device: &IMMDevice) -> Result<Self> {
        let endpoint = endpoint_id(device)?;
        let output = unsafe { device.cast::<IMMEndpoint>()?.GetDataFlow()? } == eRender;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let format = wave_format();
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | if output {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };
        unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &format, None)?;
        }
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        unsafe {
            client.Start()?;
        }
        Ok(Self {
            client,
            capture,
            endpoint,
            next: None,
        })
    }

    /// Sends every waiting packet; `false` once the receiver is gone.
    fn drain(&mut self, clock: &Clock, send: &mpsc::SyncSender<AudioChunk>) -> Result<bool> {
        let channels = usize::from(SPEC.channels);
        while unsafe { self.capture.GetNextPacketSize()? } > 0 {
            let (mut data, mut frames, mut flags, mut position) = (std::ptr::null_mut(), 0, 0, 0);
            unsafe {
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut position),
                )?;
            }
            let count = frames as usize * channels;
            let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                vec![0.0; count]
            } else {
                unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count) }.to_vec()
            };
            unsafe {
                self.capture.ReleaseBuffer(frames)?;
            }
            let length = Duration::from_secs_f64(f64::from(frames) / f64::from(SPEC.rate));
            // The QPC position belongs to the packet's first sample; the
            // mixer places later samples by count from there.
            let captured = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                self.next.unwrap_or_else(|| {
                    Instant::now()
                        .checked_sub(length)
                        .unwrap_or_else(Instant::now)
                })
            } else {
                clock.at(position)
            };
            self.next = Some(captured + length);
            match send.try_send(AudioChunk { samples, captured }) {
                Err(mpsc::TrySendError::Disconnected(_)) => return Ok(false),
                // A stalled consumer loses this packet; the mixer bridges
                // the gap from the next packet's timestamp.
                Err(mpsc::TrySendError::Full(_)) | Ok(()) => {}
            }
        }
        Ok(true)
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Maps WASAPI's QPC positions (100 ns units) onto `Instant`. Rust's
/// `Instant` reads the same counter on Windows, so one pairing taken
/// between two counter reads holds for the whole recording.
struct Clock {
    instant: Instant,
    /// The counter at `instant`, in 100 ns units.
    position: i128,
}
impl Clock {
    fn new() -> Result<Self> {
        let mut frequency = 0;
        let (mut before, mut after) = (0, 0);
        unsafe {
            QueryPerformanceFrequency(&mut frequency)?;
            QueryPerformanceCounter(&mut before)?;
        }
        let instant = Instant::now();
        unsafe {
            QueryPerformanceCounter(&mut after)?;
        }
        let ticks = (i128::from(before) + i128::from(after)) / 2;
        Ok(Self {
            instant,
            position: ticks * 10_000_000 / i128::from(frequency),
        })
    }

    fn at(&self, position: u64) -> Instant {
        let delta = i128::from(position) - self.position;
        let offset = Duration::from_nanos((delta.unsigned_abs() * 100) as u64);
        if delta >= 0 {
            self.instant + offset
        } else {
            self.instant.checked_sub(offset).unwrap_or(self.instant)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the device (a 7.1 headset's mix format included), WASAPI is
    /// asked for the one format a plain WAVEFORMATEX can describe and the
    /// mixer takes without resampling.
    #[test]
    fn requests_48k_stereo_float() {
        // WAVEFORMATEX is packed: copy the fields out before comparing.
        let WAVEFORMATEX {
            wFormatTag: tag,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: bytes_per_second,
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: extra,
        } = wave_format();
        assert_eq!(u32::from(tag), WAVE_FORMAT_IEEE_FLOAT);
        assert_eq!((rate, channels), (48_000, 2));
        assert_eq!((bits, block), (32, 8));
        assert_eq!(bytes_per_second, 48_000 * 8);
        assert_eq!(extra, 0);
        assert_eq!((SPEC.rate, SPEC.channels), (48_000, 2));
    }

    #[test]
    #[ignore = "requires native Windows audio endpoints"]
    fn native_wasapi_loopback() -> Result<()> {
        let devices = devices()?;
        assert!(devices.iter().any(|d| d.output));
        let mut source = open(DEFAULT_OUTPUT)?;
        let spec = source.spec();
        assert_eq!((spec.rate, spec.channels), (48_000, 2));
        let path = std::env::temp_dir().join("quikscope-wasapi-test.wav");
        let count = 48_000u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + count * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(count * 2).to_le_bytes());
        for i in 0..count {
            wav.extend_from_slice(
                &((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0)
                    .sin()
                    .mul_add(6000.0, 0.0) as i16)
                    .to_le_bytes(),
            );
        }
        std::fs::write(&path, wav)?;
        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            PlaySoundW(PCWSTR(wide.as_ptr()), None, SND_FILENAME | SND_ASYNC).ok()?;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut non_silent = false;
        while Instant::now() < deadline {
            if let Some(chunk) = source.read(Duration::from_millis(100))? {
                assert_eq!(chunk.samples.len() % spec.channels as usize, 0);
                assert!(
                    Instant::now().saturating_duration_since(chunk.captured)
                        < Duration::from_secs(3)
                );
                if chunk.samples.iter().any(|v| v.abs() > 0.001) {
                    non_silent = true;
                    break;
                }
            }
        }
        unsafe {
            let _ = PlaySoundW(PCWSTR::null(), None, SND_FLAGS(0));
        }
        std::fs::remove_file(path)?;
        assert!(non_silent, "loopback did not return audible samples");
        Ok(())
    }
}
