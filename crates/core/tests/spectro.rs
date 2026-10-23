//! Spectrograph behavior: aggregation of Band frames into a scrolling
//! column history (design: 20 ms column interval, 12 s history, log Band
//! axis — ADR-0003). Pure computation; time is a parameter, so tests are
//! headless.

use music_spec_core::{DB_FLOOR, Spectrograph};

fn bands(num: usize, value: f32) -> Vec<f32> {
    vec![value; num]
}

#[test]
fn fresh_spectrograph_is_empty_and_first_frame_only_opens_a_window() {
    let mut sg = Spectrograph::new(96);
    assert_eq!(sg.num_bands(), 96);
    assert_eq!(sg.len(), 0);
    assert!(sg.column(0).is_none(), "empty history must expose no columns");

    // The first frame opens the first aggregation window; nothing is
    // emitted until an interval has actually passed.
    sg.push_frame(0.0, &bands(96, -30.0));
    assert_eq!(sg.len(), 0, "no column before the interval elapses");
}

#[test]
fn columns_carry_the_window_maximum_at_the_interval_cadence() {
    let mut sg = Spectrograph::new(4);

    // t=0 opens the first window; t=0.019 falls inside it.
    sg.push_frame(0.0, &[-40.0; 4]);
    sg.push_frame(0.019, &[-10.0; 4]);
    assert_eq!(sg.len(), 0, "no column before an interval elapses");

    // t=0.021 closes window 1 (must hold -10, the window max, NOT -30),
    // then folds -30 into the newly opened window 2.
    sg.push_frame(0.021, &[-30.0; 4]);
    assert_eq!(sg.len(), 1);
    assert_eq!(sg.column(0), Some(&[-10.0; 4][..]));

    // t=0.045 closes window 2 (holding -30) and folds -20 into window 3.
    sg.push_frame(0.045, &[-20.0; 4]);
    assert_eq!(sg.len(), 2);
    assert_eq!(sg.column(0), Some(&[-30.0; 4][..]));
    assert_eq!(sg.column(1), Some(&[-10.0; 4][..]));
}
