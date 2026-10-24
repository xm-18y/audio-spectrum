use audio_spectrum_core::{bass_level, DB_CEILING, DB_FLOOR};

#[test]
fn silence_is_zero() {
    assert_eq!(bass_level(&[DB_FLOOR; 96]), 0.0);
}

#[test]
fn full_scale_bass_is_one() {
    let v = bass_level(&[DB_CEILING; 96]);
    assert!((v - 1.0).abs() < 1e-6, "got {v}");
}

#[test]
fn only_the_lowest_bands_count() {
    // Loud highs alone must not move the Bass Level.
    let mut bands = vec![DB_FLOOR; 96];
    for db in bands.iter_mut().skip(8) {
        *db = DB_CEILING;
    }
    assert_eq!(bass_level(&bands), 0.0);
    // Loud lows do.
    bands[0] = DB_CEILING;
    assert!(bass_level(&bands) > 0.0);
}

#[test]
fn partial_level_is_quadratic_in_db() {
    // 6 dB below ceiling = half amplitude = quarter power.
    let bands = [DB_CEILING - 6.0];
    let v = bass_level(&bands);
    assert!(v > 0.5 && v < 1.0, "got {v}");
}

#[test]
fn short_inputs_are_accepted() {
    assert_eq!(bass_level(&[]), 0.0);
    assert!(bass_level(&[DB_CEILING]) <= 1.0);
}
