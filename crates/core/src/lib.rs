#![forbid(unsafe_code)]
//! audio-spectrum-core — the Spectrum pipeline: PCM samples in, per-Band dBFS out.
//!
//! Pure computation: no I/O, no FFI, no UI, no threads. The public seam is
//! [`Analyzer::analyze`] (one frame of samples → per-Band dBFS on the log
//! frequency axis) and [`Smoother::process`] (per-Band dBFS → attack/release
//! smoothed display values with Peak Hold). Domain language: see GLOSSARY.md.

/// Number of mono samples in one analysis frame (ADR-0002: FFT runs on the UI
/// thread; 2048 real FFT costs tens of microseconds per frame).
pub const FRAME_SIZE: usize = 2048;

/// The one sample rate the whole pipeline speaks. Capture Backends normalize
/// their output to this before samples reach the ring buffer.
pub const SAMPLE_RATE: f32 = 48_000.0;

/// Low end of the displayed log frequency axis.
pub const FREQ_MIN_HZ: f32 = 20.0;

/// High end of the displayed log frequency axis.
pub const FREQ_MAX_HZ: f32 = 20_000.0;

/// Displayed ceiling in dBFS (full scale).
pub const DB_CEILING: f32 = 0.0;

/// Displayed floor in dBFS.
pub const DB_FLOOR: f32 = -80.0;

use realfft::{RealFftPlanner, RealToComplex, num_complex::Complex};

const BIN_COUNT: usize = FRAME_SIZE / 2 + 1;
const BIN_HZ: f32 = SAMPLE_RATE / FRAME_SIZE as f32;

/// Turns a frame of mono PCM samples into per-Band dBFS values.
pub struct Analyzer {
    num_bands: usize,
    /// Geometric Band ratio: center[i+1] / center[i].
    ratio: f32,
    centers: Vec<f32>,
    /// Inclusive range of Bin indices whose center frequencies fall inside each Band.
    band_bins: Vec<(usize, usize)>,
    /// Periodic Hann window.
    window: Vec<f32>,
    /// Magnitude a full-scale sine produces at its Bin: sum(window)/2.
    calib: f32,
    fft: std::sync::Arc<dyn RealToComplex<f32>>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    bin_db: Vec<f32>,
}

impl Analyzer {
    pub fn new(num_bands: usize) -> Analyzer {
        assert!(num_bands >= 1, "at least one Band");
        let ratio = (FREQ_MAX_HZ / FREQ_MIN_HZ).powf(1.0 / num_bands as f32);
        let centers = (0..num_bands)
            .map(|i| FREQ_MIN_HZ * ratio.powf(i as f32 + 0.5))
            .collect();
        let band_bins = (0..num_bands)
            .map(|i| {
                let lo = FREQ_MIN_HZ * ratio.powf(i as f32);
                let hi = FREQ_MIN_HZ * ratio.powf(i as f32 + 1.0);
                // Bin centers i·BIN_HZ with lo ≤ i·BIN_HZ < hi.
                let first = ((lo / BIN_HZ).ceil() as usize).min(BIN_COUNT - 1);
                let last = ((((hi / BIN_HZ).ceil() - 1.0).max(0.0)) as usize).min(BIN_COUNT - 1);
                (first, last)
            })
            .collect();
        let window: Vec<f32> = (0..FRAME_SIZE)
            .map(|n| 0.5 * (1.0 - (std::f32::consts::TAU * n as f32 / FRAME_SIZE as f32).cos()))
            .collect();
        let calib = 0.5 * window.iter().sum::<f32>();
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FRAME_SIZE);
        let input = fft.make_input_vec();
        let spectrum = fft.make_output_vec();
        let bin_db = vec![DB_FLOOR; BIN_COUNT];
        Analyzer {
            num_bands,
            ratio,
            centers,
            band_bins,
            window,
            calib,
            fft,
            input,
            spectrum,
            bin_db,
        }
    }

    pub fn num_bands(&self) -> usize {
        self.num_bands
    }

    /// Center frequency of the Band with the given index, on the log axis.
    pub fn band_center(&self, band: usize) -> f32 {
        self.centers[band]
    }

    /// Frequency range covered by the Band with the given index.
    pub fn band_edges(&self, band: usize) -> (f32, f32) {
        let lo = FREQ_MIN_HZ * self.ratio.powf(band as f32);
        (lo, lo * self.ratio)
    }

    /// Index of the Band containing the given frequency.
    pub fn band_of(&self, freq: f32) -> usize {
        let idx = (freq / FREQ_MIN_HZ).ln() / self.ratio.ln();
        (idx.floor() as usize).min(self.num_bands - 1)
    }

    /// Analyzes one frame of samples; returns one dBFS value per Band.
    pub fn analyze(&mut self, samples: &[f32; FRAME_SIZE]) -> Vec<f32> {
        self.input.copy_from_slice(samples);
        for i in 0..FRAME_SIZE {
            self.input[i] *= self.window[i];
        }
        self.fft
            .process(&mut self.input, &mut self.spectrum)
            .expect("buffer lengths are fixed by Analyzer::new");
        for (i, c) in self.spectrum.iter().enumerate() {
            let mag = (c.re * c.re + c.im * c.im).sqrt();
            let db = 20.0 * (mag / self.calib).log10();
            self.bin_db[i] = db.max(DB_FLOOR);
        }
        self.band_bins
            .iter()
            .zip(&self.centers)
            .map(|(&(first, last), &center)| {
                let mut v = self.interp_db(center);
                for i in first..=last {
                    v = v.max(self.bin_db[i]);
                }
                v
            })
            .collect()
    }

    /// Linear-in-dB interpolation of the Bin spectrum at an arbitrary frequency.
    fn interp_db(&self, freq: f32) -> f32 {
        let x = (freq / BIN_HZ).clamp(0.0, (BIN_COUNT - 1) as f32);
        let i = x.floor() as usize;
        let frac = x - i as f32;
        let a = self.bin_db[i];
        let b = self.bin_db[(i + 1).min(BIN_COUNT - 1)];
        a + (b - a) * frac
    }
}

/// One smoothed Spectrum frame: per-Band display value and Peak Hold line, both dBFS.
pub struct SmoothedSpectrum {
    pub bands: Vec<f32>,
    pub peaks: Vec<f32>,
}

/// Aggregation interval of one Spectrogram column, in seconds.
pub const SPECTRO_COLUMN_SECS: f64 = 0.020;
/// Total Spectrogram history, in seconds.
pub const SPECTRO_HISTORY_SECS: f32 = 12.0;
/// History capacity in columns (history / interval).
pub const SPECTRO_COLUMN_COUNT: usize =
    (SPECTRO_HISTORY_SECS as f64 / SPECTRO_COLUMN_SECS) as usize;

/// Scrolling Spectrogram history: aggregates per-Band frames into columns
/// holding each interval's maximum (ADR-0003: reuses the Band log axis).
/// Pure computation; time arrives as a parameter.
pub struct Spectrograph {
    num_bands: usize,
    /// Ring of columns, `head` is the next write slot.
    columns: Vec<Vec<f32>>,
    head: usize,
    filled: usize,
    /// Running maximum of the currently open aggregation window.
    pending: Vec<f32>,
    /// Window start of the open interval; `None` until the first frame.
    window_start: Option<f64>,
    /// Monotonic counter, bumped whenever a column closes. Lets the UI
    /// cache the rendered texture and skip rebuilding unchanged frames.
    revision: u64,
}

impl Spectrograph {
    pub fn new(num_bands: usize) -> Spectrograph {
        Spectrograph {
            num_bands,
            columns: vec![vec![DB_FLOOR; num_bands]; SPECTRO_COLUMN_COUNT],
            head: 0,
            filled: 0,
            pending: vec![DB_FLOOR; num_bands],
            window_start: None,
            revision: 0,
        }
    }

    /// Bumped whenever the visible history changes (a column closes).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn num_bands(&self) -> usize {
        self.num_bands
    }

    /// Number of columns currently held (≤ [`SPECTRO_COLUMN_COUNT`]).
    pub fn len(&self) -> usize {
        self.filled
    }

    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }

    /// The column `age` intervals behind the newest one, or `None`.
    pub fn column(&self, age: usize) -> Option<&[f32]> {
        if age >= self.filled {
            return None;
        }
        let idx = (self.head + SPECTRO_COLUMN_COUNT - 1 - age) % SPECTRO_COLUMN_COUNT;
        Some(&self.columns[idx])
    }

    /// Merges one Band frame (from `Analyzer::analyze`) at `frame_time`
    /// (seconds, monotonic-ish).
    ///
    /// Semantics: a frame landing inside the open window folds into its
    /// maximum. Analysis frames (2048 samples @ 48 kHz ≈ 42.7 ms) arrive
    /// sparser than the 20 ms column interval, so when elapsed windows
    /// close, the closing frame also seeds each newly opened one — the
    /// alternative (floor-filling the gaps) would stripe the display with
    /// silence columns during normal playback.
    pub fn push_frame(&mut self, frame_time: f64, bands_db: &[f32]) {
        debug_assert_eq!(bands_db.len(), self.num_bands);
        let Some(mut start) = self.window_start else {
            // First frame: open the first window, nothing emitted yet.
            self.window_start = Some(frame_time);
            self.pending.copy_from_slice(bands_db);
            return;
        };
        if frame_time - start < SPECTRO_COLUMN_SECS {
            for (p, &b) in self.pending.iter_mut().zip(bands_db) {
                *p = p.max(b);
            }
            return;
        }
        let mut closed = 0;
        while frame_time - start >= SPECTRO_COLUMN_SECS && closed < SPECTRO_COLUMN_COUNT {
            self.emit();
            self.pending.copy_from_slice(bands_db);
            start += SPECTRO_COLUMN_SECS;
            closed += 1;
        }
        self.window_start = Some(start);
    }

    fn emit(&mut self) {
        let slot = &mut self.columns[self.head];
        slot.copy_from_slice(&self.pending);
        self.head = (self.head + 1) % SPECTRO_COLUMN_COUNT;
        self.filled = (self.filled + 1).min(SPECTRO_COLUMN_COUNT);
        self.revision += 1;
    }
}

/// Temporal smoothing: fast attack, slow release, optional Peak Hold.
/// Coefficients are per frame at ~60 fps (visual layer only).
pub struct Smoother {
    num_bands: usize,
    peak_hold: bool,
    decay: DecaySpeed,
    peak_decay_scale: f32,
    bands: Vec<f32>,
    peaks: Vec<f32>,
}

/// Per-frame decay pacing of the display values and Peak Hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecaySpeed {
    VerySlow,
    Slow,
    Medium,
    Fast,
    VeryFast,
}

impl DecaySpeed {
    /// Fraction of the remaining gap closed on a falling frame.
    fn release(self) -> f32 {
        match self {
            DecaySpeed::VerySlow => 0.008,
            DecaySpeed::Slow => 0.02,
            DecaySpeed::Medium => 0.05,
            DecaySpeed::Fast => 0.12,
            DecaySpeed::VeryFast => 0.25,
        }
    }

    /// Peak Hold decay, dB per frame.
    fn peak_decay(self) -> f32 {
        match self {
            DecaySpeed::VerySlow => 0.1,
            DecaySpeed::Slow => 0.2,
            DecaySpeed::Medium => 0.4,
            DecaySpeed::Fast => 0.8,
            DecaySpeed::VeryFast => 1.6,
        }
    }
}

/// Fraction of the remaining gap closed on a rising frame.
const ATTACK: f32 = 0.85;

impl Smoother {
    pub fn new(num_bands: usize, peak_hold: bool) -> Smoother {
        Smoother {
            num_bands,
            peak_hold,
            decay: DecaySpeed::Medium,
            peak_decay_scale: 1.0,
            bands: vec![DB_FLOOR; num_bands],
            peaks: vec![DB_FLOOR; num_bands],
        }
    }

    /// Sets the decay pacing for falling display values and Peak Hold.
    pub fn set_decay(&mut self, decay: DecaySpeed) {
        self.decay = decay;
    }

    /// Scales only the Peak Hold fall speed; smaller values hold peaks longer.
    pub fn set_peak_decay_scale(&mut self, scale: f32) {
        self.peak_decay_scale = scale.clamp(0.05, 4.0);
    }

    /// Turns Peak Hold on or off without resetting the display state.
    pub fn set_peak_hold(&mut self, on: bool) {
        self.peak_hold = on;
    }

    pub fn process(&mut self, target: &[f32]) -> SmoothedSpectrum {
        debug_assert_eq!(target.len(), self.num_bands);
        for (i, &t) in target.iter().enumerate() {
            let cur = self.bands[i];
            let coef = if t > cur {
                ATTACK
            } else {
                self.decay.release()
            };
            self.bands[i] = cur + coef * (t - cur);
            // 峰值帽钳制:不得低于当前显示柱顶——柱体按剩余距离比例下落
            // (小回落时每帧仅 ~0.1dB),峰帽固定 dB/帧衰减会反超沉入柱内。
            self.peaks[i] = if self.peak_hold {
                t.max(self.peaks[i] - self.decay.peak_decay() * self.peak_decay_scale)
                    .max(self.bands[i])
            } else {
                self.bands[i]
            };
        }
        SmoothedSpectrum {
            bands: self.bands.clone(),
            peaks: self.peaks.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bass Level
// ---------------------------------------------------------------------------

/// Number of lowest Bands averaged into the Bass Level (Q6: one shared scalar
/// so every Effect pulses in sync).
pub const BASS_LEVEL_BANDS: usize = 8;

/// The shared per-frame loudness of the lowest Bands, normalized to 0..1
/// (GLOSSARY: Bass Level). Squared dB mapping: silence collapses to 0, bass
/// hits punch. Feeds Waveform accents and the Particles/Glow/Disc pulse.
pub fn bass_level(bands_db: &[f32]) -> f32 {
    if bands_db.is_empty() {
        return 0.0;
    }
    let n = bands_db.len().clamp(1, BASS_LEVEL_BANDS);
    let span = DB_CEILING - DB_FLOOR;
    let sum: f32 = bands_db[..n]
        .iter()
        .map(|&db| {
            let t = ((db - DB_FLOOR) / span).clamp(0.0, 1.0);
            t * t
        })
        .sum();
    (sum / n as f32).clamp(0.0, 1.0)
}
