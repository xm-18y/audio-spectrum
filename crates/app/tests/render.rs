//! 语谱渲染数据链(历史 → 像素)的无 GPU 验证。

use audio_spectrum_app::{StyleSettings, ThemePreset, resolve_palette, spectro_pixels};
use audio_spectrum_core::{DB_FLOOR, SPECTRO_COLUMN_COUNT, Spectrograph};

#[test]
fn spectro_pixels_light_the_newest_column() {
    let bands = 96;
    let mut sg = Spectrograph::new(bands);
    sg.push_frame(0.0, &vec![DB_FLOOR; bands]);
    sg.push_frame(0.05, &vec![0.0; bands]);
    sg.push_frame(0.10, &vec![0.0; bands]);

    let pal = resolve_palette(&StyleSettings {
        theme: ThemePreset::Classic,
        ..Default::default()
    });

    // 不透明模式:静音历史 = 背景色,最新列有能量。
    // 扁平 RGBA:每像素 4 字节,列 = 时间(右最新),行 = Band。
    let px = spectro_pixels(&sg, &pal, false);
    assert_eq!(px.len(), SPECTRO_COLUMN_COUNT * bands * 4);
    let newest = (SPECTRO_COLUMN_COUNT - 1) * 4;
    assert_eq!(px[newest + 3], 255, "opaque mode keeps alpha");
    assert_eq!(px[0], pal.bg[0], "silent history stays at bg color");
    assert_ne!(px[newest], px[0], "newest column carries energy");

    // 背景模式:静音 alpha = 0(透出背景图),响亮列 alpha > 0。
    let px = spectro_pixels(&sg, &pal, true);
    assert_eq!(px[3], 0, "silence is transparent over background");
    assert!(
        px[newest + 3] > 200,
        "full-scale stays visible"
    );
}
