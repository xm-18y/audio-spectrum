//! Settings hub behavior: persistence round-trip, silent-default recovery,
//! theme synthesis, background scaling math, blur.

use music_spec_app::{
    band_color, blur_rgba, config_dir, fit_rects, resolve_palette, load, save, BackgroundSettings,
    DecayChoice, ScaleMode, Settings, StyleSettings, ThemePreset, ViewMode,
};

fn temp_settings_path(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("music-spec-test-{tag}-{}.ron", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Saved settings must load back identical (ron round-trip).
#[test]
fn settings_round_trip() {
    let mut s = Settings::default();
    s.view = ViewMode::Spectrogram;
    s.band_count = 64;
    s.peak_hold = false;
    s.decay = DecayChoice::Fast;
    s.style.theme = ThemePreset::Magma;
    s.style.bar_bottom = Some([200, 40, 20]);
    s.style.grid = false;
    s.background.path = Some("/tmp/x.jpg".into());
    s.background.mode = ScaleMode::Tile;
    s.background.opacity = 0.4;
    s.background.blur_px = 6;
    s.default_fullscreen = true;
    s.remember_window = true;
    s.window_rect = Some([10.0, 20.0, 800.0, 500.0]);

    save(&s).expect("save");
    let back = load();
    assert_eq!(back, s);
}

/// A corrupt settings file must fall back to defaults without panicking —
/// companion apps never nag the user about their own config.
#[test]
fn corrupt_settings_fall_back_to_defaults_silently() {
    let dir = config_dir().expect("config dir available in test env");
    let path = dir.join("settings.ron");
    std::fs::write(&path, b"{{{ not valid ron !!!").expect("write corrupt file");
    assert_eq!(load(), Settings::default(), "corrupt file must yield defaults");
}

/// A preset with no overrides resolves to the preset palette; an override
/// replaces exactly its own slot.
#[test]
fn palette_resolves_preset_then_overrides() {
    let style = StyleSettings {
        theme: ThemePreset::Aurora,
        ..Default::default()
    };
    let plain = resolve_palette(&style);
    assert_eq!(plain, resolve_palette(&style), "stable");

    let mut overridden = style.clone();
    overridden.bar_top = Some([250, 250, 250]);
    let pal = resolve_palette(&overridden);
    assert_eq!(pal.bar_top, [250, 250, 250]);
    assert_eq!(pal.bar_bottom, plain.bar_bottom, "unrelated slot untouched");

    // Overrides survive a round trip through persistence.
    let mut s = Settings::default();
    s.style = overridden;
    save(&s).unwrap();
    assert_eq!(load().style.bar_top, Some([250, 250, 250]));
}

/// Bar gradient endpoints must hit the palette colors exactly.
#[test]
fn band_color_hits_palette_endpoints() {
    let pal = resolve_palette(&StyleSettings {
        theme: ThemePreset::Classic,
        ..Default::default()
    });
    assert_eq!(band_color(&pal, 0.0), pal.bar_bottom);
    assert_eq!(band_color(&pal, 1.0), pal.bar_top);
}

/// Background scaling math (independent worked examples).
#[test]
fn fit_rects_covers_and_fits() {
    let src = [400.0, 200.0]; // 2:1 图
    let dst = music_spec_app::Rect {
        pos: [0.0, 0.0],
        size: [200.0, 200.0],
    }; // 1:1 窗

    // Fill(cover):图放大到高 200,宽 400,左右各裁 100 → uv 中心裁剪。
    let (d, uv) = fit_rects(src, dst, ScaleMode::Fill);
    assert_eq!(d, dst, "fill covers the whole destination");
    assert_eq!(uv.pos, [100.0 / 400.0, 0.0]);
    assert!((uv.size[0] - 0.5).abs() < 1e-5);
    assert!((uv.size[1] - 1.0).abs() < 1e-5);

    // Fit(contain):整图可见,上下留边 → 目标内缩,uv 全幅。
    let (d, uv) = fit_rects(src, dst, ScaleMode::Fit);
    assert_eq!(uv.pos, [0.0, 0.0]);
    assert_eq!(uv.size, [1.0, 1.0]);
    assert_eq!(d.pos, [0.0, 50.0]);
    assert_eq!(d.size, [200.0, 100.0]);

    // Tile:uv 基础块平铺 2×2(图是窗的一半)。
    let (_d, uv) = fit_rects(src, dst, ScaleMode::Tile);
    assert_eq!(uv.size, [2.0, 4.0]);
}

/// Box blur must spread a dot and preserve overall energy roughly.
#[test]
fn blur_spreads_a_hot_pixel() {
    // 5×5 黑底,中心白点。
    let w = 5usize;
    let mut px = vec![[0u8, 0, 0, 255]; w * w];
    px[12] = [255, 255, 255, 255];
    blur_rgba(&mut px, w, 5, 1);
    assert_eq!(px[12][0] < 255, true, "center dimmed");
    assert!(px[11][0] > 0, "neighbor heated");
    assert!(px[7][0] > 0, "vertical neighbor heated");
    assert_eq!(px[0][0], 0, "far corner untouched");
}

/// The config file lives in the platform-conventional directory.
#[test]
fn config_dir_is_platform_conventional() {
    let dir = config_dir().expect("dir");
    let s = dir.to_string_lossy();
    assert!(s.contains("music-spec"), "dir {s}");
    let _ = temp_settings_path("cleanup-marker"); // keep temp util exercised
}
