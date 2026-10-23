use music_spec_core::{Analyzer, DB_FLOOR, FRAME_SIZE, SAMPLE_RATE};

#[test]
fn silence_reads_floor_on_every_band() {
    let mut analyzer = Analyzer::new(96);
    let bands = analyzer.analyze(&[0.0; FRAME_SIZE]);
    assert_eq!(bands.len(), 96);
    assert!(bands.iter().all(|&db| db == DB_FLOOR), "got {bands:?}");
}

#[test]
fn band_centers_are_log_spaced_over_the_audio_range() {
    let analyzer = Analyzer::new(96);
    assert_eq!(analyzer.num_bands(), 96);

    // Agreed axis: 20 Hz–20 kHz, geometric. r = (20000/20)^(1/96) = 1.0746.
    let first = analyzer.band_center(0);
    let last = analyzer.band_center(95);
    assert!((first - 20.73).abs() < 0.1, "first center {first}");
    assert!((last - 19_330.0).abs() < 40.0, "last center {last}");
    for i in 0..95 {
        let ratio = analyzer.band_center(i + 1) / analyzer.band_center(i);
        assert!((ratio - 1.0746).abs() < 1e-3, "ratio at {i}: {ratio}");
    }

    assert_eq!(analyzer.band_of(20.0), 0);
    assert_eq!(analyzer.band_of(20_000.0), 95);
    for i in (0..96).step_by(7) {
        assert_eq!(analyzer.band_of(analyzer.band_center(i)), i);
    }
}

/// A full-scale sine at an exact FFT Bin center must read 0 dBFS in its Band:
/// this fixes the whole calibration chain (Hann window gain → FFT → dB scale).
#[test]
fn full_scale_sine_at_a_bin_center_reads_about_zero_db() {
    let mut analyzer = Analyzer::new(96);
    // Bin 100 → 100 · 48000/2048 = 2343.75 Hz.
    let freq = 100.0 * SAMPLE_RATE / FRAME_SIZE as f32;
    let mut samples = [0.0; FRAME_SIZE];
    for (n, s) in samples.iter_mut().enumerate() {
        *s = (std::f32::consts::TAU * freq * n as f32 / SAMPLE_RATE).sin();
    }

    let bands = analyzer.analyze(&samples);
    let i = analyzer.band_of(freq);
    assert!(
        -0.3 < bands[i] && bands[i] < 0.1,
        "band {i} reads {} dB",
        bands[i]
    );
    let loudest = bands
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0;
    assert_eq!(loudest, i);
}

/// A 55 Hz tone falls between Bin centers; its Band must interpolate the
/// surrounding Bins instead of collapsing to the floor.
#[test]
fn a_tone_below_bin_width_still_lights_its_band() {
    let mut analyzer = Analyzer::new(96);
    let samples = sine(55.0, 1.0);
    let bands = analyzer.analyze(&samples);
    let i = analyzer.band_of(55.0);
    assert!(bands[i] > -12.0, "band {i} reads {} dB", bands[i]);
    let far = analyzer.band_of(440.0);
    assert!(bands[far] < -25.0, "far band reads {} dB", bands[far]);
}

fn sine(freq: f32, amplitude: f32) -> [f32; FRAME_SIZE] {
    let mut samples = [0.0; FRAME_SIZE];
    for (n, s) in samples.iter_mut().enumerate() {
        *s = amplitude * (std::f32::consts::TAU * freq * n as f32 / SAMPLE_RATE).sin();
    }
    samples
}

/// An off-Bin-center 440 Hz tone must light its own Band and leave the
/// rest of the axis dark.
#[test]
fn a_tone_at_440hz_is_local_to_its_band() {
    let mut analyzer = Analyzer::new(96);
    let samples = sine(440.0, 1.0);
    let bands = analyzer.analyze(&samples);
    let i = analyzer.band_of(440.0);
    assert!(bands[i] > -12.0, "band {i} reads {} dB", bands[i]);
    assert!(
        bands[i + 3] < -20.0,
        "band {} reads {} dB",
        i + 3,
        bands[i + 3]
    );
    assert!(
        bands[i - 3] < -20.0,
        "band {} reads {} dB",
        i - 3,
        bands[i - 3]
    );
    assert!(bands[analyzer.band_of(220.0)] < -25.0);
    assert!(bands[analyzer.band_of(880.0)] < -25.0);
}

use music_spec_core::Smoother;

/// The smoother must attack fast (bars jump up), release slowly (bars fall
/// gently), and settle back to the floor.
#[test]
fn smoother_attacks_fast_and_releases_slowly() {
    let num = 8;
    let silence = [DB_FLOOR; 8];
    let loud = [0.0; 8];
    let mut smoother = Smoother::new(num, true);

    // Starts settled at the floor.
    let out = smoother.process(&silence);
    assert!(out.bands.iter().all(|&v| v == DB_FLOOR));

    // One loud frame covers most of the distance up (fast attack).
    let out = smoother.process(&loud);
    assert!(
        (-30.0..0.0).contains(&out.bands[3]),
        "attack read {} dB",
        out.bands[3]
    );
    let after_attack = out.bands[3];

    // One silent frame falls only a little (slow release).
    let out = smoother.process(&silence);
    let v = out.bands[3];
    assert!(
        v < after_attack && v > after_attack - 15.0,
        "release read {v} dB"
    );

    // Many silent frames converge back to the floor.
    for _ in 0..300 {
        smoother.process(&silence);
    }
    let out = smoother.process(&silence);
    assert!(
        (out.bands[3] - DB_FLOOR).abs() < 0.5,
        "read {}",
        out.bands[3]
    );
}

/// With Peak Hold on, the held peak decays far slower than the Band value.
#[test]
fn peak_hold_decays_slower_than_the_band() {
    let silence = [DB_FLOOR; 4];
    let loud = [0.0; 4];
    let mut smoother = Smoother::new(4, true);

    smoother.process(&loud);
    for _ in 0..10 {
        smoother.process(&silence);
    }
    let out = smoother.process(&silence);
    assert!(out.peaks[1] > -5.0, "peak reads {} dB", out.peaks[1]);
    assert!(
        out.peaks[1] > out.bands[1],
        "peak {} vs band {}",
        out.peaks[1],
        out.bands[1]
    );
}

/// With Peak Hold off, peaks simply mirror the Band values.
#[test]
fn peak_hold_can_be_disabled() {
    let mut smoother = Smoother::new(4, false);
    smoother.process(&[0.0; 4]);
    for _ in 0..10 {
        smoother.process(&[DB_FLOOR; 4]);
    }
    let out = smoother.process(&[DB_FLOOR; 4]);
    assert_eq!(out.peaks, out.bands);
}

/// Band edges tile the axis exactly: each edge is the next one's start.
#[test]
fn band_edges_tile_the_axis() {
    let a = Analyzer::new(96);
    let (lo, hi) = a.band_edges(0);
    assert!((lo - 20.0).abs() < 1e-3, "lo {lo}");
    assert!((hi - 21.49).abs() < 0.01, "hi {hi}");
    for i in 0..95 {
        let (_, hi) = a.band_edges(i);
        let (next_lo, _) = a.band_edges(i + 1);
        assert!((hi - next_lo).abs() < 1e-3, "seam at {i}");
    }
    let (_, last_hi) = a.band_edges(95);
    assert!((last_hi - 20_000.0).abs() < 0.5, "last_hi {last_hi}");
}

use music_spec_core::DecaySpeed;

/// Faster decay must fall further within the same number of silent frames.
#[test]
fn decay_speed_paces_the_release() {
    let mut slow = Smoother::new(4, true);
    let mut fast = Smoother::new(4, true);
    slow.set_decay(DecaySpeed::Slow);
    fast.set_decay(DecaySpeed::Fast);

    slow.process(&[0.0; 4]);
    fast.process(&[0.0; 4]);
    for _ in 0..10 {
        slow.process(&[DB_FLOOR; 4]);
        fast.process(&[DB_FLOOR; 4]);
    }
    let slow_v = slow.process(&[DB_FLOOR; 4]).bands[1];
    let fast_v = fast.process(&[DB_FLOOR; 4]).bands[1];
    assert!(
        fast_v < slow_v,
        "fast decay must fall further: fast {fast_v} slow {slow_v}"
    );
}
