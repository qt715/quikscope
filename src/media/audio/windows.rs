//! WASAPI clients never leave their COM apartment. A bounded queue prevents
//! a paused or stalled encoder from retaining minutes of microphone data.
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
        Media::Audio::*,
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

struct Source {
    spec: AudioSpec,
    receive: mpsc::Receiver<Result<AudioChunk>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl AudioSource for Source {
    fn spec(&self) -> AudioSpec {
        self.spec
    }
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        match self.receive.recv_timeout(timeout) {
            Ok(chunk) => chunk.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => Err(anyhow!("WASAPI capture thread stopped")),
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
            let result = capture(&id, &stopping, &ready, &send);
            if let Err(error) = result {
                let message = format!("{error:#}");
                let _ = ready.send(Err(message.clone()));
                let _ = send.try_send(Err(anyhow!(message)));
            }
        })?;
    match started
        .recv()
        .context("WASAPI thread stopped during startup")?
    {
        Ok(spec) => Ok(Box::new(Source {
            spec,
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
    ready: &mpsc::SyncSender<std::result::Result<AudioSpec, String>>,
    send: &mpsc::SyncSender<Result<AudioChunk>>,
) -> Result<()> {
    let _com = Apartment::enter()?;
    let enumeration = enumerator()?;
    let device = unsafe {
        match id {
            DEFAULT_OUTPUT => enumeration.GetDefaultAudioEndpoint(eRender, eConsole)?,
            DEFAULT_INPUT => enumeration.GetDefaultAudioEndpoint(eCapture, eConsole)?,
            _ => {
                let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                enumeration.GetDevice(PCWSTR(wide.as_ptr()))?
            }
        }
    };
    let endpoint: IMMEndpoint = device.cast()?;
    let output = unsafe { endpoint.GetDataFlow()? } == eRender;
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let mix = unsafe { client.GetMixFormat()? };
    let rate = unsafe { (*mix).nSamplesPerSec };
    let channels = unsafe { (*mix).nChannels };
    unsafe {
        CoTaskMemFree(Some(mix.cast()));
    }
    let spec = AudioSpec { rate, channels };
    let format = WAVEFORMATEX {
        wFormatTag: 3,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(channels) * 4,
        nBlockAlign: channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | if output {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
    unsafe {
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &format, None)?;
    }
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    let mut frequency = 0;
    let mut qpc = 0;
    unsafe {
        QueryPerformanceFrequency(&mut frequency)?;
        QueryPerformanceCounter(&mut qpc)?;
    }
    let epoch = Instant::now();
    let qpc_epoch = qpc as f64 / frequency as f64;
    unsafe {
        client.Start()?;
    }
    let _ = ready.send(Ok(spec));
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            if unsafe { capture.GetNextPacketSize()? } == 0 {
                thread::sleep(Duration::from_millis(3));
                continue;
            }
            let (mut data, mut frames, mut flags, mut position) = (std::ptr::null_mut(), 0, 0, 0);
            unsafe {
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut position),
                )?;
            }
            let count = frames as usize * channels as usize;
            let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                vec![0.0; count]
            } else {
                unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count) }.to_vec()
            };
            // WASAPI returns QPC time converted to 100 ns units, not raw ticks.
            let delta = position as f64 / 10_000_000.0 - qpc_epoch;
            let captured = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                Instant::now()
            } else if delta >= 0.0 {
                epoch + Duration::from_secs_f64(delta)
            } else {
                epoch
                    .checked_sub(Duration::from_secs_f64(-delta))
                    .unwrap_or(epoch)
            };
            unsafe {
                capture.ReleaseBuffer(frames)?;
            }
            if let Err(mpsc::TrySendError::Disconnected(_)) =
                send.try_send(Ok(AudioChunk { samples, captured }))
            {
                break;
            }
        }
        Ok(())
    })();
    unsafe {
        client.Stop()?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires native Windows audio endpoints"]
    fn native_wasapi_loopback() -> Result<()> {
        let devices = devices()?;
        assert!(devices.iter().any(|d| d.output));
        let mut source = open(DEFAULT_OUTPUT)?;
        let spec = source.spec();
        assert!(spec.rate > 0 && spec.channels > 0);
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
