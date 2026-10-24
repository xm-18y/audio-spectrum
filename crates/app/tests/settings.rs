//! Settings hub behavior: persistence round-trip, silent-default recovery,
//! theme synthesis, background scaling math, blur.
//!
//! Tests run in parallel: every persistence case uses its own temp file
//! (the global-path convenience wrappers are only exercised indirectly).

use audio_spectrum_app::{
    DecayChoice, Rect, ScaleMode, Settings, SpectrumType, StyleSettings, ThemePreset, ViewMode,
    band_color, blur_rgba, fit_rects, load_from, resolve_palette, save_to,
};

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "audio-spectrum-test-{tag}-{}-{}.ron",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ))
}

/// Saved settings must load back identical (ron round-trip).
#[test]
#[allow(clippy::field_reassign_with_default)] // 测试可读性优先:逐字段覆写
fn settings_round_trip() {
    let mut s = Settings::default();
    s.view = ViewMode::Spectrogram;
    s.band_count = 64;
    s.decay = DecayChoice::Fast;
    s.style.theme = ThemePreset::Magma;
    s.style.bar_bottom = Some([200, 40, 20]);
    s.style.grid = false;
    s.style.spectrum_type = SpectrumType::Radial;
    s.style.blocks = true;
    s.style.peak_detached = true;
    s.style.peak_hold = Some([20, 200, 90]);
    s.background.path = Some("/tmp/x.jpg".into());
    s.background.mode = ScaleMode::Tile;
    s.background.opacity = 0.4;
    s.background.blur_px = 6;
    s.default_fullscreen = true;
    s.remember_window = true;
    s.window_rect = Some([10.0, 20.0, 800.0, 500.0]);

    let path = temp_path("roundtrip");
    save_to(&s, &path).expect("save");
    assert_eq!(load_from(&path), s, "round trip must be identity");
}

/// A corrupt settings file must fall back to defaults without panicking —
/// companion apps never nag the user about their own config.
#[test]
fn corrupt_settings_fall_back_to_defaults_silently() {
    let path = temp_path("corrupt");
    std::fs::write(&path, b"{{{ not valid ron !!!").expect("write corrupt file");
    assert_eq!(load_from(&path), Settings::default());
    // 读取不存在的文件同样静默回默认。
    let missing = temp_path("missing");
    assert_eq!(load_from(&missing), Settings::default());
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
    assert_eq!(plain.peak_hold, plain.peak, "peak hold follows peak by default");

    let mut peak_only = style.clone();
    peak_only.peak = Some([9, 8, 7]);
    assert_eq!(resolve_palette(&peak_only).peak_hold, [9, 8, 7]);

    let mut overridden = style.clone();
    overridden.bar_top = Some([250, 250, 250]);
    let pal = resolve_palette(&overridden);
    assert_eq!(pal.bar_top, [250, 250, 250]);
    assert_eq!(pal.bar_bottom, plain.bar_bottom, "unrelated slot untouched");

    // Overrides survive a round trip through persistence.
    let s = Settings {
        style: overridden,
        ..Default::default()
    };
    let path = temp_path("override");
    save_to(&s, &path).unwrap();
    assert_eq!(load_from(&path).style.bar_top, Some([250, 250, 250]));
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
fn fit_rects_covers_fits_and_tiles() {
    let dst = Rect {
        pos: [0.0, 0.0],
        size: [200.0, 200.0],
    };

    // Fill(cover):2:1 的图在 1:1 的窗上 → 放大到高 200、宽 400,左右各裁 100。
    let (d, uv) = fit_rects([400.0, 200.0], dst, ScaleMode::Fill);
    assert_eq!(d, dst, "fill covers the whole destination");
    assert!((uv.pos[0] - 0.25).abs() < 1e-5, "uv.x0 {}", uv.pos[0]);
    assert!((uv.pos[1] - 0.0).abs() < 1e-5);
    assert!((uv.size[0] - 0.5).abs() < 1e-5, "uv.w {}", uv.size[0]);
    assert!((uv.size[1] - 1.0).abs() < 1e-5);

    // Fit(contain):整图可见,上下留边 → 目标内缩,uv 全幅。
    let (d, uv) = fit_rects([400.0, 200.0], dst, ScaleMode::Fit);
    assert_eq!(uv.pos, [0.0, 0.0]);
    assert_eq!(uv.size, [1.0, 1.0]);
    assert_eq!(d.pos, [0.0, 50.0]);
    assert_eq!(d.size, [200.0, 100.0]);

    // Tile:100×50 的图在 200×200 的窗上 → 自然尺寸平铺 2×4。
    let (d, uv) = fit_rects([100.0, 50.0], dst, ScaleMode::Tile);
    assert_eq!(d, dst, "tile covers the whole destination");
    assert_eq!(uv.pos, [0.0, 0.0]);
    assert_eq!(uv.size, [2.0, 4.0]);
}

/// Box blur must spread a dot and leave far corners untouched.
#[test]
fn blur_spreads_a_hot_pixel() {
    // 5×5 黑底,中心白点。
    let w = 5usize;
    let mut px = vec![[0u8, 0, 0, 255]; w * w];
    px[12] = [255, 255, 255, 255];
    blur_rgba(&mut px, w, 5, 1);
    assert!(px[12][0] < 255, "center dimmed");
    assert!(px[11][0] > 0, "horizontal neighbor heated");
    assert!(px[7][0] > 0, "vertical neighbor heated");
    assert_eq!(px[0][0], 0, "far corner untouched");
    assert_eq!(px[12][3], 255, "alpha preserved");
}

/// 更名迁移:旧目录的设置搬到新目录,目标已存在时不覆盖。
#[test]
fn migration_moves_old_settings_once() {
    use audio_spectrum_app::migrate_settings;
    let old = temp_path("migrate-old");
    let new = temp_path("migrate-new");
    let mut s = Settings::default();
    s.style.theme = ThemePreset::Neon;
    save_to(&s, &old).unwrap();

    migrate_settings(&old, &new);
    assert_eq!(load_from(&new).style.theme, ThemePreset::Neon, "迁移后可读");

    // 目标已存在时不覆盖既有内容。
    let mut newer = Settings::default();
    newer.style.theme = ThemePreset::Magma;
    save_to(&newer, &new).unwrap();
    migrate_settings(&old, &new);
    assert_eq!(
        load_from(&new).style.theme,
        ThemePreset::Magma,
        "不得覆盖新目录的既有设置"
    );
}
