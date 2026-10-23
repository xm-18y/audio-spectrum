#![deny(unsafe_code)]
//! music-spec-capture — platform audio Capture Backends.
//!
//! Contract: every backend delivers a continuous stream of mono f32 samples
//! at [`TARGET_SAMPLE_RATE`]. Normalization (sample-format conversion,
//! channel mixdown, resampling) happens at this boundary, so the spectrum
//! pipeline never sees another format (see GLOSSARY.md “Capture Backend”).
//! Every backend's audio callback only writes to the ring buffer: no
//! allocation, no locks, no blocking (ADR-0002).
//!
//! Backends:
//! - Input: cpal on all platforms.
//! - Loopback: macOS ScreenCaptureKit (ADR-0001), Windows WASAPI loopback,
//!   Linux PulseAudio monitor (PipeWire via the PulseAudio compatibility
//!   layer).
//!
//! `unsafe` is confined to the per-platform FFI modules (and only where the
//! platform APIs require it).

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod pulse_loopback;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod windows_loopback;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// The one sample rate delivered to consumers of this crate.
pub const TARGET_SAMPLE_RATE: u32 = 48_000;

/// Ring buffer capacity in samples: two seconds, absorbing UI hiccups.
/// Overflow is dropped on the floor — acceptable for a display.
const RING_CAPACITY: usize = TARGET_SAMPLE_RATE as usize * 2;

/// Which side of the audio stack a Source captures from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    /// Microphone / line-in.
    Input,
    /// Whatever the system is currently playing to its output device.
    Loopback,
}

/// A user-selectable Source: capture kind plus a specific device.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceInfo {
    pub kind: SourceKind,
    /// Persistent platform device id (string form).
    pub id: String,
    /// Human-readable device name.
    pub name: String,
}

/// Why opening a Source failed.
#[derive(Debug)]
pub enum CaptureError {
    /// The device no longer exists (unplugged / id changed).
    DeviceGone(String),
    /// The platform backend for this SourceKind is unavailable.
    Unsupported(&'static str),
    /// The platform refused to open or start the stream.
    OpenFailed(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::DeviceGone(name) => write!(f, "设备已不存在:{name}"),
            CaptureError::Unsupported(note) => write!(f, "{note}"),
            CaptureError::OpenFailed(msg) => write!(f, "无法打开音频流:{msg}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Front door to capture: enumerates Sources and opens Capture Backends.
pub struct CaptureSystem {
    host: cpal::Host,
    loopback_note: &'static str,
}

impl CaptureSystem {
    pub fn new() -> CaptureSystem {
        CaptureSystem {
            host: cpal::default_host(),
            loopback_note: loopback_note(),
        }
    }

    /// Devices available for the given kind. Loopback lists the system's
    /// output devices (macOS: the single “current audio output” Source).
    pub fn list_devices(&self, kind: SourceKind) -> Vec<DeviceInfo> {
        match kind {
            SourceKind::Input => self.input_devices(),
            SourceKind::Loopback => loopback_devices(),
        }
    }

    /// The default Source of the given kind. The app opens
    /// `Loopback` at startup (design consensus) and falls back to Input.
    pub fn default_source(&self, kind: SourceKind) -> Option<DeviceInfo> {
        match kind {
            SourceKind::Loopback => loopback_default(),
            SourceKind::Input => self
                .host
                .default_input_device()
                .and_then(|d| device_info(SourceKind::Input, &d)),
        }
    }

    /// Empty when Loopback is fully available on this platform.
    pub fn loopback_note(&self) -> &'static str {
        self.loopback_note
    }

    /// Opens the given Source. Dropping the returned stream stops capturing.
    pub fn open(&self, source: &DeviceInfo) -> Result<CaptureStream, CaptureError> {
        match source.kind {
            SourceKind::Input => self.open_input(source),
            SourceKind::Loopback => open_loopback(source),
        }
    }

    fn input_devices(&self) -> Vec<DeviceInfo> {
        self.host
            .input_devices()
            .map(|devices| {
                devices
                    .filter_map(|d| device_info(SourceKind::Input, &d))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_input(&self, source: &DeviceInfo) -> Result<CaptureStream, CaptureError> {
        let device = self
            .host
            .input_devices()
            .map_err(|e| CaptureError::OpenFailed(e.to_string()))?
            .find(|d| device_info(SourceKind::Input, d).is_some_and(|info| info.id == source.id))
            .ok_or_else(|| CaptureError::DeviceGone(source.name.clone()))?;

        let supported = device
            .default_input_config()
            .map_err(|e| CaptureError::OpenFailed(e.to_string()))?;
        let in_rate = supported.sample_rate();
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: in_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let (prod, cons) = split_ring();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_cb = dead.clone();
        let mut ctx = FeedCtx::new(prod, in_rate, supported.channels() as usize);

        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _info| ctx.push_samples(data.iter().copied()),
                move |_err| dead_cb.store(true, Ordering::Relaxed),
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                config,
                move |data: &[i16], _info| {
                    ctx.push_samples(data.iter().map(|&x| x as f32 / 32768.0))
                },
                move |_err| dead_cb.store(true, Ordering::Relaxed),
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                config,
                move |data: &[u16], _info| {
                    ctx.push_samples(data.iter().map(|&x| x as f32 / 32768.0 - 1.0))
                },
                move |_err| dead_cb.store(true, Ordering::Relaxed),
                None,
            ),
            other => {
                return Err(CaptureError::OpenFailed(format!(
                    "暂不支持的采样格式:{other:?}"
                )));
            }
        }
        .map_err(|e| CaptureError::OpenFailed(e.to_string()))?;

        stream
            .play()
            .map_err(|e| CaptureError::OpenFailed(e.to_string()))?;

        Ok(CaptureStream::new(StreamHolder::Cpal(stream), cons, dead))
    }
}

impl Default for CaptureSystem {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Platform dispatch for Loopback
// ---------------------------------------------------------------------------

fn open_loopback(source: &DeviceInfo) -> Result<CaptureStream, CaptureError> {
    #[cfg(target_os = "macos")]
    {
        macos::open_loopback(source)
    }
    #[cfg(target_os = "windows")]
    {
        windows_loopback::open_loopback(source)
    }
    #[cfg(target_os = "linux")]
    {
        pulse_loopback::open_loopback(source)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = source;
        Err(CaptureError::Unsupported("此平台不支持 Loopback"))
    }
}

fn loopback_devices() -> Vec<DeviceInfo> {
    #[cfg(target_os = "macos")]
    {
        macos::list_loopback_devices()
    }
    #[cfg(target_os = "windows")]
    {
        windows_loopback::list_loopback_devices()
    }
    #[cfg(target_os = "linux")]
    {
        pulse_loopback::list_loopback_devices()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

fn loopback_default() -> Option<DeviceInfo> {
    #[cfg(target_os = "macos")]
    {
        macos::list_loopback_devices().into_iter().next()
    }
    #[cfg(target_os = "windows")]
    {
        windows_loopback::default_device()
    }
    #[cfg(target_os = "linux")]
    {
        pulse_loopback::default_device()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

fn loopback_note() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        if macos::macos_13_plus() {
            ""
        } else {
            "Loopback 需要 macOS 13 或更高版本(ADR-0001)"
        }
    }
    #[cfg(target_os = "windows")]
    {
        windows_loopback::NOTE
    }
    #[cfg(target_os = "linux")]
    {
        pulse_loopback::NOTE
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        "此平台不支持 Loopback"
    }
}

// ---------------------------------------------------------------------------
// Stream plumbing shared by all backends
// ---------------------------------------------------------------------------

/// Keeps the platform capture machinery alive; dropping it stops the stream.
#[cfg(target_os = "macos")]
enum StreamHolder {
    Cpal(#[allow(dead_code)] cpal::Stream),
    Sck(#[allow(dead_code)] crate::macos::SckSession),
}

#[cfg(target_os = "windows")]
enum StreamHolder {
    Cpal(#[allow(dead_code)] cpal::Stream),
    Wasapi(#[allow(dead_code)] crate::windows_loopback::WasapiSession),
}

#[cfg(target_os = "linux")]
enum StreamHolder {
    Cpal(#[allow(dead_code)] cpal::Stream),
    Pulse(#[allow(dead_code)] crate::pulse_loopback::PulseSession),
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
enum StreamHolder {
    Cpal(#[allow(dead_code)] cpal::Stream),
}

/// A running capture: backend callback → ring buffer → [`CaptureStream::pop_samples`].
pub struct CaptureStream {
    #[allow(dead_code)]
    holder: StreamHolder,
    cons: HeapCons<f32>,
    dead: Arc<AtomicBool>,
}

impl CaptureStream {
    pub(crate) fn new(holder: StreamHolder, cons: HeapCons<f32>, dead: Arc<AtomicBool>) -> Self {
        CaptureStream { holder, cons, dead }
    }

    /// Drains all currently buffered samples into `out` (appends).
    pub fn pop_samples(&mut self, out: &mut Vec<f32>) {
        out.extend(self.cons.pop_iter());
    }

    /// True once the platform reported a stream error (device gone, …);
    /// the caller should start a Reconnect.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
}

pub(crate) fn split_ring() -> (HeapProd<f32>, HeapCons<f32>) {
    HeapRb::<f32>::new(RING_CAPACITY).split()
}

fn device_info(kind: SourceKind, device: &cpal::Device) -> Option<DeviceInfo> {
    let name = device.description().ok()?.name().to_string();
    let id = device.id().ok()?.to_string();
    Some(DeviceInfo { kind, id, name })
}

/// Backend-thread state: converts incoming device audio to normalized mono
/// samples and pushes them into the ring buffer. Allocation-free.
pub(crate) struct FeedCtx {
    prod: HeapProd<f32>,
    res: LinearResampler,
    channels: usize,
    acc: f32,
    n: usize,
}

impl FeedCtx {
    /// `in_rate` is the device-side rate; audio is resampled to
    /// [`TARGET_SAMPLE_RATE`] (a no-op when they match).
    pub(crate) fn new(prod: HeapProd<f32>, in_rate: u32, channels: usize) -> FeedCtx {
        FeedCtx {
            prod,
            res: LinearResampler::new(in_rate),
            channels,
            acc: 0.0,
            n: 0,
        }
    }

    /// Mono mixdown (channel average) → resample → ring buffer.
    /// Must not allocate: runs on a backend audio thread.
    pub(crate) fn push_sample(&mut self, s: f32) {
        self.acc += s;
        self.n += 1;
        if self.n == self.channels {
            let mono = self.acc / self.channels as f32;
            self.acc = 0.0;
            self.n = 0;
            self.res.push(mono, &mut self.prod);
        }
    }

    /// [`FeedCtx::push_sample`] over an interleaved sample iterator.
    pub(crate) fn push_samples<I: Iterator<Item = f32>>(&mut self, samples: I) {
        for s in samples {
            self.push_sample(s);
        }
    }
}

/// Streaming linear resampler. Quality is fine for a spectrum display and the
/// arithmetic is fixed per sample (no allocation, stable delay).
pub(crate) struct LinearResampler {
    last: f32,
    /// Interpolation position between `last` and the next input sample.
    t: f32,
    /// Input samples per output sample: in_rate / TARGET_SAMPLE_RATE.
    step: f32,
}

impl LinearResampler {
    pub(crate) fn new(in_rate: u32) -> LinearResampler {
        let step = in_rate as f32 / TARGET_SAMPLE_RATE as f32;
        LinearResampler {
            last: 0.0,
            t: step,
            step,
        }
    }

    pub(crate) fn push(&mut self, x: f32, prod: &mut HeapProd<f32>) {
        while self.t <= 1.0 {
            let y = self.last + (x - self.last) * self.t;
            let _ = prod.try_push(y);
            self.t += self.step;
        }
        self.last = x;
        self.t -= 1.0;
    }
}
