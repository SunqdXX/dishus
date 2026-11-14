//! PC sound on Windows: WASAPI loopback of the default output → 20 ms Opus packets
//! ([`crate::capture::opus_frames`]). Same packets and timing as the Linux capture.
//!
//! * Asks Windows to convert to 48 kHz 16-bit stereo (`AUTOCONVERTPCM`); if that is refused,
//!   takes the device's own mix format when it is 48 kHz float (the usual one).
//! * Nothing playing: Windows delivers nothing, so nothing is sent (like Linux).
//! * The default output changes or goes away: reported as a failure, and the shared audio loop
//!   ([`crate::capture::audio_worker`]) opens the new default.
//! * Timestamps: the QPC time of each buffer, in microseconds.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
    },
    System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree},
};
use windows::core::IUnknown;

use super::pipeline::Com;
use crate::capture::{
    audio_worker::{OnFail, OnPacket, RunningAudio},
    opus_frames::{self, OpusFrames},
};

const RATE: u32 = 48_000;
const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xFFFE;
/// The capture buffer Windows keeps for us (100 ns units): 200 ms.
const BUFFER_HNS: i64 = 2_000_000;
const POLL: Duration = Duration::from_millis(10);
const CHECK_DEFAULT_EVERY: Duration = Duration::from_secs(1);

pub struct AudioRunning {
    stop: Arc<AtomicBool>,
    kbps: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl RunningAudio for AudioRunning {
    fn set_bitrate(&self, kbps: u32) {
        self.kbps.store(kbps, Ordering::Relaxed);
    }
}

impl Drop for AudioRunning {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start capturing (or, with `test`, a quiet 440 Hz tone that needs no sound device).
/// Returns once the capture runs; errors opening it are returned here.
pub fn start_audio(
    test: bool,
    kbps: u32,
    on_packet: OnPacket,
    on_fail: OnFail,
) -> Result<AudioRunning> {
    let stop = Arc::new(AtomicBool::new(false));
    let rate = Arc::new(AtomicU32::new(kbps));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let (s, r) = (stop.clone(), rate.clone());
    let thread = std::thread::Builder::new()
        .name("dishus-audio".into())
        .spawn(move || {
            let _com = Com::init();
            let mut frames = match OpusFrames::new(kbps) {
                Ok(f) => f,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let res = if test {
                let _ = ready_tx.send(Ok(()));
                tone(&s, &r, &mut frames, &on_packet)
            } else {
                match Loopback::open() {
                    Ok(cap) => {
                        let _ = ready_tx.send(Ok(()));
                        capture(&cap, &s, &r, &mut frames, &on_packet)
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                }
            };
            if let Err(e) = res
                && !s.load(Ordering::SeqCst)
            {
                on_fail(format!("{e:#}"));
            }
        })
        .context("starting the audio thread")?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(AudioRunning {
            stop,
            kbps: rate,
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            bail!("the audio thread ended while starting")
        }
    }
}

fn apply_bitrate(rate: &AtomicU32, current: &mut u32, frames: &mut OpusFrames) {
    let k = rate.load(Ordering::Relaxed);
    if k != *current {
        frames.set_bitrate(k);
        *current = k;
    }
}

fn capture(
    cap: &Loopback,
    stop: &AtomicBool,
    rate: &AtomicU32,
    frames: &mut OpusFrames,
    on_packet: &OnPacket,
) -> Result<()> {
    let mut kbps = rate.load(Ordering::Relaxed);
    let mut pcm = Vec::with_capacity(RATE as usize / 5 * 2);
    let mut checked = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
        apply_bitrate(rate, &mut kbps, frames);
        cap.read(&mut pcm, |pcm, pts_us| {
            frames.push(pcm, pts_us, |p, d| on_packet(p, d))
        })?;
        if checked.elapsed() >= CHECK_DEFAULT_EVERY {
            checked = Instant::now();
            if cap.default_id()? != cap.id {
                bail!("the default sound output changed");
            }
        }
    }
    Ok(())
}

/// The test tone, paced by the clock like a real capture.
fn tone(
    stop: &AtomicBool,
    rate: &AtomicU32,
    frames: &mut OpusFrames,
    on_packet: &OnPacket,
) -> Result<()> {
    let mut kbps = rate.load(Ordering::Relaxed);
    let start = Instant::now();
    let (mut made, mut phase) = (0u64, 0u64);
    let mut pcm = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
        apply_bitrate(rate, &mut kbps, frames);
        let due = start.elapsed().as_micros() as u64 * RATE as u64 / 1_000_000;
        let n = (due - made) as usize;
        pcm.resize(n * 2, 0);
        opus_frames::test_tone(&mut pcm, &mut phase);
        let pts_us = made * 1_000_000 / RATE as u64;
        frames.push(&pcm, pts_us, |p, d| on_packet(p, d))?;
        made = due;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Format {
    /// What we asked for: 48 kHz, 16-bit, stereo.
    S16Stereo,
    /// The device's mix format: 48 kHz float with this many channels.
    F32 { channels: usize },
}

struct Loopback {
    enumerator: IMMDeviceEnumerator,
    id: String,
    client: IAudioClient,
    cap: IAudioCaptureClient,
    format: Format,
}

fn device_id(dev: &IMMDevice) -> Result<String> {
    // SAFETY: GetId returns a CoTaskMem string that we own; copied, then freed once.
    unsafe {
        let p = dev.GetId()?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0.cast()));
        Ok(s?)
    }
}

impl Loopback {
    fn open() -> Result<Self> {
        // SAFETY: plain COM calls on this thread (COM initialised by the caller); every pointer
        // passed lives across the call, and the mix format is freed once after it was copied.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
                    .context("opening the Windows sound device list")?;
            let dev = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("this PC has no sound output")?;
            let id = device_id(&dev)?;
            let client: IAudioClient = dev
                .Activate(CLSCTX_ALL, None)
                .context("opening the sound output")?;
            let want = WAVEFORMATEX {
                wFormatTag: TAG_PCM,
                nChannels: 2,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * 4,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let (client, format) = match client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_HNS,
                0,
                &want,
                None,
            ) {
                Ok(()) => (client, Format::S16Stereo),
                Err(first) => {
                    // A fresh client: a failed Initialize leaves the old one unusable.
                    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
                    let mix = client.GetMixFormat()?;
                    let wf = std::ptr::read_unaligned(mix);
                    let (tag, bits, rate, ch) = (
                        wf.wFormatTag,
                        wf.wBitsPerSample,
                        wf.nSamplesPerSec,
                        wf.nChannels,
                    );
                    let usable = (tag == TAG_FLOAT || tag == TAG_EXTENSIBLE)
                        && bits == 32
                        && rate == RATE
                        && ch >= 1;
                    let res = if usable {
                        client.Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            AUDCLNT_STREAMFLAGS_LOOPBACK,
                            BUFFER_HNS,
                            0,
                            mix,
                            None,
                        )
                    } else {
                        Ok(())
                    };
                    CoTaskMemFree(Some(mix.cast()));
                    if !usable {
                        return Err(anyhow!(
                            "the sound output's format is not supported ({rate} Hz, \
                                 {bits}-bit, {ch} channels, tag {tag:#x}); converting it was \
                                 refused: {first}"
                        ));
                    }
                    res.context("opening the sound output for recording")?;
                    (
                        client,
                        Format::F32 {
                            channels: ch as usize,
                        },
                    )
                }
            };
            let cap: IAudioCaptureClient = client.GetService()?;
            client.Start().context("starting the sound recording")?;
            tracing::info!(
                "audio: recording the default output ({})",
                match format {
                    Format::S16Stereo => "converted to 48 kHz stereo".to_string(),
                    Format::F32 { channels } => format!("48 kHz float, {channels} channels"),
                }
            );
            Ok(Self {
                enumerator,
                id,
                client,
                cap,
                format,
            })
        }
    }

    fn default_id(&self) -> Result<String> {
        // SAFETY: plain COM call on the thread that created the enumerator.
        let dev = unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("the sound output went away")?;
        device_id(&dev)
    }

    /// Everything captured since the last call, as 48 kHz stereo i16, with its QPC time (µs).
    fn read(&self, pcm: &mut Vec<i16>, mut f: impl FnMut(&[i16], u64) -> Result<()>) -> Result<()> {
        loop {
            // SAFETY: GetBuffer hands out `frames` frames at `data` in our format until the
            // matching ReleaseBuffer; they are copied into `pcm` before that.
            let pts_us = unsafe {
                let n = self.cap.GetNextPacketSize().map_err(invalidated)?;
                if n == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
                self.cap
                    .GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        None,
                        Some(&mut qpc as *mut u64),
                    )
                    .map_err(invalidated)?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let frames_n = frames as usize;
                pcm.clear();
                match self.format {
                    _ if silent || data.is_null() => pcm.resize(frames_n * 2, 0),
                    Format::S16Stereo => pcm.extend_from_slice(std::slice::from_raw_parts(
                        data.cast::<i16>(),
                        frames_n * 2,
                    )),
                    Format::F32 { channels } => {
                        let s = std::slice::from_raw_parts(data.cast::<f32>(), frames_n * channels);
                        for fr in s.chunks_exact(channels) {
                            let l = fr[0];
                            let r = if channels > 1 { fr[1] } else { l };
                            pcm.push(to_i16(l));
                            pcm.push(to_i16(r));
                        }
                    }
                }
                self.cap.ReleaseBuffer(frames).map_err(invalidated)?;
                qpc / 10
            };
            f(pcm, pts_us)?;
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: plain COM call on the owning thread.
        let _ = unsafe { self.client.Stop() };
    }
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

fn invalidated(e: windows::core::Error) -> anyhow::Error {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        anyhow!("the sound output went away")
    } else {
        anyhow!(e).context("recording the sound output")
    }
}
