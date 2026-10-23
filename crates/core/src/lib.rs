#![forbid(unsafe_code)]
//! music-spec-core — the Spectrum pipeline: PCM samples in, per-Band dBFS out.
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

/// Temporal smoothing: fast attack, slow release, optional Peak Hold.
/// Coefficients are per frame at ~60 fps (visual layer only).
pub struct Smoother {
    num_bands: usize,
    peak_hold: bool,
    bands: Vec<f32>,
    peaks: Vec<f32>,
}

/// Fraction of the remaining gap closed on a rising frame.
const ATTACK: f32 = 0.85;
/// Fraction of the remaining gap closed on a falling frame.
const RELEASE: f32 = 0.05;
/// Peak Hold decay, dB per frame.
const PEAK_DECAY_DB: f32 = 0.4;

impl Smoother {
    pub fn new(num_bands: usize, peak_hold: bool) -> Smoother {
        Smoother {
            num_bands,
            peak_hold,
            bands: vec![DB_FLOOR; num_bands],
            peaks: vec![DB_FLOOR; num_bands],
        }
    }

    pub fn process(&mut self, target: &[f32]) -> SmoothedSpectrum {
        debug_assert_eq!(target.len(), self.num_bands);
        for (i, &t) in target.iter().enumerate() {
            let cur = self.bands[i];
            let coef = if t > cur { ATTACK } else { RELEASE };
            self.bands[i] = cur + coef * (t - cur);
            self.peaks[i] = if self.peak_hold {
                t.max(self.peaks[i] - PEAK_DECAY_DB)
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
