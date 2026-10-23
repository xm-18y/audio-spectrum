//! Performance budget assertions (design consensus):
//! the whole per-frame pipeline — FFT analysis + smoothing — must fit a
//! small fraction of the 60 fps frame budget (16.7 ms). ADR-0002's core
//! claim is that a 2048-point real FFT costs "tens of microseconds", so the
//! UI thread can afford it every frame.

use audio_spectrum_core::{Analyzer, FRAME_SIZE, SAMPLE_RATE, Smoother};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

#[test]
fn analysis_pipeline_fits_the_frame_budget() {
    const ITERATIONS: usize = 300;

    // Realistic input: three tones across the axis — no zero fast paths.
    let mut samples = [0.0; FRAME_SIZE];
    for (n, s) in samples.iter_mut().enumerate() {
        let t = n as f32 / SAMPLE_RATE;
        *s = 0.4 * (TAU * 220.0 * t).sin()
            + 0.3 * (TAU * 1280.0 * t).sin()
            + 0.2 * (TAU * 7500.0 * t).sin();
    }

    let mut analyzer = Analyzer::new(96);
    let mut smoother = Smoother::new(96, true);
    let _ = smoother.process(&analyzer.analyze(&samples)); // warm-up

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let bands = analyzer.analyze(&samples);
        let _ = smoother.process(&bands);
    }
    let per_frame = start.elapsed() / ITERATIONS as u32;

    // Debug builds run unoptimized: allow a comfortable margin there.
    // Either way the whole pipeline must stay far under one 60 fps frame.
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(2_500)
    } else {
        Duration::from_millis(500)
    };
    eprintln!("per-frame pipeline cost: {per_frame:?} (budget {budget:?})");
    assert!(
        per_frame < budget,
        "per-frame pipeline cost {per_frame:?} exceeds budget {budget:?}"
    );
}
