//! audio-spectrum-app 的纯逻辑层:设置中枢、主题合成、背景图数学。
//!
//! 与窗口/渲染解耦:bin(iced 壳)消费这些类型与函数;全部行为可在
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

pub mod autostart;
pub mod keep_awake;

// ---------------------------------------------------------------------------
// 视图 / 衰减
// ---------------------------------------------------------------------------

/// 频段数可调下限(0 会让 `Analyzer::new` 断言)。
pub const BAND_COUNT_MIN: usize = 1;
/// 频段数可调上限(对数轴上再细分已无意义)。
pub const BAND_COUNT_MAX: usize = 255;

/// 哪个画布处于激活态;`S` 键循环切换全部视图(GLOSSARY:View)。
/// 有序清单:频谱 → 语谱 → 波形 → 粒子 → 光效 → CD → 静态 → 屏保。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ViewMode {
    Spectrum,
    Spectrogram,
    Waveform,
    Particles,
    Glow,
    Disc,
    Static,
    Screensaver,
}

impl ViewMode {
    /// 循环/菜单/tab 共用的视图顺序(设计共识 v0.8.0)。
    pub const ALL: &'static [ViewMode] = &[
        ViewMode::Spectrum,
        ViewMode::Spectrogram,
        ViewMode::Waveform,
        ViewMode::Particles,
        ViewMode::Glow,
        ViewMode::Disc,
        ViewMode::Static,
        ViewMode::Screensaver,
    ];

    /// 菜单/tab 显示名。
    pub fn name(self) -> &'static str {
        match self {
            ViewMode::Spectrum => "频谱",
            ViewMode::Spectrogram => "语谱",
            ViewMode::Waveform => "波形",
            ViewMode::Particles => "粒子",
            ViewMode::Glow => "光效",
            ViewMode::Disc => "CD",
            ViewMode::Static => "静态",
            ViewMode::Screensaver => "屏保",
        }
    }

    /// `S` 键循环:清单中的下一个(末尾回绕)。
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

// ---------------------------------------------------------------------------
// 各视图选项(per-View 设置节;旧 settings.ron 缺节取默认,ADR-0005)
// ---------------------------------------------------------------------------

/// 波形视图的配色来源。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum WaveformColor {
    /// 按幅度取主题渐变色(响处偏顶色)。
    Gradient,
    /// 固定顶色。
    Top,
    /// 固定底色。
    Bottom,
}

impl WaveformColor {
    pub const ALL: &'static [WaveformColor] = &[
        WaveformColor::Gradient,
        WaveformColor::Top,
        WaveformColor::Bottom,
    ];
    pub fn name(self) -> &'static str {
        match self {
            WaveformColor::Gradient => "渐变",
            WaveformColor::Top => "顶色",
            WaveformColor::Bottom => "底色",
        }
    }
}

/// 波形视图选项(频谱 tab 平行;原始样本帧驱动,零采集改动)。
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct WaveformSettings {
    /// 镜像填充(上下对称)否则单线。
    pub mirrored: bool,
    /// 线宽(px)。
    pub line_width: f32,
    /// 配色来源(自定义色设置时优先,见 `custom_color`)。
    pub color: WaveformColor,
    /// 幅度增益(原始样本振幅通常远小于 1;1 = 原样,16 = 小信号也撑满)。
    pub gain: f32,
    /// 自定义单色;None = 按 `color` 走主题色。
    pub custom_color: Option<[u8; 3]>,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        WaveformSettings {
            mirrored: true,
            line_width: 2.0,
            color: WaveformColor::Gradient,
            gain: 4.0,
            custom_color: None,
        }
    }
}

/// 粒子发射样式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ParticleEmission {
    /// 底部喷泉:全屏宽度自底部喷发、受重力回落。
    Fountain,
    /// 环绕星环:绕中心旋转、半径随 Bass Level 脉动。
    Ring,
    /// 全屏星雨:自顶部匀速洒落,落底回炉。
    Rain,
    /// 星云:全屏缓慢漂移,Bass Level 呼吸膨胀。
    Nebula,
}

impl ParticleEmission {
    pub const ALL: &'static [ParticleEmission] = &[
        ParticleEmission::Fountain,
        ParticleEmission::Ring,
        ParticleEmission::Rain,
        ParticleEmission::Nebula,
    ];
    pub fn name(self) -> &'static str {
        match self {
            ParticleEmission::Fountain => "底部喷泉",
            ParticleEmission::Ring => "环绕星环",
            ParticleEmission::Rain => "全屏星雨",
            ParticleEmission::Nebula => "星云",
        }
    }
}

/// 粒子视图选项。
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct ParticleSettings {
    /// 粒子池上限。
    pub count: u32,
    /// 粒子基础半径(px)。
    pub size: f32,
    /// 拖尾长度系数(0 = 无拖尾)。
    pub trail: f32,
    pub emission: ParticleEmission,
    /// 自定义单色;None = 按能量走主题渐变。
    pub custom_color: Option<[u8; 3]>,
}

impl Default for ParticleSettings {
    fn default() -> Self {
        ParticleSettings {
            count: 140,
            size: 3.0,
            trail: 1.0,
            emission: ParticleEmission::Fountain,
            custom_color: None,
        }
    }
}

/// 光效布局。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum GlowLayout {
    /// 径向:光斑绕中心圆环排布。
    Radial,
    /// 网格:光斑铺满画布。
    Grid,
    /// 随机:光斑随机散布全屏,缓慢漂移。
    Random,
}

impl GlowLayout {
    pub const ALL: &'static [GlowLayout] = &[
        GlowLayout::Radial,
        GlowLayout::Grid,
        GlowLayout::Random,
    ];
    pub fn name(self) -> &'static str {
        match self {
            GlowLayout::Radial => "径向",
            GlowLayout::Grid => "网格",
            GlowLayout::Random => "随机",
        }
    }
}

/// 光效律动视图选项(iced canvas 无 shader:分层半透明圆模拟辉光)。
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct GlowSettings {
    /// 光斑数量(1–64)。
    pub count: u32,
    pub layout: GlowLayout,
    /// 脉动强度 0..1。
    pub intensity: f32,
    /// 色相漂移:光斑颜色随时间轮转主题渐变。
    pub hue_drift: bool,
    /// 自定义单色;None = 主题渐变(漂移)。
    pub custom_color: Option<[u8; 3]>,
}

impl Default for GlowSettings {
    fn default() -> Self {
        GlowSettings {
            count: 12,
            layout: GlowLayout::Random,
            intensity: 0.6,
            hue_drift: true,
            custom_color: None,
        }
    }
}

/// CD 视图选项(网易云式旋转唱片)。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct DiscSettings {
    /// 转速系数(1 = 基准 33⅓ rpm 观感)。
    pub speed: f32,
    /// Bass Level 律动幅度 0..1(缩放呼吸)。
    pub pulse: f32,
    /// 显示唱针。
    pub tonearm: bool,
    /// 封面图路径;None = 内置默认唱片图。
    pub cover: Option<std::path::PathBuf>,
    /// 圆形封面(加载时烘掊圆形 alpha 蒙版)否则方形。
    pub round: bool,
}

impl Default for DiscSettings {
    fn default() -> Self {
        DiscSettings {
            speed: 1.0,
            pulse: 0.25,
            tonearm: true,
            cover: None,
            round: true,
        }
    }
}

/// 屏保动画风格(Q12:六种锁定;Q13:无任何参数)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ScreensaverStyle {
    /// 变幻线(贝塞尔曲线族)。
    Lines,
    /// 彩带。
    Ribbons,
    /// 极光。
    Aurora,
    /// 管道(3D 随机行走,投影线段)。
    Pipes,
    /// 气泡。
    Bubbles,
    /// 飞越星空。
    Starfield,
}

impl ScreensaverStyle {
    pub const ALL: &'static [ScreensaverStyle] = &[
        ScreensaverStyle::Lines,
        ScreensaverStyle::Ribbons,
        ScreensaverStyle::Aurora,
        ScreensaverStyle::Pipes,
        ScreensaverStyle::Bubbles,
        ScreensaverStyle::Starfield,
    ];
    pub fn name(self) -> &'static str {
        match self {
            ScreensaverStyle::Lines => "变幻线",
            ScreensaverStyle::Ribbons => "彩带",
            ScreensaverStyle::Aurora => "极光",
            ScreensaverStyle::Pipes => "管道",
            ScreensaverStyle::Bubbles => "气泡",
            ScreensaverStyle::Starfield => "飞越星空",
        }
    }
}

/// 屏保视图选项:仅风格选择(Q13/Q17:单选、无参数)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScreensaverSettings {
    pub style: ScreensaverStyle,
}

impl Default for ScreensaverSettings {
    fn default() -> Self {
        ScreensaverSettings {
            style: ScreensaverStyle::Lines,
        }
    }
}

/// 衰减速度档位(持久化形态;运行时映射到 `audio_spectrum_core::DecaySpeed`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DecayChoice {
    VerySlow,
    Slow,
    Medium,
    Fast,
    VeryFast,
}

impl DecayChoice {
    /// 菜单档位顺序(极慢 → 极快)。
    pub const ALL: &'static [DecayChoice] = &[
        DecayChoice::VerySlow,
        DecayChoice::Slow,
        DecayChoice::Medium,
        DecayChoice::Fast,
        DecayChoice::VeryFast,
    ];

    /// 菜单显示名。
    pub fn name(self) -> &'static str {
        match self {
            DecayChoice::VerySlow => "极慢",
            DecayChoice::Slow => "慢",
            DecayChoice::Medium => "中",
            DecayChoice::Fast => "快",
            DecayChoice::VeryFast => "极快",
        }
    }
}

impl From<DecayChoice> for audio_spectrum_core::DecaySpeed {
    fn from(c: DecayChoice) -> Self {
        match c {
            DecayChoice::VerySlow => audio_spectrum_core::DecaySpeed::VerySlow,
            DecayChoice::Slow => audio_spectrum_core::DecaySpeed::Slow,
            DecayChoice::Medium => audio_spectrum_core::DecaySpeed::Medium,
            DecayChoice::Fast => audio_spectrum_core::DecaySpeed::Fast,
            DecayChoice::VeryFast => audio_spectrum_core::DecaySpeed::VeryFast,
        }
    }
}

impl From<audio_spectrum_core::DecaySpeed> for DecayChoice {
    fn from(d: audio_spectrum_core::DecaySpeed) -> Self {
        match d {
            audio_spectrum_core::DecaySpeed::VerySlow => DecayChoice::VerySlow,
            audio_spectrum_core::DecaySpeed::Slow => DecayChoice::Slow,
            audio_spectrum_core::DecaySpeed::Medium => DecayChoice::Medium,
            audio_spectrum_core::DecaySpeed::Fast => DecayChoice::Fast,
            audio_spectrum_core::DecaySpeed::VeryFast => DecayChoice::VeryFast,
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
                peak_hold: [232, 238, 248],
            },
            ThemePreset::Aurora => Palette {
                bar_bottom: [20, 160, 160],
                bar_top: [130, 255, 175],
                bg: [9, 15, 17],
                peak: [205, 255, 235],
                peak_hold: [205, 255, 235],
            },
            ThemePreset::Magma => Palette {
                bar_bottom: [140, 30, 20],
                bar_top: [255, 200, 60],
                bg: [17, 10, 8],
                peak: [255, 232, 205],
                peak_hold: [255, 232, 205],
            },
            ThemePreset::Neon => Palette {
                bar_bottom: [170, 40, 220],
                bar_top: [90, 240, 255],
                bg: [14, 10, 20],
                peak: [240, 220, 255],
                peak_hold: [240, 220, 255],
            },
            ThemePreset::Amber => Palette {
                bar_bottom: [125, 70, 10],
                bar_top: [255, 190, 80],
                bg: [18, 14, 8],
                peak: [255, 240, 210],
                peak_hold: [255, 240, 210],
            },
        }
    }
}

/// 一套渲染调色板(RGB,由 bin 转成 iced 颜色)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    pub bar_bottom: [u8; 3],
    pub bar_top: [u8; 3],
    pub bg: [u8; 3],
    pub peak: [u8; 3],
    pub peak_hold: [u8; 3],
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

/// 频谱的整体布局类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum SpectrumType {
    /// 横向对数轴上的竖条(原有效果)。
    #[default]
    Bar,
    /// 环形排列的径向频谱。
    Radial,
}

impl SpectrumType {
    pub const ALL: &'static [SpectrumType] = &[SpectrumType::Bar, SpectrumType::Radial];

    pub fn name(self) -> &'static str {
        match self {
            SpectrumType::Bar => "条形",
            SpectrumType::Radial => "圆形",
        }
    }
}

/// 样式设置:预设打底 + 逐槽颜色覆盖。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleSettings {
    pub theme: ThemePreset,
    pub bar_bottom: Option<[u8; 3]>,
    pub bar_top: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub peak: Option<[u8; 3]>,
    /// 峰值保持点颜色;未设置时沿用峰值色。
    pub peak_hold: Option<[u8; 3]>,
    /// 是否绘制频率/幅度网格。
    pub grid: bool,
    /// 柱条圆角(px)。
    pub corner_radius: f32,
    /// 柱条间隙占柱宽的比例(0..0.5)。
    pub bar_gap: f32,
    /// 频谱整体布局(条形/圆形)。
    pub spectrum_type: SpectrumType,
    /// 方块选项:无论条形还是圆形,都以离散方块绘制。
    pub blocks: bool,
    /// 方块高度(px),仅方块模式下生效。
    pub block_height: f32,
    /// 相邻方块之间的间隔(px),仅方块模式下生效。
    pub block_gap: f32,
    /// 峰值标记与柱体之间留出间隔,形成悬空拖尾帽。
    pub peak_detached: bool,
    /// 峰值保持点的高度(px)。
    pub peak_cap_height: f32,
    /// 峰值保持点与柱体顶端的间隔(px),仅在峰值保持点分离时生效。
    pub peak_gap: f32,
}

impl Default for StyleSettings {
    fn default() -> Self {
        StyleSettings {
            theme: ThemePreset::Classic,
            bar_bottom: None,
            bar_top: None,
            bg: None,
            peak: None,
            peak_hold: None,
            grid: true,
            corner_radius: 1.0,
            bar_gap: 0.15,
            spectrum_type: SpectrumType::Bar,
            blocks: false,
            block_height: 6.0,
            block_gap: 1.0,
            peak_detached: false,
            peak_cap_height: 4.0,
            peak_gap: 6.0,
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
    // 峰值保持点默认跟随峰值色,单独覆盖时使用自己的颜色。
    p.peak_hold = style.peak_hold.unwrap_or(p.peak);
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
    /// 自定义背景色;None = 主题背景色。设了图片时图片优先。
    pub color: Option<[u8; 3]>,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        BackgroundSettings {
            path: None,
            mode: ScaleMode::Fill,
            opacity: 1.0,
            blur_px: 0,
            color: None,
        }
    }
}

/// 简单矩形(pos + size),lib 层无 GUI 依赖。
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

/// 语谱纹理像素(扁平 RGBA8,行主序:行 = Band,顶部 = 高频;列 = 时间,
/// 右 = 最新)。颜色 = 调色板背景色向柱渐变色按强度混合;`bg_on` 时 alpha
/// 随强度(静音透明,透出背景图),否则不透明。
///
/// 直接写扁平 RGBA,避免「`Vec<[u8;4]>` 再整体展开」的额外分配与拷贝。
pub fn spectro_pixels(
    sg: &audio_spectrum_core::Spectrograph,
    pal: &Palette,
    bg_on: bool,
) -> Vec<u8> {
    let bands = sg.num_bands();
    let count = audio_spectrum_core::SPECTRO_COLUMN_COUNT;
    let span = audio_spectrum_core::DB_CEILING - audio_spectrum_core::DB_FLOOR;
    let mut out = Vec::with_capacity(count * bands * 4);
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
            out.extend_from_slice(&[
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
    pub decay: DecayChoice,
    pub style: StyleSettings,
    pub background: BackgroundSettings,
    /// 启动即进入全屏。
    pub default_fullscreen: bool,
    /// 记住上次窗口尺寸与位置。
    pub remember_window: bool,
    /// 开机自启动(切换时写入系统;Windows 注册表 Run 键 / macOS
    /// LaunchAgent / Linux XDG autostart,见 [`autostart`])。
    pub launch_at_login: bool,
    /// 阻止系统休眠(切换时生效,进程退出自动解除,见 [`keep_awake`])。
    pub prevent_sleep: bool,
    /// 显示性能 HUD(右下角浮层;默认关闭,纯视觉)。
    pub show_hud: bool,
    /// 上次窗口矩形 [x, y, w, h]。
    pub window_rect: Option<[f32; 4]>,
    /// 各视图选项(per-View 节;频谱的选项即上方平铺字段,ADR-0005)。
    pub waveform: WaveformSettings,
    pub particles: ParticleSettings,
    pub glow: GlowSettings,
    pub disc: DiscSettings,
    pub screensaver: ScreensaverSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            view: ViewMode::Spectrum,
            always_on_top: false,
            band_count: 96,
            decay: DecayChoice::Medium,
            style: StyleSettings::default(),
            background: BackgroundSettings::default(),
            default_fullscreen: false,
            remember_window: false,
            launch_at_login: false,
            prevent_sleep: false,
            show_hud: false,
            window_rect: None,
            waveform: WaveformSettings::default(),
            particles: ParticleSettings::default(),
            glow: GlowSettings::default(),
            disc: DiscSettings::default(),
            screensaver: ScreensaverSettings::default(),
        }
    }
}

/// Windows 上把 wgpu 后端钉到 DX12(用户显式设置的 `WGPU_BACKEND` 优先)。
///
/// 缘由:AMD + 虚拟显示驱动环境下,wgpu 默认枚举先选 Vulkan,其交换链在
/// 全屏切换时返回 SurfaceError,iced 0.14 的 present 不容忍即 panic;
/// DX12/GL 实测健康(A.D.R.-0006 诊断记录)。
#[cfg(target_os = "windows")]
pub fn pin_windows_wgpu_backend() {
    if std::env::var_os("WGPU_BACKEND").is_some() {
        return;
    }
    // SAFETY:进程启动早期、任何线程创建之前调用;env 只写一次。
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("WGPU_BACKEND", "dx12");
    }
}

/// 非 Windows 无需钉后端(macOS Metal / Linux 按默认枚举)。
#[cfg(not(target_os = "windows"))]
pub fn pin_windows_wgpu_backend() {}

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
        .map(normalize)
        .unwrap_or_default()
}

/// 载入后的合法性收敛:频段数钳制到可调范围(旧配置或手改越界时)。
fn normalize(mut s: Settings) -> Settings {
    s.band_count = s.band_count.clamp(BAND_COUNT_MIN, BAND_COUNT_MAX);
    s
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_mode_cycle_visits_all_eight_views() {
        let mut seen = Vec::new();
        let mut v = ViewMode::Spectrum;
        for _ in 0..ViewMode::ALL.len() {
            seen.push(v);
            v = v.next();
        }
        assert_eq!(seen, ViewMode::ALL.to_vec());
        // 一圈后回到起点。
        assert_eq!(v, ViewMode::ALL[0]);
    }

    #[test]
    fn view_names_are_unique() {
        let mut names: Vec<_> = ViewMode::ALL.iter().map(|v| v.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ViewMode::ALL.len());
    }

    #[test]
    fn old_settings_without_per_view_sections_load_with_defaults() {
        // 旧版 settings.ron(无特效节):缺失字段走 struct 级 serde(default)。
        let s: Settings =
            ron::from_str("(view: Spectrogram, band_count: 64)").expect("旧配置应可解析");
        assert_eq!(s.view, ViewMode::Spectrogram);
        assert_eq!(s.band_count, 64);
        assert_eq!(s.waveform, WaveformSettings::default());
        assert_eq!(s.particles, ParticleSettings::default());
        assert_eq!(s.glow, GlowSettings::default());
        assert_eq!(s.disc, DiscSettings::default());
        assert_eq!(s.screensaver, ScreensaverSettings::default());
    }

    #[test]
    fn old_style_section_gets_new_spectrum_option_defaults() {
        // 旧版样式节没有 spectrum_type / blocks / peak_detached,serde(default)补齐。
        let s: Settings = ron::from_str("(style: (theme: Neon))").expect("旧样式应可解析");
        assert_eq!(s.style.theme, ThemePreset::Neon);
        assert_eq!(s.style.spectrum_type, SpectrumType::Bar);
        assert!(!s.style.blocks);
        assert!(!s.style.peak_detached);
    }

    #[test]
    fn settings_round_trip_preserves_per_view_sections() {
        let s = Settings {
            view: ViewMode::Disc,
            disc: DiscSettings {
                speed: 1.5,
                ..Default::default()
            },
            screensaver: ScreensaverSettings {
                style: ScreensaverStyle::Aurora,
            },
            ..Default::default()
        };
        let text = ron::to_string(&s).expect("序列化");
        let back: Settings = ron::from_str(&text).expect("反序列化");
        assert_eq!(back, s);
    }
}
