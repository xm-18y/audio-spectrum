//! audio-spectrum-app 的纯逻辑层:设置中枢、主题合成、背景图数学。
//!
//! 与窗口/渲染解耦:bin(egui 壳)消费这些类型与函数;全部行为可在
//! 无 GPU 环境下测试。持久化契约:ron 文本、平台惯例目录
//! (`dirs::config_dir()/audio-spectrum/settings.ron`)、损坏或缺失时静默
//! 回默认值(陪伴应用零打扰原则,设计共识 v0.3.0)。

use serde::{Deserialize, Serialize};
use std::path::Path;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos_menu;
#[cfg(target_os = "macos")]
pub use macos_menu::{MenuCommand, NativeMenu};

// ---------------------------------------------------------------------------
// 视图 / 衰减
// ---------------------------------------------------------------------------

/// 哪个画布处于激活态;`S` 键切换。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ViewMode {
    Spectrum,
    Spectrogram,
}

/// 衰减速度档位(持久化形态;运行时映射到 `audio_spectrum_core::DecaySpeed`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DecayChoice {
    Slow,
    Medium,
    Fast,
}

impl From<DecayChoice> for audio_spectrum_core::DecaySpeed {
    fn from(c: DecayChoice) -> Self {
        match c {
            DecayChoice::Slow => audio_spectrum_core::DecaySpeed::Slow,
            DecayChoice::Medium => audio_spectrum_core::DecaySpeed::Medium,
            DecayChoice::Fast => audio_spectrum_core::DecaySpeed::Fast,
        }
    }
}

impl From<audio_spectrum_core::DecaySpeed> for DecayChoice {
    fn from(d: audio_spectrum_core::DecaySpeed) -> Self {
        match d {
            audio_spectrum_core::DecaySpeed::Slow => DecayChoice::Slow,
            audio_spectrum_core::DecaySpeed::Medium => DecayChoice::Medium,
            audio_spectrum_core::DecaySpeed::Fast => DecayChoice::Fast,
        }
    }
}

// ---------------------------------------------------------------------------
// 主题(预设 + 覆盖)
// ---------------------------------------------------------------------------

/// 内置主题预设。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ThemePreset {
    Classic,
    Aurora,
    Magma,
    Neon,
    Amber,
}

impl ThemePreset {
    pub const ALL: &'static [ThemePreset] = &[
        ThemePreset::Classic,
        ThemePreset::Aurora,
        ThemePreset::Magma,
        ThemePreset::Neon,
        ThemePreset::Amber,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ThemePreset::Classic => "经典蓝红",
            ThemePreset::Aurora => "极光",
            ThemePreset::Magma => "熔岩",
            ThemePreset::Neon => "霓虹",
            ThemePreset::Amber => "琥珀",
        }
    }

    fn palette(self) -> Palette {
        match self {
            ThemePreset::Classic => Palette {
                bar_bottom: [56, 92, 224],
                bar_top: [255, 72, 52],
                bg: [12, 14, 19],
                peak: [232, 238, 248],
            },
            ThemePreset::Aurora => Palette {
                bar_bottom: [20, 160, 160],
                bar_top: [130, 255, 175],
                bg: [9, 15, 17],
                peak: [205, 255, 235],
            },
            ThemePreset::Magma => Palette {
                bar_bottom: [140, 30, 20],
                bar_top: [255, 200, 60],
                bg: [17, 10, 8],
                peak: [255, 232, 205],
            },
            ThemePreset::Neon => Palette {
                bar_bottom: [170, 40, 220],
                bar_top: [90, 240, 255],
                bg: [14, 10, 20],
                peak: [240, 220, 255],
            },
            ThemePreset::Amber => Palette {
                bar_bottom: [125, 70, 10],
                bar_top: [255, 190, 80],
                bg: [18, 14, 8],
                peak: [255, 240, 210],
            },
        }
    }
}

/// 一套渲染调色板(RGB,由 bin 转成 egui 颜色)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    pub bar_bottom: [u8; 3],
    pub bar_top: [u8; 3],
    pub bg: [u8; 3],
    pub peak: [u8; 3],
}

/// 频谱柱渐变:基色 → 顶色线性插值。
pub fn band_color(p: &Palette, t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    [
        mix(p.bar_bottom[0], p.bar_top[0]),
        mix(p.bar_bottom[1], p.bar_top[1]),
        mix(p.bar_bottom[2], p.bar_top[2]),
    ]
}

/// 样式设置:预设打底 + 逐槽颜色覆盖。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct StyleSettings {
    pub theme: ThemePreset,
    pub bar_bottom: Option<[u8; 3]>,
    pub bar_top: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub peak: Option<[u8; 3]>,
    /// 是否绘制频率/幅度网格。
    pub grid: bool,
    /// 柱条圆角(px)。
    pub corner_radius: f32,
    /// 柱条间隙占柱宽的比例(0..0.5)。
    pub bar_gap: f32,
}

impl Default for StyleSettings {
    fn default() -> Self {
        StyleSettings {
            theme: ThemePreset::Classic,
            bar_bottom: None,
            bar_top: None,
            bg: None,
            peak: None,
            grid: true,
            corner_radius: 1.0,
            bar_gap: 0.15,
        }
    }
}

/// 预设调色板 + 覆盖项 → 实际渲染调色板。
pub fn resolve_palette(style: &StyleSettings) -> Palette {
    let mut p = style.theme.palette();
    if let Some(c) = style.bar_bottom {
        p.bar_bottom = c;
    }
    if let Some(c) = style.bar_top {
        p.bar_top = c;
    }
    if let Some(c) = style.bg {
        p.bg = c;
    }
    if let Some(c) = style.peak {
        p.peak = c;
    }
    p
}

// ---------------------------------------------------------------------------
// 背景图
// ---------------------------------------------------------------------------

/// 背景图与窗口的几何关系。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ScaleMode {
    /// 填满(cover):放大裁剪,不留边。
    Fill,
    /// 适应(contain):完整可见,留边。
    Fit,
    /// 平铺(tile):自然尺寸重复(需 Repeat 纹理 wrap)。
    Tile,
}

impl ScaleMode {
    pub fn name(self) -> &'static str {
        match self {
            ScaleMode::Fill => "填满",
            ScaleMode::Fit => "适应",
            ScaleMode::Tile => "平铺",
        }
    }
}

/// 背景图设置。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct BackgroundSettings {
    pub path: Option<std::path::PathBuf>,
    pub mode: ScaleMode,
    /// 图像不透明度 0..1(其余透出调色板背景色)。
    pub opacity: f32,
    /// 预烘焙盒式模糊半径(px);0 = 不模糊。设计共识:不做实时 shader。
    pub blur_px: u32,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        BackgroundSettings {
            path: None,
            mode: ScaleMode::Fill,
            opacity: 1.0,
            blur_px: 0,
        }
    }
}

/// 简单矩形(pos + size),lib 层无 egui 依赖。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// 计算背景图的绘制矩形与 uv 采样区。
/// 返回 `(目标子矩形, 源 uv 子矩形)`;Tile 模式下 uv 可能超出 [0,1]
/// (配合 Repeat wrap 实现平铺)。
pub fn fit_rects(src: [f32; 2], dst: Rect, mode: ScaleMode) -> (Rect, Rect) {
    let full_uv = Rect {
        pos: [0.0, 0.0],
        size: [1.0, 1.0],
    };
    match mode {
        ScaleMode::Fill => {
            // 覆盖缩放:源中可见区域 = dst / cover_scale,居中裁剪。
            let scale = (dst.size[0] / src[0]).max(dst.size[1] / src[1]);
            let vis = [dst.size[0] / scale, dst.size[1] / scale];
            let uv = Rect {
                pos: [
                    (src[0] - vis[0]) * 0.5 / src[0],
                    (src[1] - vis[1]) * 0.5 / src[1],
                ],
                size: [vis[0] / src[0], vis[1] / src[1]],
            };
            (dst, uv)
        }
        ScaleMode::Fit => {
            // 完整缩放:目标内居中留边。
            let scale = (dst.size[0] / src[0]).min(dst.size[1] / src[1]);
            let shown = [src[0] * scale, src[1] * scale];
            let d = Rect {
                pos: [
                    dst.pos[0] + (dst.size[0] - shown[0]) * 0.5,
                    dst.pos[1] + (dst.size[1] - shown[1]) * 0.5,
                ],
                size: shown,
            };
            (d, full_uv)
        }
        ScaleMode::Tile => {
            // 自然尺寸 1:1 重复:uv 跨度 = 窗口尺寸 / 图尺寸。
            let uv = Rect {
                pos: [0.0, 0.0],
                size: [dst.size[0] / src[0], dst.size[1] / src[1]],
            };
            (dst, uv)
        }
    }
}

/// 预烘焙盒式模糊(水平 + 垂直各一遭,边缘 clamp)。原地修改。
pub fn blur_rgba(pixels: &mut [[u8; 4]], width: usize, height: usize, radius: usize) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    let mut tmp = pixels.to_vec();
    // 水平(滑动窗口,边缘 clamp)。
    for y in 0..height {
        let base = y * width;
        let row = &mut tmp[base..base + width];
        let src = &pixels[base..base + width];
        let mut acc = [0u32; 3];
        let mut count = radius.min(width - 1) + 1;
        for p in &src[..count] {
            for c in 0..3 {
                acc[c] += p[c] as u32;
            }
        }
        for (x, cell) in row.iter_mut().enumerate() {
            for c in 0..3 {
                cell[c] = (acc[c] / count as u32) as u8;
            }
            cell[3] = 255;
            let out_idx = x.saturating_sub(radius);
            let in_idx = (x + radius + 1).min(width - 1);
            for c in 0..3 {
                acc[c] = acc[c].saturating_sub(src[out_idx][c] as u32) + src[in_idx][c] as u32;
            }
            count = (in_idx - out_idx).min(radius * 2 + 1);
        }
    }
    // 垂直(同法,作用于水平结果)。
    for x in 0..width {
        let mut acc = [0u32; 3];
        let mut count = radius.min(height - 1) + 1;
        for p in tmp[x..count * width].iter().step_by(width) {
            for c in 0..3 {
                acc[c] += p[c] as u32;
            }
        }
        let column: Vec<usize> = (0..height).map(|y| y * width + x).collect();
        for (y, idx) in column.into_iter().enumerate() {
            let cell = &mut pixels[idx];
            for c in 0..3 {
                cell[c] = (acc[c] / count as u32) as u8;
            }
            cell[3] = 255;
            let out_idx = y.saturating_sub(radius);
            let in_idx = (y + radius + 1).min(height - 1);
            for c in 0..3 {
                acc[c] = acc[c].saturating_sub(tmp[out_idx * width + x][c] as u32)
                    + tmp[in_idx * width + x][c] as u32;
            }
            count = (in_idx - out_idx).min(radius * 2 + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// 语谱像素
// ---------------------------------------------------------------------------

/// 语谱纹理像素(行主序:行 = Band,顶部 = 高频;列 = 时间,右 = 最新)。
/// 颜色 = 调色板背景色向柱渐变色按强度混合;`bg_on` 时 alpha 随强度
/// (静音透明,透出背景图),否则不透明。
pub fn spectro_pixels(
    sg: &audio_spectrum_core::Spectrograph,
    pal: &Palette,
    bg_on: bool,
) -> Vec<[u8; 4]> {
    let bands = sg.num_bands();
    let count = audio_spectrum_core::SPECTRO_COLUMN_COUNT;
    let span = audio_spectrum_core::DB_CEILING - audio_spectrum_core::DB_FLOOR;
    let mut out = Vec::with_capacity(count * bands);
    for band in (0..bands).rev() {
        for x in 0..count {
            let age = count - 1 - x;
            let db = sg
                .column(age)
                .map(|c| c[band])
                .unwrap_or(audio_spectrum_core::DB_FLOOR);
            let t = ((db - audio_spectrum_core::DB_FLOOR) / span).clamp(0.0, 1.0);
            let hot = band_color(pal, t);
            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            let alpha = if bg_on { (t * 255.0) as u8 } else { 255 };
            out.push([
                mix(pal.bg[0], hot[0]),
                mix(pal.bg[1], hot[1]),
                mix(pal.bg[2], hot[2]),
                alpha,
            ]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 设置中枢
// ---------------------------------------------------------------------------

/// 全部持久化设置(设计共识 v0.3.0:一个 struct 一路 serde)。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub view: ViewMode,
    pub always_on_top: bool,
    pub band_count: usize,
    pub peak_hold: bool,
    pub decay: DecayChoice,
    pub style: StyleSettings,
    pub background: BackgroundSettings,
    /// 启动即进入全屏。
    pub default_fullscreen: bool,
    /// 记住上次窗口尺寸与位置。
    pub remember_window: bool,
    /// 显示性能 HUD(右下角浮层;默认关闭,纯视觉)。
    pub show_hud: bool,
    /// 上次窗口矩形 [x, y, w, h]。
    pub window_rect: Option<[f32; 4]>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            view: ViewMode::Spectrum,
            always_on_top: false,
            band_count: 96,
            peak_hold: true,
            decay: DecayChoice::Medium,
            style: StyleSettings::default(),
            background: BackgroundSettings::default(),
            default_fullscreen: false,
            remember_window: false,
            show_hud: false,
            window_rect: None,
        }
    }
}

/// 平台惯例配置目录(`~/Library/Application Support` / `%APPDATA%` / `~/.config`)。
pub fn config_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("audio-spectrum"))
}

/// 一次性迁移:应用更名前(music-spec)的旧配置,搬入新目录。
/// 目标已存在或旧文件缺失时为空操作。
pub fn migrate_settings(old: &Path, new: &Path) {
    if new.exists() || !old.exists() {
        return;
    }
    let settings = load_from(old);
    let _ = save_to(&settings, new);
}

/// 配置文件路径;`None` 表示该平台无法定位配置目录。
pub fn settings_path() -> Option<std::path::PathBuf> {
    config_dir().map(|d| d.join("settings.ron"))
}

/// 从指定路径读取;缺失、损坏或解析失败 → 静默回默认值。
pub fn load_from(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| ron::from_str(&text).ok())
        .unwrap_or_default()
}

/// 保存到指定路径(创建父目录)。
pub fn save_to(settings: &Settings, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = ron::to_string(settings)
        .map_err(|e| std::io::Error::other(format!("ron 序列化失败:{e}")))?;
    std::fs::write(path, text)
}

/// 默认路径的读取便捷封装(含一次性别名迁移)。
pub fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    if let Some(old_dir) = dirs::config_dir().map(|d| d.join("music-spec")) {
        migrate_settings(&old_dir.join("settings.ron"), &path);
    }
    load_from(&path)
}

/// 默认路径的保存便捷封装。
pub fn save(settings: &Settings) -> std::io::Result<()> {
    match settings_path() {
        Some(p) => save_to(settings, &p),
        None => Err(std::io::Error::other("无法定位配置目录")),
    }
}
