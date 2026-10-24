#![forbid(unsafe_code)]
// Windows:GUI 程序不弹出控制台(仅 release;debug 保留控制台看日志)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! audio-spectrum-app — the iced shell of audio-spectrum.
//!
//! 布局契约:频谱内容占据菜单栏以下的整个窗体;配置入口为应用菜单
//! 栏(非 macOS,常驻顶栏)或原生菜单栏(macOS);全屏时菜单栏隐藏,
//! ESC/右键/双击退出。设置经 `audio_spectrum_app` lib 持久化(ron/惯例
//! 目录/300ms 防抖)。
//!
//! GUI 方案:iced(Elm 架构)。画布(背景图、频谱柱、语谱纹理、网格、
//! 悬停读数)由单个 `canvas::Program` 绘制,并在每次重绘请求时发布
//! `Message::Frame` 驱动常驻 60fps(设计共识:spectrum animation never
//! sleeps);配置浮层(菜单栏下拉/状态/HUD/设置窗)用普通 widget 叠在
//! `stack` 上层。
//!
//! Threading model (ADR-0002): the audio callback writes the ring buffer
//! inside `audio-spectrum-capture`; this UI thread pops samples, runs the FFT and
//! paints, at a constant repaint rate.

use audio_spectrum_app::{
    BackgroundSettings, DecayChoice, GlowLayout, Palette, ParticleEmission, ScaleMode,
    ScreensaverStyle, Settings, SpectrumType, ThemePreset, ViewMode, WaveformColor, BAND_COUNT_MAX,
    BAND_COUNT_MIN, autostart, band_color as palette_band_color, blur_rgba, keep_awake,
    load as load_settings, pin_windows_wgpu_backend, resolve_palette, save as save_settings,
    spectro_pixels,
};
#[cfg(target_os = "macos")]
use audio_spectrum_app::{MenuCommand, NativeMenu};
use audio_spectrum_capture::{CaptureError, CaptureStream, CaptureSystem, DeviceInfo, SourceKind};
use audio_spectrum_core::{
    Analyzer, DB_CEILING, DB_FLOOR, DecaySpeed, FRAME_SIZE, FREQ_MIN_HZ, SPECTRO_COLUMN_COUNT,
    Smoother, Spectrograph, bass_level,
};

mod updater;
use updater::{UpdateEvent, UpdatePhase};
use iced::alignment::{Horizontal, Vertical};
use iced::keyboard::{self, key::Named};
use iced::widget::image::Handle as ImageHandle;
use iced::widget::{
    Space, button, canvas, checkbox, column, container, mouse_area, row, scrollable, slider, stack,
    text,
};
use iced::window;
use iced_aw::ColorPicker;
use iced::{Color, Element, Font, Length, Padding, Pixels, Point, Rectangle, Size, Subscription, Task, Theme, Vector};

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

type CanvasFrame = canvas::Frame<iced::Renderer>;
type CanvasGeometry = canvas::Geometry<iced::Renderer>;

/// `ln(FREQ_MAX_HZ / FREQ_MIN_HZ)` = ln(1000) — the log-frequency axis span.
const LOG_SPAN: f32 = 6.907_755;

const FREQ_TICKS: &[f32] = &[
    63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
const DB_TICKS: &[f32] = &[0.0, -20.0, -40.0, -60.0];
/// 非方块布局的峰值帽下落减速系数(越小下落越慢)。
const PEAK_HOLD_SLOW_SCALE: f32 = 0.5;
/// 频谱显示增益:柱体与峰值帽的视觉幅度放大 1/4。
const SPECTRUM_GAIN: f32 = 1.25;
/// 方块尺寸的参考绘图高度:绘图高度为 440px 时,设置值即实际像素;
/// 全屏/放大窗口时按高度等比缩放,保持方块数量与视觉比例一致。
const BLOCK_SCALE_REF_HEIGHT: f32 = 440.0;
/// 圆形中心的雷达扫描:拖尾约占圆周 1/6,另含转速与拖尾线数量。
const RADAR_SWEEP: f32 = std::f32::consts::TAU / 6.0;
const RADAR_TURNS_PER_SEC: f32 = 0.22;
const RADAR_TRAIL_LINES: usize = 90;

/// Embedded at compile time so the binary has no runtime asset dependency.
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/audio-spectrum-256.png");

const GRID: Color = Color::from_rgb(36.0 / 255.0, 41.0 / 255.0, 52.0 / 255.0);
const FREQ_GRID: Color = Color::from_rgb(30.0 / 255.0, 34.0 / 255.0, 43.0 / 255.0);
const AXIS_TEXT: Color = Color::from_rgb(108.0 / 255.0, 114.0 / 255.0, 130.0 / 255.0);
const CHIP_BG: Color = Color::from_rgba(12.0 / 255.0, 14.0 / 255.0, 19.0 / 255.0, 0.63);
const TOOLTIP_BG: Color = Color::from_rgba(16.0 / 255.0, 18.0 / 255.0, 24.0 / 255.0, 0.92);
const PANEL_BG: Color = Color::from_rgba(0.06, 0.07, 0.09, 0.97);
const SELECTED_BG: Color = Color::from_rgb(0.16, 0.28, 0.55);
const BTN_BG: Color = Color::from_rgb(0.13, 0.15, 0.19);
const BTN_FG: Color = Color::from_rgb(0.85, 0.87, 0.92);
const SECTION_FG: Color = Color::from_rgb(0.55, 0.58, 0.66);
const DIM_OVERLAY: Color = Color::from_rgba(0.0, 0.0, 0.0, 0.5);

/// 大图解码的像素上限:超出此长边立即降采样,避免全分辨率 RGBA 常驻。
/// 背景按窗口覆盖绘制 2560 足够;封面只是中心小圆贴图,1024 足够。
const BG_MAX_EDGE: u32 = 2560;
const COVER_MAX_EDGE: u32 = 1024;

/// 设置防抖落盘间隔。
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// 双击判定窗口:两次画布左键按下间隔不超过此值视为双击。
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

/// 全屏状态回读节流间隔(兼容系统绿色按钮;iced 无 ModeChanged 事件)。
const MODE_POLL_SECS: f64 = 0.25;

/// iced 依赖 cosmic-text 的系统字体库 + 逐字形回退,但主字体仍指向平台
/// CJK 家族以保证确定性;找不到时回退系统默认 + 字形回退。
#[cfg(target_os = "macos")]
const CJK_FAMILY: &str = "PingFang SC";
#[cfg(target_os = "windows")]
const CJK_FAMILY: &str = "Microsoft YaHei";
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
const CJK_FAMILY: &str = "Noto Sans CJK SC";
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    unix,
    target_os = "android",
    target_os = "emscripten"
)))]
const CJK_FAMILY: &str = "DejaVu Sans";
const CJK_FONT: Font = Font::with_name(CJK_FAMILY);

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Message {
    /// 每帧心跳(画布在每次重绘请求时发布,自维持 vsync 帧循环)。
    Frame,
    Key(keyboard::Event),
    Window(window::Id, window::Event),
    /// 实际窗口模式回读(节流;兼容系统按钮进入的全屏)。
    WindowMode(window::Mode),
    /// 画布右键(仅全屏时退出全屏)。
    CanvasRightClick,
    /// 画布左键:收起打开中的菜单栏下拉;双击切换全屏。
    CanvasClick,
    /// 展开应用菜单栏的某个下拉(再点一次收起)。
    #[cfg(not(target_os = "macos"))]
    ToggleMenu(MenuTab),
    // —— 配置动作(菜单栏/特效窗体共用)——
    OpenEffects,
    CloseEffects,
    ToggleFullscreen,
    ToggleAlwaysOnTop,
    ToggleFrozen,
    ToggleHud,
    SelectSource(DeviceInfo),
    SetBandCount(usize),
    SetDecay(DecayChoice),
    ToggleGrid,
    SetTheme(ThemePreset),
    ColorSlotReset(ColorSlot),
    SetCornerRadius(f32),
    SetBarGap(f32),
    SetSpectrumType(SpectrumType),
    ToggleBlocks,
    SetBlockHeight(f32),
    SetBlockGap(f32),
    TogglePeakDetached,
    SetPeakCapHeight(f32),
    SetPeakGap(f32),
    // —— 特效窗体(ADR-0005):各视图独立配置 + 通用页 ——
    /// 选中 tab:视图 tab 即切主窗口视图(Q9),通用 tab 仅换页。
    SelectTab(EffectsTab),
    SetWaveformMirrored(bool),
    SetWaveformWidth(f32),
    SetWaveformColor(WaveformColor),
    SetWaveformGain(f32),
    /// 特效自定义色:重置(None = 跟随主题)。
    EffectColorReset(EffectColorSlot),
    /// 颜色选择器:开/关(记录快照)、取消、提交。
    ToggleColorPicker(PickerId),
    CancelColorPicker,
    SubmitColorPicker(iced::Color),
    SetParticleCount(u32),
    SetParticleSize(f32),
    SetParticleTrail(f32),
    SetParticleEmission(ParticleEmission),
    SetGlowCount(u32),
    SetGlowLayout(GlowLayout),
    SetGlowIntensity(f32),
    SetGlowHueDrift(bool),
    SetDiscSpeed(f32),
    SetDiscPulse(f32),
    ToggleDiscTonearm,
    PickCover,
    ClearCover,
    SetDiscRound(bool),
    SetScreensaverStyle(ScreensaverStyle),
    PickBackground,
    ClearBackground,
    SetScaleMode(ScaleMode),
    SetOpacity(f32),
    SetBlur(f32),
    ToggleFullscreenDefault,
    ToggleRememberWindow,
    /// 「设置」菜单:开机自启动(写系统,见 autostart 模块)。
    ToggleLaunchAtLogin,
    /// 「设置」菜单:阻止系统休眠(见 keep_awake 模块)。
    TogglePreventSleep,
    // —— 关于 / 更新 ——
    /// 打开「关于」窗口(菜单栏末尾「关于」下拉子项 / macOS 原生菜单)。
    OpenAbout,
    /// 关闭「关于」窗口(点遮罩)。
    CloseAbout,
    /// 打开「检测更新」窗口(同上)。
    OpenUpdate,
    /// 关闭「检测更新」窗口(点遮罩)。
    CloseUpdate,
    CheckUpdates,
    DownloadUpdate,
}

/// 应用菜单栏的顶层菜单(非 macOS)。
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuTab {
    Source,
    Settings,
    About,
}

/// 颜色覆盖槽(频谱的四个可覆盖色)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorSlot {
    BarBottom,
    BarTop,
    Bg,
    Peak,
    PeakHold,
}

/// 颜色选择器的目标槽(一次只开一个弹窗)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerId {
    Spectrum(ColorSlot),
    Waveform,
    Particles,
    Glow,
    Background,
}

/// 特效窗体的 tab(GLOSSARY:Effects Window;九页 = 八视图 + 通用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectsTab {
    View(ViewMode),
    General,
}

/// 特效自定义色槽(波形/粒子/光效各一;索引即草稿数组下标)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectColorSlot {
    Waveform,
    Particles,
    Glow,
    Background,
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

struct SpectrumApp {
    system: CaptureSystem,
    /// The selected Source; the stream below is (re)opened for it.
    source: Option<DeviceInfo>,
    /// `Some` = capturing; `None` + `reconnect_at` = Reconnect in progress.
    stream: Option<CaptureStream>,
    reconnect_at: Option<f64>,
    /// Device list cache, refreshed once per second (enumeration is not free).
    loopback_devices: Vec<DeviceInfo>,
    input_devices: Vec<DeviceInfo>,
    last_device_refresh: f64,
    frame_cost_ms: f64,
    frame_interval_ms: f64,
    last_frame_at: Option<f64>,
    /// 全部持久化设置(唯一事实源)。
    settings: Settings,
    /// 特效窗体开关(⌘, / 菜单栏打开;ADR-0005:原设置窗并入)。
    effects_open: bool,
    /// 特效窗体当前显示「通用」页(否则显示当前视图的配置页)。
    effects_general: bool,
    /// macOS 原生菜单(首帧安装)。
    #[cfg(target_os = "macos")]
    native_menu: Option<NativeMenu>,
    /// 冻结显示(Space):分析继续,画面与滚动停止。瞬态,不持久化。
    frozen: bool,
    /// 双击检测:上一次画布左键按下时刻(None = 无进行中的双击)。
    last_canvas_click: Option<Instant>,
    /// 视口是否全屏(节流回读 `window::mode`,兼容系统绿色按钮)。
    fullscreen: bool,
    last_mode_poll: f64,
    /// 防 Dirty 标记:设置变更时间,防抖后落盘。
    settings_dirty: Option<Instant>,
    /// 启动时的窗口矩形,避免启动即触发一轮「位置变化」保存。
    startup_rect: Option<[f32; 4]>,
    /// 启动全屏待应用:窗口以窗口模式创建,首帧后经 window::set_mode
    /// 进入全屏(绕开 iced 0.14 + wgpu 27 在部分虚拟显示环境下的
    /// 「启动即全屏」初始化崩溃,见 README 已知取舍)。
    fullscreen_start: bool,
    analyzer: Analyzer,
    smoother: Smoother,
    spectrograph: Spectrograph,
    /// 语谱纹理缓存:版本/频段/背景/调色板不变时复用上一帧纹理。
    spectro_cache: RefCell<SpectroCache>,
    target: Vec<f32>,
    last_bands: Vec<f32>,
    last_peaks: Vec<f32>,
    staging: Vec<f32>,
    /// 背景缓存:解码原图(扁平 RGBA,供模糊参数变化时重烘焙)与成品纹理。
    bg_raw: Option<(Vec<u8>, usize, usize)>,
    /// 模糊烘焙的整幅暂存(水平结果),跨拖动复用,免反复分配。
    bg_scratch: Vec<u8>,
    bg_key: Option<(PathBuf, u32)>,
    bg_handle: Option<ImageHandle>,
    bg_src_size: [f32; 2],
    /// 进程起始时刻(`now` 秒基准,喂给 Spectrograph/重连计时)。
    start: Instant,
    /// 主窗口(iced 动态窗口 Id;首个 Opened 事件确定)。
    main_window: Option<window::Id>,
    window_size: Size,
    window_pos: Option<Point>,
    /// 展开中的应用菜单栏下拉(非 macOS;None = 全部收起)。
    #[cfg(not(target_os = "macos"))]
    open_menu: Option<MenuTab>,
    /// 「关于」窗口开关(菜单栏「关于」下拉子项 / macOS 原生菜单打开)。
    about_open: bool,
    /// 「检测更新」窗口开关(同上)。
    update_open: bool,
    /// 「关于」窗口展示的程序图标(启动时解码一次;解码失败则不展示)。
    about_icon: Option<ImageHandle>,
    /// 应用内更新:状态机 + 后台线程事件流(每帧轮询;启动即自动检测)。
    update_phase: UpdatePhase,
    update_rx: Option<std::sync::mpsc::Receiver<UpdateEvent>>,
    /// 颜色选择器:当前打开的槽 + 弹窗内实时色。
    picker_open: Option<PickerId>,
    picker_working: [u8; 3],
    /// 最近一帧的原始样本(波形视图直接绘制;pump_audio 随分析一同留存)。
    last_samples: [f32; FRAME_SIZE],
    has_samples: bool,
    /// Bass Level EMA(粒子/光效/CD 共用的律动标量,Q6-b)。
    bass: f32,
    /// CD 累计转角(弧度)。
    disc_angle: f32,
    /// 动画随机源(确定性 xorshift,无外部依赖)。
    rng: Rng,
    /// 粒子池(仅粒子视图激活时推进)。
    particles: ParticlePool,
    /// 屏保各风格的状态(仅屏保视图激活时推进)。
    screensaver: ScreensaverState,
    /// CD 封面纹理缓存键:(路径,圆形);None 路径 = 内置默认唱片图。
    cover_key: Option<(Option<PathBuf>, bool)>,
    cover_handle: Option<ImageHandle>,
    /// 特效自定义色的十六进制输入草稿(波形/粒子/光效/背景色)。
    /// 光效随机布局的光斑状态(仅 Random 布局使用;数量变化时重建)。
    glow_blobs: Vec<GlowBlob>,
    glow_key: u32,
}

impl SpectrumApp {
    fn new(settings: Settings) -> SpectrumApp {
        let system = CaptureSystem::new();
        // Design consensus: open Loopback (whatever the system is playing)
        // at startup; fall back to the default Input device.
        let source = system
            .default_source(SourceKind::Loopback)
            .or_else(|| system.default_source(SourceKind::Input));
        let band_count = settings.band_count;
        let fullscreen_start = settings.default_fullscreen;
        let decay: DecaySpeed = settings.decay.into();
        // 峰值保持点常驻,没有单独的开关。
        let mut smoother = Smoother::new(band_count, true);
        smoother.set_decay(decay);
        smoother.set_peak_decay_scale(peak_hold_decay_scale(settings.style.blocks));
        SpectrumApp {
            system,
            source,
            stream: None,
            reconnect_at: None,
            loopback_devices: Vec::new(),
            input_devices: Vec::new(),
            last_device_refresh: f64::NEG_INFINITY,
            frame_cost_ms: 0.0,
            frame_interval_ms: 16.7,
            last_frame_at: None,
            settings,
            effects_open: false,
            effects_general: false,
            #[cfg(target_os = "macos")]
            native_menu: None,
            #[cfg(not(target_os = "macos"))]
            open_menu: None,
            about_open: false,
            update_open: false,
            about_icon: decode_icon(),
            // 启动即自动检测更新(后台线程,结果经菜单红点/更新窗呈现)。
            update_phase: UpdatePhase::Checking,
            update_rx: Some(updater::spawn_check()),
            frozen: false,
            last_canvas_click: None,
            fullscreen: false,
            last_mode_poll: f64::NEG_INFINITY,
            fullscreen_start,
            settings_dirty: None,
            startup_rect: None,
            analyzer: Analyzer::new(band_count),
            smoother,
            spectrograph: Spectrograph::new(band_count),
            spectro_cache: RefCell::new(SpectroCache::default()),
            target: vec![DB_FLOOR; band_count],
            last_bands: vec![DB_FLOOR; band_count],
            last_peaks: vec![DB_FLOOR; band_count],
            staging: Vec::new(),
            bg_raw: None,
            bg_scratch: Vec::new(),
            bg_key: None,
            bg_handle: None,
            bg_src_size: [1.0, 1.0],
            start: Instant::now(),
            main_window: None,
            window_size: Size::new(920.0, 440.0),
            window_pos: None,
            picker_open: None,
            picker_working: [255, 255, 255],
            last_samples: [0.0; FRAME_SIZE],
            has_samples: false,
            bass: 0.0,
            disc_angle: 0.0,
            rng: Rng::new(0x9E37_79B9_7F4A_7C15),
            particles: ParticlePool::default(),
            screensaver: ScreensaverState::default(),
            cover_key: None,
            cover_handle: None,
            glow_blobs: Vec::new(),
            glow_key: 0,
        }
    }

    /// 标记设置已变更(300ms 防抖后落盘)。
    fn mark_dirty(&mut self) {
        self.settings_dirty = Some(Instant::now());
    }

    /// 更新是否有未安装的新版(菜单「关于」右上角红点)。
    fn update_pending(&self) -> bool {
        matches!(
            self.update_phase,
            UpdatePhase::Available(_) | UpdatePhase::Downloading { .. }
        )
    }

    /// 轮询更新线程事件,推进状态机(每帧一次;无事件零开销)。
    /// 轮询更新线程事件,推进状态机(每帧一次;无事件零开销)。
    fn poll_update(&mut self) {
        let Some(rx) = self.update_rx.as_ref() else { return };
        // 先收集后应用:避免通道借用与状态可变借用冲突。
        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(ev) => events.push(ev),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        for ev in events {
            self.apply_update_event(ev);
        }
        if disconnected
            && matches!(
                self.update_phase,
                UpdatePhase::Checking | UpdatePhase::Downloading { .. }
            )
        {
            // 线程异常退出且未发终态事件:回 Idle 允许重新检查。
            self.update_phase = UpdatePhase::Idle;
            self.update_rx = None;
        }
    }

    fn apply_update_event(&mut self, ev: UpdateEvent) {
        match ev {
            UpdateEvent::Checked(Ok(outcome)) => match outcome {
                updater::CheckOutcome::UpToDate(label) => {
                    self.update_phase = UpdatePhase::UpToDate(label)
                }
                updater::CheckOutcome::Available(info) => {
                    self.update_phase = UpdatePhase::Available(info)
                }
            },
            UpdateEvent::Checked(Err(e)) => self.update_phase = UpdatePhase::Failed(e),
            UpdateEvent::Progress { received, total } => {
                self.update_phase = UpdatePhase::Downloading { received, total }
            }
            // 下载完成:立即换装重启(设计共识:零确认;失败回滚留在本进程)。
            UpdateEvent::Downloaded(path) => {
                if let Err(e) = updater::install(&path) {
                    self.update_phase = UpdatePhase::Failed(e);
                }
            }
            UpdateEvent::Failed(e) => self.update_phase = UpdatePhase::Failed(e),
        }
    }

    /// 发起一次更新检查(仅在无后台任务进行中时)。
    fn start_check(&mut self) {
        if !matches!(
            self.update_phase,
            UpdatePhase::Checking | UpdatePhase::Downloading { .. }
        ) {
            self.update_phase = UpdatePhase::Checking;
            self.update_rx = Some(updater::spawn_check());
        }
    }

    /// 重建分析管线(频段数变化后);显示状态清零重来。
    fn rebuild_analysis(&mut self) {
        let band_count = self.settings.band_count;
        self.analyzer = Analyzer::new(band_count);
        let decay: DecaySpeed = self.settings.decay.into();
        self.smoother = Smoother::new(band_count, true);
        self.smoother.set_decay(decay);
        self.smoother
            .set_peak_decay_scale(peak_hold_decay_scale(self.settings.style.blocks));
        self.spectrograph = Spectrograph::new(band_count);
        self.target = vec![DB_FLOOR; band_count];
        self.last_bands = vec![DB_FLOOR; band_count];
        self.last_peaks = vec![DB_FLOOR; band_count];
    }

    /// Source lifecycle: detect dead streams, schedule and perform Reconnects.
    fn tick(&mut self, now: f64) {
        if self.stream.as_ref().is_some_and(|s| s.is_dead()) {
            self.stream = None;
            self.staging.clear();
            self.reconnect_at = Some(now + 0.5);
        }
        if now - self.last_device_refresh > 1.0 {
            self.last_device_refresh = now;
            self.loopback_devices = self.system.list_devices(SourceKind::Loopback);
            self.input_devices = self.system.list_devices(SourceKind::Input);
        }
        match (self.stream.is_none(), self.reconnect_at) {
            (true, Some(at)) if now >= at => self.attempt_open(now),
            (true, None) if self.source.is_some() => self.attempt_open(now),
            _ => {}
        }
        // 设置防抖落盘。
        if let Some(at) = self.settings_dirty
            && at.elapsed() >= SAVE_DEBOUNCE
        {
            let _ = save_settings(&self.settings); // 失败静默:下次变更再试
            self.settings_dirty = None;
        }
    }

    fn attempt_open(&mut self, now: f64) {
        self.reconnect_at = None;
        let Some(source) = self.source.clone() else {
            return;
        };
        match self.system.open(&source) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.staging.clear();
            }
            Err(CaptureError::DeviceGone(_)) => {
                // The device vanished: fall back to the kind's default Source.
                match self.system.default_source(source.kind) {
                    Some(next) => {
                        if next.id != source.id {
                            self.source = Some(next);
                        }
                        self.reconnect_at = Some(now + 0.5);
                    }
                    None => {
                        self.source = None;
                        self.reconnect_at = Some(now + 2.0);
                    }
                }
            }
            Err(_) => {
                self.reconnect_at = Some(now + 2.0);
            }
        }
    }

    /// Audio in → latest frame analyzed (FFT on this UI thread, ADR-0002).
    fn pump_audio(&mut self) {
        if let Some(stream) = self.stream.as_mut() {
            stream.pop_samples(&mut self.staging);
        }
        if self.staging.len() >= FRAME_SIZE {
            let mut frame = [0.0; FRAME_SIZE];
            let start = self.staging.len() - FRAME_SIZE;
            frame.copy_from_slice(&self.staging[start..]);
            self.staging.clear();
            self.target = self.analyzer.analyze(&frame);
            // 波形视图直接吃这同一帧原始样本(零采集改动,设计共识)。
            self.last_samples = frame;
            self.has_samples = true;
        }
    }

    /// 确保背景纹理与设置一致(路径或模糊变化时重解码/重烘焙)。
    fn ensure_background(&mut self) {
        let want_blur = self.settings.background.blur_px;
        let want_path = self.settings.background.path.clone();
        let want_key = want_path.clone().map(|p| (p, want_blur));
        if want_key == self.bg_key {
            return;
        }
        let old_key = self.bg_key.clone();
        self.bg_key = want_key;
        match &want_path {
            None => {
                self.bg_handle = None;
                self.bg_raw = None;
            }
            Some(path) => {
                // 路径变化 → 解码原图;仅模糊变化 → 复用原图重烘焙。
                let path_changed = old_key.as_ref().map(|(p, _)| p) != want_path.as_ref();
                if path_changed || self.bg_raw.is_none() {
                    match image::open(path) {
                        Ok(img) => {
                            // 解码 → 降采样 → into_rgba8 原地搬移,全程扁平 RGBA,
                            // 不经 `Vec<[u8;4]>` 中转(省 2 份整图拷贝)。
                            let rgba = downscale_to_edge(img, BG_MAX_EDGE).into_rgba8();
                            let (w, h) = rgba.dimensions();
                            self.bg_raw = Some((rgba.into_raw(), w as usize, h as usize));
                        }
                        Err(e) => {
                            eprintln!("背景图解码失败:{e}");
                            self.bg_handle = None;
                            self.bg_raw = None;
                            self.bg_key = None; // 失败不缓存,下次再试
                            return;
                        }
                    }
                }
                let (w, h) = self
                    .bg_raw
                    .as_ref()
                    .map(|(_, w, h)| (*w, *h))
                    .expect("bg_raw just set");
                self.bg_src_size = [w as f32, h as f32];
                if want_blur == 0 {
                    // 零模糊:原图缓冲直接移交纹理,零额外拷贝。
                    let (raw, _, _) = self.bg_raw.take().expect("bg_raw just set");
                    self.bg_handle = Some(ImageHandle::from_rgba(w as u32, h as u32, raw));
                } else {
                    // 有模糊:克隆一份原地烘焙,原图保留供滑块复用;
                    // 滑动窗口整幅暂存复用 bg_scratch,拖动时不反复分配。
                    let mut baked = self
                        .bg_raw
                        .as_ref()
                        .expect("bg_raw just set")
                        .0
                        .clone();
                    blur_rgba(&mut baked, w, h, want_blur as usize, &mut self.bg_scratch);
                    self.bg_handle = Some(ImageHandle::from_rgba(w as u32, h as u32, baked));
                }
            }
        }
    }

    // ---- 窗口命令 ----

    fn do_toggle_fullscreen(&mut self) -> Task<Message> {
        // 全屏隐藏菜单栏,进入前收起下拉。
        #[cfg(not(target_os = "macos"))]
        {
            self.open_menu = None;
        }
        match self.main_window {
            Some(id) => window::set_mode(
                id,
                if self.fullscreen {
                    window::Mode::Windowed
                } else {
                    window::Mode::Fullscreen
                },
            ),
            None => Task::none(),
        }
    }

    fn do_toggle_always_on_top(&mut self) -> Task<Message> {
        self.settings.always_on_top = !self.settings.always_on_top;
        self.mark_dirty();
        let level = if self.settings.always_on_top {
            window::Level::AlwaysOnTop
        } else {
            window::Level::Normal
        };
        match self.main_window {
            Some(id) => window::set_level(id, level),
            None => Task::none(),
        }
    }

    fn do_select_source(&mut self, info: DeviceInfo) -> Task<Message> {
        self.source = Some(info);
        self.stream = None;
        self.reconnect_at = None;
        self.staging.clear();
        Task::none()
    }

    /// 开机自启动:写系统(注册表/LaunchAgent/XDG autostart)并持久化。
    fn do_toggle_launch_at_login(&mut self) {
        self.settings.launch_at_login = !self.settings.launch_at_login;
        autostart::set_enabled(self.settings.launch_at_login);
        self.mark_dirty();
    }

    /// 阻止休眠:三平台电源抑制(见 keep_awake 模块)并持久化。
    fn do_toggle_prevent_sleep(&mut self) {
        self.settings.prevent_sleep = !self.settings.prevent_sleep;
        keep_awake::set_enabled(self.settings.prevent_sleep);
        self.mark_dirty();
    }

    /// 键盘:S 切换 频谱/语谱,Space 冻结/解冻,⌘,/Ctrl+, 设置,F11 全屏
    /// (仅非 macOS),ESC 退全屏/关设置窗/收起菜单栏下拉(设计共识)。
    fn on_key(&mut self, event: keyboard::Event) -> Task<Message> {
        let keyboard::Event::KeyPressed {
            key,
            modifiers,
            repeat: false,
            ..
        } = event
        else {
            return Task::none();
        };
        let primary = if cfg!(target_os = "macos") {
            modifiers.logo()
        } else {
            modifiers.control()
        };
        match key {
            keyboard::Key::Named(Named::Space) => {
                self.frozen = !self.frozen;
                Task::none()
            }
            // F11:全屏切换(仅非 macOS;macOS 的 F11 是系统调度中心热键,
            // 全屏走 ⌘F/原生菜单/窗口绿色按钮)。
            keyboard::Key::Named(Named::F11) if !cfg!(target_os = "macos") => {
                self.do_toggle_fullscreen()
            }
            // ESC 优先退全屏;其次关特效窗体/收起菜单栏下拉。
            keyboard::Key::Named(Named::Escape) => {
                if self.fullscreen {
                    self.do_toggle_fullscreen()
                } else {
                    self.effects_open = false;
                    #[cfg(not(target_os = "macos"))]
                    {
                        self.open_menu = None;
                    }
                    Task::none()
                }
            }
            keyboard::Key::Character(c) if c.eq_ignore_ascii_case("s") => {
                // S 键循环全部视图(设计共识 v0.8.0)。
                self.settings.view = self.settings.view.next();
                self.mark_dirty();
                Task::none()
            }
            keyboard::Key::Character(c) if c.eq_ignore_ascii_case(",") && primary => {
                self.effects_open = true;
                Task::none()
            }
            _ => Task::none(),
        }
    }

    /// 窗口事件:主窗口 Id、尺寸(平铺烘焙/设置窗高度)、位置记忆。
    fn on_window_event(&mut self, id: window::Id, event: window::Event) {
        let is_main = self.main_window.is_none_or(|m| m == id);
        if !is_main {
            return;
        }
        match event {
            window::Event::Opened { position, size } => {
                self.main_window = Some(id);
                self.window_size = size;
                if let Some(p) = position {
                    self.window_pos = Some(p);
                }
                let p = self.window_pos.unwrap_or(Point::ORIGIN);
                self.startup_rect = Some([p.x, p.y, size.width, size.height]);
            }
            window::Event::Moved(p) => {
                self.window_pos = Some(p);
                self.note_geometry();
            }
            window::Event::Resized(size) => {
                self.window_size = size;
                self.note_geometry();
            }
            _ => {}
        }
    }

    /// 窗口几何记忆(防抖):仅在记住开启且非全屏时跟踪。
    fn note_geometry(&mut self) {
        if !self.settings.remember_window || self.fullscreen {
            return;
        }
        let Some(p) = self.window_pos else {
            return;
        };
        let geom = [p.x, p.y, self.window_size.width, self.window_size.height];
        if Some(geom) == self.startup_rect || Some(geom) == self.settings.window_rect {
            return;
        }
        self.settings.window_rect = Some(geom);
        self.mark_dirty();
    }

    /// 颜色覆盖槽重置：清除覆盖，草稿回到预设值。
    fn color_reset(&mut self, slot: ColorSlot) {
        match slot {
            ColorSlot::BarBottom => self.settings.style.bar_bottom = None,
            ColorSlot::BarTop => self.settings.style.bar_top = None,
            ColorSlot::Bg => self.settings.style.bg = None,
            ColorSlot::Peak => self.settings.style.peak = None,
            ColorSlot::PeakHold => self.settings.style.peak_hold = None,
        }
        self.mark_dirty();
    }

    /// 颜色选择器打开时的初始色(与各行渲染的当前色一致)。
    fn resolve_picker_current(&self, id: PickerId) -> [u8; 3] {
        let pal = resolve_palette(&self.settings.style);
        match id {
            PickerId::Spectrum(ColorSlot::BarBottom) => pal.bar_bottom,
            PickerId::Spectrum(ColorSlot::BarTop) => pal.bar_top,
            PickerId::Spectrum(ColorSlot::Bg) => pal.bg,
            PickerId::Spectrum(ColorSlot::Peak) => pal.peak,
            PickerId::Spectrum(ColorSlot::PeakHold) => pal.peak_hold,
            PickerId::Waveform => self.settings.waveform.custom_color.unwrap_or(pal.bar_top),
            PickerId::Particles => self.settings.particles.custom_color.unwrap_or(pal.bar_top),
            PickerId::Glow => self.settings.glow.custom_color.unwrap_or(pal.bar_top),
            PickerId::Background => self.settings.background.color.unwrap_or(pal.bg),
        }
    }

    /// 把选择器提交的颜色写到对应槽。
    fn apply_picker_color(&mut self, id: PickerId, rgb: [u8; 3]) {
        match id {
            PickerId::Spectrum(slot) => self.set_color_slot(slot, rgb),
            PickerId::Waveform => self.settings.waveform.custom_color = Some(rgb),
            PickerId::Particles => self.settings.particles.custom_color = Some(rgb),
            PickerId::Glow => self.settings.glow.custom_color = Some(rgb),
            PickerId::Background => self.settings.background.color = Some(rgb),
        }
    }

    fn set_color_slot(&mut self, slot: ColorSlot, rgb: [u8; 3]) {
        match slot {
            ColorSlot::BarBottom => self.settings.style.bar_bottom = Some(rgb),
            ColorSlot::BarTop => self.settings.style.bar_top = Some(rgb),
            ColorSlot::Bg => self.settings.style.bg = Some(rgb),
            ColorSlot::Peak => self.settings.style.peak = Some(rgb),
            ColorSlot::PeakHold => self.settings.style.peak_hold = Some(rgb),
        }
        self.mark_dirty();
    }

    /// 应用原生菜单命令(macOS)。
    #[cfg(target_os = "macos")]
    fn apply_menu_command(&mut self, cmd: MenuCommand) -> Task<Message> {
        match cmd {
            MenuCommand::OpenEffects => {
                self.effects_open = true;
                self.effects_general = true;
                Task::none()
            }
            MenuCommand::OpenAbout => {
                self.about_open = true;
                Task::none()
            }
            MenuCommand::OpenUpdate => {
                self.update_open = true;
                Task::none()
            }
            MenuCommand::SetView(v) => {
                self.settings.view = v;
                self.mark_dirty();
                Task::none()
            }
            MenuCommand::ToggleFullscreen => self.do_toggle_fullscreen(),
            MenuCommand::ToggleAlwaysOnTop => self.do_toggle_always_on_top(),
            MenuCommand::ToggleFrozen => {
                self.frozen = !self.frozen;
                Task::none()
            }
            MenuCommand::ToggleHud => {
                self.settings.show_hud = !self.settings.show_hud;
                self.mark_dirty();
                Task::none()
            }
            MenuCommand::ToggleLaunchAtLogin => {
                self.do_toggle_launch_at_login();
                Task::none()
            }
            MenuCommand::TogglePreventSleep => {
                self.do_toggle_prevent_sleep();
                Task::none()
            }
            MenuCommand::SelectSource(info) => self.do_select_source(info),
        }
    }
}

// ---------------------------------------------------------------------------
// 动画状态(粒子/屏保/CD;仅激活视图被推进,冻结时定格)
// ---------------------------------------------------------------------------

/// 微型 xorshift 随机源(动画零外部依赖,确定性可复现)。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 32) as u32
    }
    fn next_f32(&mut self) -> f32 {
        self.next_u32() as f32 / u32::MAX as f32
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next_u32() % (hi - lo + 1).max(1) as u32) as i32
    }
}

/// 单位空间粒子(喷泉/星雨/星云:x/y ∈ 0..1;星环:极坐标,半径占短边比例)。
enum ParticleKind {
    Fountain { x: f32, y: f32, vx: f32, vy: f32 },
    Ring { angle: f32, radius: f32, speed: f32 },
    Rain { x: f32, y: f32, vy: f32, vx: f32 },
    Nebula { x: f32, y: f32, vx: f32, vy: f32, phase: f32 },
}

struct Particle {
    kind: ParticleKind,
    seed: f32,
    age: f32,
    life: f32,
}

struct ParticlePool {
    /// (数量,发射样式):任一变化即重建池。
    key: (u32, ParticleEmission),
    items: Vec<Particle>,
}

impl Default for ParticlePool {
    fn default() -> Self {
        ParticlePool {
            key: (0, ParticleEmission::Fountain),
            items: Vec::new(),
        }
    }
}

/// 语谱纹理缓存键:历史版本 + 频段数 + 背景模式 + 调色板。
/// 四者任一变化才重建 RGBA 与 GPU 纹理;冻结/无新列时零成本复用。
#[derive(PartialEq)]
struct SpectroKey {
    revision: u64,
    bands: usize,
    bg_on: bool,
    palette: Palette,
}

#[derive(Default)]
struct SpectroCache {
    key: Option<SpectroKey>,
    handle: Option<ImageHandle>,
}

struct Star {
    x: f32,
    y: f32,
    z: f32,
    /// 个体粗细系数(粗细变化)。
    size: f32,
}

/// 光效随机布局的光斑(单位空间坐标 + 慢速漂移)。
struct GlowBlob {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    phase: f32,
    scale: f32,
}

struct Bubble {
    x: f32,
    y: f32,
    r: f32,
    seed: f32,
    rise: f32,
}

struct PipeSeg {
    a: [f32; 2],
    b: [f32; 2],
    t: f32,
}

/// 3D 管道随机行走状态(格坐标世界界内,投影在渲染时进行)。
#[derive(Default)]
struct PipeState {
    cell: [i32; 3],
    prev_cell: [i32; 3],
    dir: [i32; 3],
    progress: f32,
    segs: Vec<PipeSeg>,
    color_t: f32,
}

#[derive(Default)]
struct ScreensaverState {
    stars: Vec<Star>,
    bubbles: Vec<Bubble>,
    /// 多个行走者,各自随机出生点;死角后随机重生(位置随机变换)。
    pipes: Vec<PipeState>,
    pipes_inited: bool,
}

/// 管道格坐标 → 单位空间投影(斜二测:x-y 面拉开,z 向左上退)。
fn pipe_project(cell: [i32; 3]) -> [f32; 2] {
    [
        (cell[0] as f32 - cell[2] as f32 * 0.55) / 15.0,
        (cell[1] as f32 - cell[2] as f32 * 0.35) / 15.0,
    ]
}

/// 笔触多边形:沿采样点以逐点宽度向两侧偏移,闭合成填充路径。
fn brush_polygon(points: &[Point], half_widths: &[f32]) -> canvas::Path {
    let n = points.len();
    let mut top = Vec::with_capacity(n);
    let mut bot = Vec::with_capacity(n);
    for i in 0..n {
        let prev = points[i.saturating_sub(1)];
        let next = points[(i + 1).min(n - 1)];
        let (tx, ty) = (next.x - prev.x, next.y - prev.y);
        let len = (tx * tx + ty * ty).sqrt().max(0.001);
        let (nx, ny) = (-ty / len, tx / len);
        let w = half_widths[i];
        top.push(Point::new(points[i].x + nx * w, points[i].y + ny * w));
        bot.push(Point::new(points[i].x - nx * w, points[i].y - ny * w));
    }
    canvas::Path::new(|p| {
        p.move_to(top[0]);
        for pt in &top[1..] {
            p.line_to(*pt);
        }
        for pt in bot.iter().rev() {
            p.line_to(*pt);
        }
        p.close();
    })
}

fn lerp2(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
    ]
}

impl SpectrumApp {
    /// 进程起始以来的秒数(动画时间基准)。
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// 推进激活视图的动画状态(冻结时调用方跳过;Q15:屏保/静态不消费
    /// 音频,采集生命周期不变)。
    fn advance_animations(&mut self, dt: f32) {
        match self.settings.view {
            ViewMode::Disc => {
                // 基准 33⅓ rpm 观感:0.5556 转/秒。
                self.disc_angle += dt * self.settings.disc.speed.max(0.0) * 3.49;
            }
            ViewMode::Particles => self.advance_particles(dt),
            ViewMode::Glow => self.advance_glow(dt),
            ViewMode::Screensaver => self.advance_screensaver(dt),
            _ => {}
        }
    }

    fn spawn_particle(&mut self, initial: bool) -> Particle {
        let ps = self.settings.particles;
        let bass = self.bass;
        let kind = match ps.emission {
            ParticleEmission::Ring => ParticleKind::Ring {
                angle: self.rng.next_f32() * std::f32::consts::TAU,
                radius: 0.30 + self.rng.next_f32() * 0.16,
                speed: 0.5 + self.rng.next_f32() * 0.9,
            },
            ParticleEmission::Rain => ParticleKind::Rain {
                x: self.rng.next_f32(),
                y: if initial {
                    self.rng.next_f32() * 1.2 - 0.1
                } else {
                    -0.1
                },
                vy: 0.12 + self.rng.next_f32() * 0.22,
                vx: (self.rng.next_f32() - 0.5) * 0.03,
            },
            ParticleEmission::Nebula => ParticleKind::Nebula {
                x: 0.06 + self.rng.next_f32() * 0.88,
                y: 0.06 + self.rng.next_f32() * 0.88,
                vx: (self.rng.next_f32() - 0.5) * 0.06,
                vy: (self.rng.next_f32() - 0.5) * 0.06,
                phase: self.rng.next_f32() * std::f32::consts::TAU,
            },
            ParticleEmission::Fountain => ParticleKind::Fountain {
                // 全屏宽度喷发(Q 反馈:尽量铺满)。
                x: 0.02 + self.rng.next_f32() * 0.96,
                y: if initial {
                    self.rng.next_f32() * 1.2 - 0.2
                } else {
                    1.0
                },
                vx: (self.rng.next_f32() - 0.5) * 0.3,
                vy: -(0.5 + self.rng.next_f32() * 0.55) * (0.55 + bass),
            },
        };
        Particle {
            kind,
            seed: self.rng.next_f32(),
            age: match ps.emission {
                ParticleEmission::Fountain if initial => self.rng.next_f32() * 0.8,
                _ => 0.0,
            },
            life: match ps.emission {
                ParticleEmission::Fountain => 1.6 + self.rng.next_f32() * 1.2,
                _ => f32::INFINITY,
            },
        }
    }

    fn advance_particles(&mut self, dt: f32) {
        let ps = self.settings.particles;
        let key = (ps.count, ps.emission);
        if self.particles.key != key || self.particles.items.len() != ps.count as usize {
            self.particles.key = key;
            self.particles.items = (0..ps.count)
                .map(|_| self.spawn_particle(true))
                .collect();
        }
        let bass = self.bass;
        for p in &mut self.particles.items {
            p.age += dt;
            match &mut p.kind {
                ParticleKind::Fountain { x, y, vx, vy } => {
                    *vy += 1.3 * dt;
                    *x += *vx * dt;
                    *y += *vy * dt;
                }
                ParticleKind::Ring { angle, speed, .. } => {
                    *angle += *speed * (0.45 + bass * 1.6) * dt;
                }
                ParticleKind::Rain { x, y, vy, vx } => {
                    *x += *vx * dt;
                    *y += *vy * dt;
                    if *y > 1.08 {
                        *y = -0.08;
                        *x = self.rng.next_f32();
                    }
                }
                ParticleKind::Nebula { x, y, vx, vy, .. } => {
                    *x += *vx * dt;
                    *y += *vy * dt;
                    if *x < 0.03 || *x > 0.97 {
                        *vx = -(*vx);
                    }
                    if *y < 0.03 || *y > 0.97 {
                        *vy = -(*vy);
                    }
                    *x = x.clamp(0.02, 0.98);
                    *y = y.clamp(0.02, 0.98);
                }
            }
        }
        let mut respawn: Vec<usize> = Vec::new();
        for (i, p) in self.particles.items.iter().enumerate() {
            if let ParticleKind::Fountain { y, .. } = &p.kind
                && (*y > 1.15 || p.age > p.life)
            {
                respawn.push(i);
            }
        }
        for i in respawn {
            self.particles.items[i] = self.spawn_particle(false);
        }
    }

    fn advance_glow(&mut self, dt: f32) {
        let g = self.settings.glow;
        if g.layout != GlowLayout::Random {
            self.glow_blobs.clear();
            self.glow_key = 0;
            return;
        }
        let n = g.count.max(1) as usize;
        if self.glow_key != g.count || self.glow_blobs.len() != n {
            self.glow_key = g.count;
            self.glow_blobs = (0..n)
                .map(|_| GlowBlob {
                    x: 0.08 + self.rng.next_f32() * 0.84,
                    y: 0.08 + self.rng.next_f32() * 0.84,
                    vx: (self.rng.next_f32() - 0.5) * 0.05,
                    vy: (self.rng.next_f32() - 0.5) * 0.05,
                    phase: self.rng.next_f32() * std::f32::consts::TAU,
                    scale: 0.7 + self.rng.next_f32() * 0.6,
                })
                .collect();
        }
        for b in &mut self.glow_blobs {
            b.x += b.vx * dt;
            b.y += b.vy * dt;
            if b.x < 0.04 || b.x > 0.96 {
                b.vx = -b.vx;
            }
            if b.y < 0.04 || b.y > 0.96 {
                b.vy = -b.vy;
            }
            b.x = b.x.clamp(0.02, 0.98);
            b.y = b.y.clamp(0.02, 0.98);
        }
    }

    fn advance_screensaver(&mut self, dt: f32) {
        match self.settings.screensaver.style {
            ScreensaverStyle::Starfield => self.advance_stars(dt),
            ScreensaverStyle::Bubbles => self.advance_bubbles(dt),
            ScreensaverStyle::Pipes => self.advance_pipes(dt),
            // 变幻线/彩带/极光为纯时间函数,无状态。
            _ => {}
        }
    }

    fn advance_stars(&mut self, dt: f32) {
        if self.screensaver.stars.is_empty() {
            for _ in 0..420 {
                self.screensaver.stars.push(Star {
                    x: self.rng.next_f32() * 2.0 - 1.0,
                    y: self.rng.next_f32() * 2.0 - 1.0,
                    z: 0.1 + self.rng.next_f32() * 0.9,
                    size: 0.5 + self.rng.next_f32() * 1.4,
                });
            }
        }
        for s in &mut self.screensaver.stars {
            s.z -= dt * 0.3;
            if s.z <= 0.06 {
                s.x = self.rng.next_f32() * 2.0 - 1.0;
                s.y = self.rng.next_f32() * 2.0 - 1.0;
                s.z = 1.0;
            }
        }
    }

    fn new_bubble(&mut self, scattered: bool) -> Bubble {
        Bubble {
            x: self.rng.next_f32(),
            y: if scattered {
                self.rng.next_f32() * 1.2 - 0.1
            } else {
                1.12
            },
            r: 2.0 + self.rng.next_f32() * 42.0,
            seed: self.rng.next_f32(),
            rise: 0.05 + self.rng.next_f32() * 0.07,
        }
    }

    fn advance_bubbles(&mut self, dt: f32) {
        if self.screensaver.bubbles.is_empty() {
            for _ in 0..160 {
                let b = self.new_bubble(true);
                self.screensaver.bubbles.push(b);
            }
        }
        for b in &mut self.screensaver.bubbles {
            b.y -= b.rise * dt;
        }
        let mut respawn: Vec<usize> = Vec::new();
        for (i, b) in self.screensaver.bubbles.iter().enumerate() {
            if b.y < -0.12 {
                respawn.push(i);
            }
        }
        for i in respawn {
            self.screensaver.bubbles[i] = self.new_bubble(false);
        }
    }

    fn advance_pipes(&mut self, dt: f32) {
        const WALKERS: usize = 4;
        if !self.screensaver.pipes_inited {
            self.screensaver.pipes_inited = true;
            for j in 0..WALKERS {
                let mut walker = PipeState {
                    color_t: j as f32 * 0.25,
                    ..Default::default()
                };
                walker.cell = [
                    self.rng.range(-4, 4),
                    self.rng.range(-4, 4),
                    self.rng.range(-4, 4),
                ];
                walker.prev_cell = walker.cell;
                walker.dir =
                    [[1, 0, 0], [0, 1, 0], [0, 0, 1]][(self.rng.next_u32() % 3) as usize];
                self.screensaver.pipes.push(walker);
            }
        }
        for j in 0..self.screensaver.pipes.len() {
            self.advance_one_pipe(j, dt);
        }
    }

    fn advance_one_pipe(&mut self, j: usize, dt: f32) {
        const BOUND: i32 = 7;
        const STEP_SECS: f32 = 0.14;
        const MAX_SEGS: usize = 130;
        self.screensaver.pipes[j].progress += dt / STEP_SECS;
        while self.screensaver.pipes[j].progress >= 1.0 {
            self.screensaver.pipes[j].progress -= 1.0;
            let dirs = [
                [1i32, 0, 0],
                [-1, 0, 0],
                [0, 1, 0],
                [0, -1, 0],
                [0, 0, 1],
                [0, 0, -1],
            ];
            let in_range = |c: [i32; 3]| c.iter().all(|&v| v.abs() <= BOUND);
            let cell = self.screensaver.pipes[j].cell;
            let dir = self.screensaver.pipes[j].dir;
            let straight = [cell[0] + dir[0], cell[1] + dir[1], cell[2] + dir[2]];
            let turns: Vec<[i32; 3]> = dirs
                .into_iter()
                .filter(|&d| {
                    d[0] * dir[0] + d[1] * dir[1] + d[2] * dir[2] == 0
                        && in_range([cell[0] + d[0], cell[1] + d[1], cell[2] + d[2]])
                })
                .collect();
            let next = if in_range(straight) && self.rng.next_f32() < 0.65 {
                Some(straight)
            } else if !turns.is_empty() {
                Some(turns[(self.rng.next_u32() as usize) % turns.len()])
            } else {
                None
            };
            match next {
                Some(ncell) => {
                    let p = &mut self.screensaver.pipes[j];
                    let a = pipe_project(cell);
                    let b = pipe_project(ncell);
                    p.segs.push(PipeSeg { a, b, t: p.color_t });
                    p.color_t = (p.color_t + 0.055).fract();
                    if p.segs.len() > MAX_SEGS {
                        p.segs.drain(..13);
                    }
                    p.prev_cell = cell;
                    p.cell = ncell;
                    p.dir = [ncell[0] - cell[0], ncell[1] - cell[1], ncell[2] - cell[2]];
                }
                None => {
                    // 死角:随机重生(位置随机变换)。
                    let p = &mut self.screensaver.pipes[j];
                    p.segs.clear();
                    p.cell = [
                        self.rng.range(-4, 4),
                        self.rng.range(-4, 4),
                        self.rng.range(-4, 4),
                    ];
                    p.prev_cell = p.cell;
                    p.dir =
                        [[1, 0, 0], [0, 1, 0], [0, 0, 1]][(self.rng.next_u32() % 3) as usize];
                    p.progress = 0.0;
                }
            }
        }
    }

    /// 确保 CD 封面纹理与设置一致(路径或圆形变化时重解码/烘掊圆形蒙版;
    /// None 路径 = 内置默认唱片图)。失败静默用主题色圆盘兕底。
    fn ensure_cover(&mut self) {
        let want = (self.settings.disc.cover.clone(), self.settings.disc.round);
        if self.cover_key.as_ref() == Some(&want) {
            return;
        }
        self.cover_key = Some(want.clone());
        let decoded = match want.0.as_ref() {
            Some(path) => image::open(path)
                .map_err(|e| eprintln!("封面解码失败:{e}"))
                .ok(),
            None => image::load_from_memory(APP_ICON_PNG).ok(),
        };
        match decoded {
            Some(img) => {
                // 解码 → 降采样 → into_rgba8 原地搬移,全程扁平 RGBA,
                // 裁方/蒙版/上传零格式转换(省 2 份整图拷贝)。
                let rgba = downscale_to_edge(img, COVER_MAX_EDGE).into_rgba8();
                let (w, h) = rgba.dimensions();
                let flat = rgba.into_raw();
                // 中心裁方:非方图不再拉伸变形(短边为边长,居中裁切)。
                let (w, h) = (w as usize, h as usize);
                let side = w.min(h);
                let (x0, y0) = ((w - side) / 2, (h - side) / 2);
                let mut cropped = Vec::with_capacity(side * side * 4);
                for y in y0..y0 + side {
                    let a = (y * w + x0) * 4;
                    cropped.extend_from_slice(&flat[a..a + side * 4]);
                }
                let (w, h) = (side, side);
                if want.1 {
                    bake_circle_mask(&mut cropped, w, h);
                }
                self.cover_handle = Some(ImageHandle::from_rgba(w as u32, h as u32, cropped));
            }
            None => self.cover_handle = None,
        }
    }
}

/// 长边超过 `max_edge` 时按比例降采样;小图原样返回。
fn downscale_to_edge(img: image::DynamicImage, max_edge: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_edge && h <= max_edge {
        return img;
    }
    let scale = max_edge as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    img.resize_exact(nw, nh, image::imageops::FilterType::Triangle)
}

/// 圆形封面蒙版:圆外 alpha 归零,边缘 2px 线性羽化(扁平 RGBA)。
/// 加载时一次性烘焙,运行时零成本(iced canvas 无路径裁剪,图像只能矩形绘制)。
fn bake_circle_mask(pixels: &mut [u8], w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    let cx = (w as f32 - 1.0) / 2.0;
    let cy = (h as f32 - 1.0) / 2.0;
    let r = w.min(h) as f32 / 2.0;
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let a = ((r - d) / 2.0).clamp(0.0, 1.0);
            if a < 1.0 {
                let idx = (y * w + x) * 4 + 3;
                pixels[idx] = (pixels[idx] as f32 * a) as u8;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// update / view / subscription
// ---------------------------------------------------------------------------

fn update(app: &mut SpectrumApp, message: Message) -> Task<Message> {
    // 菜单栏下拉中的命令项:执行即收起下拉(非 macOS)。
    #[cfg(not(target_os = "macos"))]
    if matches!(
        message,
            | Message::ToggleFullscreen
            | Message::ToggleFullscreenDefault
            | Message::ToggleFrozen
            | Message::ToggleAlwaysOnTop
            | Message::ToggleRememberWindow
            | Message::ToggleLaunchAtLogin
            | Message::TogglePreventSleep
            | Message::ToggleHud
            | Message::SetBandCount(_)
            | Message::SetDecay(_)
            | Message::SelectSource(_)
            | Message::OpenEffects
            | Message::OpenAbout
            | Message::OpenUpdate
    ) {
        app.open_menu = None;
    }

    match message {
        Message::Frame => {
            let frame_start = Instant::now();
            let now = app.start.elapsed().as_secs_f64();
            let mut tasks: Vec<Task<Message>> = Vec::new();

            // macOS 原生菜单:首帧安装,每帧轮询命令、同步勾选、重建声源。
            #[cfg(target_os = "macos")]
            {
                if app.native_menu.is_none() {
                    app.native_menu = NativeMenu::install();
                    if std::env::var_os("MUSIC_SPEC_DEBUG").is_some() {
                        eprintln!(
                            "[menu] install: {}",
                            if app.native_menu.is_some() {
                                "ok"
                            } else {
                                "FAILED"
                            }
                        );
                    }
                }
                let commands: Vec<MenuCommand> = match app.native_menu.as_mut() {
                    None => Vec::new(),
                    Some(menu) => {
                        menu.rebuild_sources(
                            &app.loopback_devices,
                            &app.input_devices,
                            app.source.as_ref().map(|d| d.id.as_str()),
                        );
                        let polled = menu.poll();
                        menu.sync(
                            app.settings.view,
                            app.fullscreen,
                            app.settings.always_on_top,
                            app.frozen,
                            app.settings.show_hud,
                            app.settings.launch_at_login,
                            app.settings.prevent_sleep,
                        );
                        polled
                    }
                };
                for cmd in commands {
                    tasks.push(app.apply_menu_command(cmd));
                }
            }

            // 全屏状态回读(节流;兼容系统绿色按钮)。
            if let Some(id) = app.main_window {
                // 启动全屏:首帧后应用(窗口以窗口模式创建)。
                if app.fullscreen_start {
                    app.fullscreen_start = false;
                    tasks.push(window::set_mode(id, window::Mode::Fullscreen));
                }
                if now - app.last_mode_poll >= MODE_POLL_SECS {
                    app.last_mode_poll = now;
                    tasks.push(window::mode(id).map(Message::WindowMode));
                }
            }

            app.poll_update();
            app.tick(now);
            app.pump_audio();
            // 冻结时分析继续(环缓冲不积压),但显示与语谱滚动停住。
            if !app.frozen {
                app.spectrograph.push_frame(now, &app.target);
                let smoothed = app.smoother.process(&app.target);
                app.last_bands = smoothed.bands;
                app.last_peaks = smoothed.peaks;
            }
            // Bass Level EMA:所有 Effect 同步律动的公共标量(Q6-b)。
            let raw_bass = bass_level(&app.last_bands);
            app.bass += 0.3 * (raw_bass - app.bass);
            app.ensure_background();
            app.ensure_cover();
            // 动画推进(冻结时定格;仅激活视图持有状态被推进;Q15:屏保/
            // 静态不消费音频,采集生命周期不变)。
            if !app.frozen {
                let dt = app
                    .last_frame_at
                    .map_or(0.0, |prev| (now - prev) as f32)
                    .clamp(0.0, 0.1);
                app.advance_animations(dt);
            }

            // HUD statistics: EMA over ~60 frames.
            let cost_ms = frame_start.elapsed().as_secs_f64() * 1000.0;
            app.frame_cost_ms += 0.1 * (cost_ms - app.frame_cost_ms);
            if let Some(prev) = app.last_frame_at {
                let interval_ms = (now - prev) * 1000.0;
                if interval_ms > 0.0 && interval_ms < 1000.0 {
                    app.frame_interval_ms += 0.1 * (interval_ms - app.frame_interval_ms);
                }
            }
            app.last_frame_at = Some(now);

            Task::batch(tasks)
        }
        Message::Key(event) => app.on_key(event),
        Message::Window(id, event) => {
            app.on_window_event(id, event);
            Task::none()
        }
        Message::WindowMode(mode) => {
            app.fullscreen = mode == window::Mode::Fullscreen;
            Task::none()
        }
        Message::CanvasRightClick => {
            // 全屏:右键退出(设计共识);非全屏无右键行为,
            // 配置入口为应用菜单栏(非 macOS)/原生菜单栏(macOS)。
            if app.fullscreen {
                app.do_toggle_fullscreen()
            } else {
                Task::none()
            }
        }
        #[cfg(not(target_os = "macos"))]
        Message::ToggleMenu(tab) => {
            app.open_menu = if app.open_menu == Some(tab) {
                None
            } else {
                Some(tab)
            };
            Task::none()
        }
        Message::CanvasClick => {
            // 左键优先收起打开中的菜单栏下拉(不算双击)。
            #[cfg(not(target_os = "macos"))]
            if app.open_menu.is_some() {
                app.open_menu = None;
                return Task::none();
            }
            // 双击主体内容:切换全屏/退出全屏。
            let now = Instant::now();
            let is_double = app
                .last_canvas_click
                .is_some_and(|t| now.duration_since(t) <= DOUBLE_CLICK_INTERVAL);
            app.last_canvas_click = if is_double { None } else { Some(now) };
            if is_double {
                app.do_toggle_fullscreen()
            } else {
                Task::none()
            }
        }
        Message::OpenEffects => {
            app.effects_open = true;
            Task::none()
        }
        Message::CloseEffects => {
            app.effects_open = false;
            Task::none()
        }
        Message::SelectTab(tab) => match tab {
            EffectsTab::General => {
                app.effects_general = true;
                Task::none()
            }
            EffectsTab::View(v) => {
                app.effects_general = false;
                if app.settings.view != v {
                    app.settings.view = v;
                    app.mark_dirty();
                }
                Task::none()
            }
        }
        Message::ToggleFullscreen => app.do_toggle_fullscreen(),
        Message::ToggleAlwaysOnTop => app.do_toggle_always_on_top(),
        Message::ToggleFrozen => {
            app.frozen = !app.frozen;
            Task::none()
        }
        Message::ToggleHud => {
            app.settings.show_hud = !app.settings.show_hud;
            app.mark_dirty();
            Task::none()
        }
        Message::SelectSource(info) => app.do_select_source(info),
        Message::CheckUpdates => {
            app.start_check();
            Task::none()
        }
        Message::DownloadUpdate => {
            if let UpdatePhase::Available(info) = app.update_phase.clone() {
                let total = info.asset_size;
                app.update_phase = UpdatePhase::Downloading { received: 0, total };
                app.update_rx = Some(updater::spawn_download(info));
            }
            Task::none()
        }
        Message::OpenAbout => {
            app.about_open = true;
            Task::none()
        }
        Message::CloseAbout => {
            app.about_open = false;
            Task::none()
        }
        Message::OpenUpdate => {
            app.update_open = true;
            Task::none()
        }
        Message::CloseUpdate => {
            app.update_open = false;
            Task::none()
        }
        Message::SetBandCount(n) => {
            let n = n.clamp(BAND_COUNT_MIN, BAND_COUNT_MAX);
            if app.settings.band_count != n {
                app.settings.band_count = n;
                app.rebuild_analysis();
                app.mark_dirty();
            }
            Task::none()
        }
        Message::SetDecay(d) => {
            app.settings.decay = d;
            app.smoother.set_decay(d.into());
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleGrid => {
            app.settings.style.grid = !app.settings.style.grid;
            app.mark_dirty();
            Task::none()
        }
        Message::SetTheme(t) => {
            app.settings.style.theme = t;
            app.mark_dirty();
            Task::none()
        }
        Message::ColorSlotReset(slot) => {
            app.color_reset(slot);
            Task::none()
        }
        Message::SetCornerRadius(v) => {
            app.settings.style.corner_radius = v;
            app.mark_dirty();
            Task::none()
        }
        Message::SetBarGap(v) => {
            app.settings.style.bar_gap = v.clamp(0.0, 0.45);
            app.mark_dirty();
            Task::none()
        }
        Message::SetSpectrumType(t) => {
            app.settings.style.spectrum_type = t;
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleBlocks => {
            app.settings.style.blocks = !app.settings.style.blocks;
            app.smoother
                .set_peak_decay_scale(peak_hold_decay_scale(app.settings.style.blocks));
            app.mark_dirty();
            Task::none()
        }
        Message::SetBlockHeight(v) => {
            app.settings.style.block_height = v.clamp(1.0, 50.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetBlockGap(v) => {
            app.settings.style.block_gap = v.clamp(1.0, 50.0);
            app.mark_dirty();
            Task::none()
        }
        Message::TogglePeakDetached => {
            app.settings.style.peak_detached = !app.settings.style.peak_detached;
            app.mark_dirty();
            Task::none()
        }
        Message::SetPeakCapHeight(v) => {
            app.settings.style.peak_cap_height = v.clamp(0.0, 10.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetPeakGap(v) => {
            app.settings.style.peak_gap = v.clamp(1.0, 10.0);
            app.mark_dirty();
            Task::none()
        }
        // —— 各视图独立配置(特效窗体,ADR-0005)——
        Message::SetWaveformMirrored(v) => {
            app.settings.waveform.mirrored = v;
            app.mark_dirty();
            Task::none()
        }
        Message::SetWaveformWidth(v) => {
            app.settings.waveform.line_width = v.clamp(0.5, 8.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetWaveformColor(c) => {
            app.settings.waveform.color = c;
            app.mark_dirty();
            Task::none()
        }
        Message::SetWaveformGain(v) => {
            app.settings.waveform.gain = v.clamp(1.0, 16.0);
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleColorPicker(id) => {
            if app.picker_open == Some(id) {
                app.picker_open = None;
            } else {
                app.picker_working = app.resolve_picker_current(id);
                app.picker_open = Some(id);
            }
            Task::none()
        }
        Message::CancelColorPicker => {
            app.picker_open = None;
            Task::none()
        }
        Message::SubmitColorPicker(c) => {
            let rgb = [
                (c.r * 255.0).round() as u8,
                (c.g * 255.0).round() as u8,
                (c.b * 255.0).round() as u8,
            ];
            if let Some(id) = app.picker_open {
                app.apply_picker_color(id, rgb);
            }
            app.picker_open = None;
            app.mark_dirty();
            Task::none()
        }
        Message::EffectColorReset(slot) => {
            match slot {
                EffectColorSlot::Waveform => app.settings.waveform.custom_color = None,
                EffectColorSlot::Particles => app.settings.particles.custom_color = None,
                EffectColorSlot::Glow => app.settings.glow.custom_color = None,
                EffectColorSlot::Background => app.settings.background.color = None,
            }
            app.mark_dirty();
            Task::none()
        }
        Message::SetParticleCount(n) => {
            app.settings.particles.count = n.clamp(10, 400);
            app.mark_dirty();
            Task::none()
        }
        Message::SetParticleSize(v) => {
            app.settings.particles.size = v.clamp(1.0, 100.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetParticleTrail(v) => {
            app.settings.particles.trail = v.clamp(0.0, 3.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetParticleEmission(e) => {
            app.settings.particles.emission = e;
            app.mark_dirty();
            Task::none()
        }
        Message::SetGlowCount(n) => {
            app.settings.glow.count = n.clamp(1, 64);
            app.mark_dirty();
            Task::none()
        }
        Message::SetGlowLayout(l) => {
            app.settings.glow.layout = l;
            app.mark_dirty();
            Task::none()
        }
        Message::SetGlowIntensity(v) => {
            app.settings.glow.intensity = v.clamp(0.0, 1.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetGlowHueDrift(v) => {
            app.settings.glow.hue_drift = v;
            app.mark_dirty();
            Task::none()
        }
        Message::SetDiscSpeed(v) => {
            app.settings.disc.speed = v.clamp(0.0, 3.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetDiscPulse(v) => {
            app.settings.disc.pulse = v.clamp(0.0, 1.0);
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleDiscTonearm => {
            app.settings.disc.tonearm = !app.settings.disc.tonearm;
            app.mark_dirty();
            Task::none()
        }
        Message::PickCover => {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("图片", &["png", "jpg", "jpeg"])
                .set_title("选择封面")
                .pick_file()
            {
                app.settings.disc.cover = Some(path);
                app.mark_dirty();
            }
            Task::none()
        }
        Message::ClearCover => {
            app.settings.disc.cover = None;
            app.mark_dirty();
            Task::none()
        }
        Message::SetDiscRound(v) => {
            app.settings.disc.round = v;
            app.mark_dirty();
            Task::none()
        }
        Message::SetScreensaverStyle(style) => {
            app.settings.screensaver.style = style;
            app.mark_dirty();
            Task::none()
        }
        Message::PickBackground => {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("图片", &["png", "jpg", "jpeg"])
                .set_title("选择背景图")
                .pick_file()
            {
                app.settings.background.path = Some(path);
                app.mark_dirty();
            }
            Task::none()
        }
        Message::ClearBackground => {
            app.settings.background.path = None;
            app.mark_dirty();
            Task::none()
        }
        Message::SetScaleMode(m) => {
            app.settings.background.mode = m;
            app.mark_dirty();
            Task::none()
        }
        Message::SetOpacity(v) => {
            app.settings.background.opacity = v.clamp(0.05, 1.0);
            app.mark_dirty();
            Task::none()
        }
        Message::SetBlur(v) => {
            app.settings.background.blur_px = v.clamp(0.0, 24.0) as u32;
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleFullscreenDefault => {
            app.settings.default_fullscreen = !app.settings.default_fullscreen;
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleRememberWindow => {
            app.settings.remember_window = !app.settings.remember_window;
            app.mark_dirty();
            Task::none()
        }
        Message::ToggleLaunchAtLogin => {
            app.do_toggle_launch_at_login();
            Task::none()
        }
        Message::TogglePreventSleep => {
            app.do_toggle_prevent_sleep();
            Task::none()
        }
    }
}

fn subscription(_app: &SpectrumApp) -> Subscription<Message> {
    Subscription::batch([
        keyboard::listen().map(Message::Key),
        window::events().map(|(id, event)| Message::Window(id, event)),
    ])
}

fn view(app: &SpectrumApp) -> Element<'_, Message> {
    let palette = resolve_palette(&app.settings.style);
    let bg_on = app.bg_handle.is_some();

    // 语谱纹理:仅在历史版本/频段/背景/调色板变化时重建并上传;
    // 冻结或尚未产生新列时直接复用上一帧纹理(省掉 230KB 分配与整图上传)。
    let spectro = if app.settings.view == ViewMode::Spectrogram {
        let key = SpectroKey {
            revision: app.spectrograph.revision(),
            bands: app.settings.band_count,
            bg_on,
            palette,
        };
        let mut cache = app.spectro_cache.borrow_mut();
        if cache.key.as_ref() != Some(&key) {
            let flat = spectro_pixels(&app.spectrograph, &palette, bg_on);
            cache.handle = Some(ImageHandle::from_rgba(
                SPECTRO_COLUMN_COUNT as u32,
                key.bands as u32,
                flat,
            ));
            cache.key = Some(key);
        }
        cache.handle.clone()
    } else {
        None
    };

    // 频谱占据菜单栏以下的整个窗体:状态与 HUD 为画布左下/右下浮层,
    // 配置走应用菜单栏(非 macOS)/原生菜单(macOS)/设置窗。
    // 层序(自底向上):背景 → 语谱纹理 → 主体。canvas 同帧内图像恒
    // 绘制在几何之上(合成层顺序固定 primitives → images → text),
    // 背景图/纹理必须各占独立 widget 层,主体画布叠在最上才能压住它们;
    // 仅主体画布发布 Frame,维持单消息/帧。
    let mut plot_layers: Vec<Element<'_, Message>> = vec![
        canvas(BackgroundView {
            palette,
            bg: app.bg_handle.clone(),
            bg_src_size: app.bg_src_size,
            bg_settings: &app.settings.background,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    ];
    if let Some(handle) = &spectro {
        plot_layers.push(
            canvas(TextureView {
                handle: handle.clone(),
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        );
    }
    plot_layers.push(
        canvas(SpectrumView {
            app,
            palette,
            bg_on,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    );
    // 唱针独立图层:canvas 同帧内图像恒绘制在几何之上,唱针要在封面之上
    // 只能再叠一层画布(不发布 Frame,心跳由主体画布独占)。
    if app.settings.view == ViewMode::Disc && app.settings.disc.tonearm {
        plot_layers.push(
            canvas(DiscTonearmView { app })
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    }

    // 应用菜单栏(非 macOS,常驻顶栏;全屏时隐藏保持纯净画面)。
    #[cfg(not(target_os = "macos"))]
    let bar: Option<Element<'_, Message>> =
        (!app.fullscreen).then(|| menu_bar(app));
    #[cfg(target_os = "macos")]
    let bar: Option<Element<'_, Message>> = None;
    let mut top = column![];
    if let Some(bar) = bar {
        top = top.push(bar);
    }
    top = top.push(stack(plot_layers).width(Length::Fill).height(Length::Fill));

    let mut layers: Vec<Element<'_, Message>> = vec![top.into()];

    // 右下性能 HUD(默认关;设置菜单可开)。
    if app.settings.show_hud {
        let hud = format!(
            "{:.2}ms/帧 · {:.0}fps · ≈{:.1}% CPU",
            app.frame_cost_ms,
            1000.0 / app.frame_interval_ms.max(0.001),
            100.0 * app.frame_cost_ms / app.frame_interval_ms
        );
        layers.push(
            container(
                container(text(hud).size(10.0).color(AXIS_TEXT).font(Font::MONOSPACE))
                    .padding(Padding {
                        top: 4.0,
                        right: 10.0,
                        bottom: 4.0,
                        left: 10.0,
                    })
                    .style(|_theme: &Theme| chip_style()),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Right)
            .align_y(Vertical::Bottom)
            .padding(12.0)
            .into(),
        );
    }

    // 非 macOS:菜单栏下拉浮层(锚定在对应菜单项下方)。
    #[cfg(not(target_os = "macos"))]
    if let Some(tab) = app.open_menu
        && !app.fullscreen {
        layers.push(dropdown(app, tab));
    }

    // 特效窗体(ADR-0005:tab 化的全部视图配置 + 通用页;可滚动,不常驻)。
    if app.effects_open {
        layers.push(dim_overlay(Message::CloseEffects));
        layers.push(effects_panel(app));
    }

    // 「关于」窗口(菜单栏「关于」下拉子项 / macOS 原生菜单打开;点遮罩关闭)。
    if app.about_open {
        layers.push(dim_overlay(Message::CloseAbout));
        layers.push(about_overlay(app));
    }

    // 「检测更新」窗口(同上;启动已自动检测,红点在菜单栏「关于」上)。
    if app.update_open {
        layers.push(dim_overlay(Message::CloseUpdate));
        layers.push(update_overlay(app));
    }

    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

// ---------------------------------------------------------------------------
// 画布(三层 widget:背景 → 语谱纹理 → 频谱主体 + 网格 + 悬停读数)
// ---------------------------------------------------------------------------

/// 背景层画布:调色板底色 + 背景图。独立 widget 层(置于最底):
/// canvas 同帧内图像恒绘制在几何之上(合成层顺序固定
/// primitives → images → text),只有让频谱主体单独一层叠在其上,
/// 背景图才能被压在频谱后面。
struct BackgroundView<'a> {
    palette: Palette,
    bg: Option<ImageHandle>,
    bg_src_size: [f32; 2],
    bg_settings: &'a BackgroundSettings,
}

impl canvas::Program<Message> for BackgroundView<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        // 不发布消息:Frame 心跳由主体画布(SpectrumView)独占。
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<CanvasGeometry> {
        let mut frame = CanvasFrame::new(renderer, bounds.size());
        self.draw_background(&mut frame, Rectangle::new(Point::ORIGIN, bounds.size()));
        vec![frame.into_geometry()]
    }
}

impl BackgroundView<'_> {
    /// 背景:图片(填满/适应/平铺,不透明度经图像 alpha);无图时铺
    /// 调色板背景色。填满模式用画布裁剪实现居中裁切。
    fn draw_background(&self, frame: &mut CanvasFrame, full: Rectangle) {
        let bg = self
            .bg_settings
            .color
            .map(rgb)
            .unwrap_or_else(|| rgb(self.palette.bg));
        frame.fill_rectangle(full.position(), full.size(), bg);

        let Some(handle) = &self.bg else {
            return;
        };
        let [iw, ih] = self.bg_src_size;
        let opacity = self.bg_settings.opacity;
        let image = canvas::Image::new(handle.clone()).opacity(opacity);
        let (fw, fh) = (full.width, full.height);
        match self.bg_settings.mode {
            // 填满(cover):放大到覆盖窗口,居中,越界裁剪。
            ScaleMode::Fill => {
                let scale = (fw / iw).max(fh / ih);
                let size = Size::new(iw * scale, ih * scale);
                let pos = Point::new(
                    full.x + (fw - size.width) * 0.5,
                    full.y + (fh - size.height) * 0.5,
                );
                frame.with_clip(full, |frame| {
                    frame.draw_image(Rectangle::new(pos, size), image);
                });
            }
            // 适应(contain):完整可见,居中留边。
            ScaleMode::Fit => {
                let scale = (fw / iw).min(fh / ih);
                let size = Size::new(iw * scale, ih * scale);
                let pos = Point::new(
                    full.x + (fw - size.width) * 0.5,
                    full.y + (fh - size.height) * 0.5,
                );
                frame.draw_image(Rectangle::new(pos, size), image);
            }
            // 平铺(tile):自然尺寸重复。
            ScaleMode::Tile => {
                frame.with_clip(full, |frame| {
                    let mut y = full.y;
                    while y < full.y + fh {
                        let mut x = full.x;
                        while x < full.x + fw {
                            frame.draw_image(
                                Rectangle::new(Point::new(x, y), Size::new(iw, ih)),
                                image.clone(),
                            );
                            x += iw;
                        }
                        y += ih;
                    }
                });
            }
        }
    }
}

/// 语谱纹理层画布(独立 widget 层:背景之上、主体之下,网格/读数叠其上)。
struct TextureView {
    handle: ImageHandle,
}

impl canvas::Program<Message> for TextureView {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<CanvasGeometry> {
        let full = Rectangle::new(Point::ORIGIN, bounds.size());
        let plot = plot_rect(full);
        let mut frame = CanvasFrame::new(renderer, bounds.size());
        frame.draw_image(plot, canvas::Image::new(self.handle.clone()));
        vec![frame.into_geometry()]
    }
}

/// CD 唱针图层(仅 Disc 视图 + 唱针开启时入栈):独立画布保证唱针
/// 悬浮在封面图像之上。
struct DiscTonearmView<'a> {
    app: &'a SpectrumApp,
}

impl canvas::Program<Message> for DiscTonearmView<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<CanvasGeometry> {
        let (w, h) = (bounds.width, bounds.height);
        let m = w.min(h);
        let cx = bounds.x + w * 0.5;
        let cy = bounds.y + h * 0.5;
        let scale = 1.0
            + self.app.settings.disc.pulse.clamp(0.0, 1.0) * 0.22 * self.app.bass;
        let r_disc = m * 0.36 * scale;
        let mut frame = CanvasFrame::new(renderer, bounds.size());
        let pivot = Point::new(cx + r_disc * 1.04, cy - r_disc * 1.04);
        let sway = (self.app.now() as f32 * 0.8).sin() * 0.03;
        let a = std::f32::consts::FRAC_PI_4 + sway;
        let tip = Point::new(
            pivot.x - a.cos() * r_disc * 1.30,
            pivot.y + a.sin() * r_disc * 1.30,
        );
        // 碟面投影(镜像):半透明阴影,带一点动态偏移。
        let shadow_dx = 3.0 + 1.5 * (self.app.now() as f32 * 0.6).sin();
        let shadow_dy = 4.0 + 1.5 * (self.app.now() as f32 * 0.45).cos();
        let shadow_move = |pt: Point| Point::new(pt.x + shadow_dx, pt.y + shadow_dy);
        let a_sh = std::f32::consts::FRAC_PI_4 + sway + 0.01;
        let tip_sh = Point::new(
            pivot.x - a_sh.cos() * r_disc * 1.30,
            pivot.y + a_sh.sin() * r_disc * 1.30,
        );
        let elbow_sh = Point::new(
            pivot.x - a_sh.cos() * r_disc * 0.72 + a_sh.sin() * r_disc * 0.05,
            pivot.y + a_sh.sin() * r_disc * 0.72 + a_sh.cos() * r_disc * 0.05,
        );
        let shadow = Color::from_rgba(0.0, 0.0, 0.0, 0.22);
        frame.stroke(
            &canvas::Path::line(shadow_move(pivot), shadow_move(elbow_sh)),
            canvas::Stroke::default().with_width(7.0).with_color(shadow),
        );
        frame.stroke(
            &canvas::Path::line(shadow_move(elbow_sh), shadow_move(tip_sh)),
            canvas::Stroke::default().with_width(5.0).with_color(shadow),
        );
        // 主臂:两段(枢轴 → 肘部 → 头壳),肘部微折更真实。
        let elbow = Point::new(
            pivot.x - a.cos() * r_disc * 0.72 + a.sin() * r_disc * 0.05,
            pivot.y + a.sin() * r_disc * 0.72 + a.cos() * r_disc * 0.05,
        );
        let metal = Color::from_rgb(0.78, 0.80, 0.84);
        let metal_dark = Color::from_rgb(0.42, 0.44, 0.50);
        frame.stroke(
            &canvas::Path::line(pivot, elbow),
            canvas::Stroke::default()
                .with_width(6.0)
                .with_color(Color { a: 0.95, ..metal }),
        );
        frame.stroke(
            &canvas::Path::line(elbow, tip),
            canvas::Stroke::default()
                .with_width(4.5)
                .with_color(Color { a: 0.95, ..metal }),
        );
        // 头壳(唱头):沿臂方向的小矩形体 + 唱针尖点。
        let hx = (tip.x - elbow.x) / (r_disc * 0.58);
        let hy = (tip.y - elbow.y) / (r_disc * 0.58);
        let head_len = r_disc * 0.10;
        let head_w = r_disc * 0.045;
        let head = canvas::Path::new(|p| {
            p.move_to(Point::new(tip.x + hy * head_w, tip.y - hx * head_w));
            p.line_to(Point::new(tip.x - hx * head_len + hy * head_w, tip.y - hy * head_len - hx * head_w));
            p.line_to(Point::new(tip.x - hx * head_len - hy * head_w, tip.y - hy * head_len + hx * head_w));
            p.line_to(Point::new(tip.x - hy * head_w, tip.y + hx * head_w));
            p.close();
        });
        frame.fill(&head, metal_dark);
        frame.fill(
            &canvas::Path::circle(
                Point::new(tip.x - hx * head_len * 0.9, tip.y - hy * head_len * 0.9),
                2.0,
            ),
            Color::from_rgb(0.95, 0.95, 0.98),
        );
        // 配重(枢轴反向短粗杆)。
        let cw_dir = Point::new(a.cos(), -a.sin());
        let cw_end = Point::new(pivot.x + cw_dir.x * r_disc * 0.16, pivot.y + cw_dir.y * r_disc * 0.16);
        frame.stroke(
            &canvas::Path::line(pivot, cw_end),
            canvas::Stroke::default()
                .with_width(9.0)
                .with_color(Color { a: 0.95, ..metal_dark }),
        );
        // 枢轴底座:金属外圈 + 深色内芯。
        frame.fill(&canvas::Path::circle(pivot, 10.0), metal);
        frame.fill(
            &canvas::Path::circle(pivot, 6.0),
            Color::from_rgb(0.16, 0.17, 0.21),
        );
                vec![frame.into_geometry()]
    }
}

struct SpectrumView<'a> {
    app: &'a SpectrumApp,
    palette: Palette,
    /// 背景图层存在(底色转半透明透出背景)。
    bg_on: bool,
}

impl canvas::Program<Message> for SpectrumView<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            // 常驻重绘(设计共识):每次重绘请求发布 Frame 消息,消息处理
            // 产生新的画面 → 新的重绘请求,自维持 vsync 帧循环。
            canvas::Event::Window(window::Event::RedrawRequested(_)) => {
                Some(canvas::Action::publish(Message::Frame))
            }
            // 右键:全屏时退出;左键:收起下拉/双击切换全屏(判定在 update)。
            canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(button)) => match button {
                iced::mouse::Button::Right => cursor
                    .position_over(bounds)
                    .map(|_| canvas::Action::publish(Message::CanvasRightClick).and_capture()),
                iced::mouse::Button::Left => cursor
                    .position_over(bounds)
                    .map(|_| canvas::Action::publish(Message::CanvasClick).and_capture()),
                _ => None,
            },
            _ => None,
        }
    }

    /// 单层几何:底色修正/频谱柱、网格、悬停读数(背景图与语谱纹理
    /// 在下层 widget,见 [`BackgroundView`]/[`TextureView`])。
    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> Vec<CanvasGeometry> {
        let full = Rectangle::new(Point::ORIGIN, bounds.size());
        let plot = plot_rect(full);

        let mut frame = CanvasFrame::new(renderer, bounds.size());
        match self.app.settings.view {
            ViewMode::Spectrum => self.draw_spectrum(&mut frame, plot),
            ViewMode::Spectrogram => self.draw_spectrogram_overlay(&mut frame, plot),
            ViewMode::Waveform => self.draw_waveform(&mut frame, plot),
            ViewMode::Particles => self.draw_particles(&mut frame, plot),
            ViewMode::Glow => self.draw_glow(&mut frame, plot),
            ViewMode::Disc => self.draw_disc(&mut frame, plot),
            // 静态:通用背景图独占画面(背景层已绘,主体层无几何)。
            ViewMode::Static => {}
            ViewMode::Screensaver => self.draw_screensaver(&mut frame, plot),
        }
        // 悬停读数仅线性频谱/语谱有意义(圆形布局无法用 x 轴反查频段)。
        let hoverable = match self.app.settings.view {
            ViewMode::Spectrogram => true,
            ViewMode::Spectrum => {
                self.app.settings.style.spectrum_type != SpectrumType::Radial
            }
            _ => false,
        };
        if hoverable {
            self.draw_hover(&mut frame, plot, bounds, cursor);
        }

        vec![frame.into_geometry()]
    }
}

impl SpectrumView<'_> {
    fn draw_spectrum(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let pal = &self.palette;
        let bg_on = self.bg_on;
        // 画布底:有背景时半透明,无背景时实底。
        let plot_bg = Color::from_rgba8(
            pal.bg[0],
            pal.bg[1],
            pal.bg[2],
            if bg_on { 150.0 / 255.0 } else { 1.0 },
        );
        // 全出血:底色铺满画布,无圆角无边距。
        frame.fill_rectangle(plot.position(), plot.size(), plot_bg);

        let kind = self.app.settings.style.spectrum_type;
        let blocks = self.app.settings.style.blocks;
        if self.app.settings.style.grid {
            match kind {
                SpectrumType::Radial => self.draw_radial_grid(frame, plot),
                SpectrumType::Bar => self.draw_linear_grid(frame, plot),
            }
        }
        match (kind, blocks) {
            (SpectrumType::Bar, false) => self.draw_spectrum_bars(frame, plot),
            (SpectrumType::Bar, true) => self.draw_spectrum_linear_blocks(frame, plot),
            (SpectrumType::Radial, false) => self.draw_spectrum_radial(frame, plot),
            (SpectrumType::Radial, true) => self.draw_spectrum_radial_blocks(frame, plot),
        }
    }

    /// 条形布局的幅度与频率网格。
    fn draw_linear_grid(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let (l, r) = (plot.x, plot.x + plot.width);
        let (top, h) = (plot.y, plot.height);
        for &db in DB_TICKS {
            let y = db_to_y(db, top, h);
            frame.stroke(
                &canvas::Path::line(Point::new(l, y), Point::new(r, y)),
                grid_stroke(GRID),
            );
            frame.fill_text(canvas::Text {
                content: format!("{}dB", db as i32),
                position: Point::new(l + 4.0, y - 2.0),
                size: Pixels(10.0),
                color: AXIS_TEXT,
                font: Font::MONOSPACE,
                align_x: Horizontal::Left.into(),
                align_y: Vertical::Bottom,
                ..Default::default()
            });
        }
        self.draw_freq_grid(frame, plot);
    }

    /// 径向布局的同心 dB 环与频率辐条。
    fn draw_radial_grid(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let (cx, cy, r0, span) = radial_geometry(plot);
        for &db in DB_TICKS {
            let radius = r0 + span * db_t(db);
            frame.stroke(
                &canvas::Path::circle(Point::new(cx, cy), radius),
                grid_stroke(GRID),
            );
        }
        for &f in FREQ_TICKS {
            let a = radial_angle(freq_to_t(f));
            let (sin, cos) = a.sin_cos();
            frame.stroke(
                &canvas::Path::line(
                    Point::new(cx + cos * r0, cy + sin * r0),
                    Point::new(cx + cos * (r0 + span), cy + sin * (r0 + span)),
                ),
                grid_stroke(FREQ_GRID),
            );
        }
    }

    /// 圆形布局中心:默认填充渐变底色,其上叠加渐变顶色的雷达
    /// 扫描线与拖尾。
    fn draw_radar_sweep(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let (cx, cy, r0, _) = radial_geometry(plot);
        let base = self.palette.bar_bottom;
        let tip = self.palette.bar_top;
        let center = Point::new(cx, cy);
        // 中心圆默认用渐变底色铺底,替代原来的背景色。
        frame.fill(
            &canvas::Path::circle(center, r0),
            Color::from_rgba8(base[0], base[1], base[2], 1.0),
        );
        let head = radial_angle(
            (self.app.now() as f32 * RADAR_TURNS_PER_SEC).rem_euclid(1.0),
        );
        let tail = head - RADAR_SWEEP;
        let step = RADAR_SWEEP / (RADAR_TRAIL_LINES - 1) as f32;
        let color = |alpha: f32| Color::from_rgba8(tip[0], tip[1], tip[2], alpha);
        for i in 0..RADAR_TRAIL_LINES {
            let t = i as f32 / (RADAR_TRAIL_LINES - 1) as f32;
            let angle = tail + step * i as f32;
            let (s, c) = angle.sin_cos();
            let point = Point::new(cx + c * r0, cy + s * r0);
            let line = canvas::Path::line(center, point);
            frame.stroke(
                &line,
                canvas::Stroke::default()
                    .with_width(1.2 + 0.8 * t)
                    .with_color(color(t * 0.8)),
            );
        }
        // 前沿扫描线使用渐变顶色。
        let (s, c) = head.sin_cos();
        let edge = canvas::Path::line(center, Point::new(cx + c * r0, cy + s * r0));
        frame.stroke(
            &edge,
            canvas::Stroke::default()
                .with_width(2.5)
                .with_color(color(0.95)),
        );
    }

    /// 条形布局:保持不变的对数轴竖条 + 垂直渐变。
    fn draw_spectrum_bars(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let pal = &self.palette;
        let style = &self.app.settings.style;
        let (l, w) = (plot.x, plot.width);
        let (top, bottom, h) = (plot.y, plot.y + plot.height, plot.height);
        let peak_color =
            Color::from_rgba8(pal.peak_hold[0], pal.peak_hold[1], pal.peak_hold[2], 225.0 / 255.0);
        let rounding = style.corner_radius;
        for i in 0..self.app.analyzer.num_bands() {
            let (f_lo, f_hi) = self.app.analyzer.band_edges(i);
            let x0 = freq_to_x(f_lo, l, w) + 0.5;
            let x1 = freq_to_x(f_hi, l, w) - 0.5;
            let raw_w = x1 - x0;
            if raw_w < 0.5 {
                continue;
            }
            let inset = (raw_w * style.bar_gap * 0.5).min(raw_w * 0.45);
            let (x0, x1) = (x0 + inset, x1 - inset);
            let bar_top = spectrum_to_y(self.app.last_bands[i], top, h);
            if bottom - bar_top > 0.5 {
                // 垂直渐变(基色 → 顶色)以 12 段实心矩形近似(等价
                // egui 时代的 12 段渐变网格,肉眼无差)。
                const SEGMENTS: usize = 12;
                for s in 0..SEGMENTS {
                    let t0 = s as f32 / SEGMENTS as f32;
                    let t1 = (s + 1) as f32 / SEGMENTS as f32;
                    let y0 = bottom + (bar_top - bottom) * t0;
                    let y1 = bottom + (bar_top - bottom) * t1;
                    frame.fill_rectangle(
                        Point::new(x0, y0),
                        Size::new(x1 - x0, y1 - y0),
                        band_color(pal, (t0 + t1) * 0.5),
                    );
                }
            }
            draw_peak_cap(
                frame,
                (x0, x1),
                self.app.last_peaks[i],
                plot,
                peak_color,
                rounding,
                (
                    style.peak_cap_height,
                    if style.peak_detached { style.peak_gap } else { 0.0 },
                ),
            );
        }
    }

    /// 方块选项下的条形布局:每个频段由自下而上堆叠的方块组成。
    fn draw_spectrum_linear_blocks(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let pal = &self.palette;
        let style = &self.app.settings.style;
        let (l, w) = (plot.x, plot.width);
        let (top, bottom, h) = (plot.y, plot.y + plot.height, plot.height);
        let peak_color =
            Color::from_rgba8(pal.peak_hold[0], pal.peak_hold[1], pal.peak_hold[2], 225.0 / 255.0);
        for i in 0..self.app.analyzer.num_bands() {
            let (f_lo, f_hi) = self.app.analyzer.band_edges(i);
            let x0 = freq_to_x(f_lo, l, w) + 0.5;
            let x1 = freq_to_x(f_hi, l, w) - 0.5;
            let raw_w = x1 - x0;
            if raw_w < 0.5 {
                continue;
            }
            let inset = (raw_w * style.bar_gap * 0.5).min(raw_w * 0.45);
            let (x0, x1) = (x0 + inset, x1 - inset);
            // 方块高度与间隔由样式设置控制,并按绘图高度等比缩放。
            let bar_top = spectrum_to_y(self.app.last_bands[i], top, h);
            let level_h = bottom - bar_top;
            let cell = scaled_block(style.block_height, h);
            let cell_gap = scaled_block(style.block_gap, h);
            let rows = (level_h / (cell + cell_gap)).floor() as usize;
            for row in 0..rows {
                let y1 = bottom - row as f32 * (cell + cell_gap);
                let y0 = (y1 - cell).max(top);
                if y1 - y0 < 0.5 {
                    continue;
                }
                let t = ((bottom - (y0 + y1) * 0.5) / h).clamp(0.0, 1.0);
                fill_rounded(
                    frame,
                    Rectangle::new(Point::new(x0, y0), Size::new(x1 - x0, y1 - y0)),
                    style.corner_radius.min(1.5),
                    band_color(pal, t),
                );
            }
            draw_peak_cap(
                frame,
                (x0, x1),
                self.app.last_peaks[i],
                plot,
                peak_color,
                style.corner_radius,
                (
                    style.peak_cap_height,
                    if style.peak_detached { style.peak_gap } else { 0.0 },
                ),
            );
        }
    }

    /// 圆形布局:频段沿圆周由内向外辐射,径向长度即电平。
    fn draw_spectrum_radial(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let pal = &self.palette;
        let style = &self.app.settings.style;
        let (cx, cy, r0, span) = radial_geometry(plot);
        self.draw_radar_sweep(frame, plot);
        let peak_color =
            Color::from_rgba8(pal.peak_hold[0], pal.peak_hold[1], pal.peak_hold[2], 225.0 / 255.0);
        for i in 0..self.app.analyzer.num_bands() {
            let (f_lo, f_hi) = self.app.analyzer.band_edges(i);
            let (t0, t1) = (freq_to_t(f_lo), freq_to_t(f_hi));
            let dt = t1 - t0;
            if dt <= 0.0 {
                continue;
            }
            let inset = (dt * style.bar_gap * 0.5).min(dt * 0.45);
            let a0 = radial_angle(t0 + inset);
            let a1 = radial_angle(t1 - inset);
            let outer = r0 + span * spectrum_t(self.app.last_bands[i]);
            if outer - r0 > 0.5 {
                // 径向渐变(内=基色,外=顶色)以 8 段弧近似。
                const SEGMENTS: usize = 8;
                for s in 0..SEGMENTS {
                    let u0 = s as f32 / SEGMENTS as f32;
                    let u1 = (s + 1) as f32 / SEGMENTS as f32;
                    let ri = r0 + (outer - r0) * u0;
                    let ro = r0 + (outer - r0) * u1;
                    let arc = radial_arc(cx, cy, (ri + ro) * 0.5, a0, a1);
                    frame.stroke(
                        &arc,
                        canvas::Stroke::default()
                            .with_width((ro - ri).max(0.5))
                            .with_color(band_color(pal, (u0 + u1) * 0.5)),
                    );
                }
            }
            let peak = self.app.last_peaks[i];
            // 峰值帽:同一直径上的短弧;衰减到最低电平时落在柱体根部。
            let rp = r0 + span * spectrum_t(peak);
            let gap = if style.peak_detached { style.peak_gap } else { 0.0 };
            let arc = radial_arc(cx, cy, rp + gap, a0, a1);
            frame.stroke(
                &arc,
                canvas::Stroke::default()
                    .with_width(style.peak_cap_height.max(1.0))
                    .with_color(peak_color),
            );
        }
    }

    /// 方块选项下的圆形布局:方块沿径向堆叠,径向长度即电平。
    fn draw_spectrum_radial_blocks(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let pal = &self.palette;
        let style = &self.app.settings.style;
        let (cx, cy, r0, span) = radial_geometry(plot);
        self.draw_radar_sweep(frame, plot);
        let peak_color =
            Color::from_rgba8(pal.peak_hold[0], pal.peak_hold[1], pal.peak_hold[2], 225.0 / 255.0);
        for i in 0..self.app.analyzer.num_bands() {
            let (f_lo, f_hi) = self.app.analyzer.band_edges(i);
            let (t0, t1) = (freq_to_t(f_lo), freq_to_t(f_hi));
            let dt = t1 - t0;
            if dt <= 0.0 {
                continue;
            }
            let inset = (dt * style.bar_gap * 0.5).min(dt * 0.45);
            let a0 = radial_angle(t0 + inset);
            let a1 = radial_angle(t1 - inset);
            let outer = r0 + span * spectrum_t(self.app.last_bands[i]);
            let level_h = outer - r0;
            // 方块径向厚度与间隔由样式设置控制,并按绘图尺寸等比缩放。
            let dim = plot.width.min(plot.height);
            let cell = scaled_block(style.block_height, dim);
            let cell_gap = scaled_block(style.block_gap, dim);
            if level_h > 0.5 {
                // 每块都是贯穿该频段角度的环形扇区(沿径向切分),
                // 而不是落在角平分线上的单个方块。
                let rows = (level_h / (cell + cell_gap)).floor() as usize;
                for row in 0..rows {
                    let rr = r0 + row as f32 * (cell + cell_gap) + cell * 0.5;
                    let t = (row as f32 / rows.max(1) as f32).clamp(0.0, 1.0);
                    let arc = radial_arc(cx, cy, rr, a0, a1);
                    frame.stroke(
                        &arc,
                        canvas::Stroke::default()
                            .with_width(cell)
                            .with_color(band_color(pal, t)),
                    );
                }
            }
            let peak = self.app.last_peaks[i];
            // 与条形布局共用同一峰值帽厚度;最低时落在柱体根部。
            let rp = r0 + span * spectrum_t(peak);
            let gap = if style.peak_detached { style.peak_gap } else { 0.0 };
            let arc = radial_arc(cx, cy, rp + gap, a0, a1);
            frame.stroke(
                &arc,
                canvas::Stroke::default()
                    .with_width(style.peak_cap_height.max(1.0))
                    .with_color(peak_color),
            );
        }
    }

    /// 语谱主体层:底色修正 + 频率网格。纹理在独立 widget 层(TextureView),
    /// 网格/读数因此可靠地叠在纹理上方。
    fn draw_spectrogram_overlay(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        if self.bg_on {
            // 有背景图:纹理上叠半透明底色,与频谱视图观感一致。
            let pal = &self.palette;
            let tint = Color::from_rgba8(pal.bg[0], pal.bg[1], pal.bg[2], 150.0 / 255.0);
            // 全出血:半透明底色修正铺满绘图区。
            fill_rounded(frame, plot, 0.0, tint);
        }
        if self.app.settings.style.grid {
            self.draw_freq_grid(frame, plot);
        }
    }

    /// 频率轴刻度(两个视图共享;刻度线贯穿绘图区,标签内嵌在底缘,
    /// 叠在频谱/纹理上方)。
    fn draw_freq_grid(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let (l, w) = (plot.x, plot.width);
        for &f in FREQ_TICKS {
            let x = freq_to_x(f, l, w);
            frame.stroke(
                &canvas::Path::line(Point::new(x, plot.y), Point::new(x, plot.y + plot.height)),
                grid_stroke(FREQ_GRID),
            );
            let label = if f >= 1000.0 {
                format!("{}k", (f / 1000.0) as i32)
            } else {
                format!("{}", f as i32)
            };
            frame.fill_text(canvas::Text {
                content: label,
                position: Point::new(x, plot.y + plot.height - 3.0),
                size: Pixels(10.0),
                color: AXIS_TEXT,
                font: Font::MONOSPACE,
                align_x: Horizontal::Center.into(),
                align_y: Vertical::Bottom,
                ..Default::default()
            });
        }
    }

    /// 悬停读数(设计共识:光标跟随):该处的频率与 dB。
    fn draw_hover(
        &self,
        frame: &mut CanvasFrame,
        plot: Rectangle,
        bounds: Rectangle,
        cursor: iced::mouse::Cursor,
    ) {
        let Some(pos) = cursor.position_over(bounds) else {
            return;
        };
        // 画布占满窗口:局部坐标 = 窗口坐标。
        if !plot.contains(pos) {
            return;
        }
        let app = self.app;
        let bands = app.settings.band_count;
        let band = (((plot.y + plot.height - pos.y) / plot.height) * bands as f32).floor() as usize;
        let band = band.min(bands - 1);
        let freq = app.analyzer.band_center(band);
        let freq_txt = if freq >= 1000.0 {
            format!("{:.1}kHz", freq / 1000.0)
        } else {
            format!("{freq:.0}Hz")
        };
        let value = match app.settings.view {
            ViewMode::Spectrum => app.last_bands[band],
            ViewMode::Spectrogram => {
                let x_frac = (pos.x - plot.x) / plot.width;
                let age = (x_frac * (SPECTRO_COLUMN_COUNT - 1) as f32).round() as usize;
                let age_from_newest = SPECTRO_COLUMN_COUNT - 1 - age;
                app.spectrograph
                    .column(age_from_newest)
                    .map(|c| c[band])
                    .unwrap_or(DB_FLOOR)
            }
            // 悬停读数仅频谱/语谱启用(Q11);其余视图不可达。
            _ => DB_FLOOR,
        };
        let content = format!("{freq_txt} · {value:.0}dB");
        let (tw, th) = estimate_text_size(&content, 11.0);
        let tip = Rectangle::new(
            Point::new(pos.x + 12.0, pos.y - 16.0 - th),
            Size::new(tw, th),
        );
        fill_rounded(frame, tip, 3.0, TOOLTIP_BG);
        frame.fill_text(canvas::Text {
            content,
            position: Point::new(tip.x + 6.0, tip.y + 3.0),
            size: Pixels(11.0),
            color: Color::WHITE,
            font: Font::MONOSPACE,
            align_x: Horizontal::Left.into(),
            align_y: Vertical::Top,
            ..Default::default()
        });
    }
}

impl SpectrumView<'_> {
    /// 特效视图共用的绘图区底色(与频谱一致:有背景图时半透明)。
    fn plot_bg(&self) -> Color {
        let pal = &self.palette;
        Color::from_rgba8(
            pal.bg[0],
            pal.bg[1],
            pal.bg[2],
            if self.bg_on { 150.0 / 255.0 } else { 1.0 },
        )
    }

    /// 波形:原始 2048 样本帧降采样(每点取段内峰值),镜像填充或单线;
    /// 颜色按幅度取主题渐变或固定槽色。
    fn draw_waveform(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        frame.fill_rectangle(plot.position(), plot.size(), self.plot_bg());
        if !self.app.has_samples {
            return;
        }
        let wf = &self.app.settings.waveform;
        let pal = &self.palette;
        let n = 512usize.min(plot.width.max(2.0) as usize);
        let step = FRAME_SIZE / n;
        let mid = plot.y + plot.height * 0.5;
        let amp = plot.height * 0.48;
        let col_w = plot.width / n as f32;
        let stroke_w = wf.line_width.max(0.5);
        let gain = wf.gain.clamp(1.0, 16.0);
        let custom = wf.custom_color.map(rgb);
        let color_of = |t: f32| match (custom, wf.color) {
            (Some(c), _) => c,
            (None, WaveformColor::Gradient) => band_color(pal, t),
            (None, WaveformColor::Top) => rgb(pal.bar_top),
            (None, WaveformColor::Bottom) => rgb(pal.bar_bottom),
        };
        let mut prev: Option<(f32, f32)> = None;
        for i in 0..n {
            let seg = &self.app.last_samples[i * step..(i + 1) * step];
            let peak = seg
                .iter()
                .fold(0.0f32, |m, &v| m.max(v.abs()))
                .mul_add(gain, 0.0)
                .clamp(0.0, 1.0);
            let x = plot.x + (i as f32 + 0.5) * col_w;
            let color = color_of(peak);
            if wf.mirrored {
                let half = (peak * amp).max(stroke_w * 0.5);
                frame.fill_rectangle(
                    Point::new(x - col_w * 0.5, mid - half),
                    Size::new(col_w + 0.5, half * 2.0),
                    color,
                );
            } else {
                let y = mid - peak * amp;
                if let Some((px, py)) = prev {
                    frame.stroke(
                        &canvas::Path::line(Point::new(px, py), Point::new(x, y)),
                        canvas::Stroke::default().with_width(stroke_w).with_color(color),
                    );
                }
                prev = Some((x, y));
            }
        }
    }

    /// 粒子:四发射式;拖尾沿速度反向;自定义色优先,否则按能量走主题渐变。
    fn draw_particles(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        frame.fill_rectangle(plot.position(), plot.size(), self.plot_bg());
        let pal = &self.palette;
        let ps = &self.app.settings.particles;
        let custom = ps.custom_color.map(rgb);
        let (w, h) = (plot.width, plot.height);
        let m = w.min(h);
        let cx = plot.x + w * 0.5;
        let cy = plot.y + h * 0.5;
        let now = self.app.now() as f32;
        let bass = self.app.bass;
        for p in &self.app.particles.items {
            let life_t = if p.life.is_finite() {
                (p.age / p.life).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let fade = (1.0 - life_t).max(0.15);
            let (px, py, tx, ty, t, radius) = match &p.kind {
                ParticleKind::Fountain { x, y, vx, vy } => (
                    plot.x + x * w,
                    plot.y + y * h,
                    -vx * w * ps.trail * 0.06,
                    -vy * h * ps.trail * 0.06,
                    (-vy).clamp(0.0, 1.0) * 0.9,
                    ps.size * (0.7 + 0.6 * fade),
                ),
                ParticleKind::Ring { angle, radius, .. } => {
                    let r = radius * m * (1.0 + 0.25 * bass)
                        + 0.03 * m * (now * 1.3 + p.seed * std::f32::consts::TAU).sin();
                    let (dx, dy) = (angle.cos(), angle.sin());
                    let trail = 18.0 * ps.trail.max(0.0) * (0.5 + bass);
                    (
                        cx + r * dx,
                        cy + r * dy,
                        -dx * trail,
                        -dy * trail,
                        0.35 + 0.65 * bass,
                        ps.size * (0.7 + 0.6 * fade),
                    )
                }
                ParticleKind::Rain { x, y, vy, vx } => (
                    plot.x + x * w,
                    plot.y + y * h,
                    -vx * w * ps.trail * 2.0,
                    -vy * h * ps.trail * 2.0,
                    (vy / 0.34).clamp(0.0, 1.0),
                    ps.size * 0.8,
                ),
                ParticleKind::Nebula { x, y, phase, .. } => {
                    let breathe = 0.6 + 0.9 * bass + 0.25 * (now * 1.1 + phase).sin();
                    (
                        plot.x + x * w,
                        plot.y + y * h,
                        0.0,
                        0.0,
                        (0.3 + 0.7 * bass).clamp(0.0, 1.0),
                        ps.size * breathe.max(0.3),
                    )
                }
            };
            let color = custom.unwrap_or_else(|| band_color(pal, t));
            let alpha = 0.25 + 0.75 * fade;
            if ps.trail > 0.01 {
                frame.stroke(
                    &canvas::Path::line(Point::new(px, py), Point::new(px + tx, py + ty)),
                    canvas::Stroke::default()
                        .with_width((ps.size * 0.6).max(1.0))
                        .with_color(Color { a: alpha * 0.5, ..color }),
                );
            }
            // 每粒子光晕:两圈外扩淡圆 + 实心核。
            for (k, ga) in [(2.6, 0.10), (1.6, 0.16)] {
                frame.fill(
                    &canvas::Path::circle(Point::new(px, py), (radius * k).max(1.0)),
                    Color { a: ga * alpha, ..color },
                );
            }
            frame.fill(
                &canvas::Path::circle(Point::new(px, py), radius.max(0.5)),
                Color { a: alpha, ..color },
            );
        }
    }

    /// 光效律动:分层半透明圆模拟辉光(iced canvas 无 shader);径向/网格
    /// /随机三布局(随机含慢速漂移),自定义色优先。
    fn draw_glow(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        frame.fill_rectangle(plot.position(), plot.size(), self.plot_bg());
        let g = &self.app.settings.glow;
        let pal = &self.palette;
        let n = g.count.max(1) as usize;
        let now = self.app.now() as f32;
        let bass = self.app.bass;
        let (w, h) = (plot.width, plot.height);
        let m = w.min(h);
        for i in 0..n {
            let fi = i as f32;
            let (cx, cy, scale, phase) = match g.layout {
                GlowLayout::Radial => {
                    let a = fi / n as f32 * std::f32::consts::TAU + now * 0.05;
                    let r = m * 0.30;
                    (
                        plot.x + w * 0.5 + r * a.cos(),
                        plot.y + h * 0.5 + r * a.sin(),
                        1.0,
                        fi * 1.7,
                    )
                }
                GlowLayout::Grid => {
                    let cols = (n as f32).sqrt().ceil().max(1.0);
                    let rows = (n as f32 / cols).ceil().max(1.0);
                    (
                        plot.x + (fi % cols + 0.5) * w / cols,
                        plot.y + ((fi / cols).floor() + 0.5) * h / rows,
                        1.0,
                        fi * 1.7,
                    )
                }
                GlowLayout::Random => {
                    let blob = &self.app.glow_blobs[i.min(self.app.glow_blobs.len().max(1) - 1)];
                    (plot.x + blob.x * w, plot.y + blob.y * h, blob.scale, blob.phase)
                }
            };
            let pulse = 0.5 + 0.5 * (now * 1.2 + phase).sin();
            let strength =
                (0.2 + 0.8 * bass * g.intensity + 0.2 * pulse * g.intensity).clamp(0.05, 1.0);
            let color = g.custom_color.map(rgb).unwrap_or_else(|| {
                let ct = if g.hue_drift {
                    (fi / n as f32 + now * 0.06).fract()
                } else {
                    fi / n as f32
                };
                band_color(pal, ct)
            });
            for layer in 0..4usize {
                let lr = m
                    * (0.10 + 0.16 * strength)
                    * (1.0 - layer as f32 * 0.22)
                    * scale
                    * (0.85 + 0.30 * pulse * g.intensity);
                let la = ((0.04 + 0.06 * strength) * (1.0 + layer as f32 * 0.9)).min(0.55);
                frame.fill(
                    &canvas::Path::circle(Point::new(cx, cy), lr),
                    Color { a: la, ..color },
                );
            }
        }
    }

    /// CD(网易云式):旋转盘体 + 中心封面 + 唱针;Bass Level 缩放呼吸
    /// (律动)。盘体几何与封面随 frame 旋转(draw_image 会烘培变换)。
    fn draw_disc(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        frame.fill_rectangle(plot.position(), plot.size(), self.plot_bg());
        let d = &self.app.settings.disc;
        let pal = &self.palette;
        let (w, h) = (plot.width, plot.height);
        let m = w.min(h);
        let cx = plot.x + w * 0.5;
        let cy = plot.y + h * 0.5;
        let scale = 1.0 + d.pulse.clamp(0.0, 1.0) * 0.22 * self.app.bass;
        let r_disc = m * 0.36 * scale;
        let r_cover = r_disc * 0.62;
        let angle = self.app.disc_angle;

        // 外圈(与封面反向旋转):盘体 + 密轨槽 + 分轨环 + 高光弧。
        frame.with_save(|frame| {
            frame.translate(Vector::new(cx, cy));
            frame.rotate(-angle);
            frame.translate(Vector::new(-cx, -cy));
            frame.fill(
                &canvas::Path::circle(Point::new(cx, cy), r_disc),
                Color::from_rgba(0.04, 0.05, 0.07, 0.93),
            );
            frame.stroke(
                &canvas::Path::circle(Point::new(cx, cy), r_disc),
                canvas::Stroke::default()
                    .with_width(2.0)
                    .with_color(Color { a: 0.9, ..rgb(pal.bar_bottom) }),
            );
            // 密轨槽细环。
            for k in 1..=5 {
                let t = k as f32 / 5.0;
                let rr = r_cover * 1.08 + (r_disc * 0.94 - r_cover * 1.08) * t;
                frame.stroke(
                    &canvas::Path::circle(Point::new(cx, cy), rr),
                    canvas::Stroke::default()
                        .with_width(1.0)
                        .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.03 + 0.02 * t)),
                );
            }
            // 分轨环(略粗,像碟面轨道分隔)。
            for rr in [r_cover * 1.22, r_disc * 0.86] {
                frame.stroke(
                    &canvas::Path::circle(Point::new(cx, cy), rr),
                    canvas::Stroke::default()
                        .with_width(3.0)
                        .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.09)),
                );
            }
            // 高光弧(随外圈转向)。
            for (start, sweep) in [(0.3f32, 1.0f32), (2.9, 0.6)] {
                let arc = canvas::Path::new(|p| {
                    p.arc(canvas::path::Arc {
                        center: Point::new(cx, cy),
                        radius: r_disc * 0.93,
                        start_angle: start.into(),
                        end_angle: (start + sweep).into(),
                    });
                });
                frame.stroke(
                    &arc,
                    canvas::Stroke::default()
                        .with_width(r_disc * 0.06)
                        .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.08)),
                );
            }
        });

        // 中心贴图(与外圈反向旋转)。
        frame.with_save(|frame| {
            frame.translate(Vector::new(cx, cy));
            frame.rotate(angle);
            frame.translate(Vector::new(-cx, -cy));
            match &self.app.cover_handle {
                Some(handle) => frame.draw_image(
                    Rectangle::new(
                        Point::new(cx - r_cover, cy - r_cover),
                        Size::new(r_cover * 2.0, r_cover * 2.0),
                    ),
                    canvas::Image::new(handle.clone()),
                ),
                None => frame.fill(
                    &canvas::Path::circle(Point::new(cx, cy), r_cover),
                    rgb(pal.bar_top),
                ),
            }
        });
        // 封面边缘细环(在图像外缘,压住蒙版羽化接缝)。
        frame.stroke(
            &canvas::Path::circle(Point::new(cx, cy), r_cover + 1.0),
            canvas::Stroke::default()
                .with_width(2.0)
                .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.12)),
        );
    }

    /// 屏保:六种无参数动画(Q13/Q17),仅风格选择。
    fn draw_screensaver(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        frame.fill_rectangle(plot.position(), plot.size(), self.plot_bg());
        match self.app.settings.screensaver.style {
            ScreensaverStyle::Lines => self.ss_lines(frame, plot),
            ScreensaverStyle::Ribbons => self.ss_ribbons(frame, plot),
            ScreensaverStyle::Aurora => self.ss_aurora(frame, plot),
            ScreensaverStyle::Pipes => self.ss_pipes(frame, plot),
            ScreensaverStyle::Bubbles => self.ss_bubbles(frame, plot),
            ScreensaverStyle::Starfield => self.ss_starfield(frame, plot),
        }
    }

    /// 变幻线:七条贝塞尔笔触,宽度沿线变化(笔触感),外扩辉光层。
    fn ss_lines(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let now = self.app.now() as f32;
        let (w, h) = (plot.width, plot.height);
        let cy = plot.y + h * 0.5;
        for i in 0..7usize {
            let fi = i as f32;
            let wave = |f1: f32, f2: f32, ph: f32, a: f32| {
                cy + a * h * (now * f1 + ph).sin() * (1.0 + 0.4 * (now * f2 + ph).sin())
            };
            // 三次贝塞尔采样点。
            let p0 = Point::new(plot.x + w * 0.02, wave(0.7, 0.23, fi * 1.7, 0.30));
            let p1 = Point::new(plot.x + w * 0.33, wave(0.5, 0.31, fi * 1.1 + 2.0, 0.46));
            let p2 = Point::new(plot.x + w * 0.66, wave(0.9, 0.17, fi * 2.3 + 4.0, 0.46));
            let p3 = Point::new(plot.x + w * 0.98, wave(0.6, 0.27, fi * 1.5 + 1.0, 0.30));
            let steps = 40;
            let mut pts = Vec::with_capacity(steps + 1);
            let mut widths = Vec::with_capacity(steps + 1);
            for k in 0..=steps {
                let t = k as f32 / steps as f32;
                let u = 1.0 - t;
                let bez = |a: f32, b: f32, c: f32, d: f32| {
                    u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d
                };
                pts.push(Point::new(
                    bez(p0.x, p1.x, p2.x, p3.x),
                    bez(p0.y, p1.y, p2.y, p3.y),
                ));
                // 笔触宽度:两端收笔,中段随正弦起伏。
                let taper = (t * (1.0 - t) * 4.0).clamp(0.12, 1.0);
                let wave_w = 0.5 + 0.5 * (t * 9.0 + fi * 2.0 + now * 1.4).sin();
                widths.push(1.5 + 9.0 * taper * (0.35 + 0.65 * wave_w));
            }
            let color = band_color(&self.palette, (fi * 0.14 + now * 0.05).fract());
            // 外扩辉光层。
            let glow_widths: Vec<f32> = widths.iter().map(|w| w * 2.6).collect();
            let glow = brush_polygon(&pts, &glow_widths);
            frame.fill(&glow, Color { a: 0.14, ..color });
            // 笔触主体。
            let body = brush_polygon(&pts, &widths);
            frame.fill(&body, Color { a: 0.88, ..color });
        }
    }

    /// 彩带:七条漂移正弦丝带铺满全屏,宽窄双描边。
    fn ss_ribbons(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let now = self.app.now() as f32;
        let (w, h) = (plot.width, plot.height);
        let steps = 48;
        for i in 0..7usize {
            let fi = i as f32;
            let amp = h * (0.14 + 0.06 * (now * 0.6 + fi).sin());
            let speed = 0.7 + 0.22 * fi;
            let cy = plot.y + h * (0.10 + 0.13 * fi);
            let path = canvas::Path::new(|p| {
                for k in 0..=steps {
                    let x = plot.x + w * k as f32 / steps as f32;
                    let y = cy
                        + amp * (x * 0.012 + now * speed + fi * 1.3).sin()
                        + h * 0.06 * (x * 0.004 - now * 0.4).sin();
                    if k == 0 {
                        p.move_to(Point::new(x, y));
                    } else {
                        p.line_to(Point::new(x, y));
                    }
                }
            });
            let color = band_color(&self.palette, (fi * 0.15 + now * 0.04).fract());
            frame.stroke(
                &path,
                canvas::Stroke::default()
                    .with_width(14.0)
                    .with_color(Color { a: 0.14, ..color }),
            );
            frame.stroke(
                &path,
                canvas::Stroke::default()
                    .with_width(4.0 + 2.0 * (now * 0.8 + fi).sin().abs())
                    .with_color(Color { a: 0.55, ..color }),
            );
        }
    }

    /// 极光:七道光幕铺满全屏,自底近顶;三层宽窄光柱叠成柔边。
    fn ss_aurora(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let now = self.app.now() as f32;
        let (w, h) = (plot.width, plot.height);
        let base_y = plot.y + h * 0.99;
        let steps = 48;
        for curtain in 0..7usize {
            let fc = curtain as f32;
            let center_x = plot.x + w * (0.5 + 0.48 * (now * 0.10 + fc * 1.8).sin());
            let half = w * 0.55;
            let floor = base_y - fc * 8.0;
            for &(width, alpha) in &[(32.0, 0.05), (17.0, 0.08), (8.0, 0.13)] {
                let color = band_color(&self.palette, (0.14 * fc + 0.05).fract());
                for k in 0..=steps {
                    let t = k as f32 / steps as f32;
                    let x = center_x - half + 2.0 * half * t;
                    let height = h
                        * (0.55 + 0.22 * (t * 6.0 + now * 0.9 + fc * 2.0).sin())
                        + h * 0.16 * (t * 13.0 - now * 0.6 + fc).sin();
                    frame.stroke(
                        &canvas::Path::line(Point::new(x, floor), Point::new(x, floor - height)),
                        canvas::Stroke::default()
                            .with_width(width)
                            .with_color(Color { a: alpha, ..color }),
                    );
                }
            }
        }
    }

    /// 管道:多个 3D 行走者,斜二测投影;行走进度内插当前管段。
    fn ss_pipes(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let (w, h) = (plot.width, plot.height);
        let m = w.min(h);
        let cx = plot.x + w * 0.5;
        let cy = plot.y + h * 0.5;
        let project = |c: [f32; 2]| Point::new(cx + c[0] * m, cy + c[1] * m);
        for walker in &self.app.screensaver.pipes {
            for seg in &walker.segs {
                let color = band_color(&self.palette, seg.t);
                frame.stroke(
                    &canvas::Path::line(project(seg.a), project(seg.b)),
                    canvas::Stroke::default()
                        .with_width(m * 0.02)
                        .with_color(Color { a: 0.85, ..color }),
                );
                frame.fill(&canvas::Path::circle(project(seg.a), m * 0.013), color);
            }
            if walker.progress > 0.0 {
                let head = lerp2(
                    pipe_project(walker.prev_cell),
                    pipe_project(walker.cell),
                    walker.progress,
                );
                let from = pipe_project(walker.prev_cell);
                let color = band_color(&self.palette, walker.color_t);
                frame.stroke(
                    &canvas::Path::line(project(from), project(head)),
                    canvas::Stroke::default()
                        .with_width(m * 0.02)
                        .with_color(Color { a: 0.85, ..color }),
                );
            }
        }
    }

    /// 气泡:底部升起、横向轻摆,顶部重生。
    fn ss_bubbles(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let now = self.app.now() as f32;
        let (w, h) = (plot.width, plot.height);
        for b in &self.app.screensaver.bubbles {
            let x = plot.x + (b.x + 0.02 * (now * 1.5 + b.seed * 10.0).sin()) * w;
            let y = plot.y + b.y * h;
            if y < plot.y - b.r {
                continue;
            }
            frame.fill(
                &canvas::Path::circle(Point::new(x, y), b.r),
                Color::from_rgba(1.0, 1.0, 1.0, 0.05),
            );
            frame.stroke(
                &canvas::Path::circle(Point::new(x, y), b.r),
                canvas::Stroke::default()
                    .with_width(1.5)
                    .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.40)),
            );
            frame.fill(
                &canvas::Path::circle(Point::new(x - b.r * 0.35, y - b.r * 0.35), b.r * 0.18),
                Color::from_rgba(1.0, 1.0, 1.0, 0.45),
            );
        }
    }

    /// 飞越星空:z 深度投影,近处拉出速度线。
    fn ss_starfield(&self, frame: &mut CanvasFrame, plot: Rectangle) {
        let (w, h) = (plot.width, plot.height);
        let cx = plot.x + w * 0.5;
        let cy = plot.y + h * 0.5;
        let focus = w.min(h) * 0.5;
        for s in &self.app.screensaver.stars {
            let k = 0.35 / s.z;
            let px = cx + s.x * focus * k;
            let py = cy + s.y * focus * k;
            if !plot.contains(Point::new(px, py)) {
                continue;
            }
            let kprev = 0.35 / (s.z + 0.06);
            let size = ((0.5 + (1.0 - s.z) * 3.2) * s.size).max(0.5);
            let alpha = (1.25 - s.z).clamp(0.15, 1.0);
            frame.stroke(
                &canvas::Path::line(
                    Point::new(cx + s.x * focus * kprev, cy + s.y * focus * kprev),
                    Point::new(px, py),
                ),
                canvas::Stroke::default()
                    .with_width(size)
                    .with_color(Color::from_rgba(1.0, 1.0, 1.0, alpha)),
            );
        }
    }
}

/// 中央画布的绘图区(留出坐标轴边距)。
/// 绘图区:铺满整个画布(全出血;频率刻度内嵌在绘图区底部内缘)。
fn plot_rect(full: Rectangle) -> Rectangle {
    full
}

fn freq_to_x(freq: f32, left: f32, width: f32) -> f32 {
    left + freq_to_t(freq) * width
}

fn db_to_y(db: f32, top: f32, height: f32) -> f32 {
    top + (1.0 - db_t(db)) * height
}

/// 电平归一化到 0..1(DB_FLOOR → 0,DB_CEILING → 1)。
fn db_t(db: f32) -> f32 {
    ((db - DB_FLOOR) / (DB_CEILING - DB_FLOOR)).clamp(0.0, 1.0)
}

/// 频谱显示电平:在 dB 归一化后施加 1/4 视觉增益。
fn spectrum_t(db: f32) -> f32 {
    (db_t(db) * SPECTRUM_GAIN).clamp(0.0, 1.0)
}

fn spectrum_to_y(db: f32, top: f32, height: f32) -> f32 {
    top + (1.0 - spectrum_t(db)) * height
}

/// 对数频率轴归一化到 0..1。
fn freq_to_t(freq: f32) -> f32 {
    ((freq / FREQ_MIN_HZ).ln() / LOG_SPAN).clamp(0.0, 1.0)
}

/// 圆形频谱的几何:圆心、内半径与可用的径向长度。
fn radial_geometry(plot: Rectangle) -> (f32, f32, f32, f32) {
    let cx = plot.x + plot.width * 0.5;
    let cy = plot.y + plot.height * 0.5;
    let r_max = (plot.width.min(plot.height) * 0.5 - 16.0).max(8.0);
    let r0 = (r_max * 0.30).max(5.0);
    (cx, cy, r0, (r_max - r0).max(1.0))
}

/// 频段位置(0..1) → 弧度;0 从正上方开始顺时针一周。
fn radial_angle(t: f32) -> f32 {
    -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * t.clamp(0.0, 1.0)
}

fn radial_arc(cx: f32, cy: f32, radius: f32, start: f32, end: f32) -> canvas::Path {
    canvas::Path::new(|p| {
        p.arc(canvas::path::Arc {
            center: Point::new(cx, cy),
            radius,
            start_angle: start.into(),
            end_angle: end.into(),
        });
    })
}

/// 方块高度/间隔按绘图尺寸相对参考高度等比缩放。
fn scaled_block(value: f32, dimension: f32) -> f32 {
    (value * dimension / BLOCK_SCALE_REF_HEIGHT).max(1.0)
}

/// 非方块显示时让峰值帽下落更慢;方块显示保持原有速度。
fn peak_hold_decay_scale(blocks: bool) -> f32 {
    if blocks { 1.0 } else { PEAK_HOLD_SLOW_SCALE }
}

/// 线性布局的峰值帽:按设置高度绘制圆角矩形,`gap` 为与柱顶的间隔。
fn draw_peak_cap(
    frame: &mut CanvasFrame,
    x: (f32, f32),
    peak: f32,
    plot: Rectangle,
    color: Color,
    rounding: f32,
    cap: (f32, f32),
) {
    let py = spectrum_to_y(peak, plot.y, plot.height);
    let (cap_height, gap) = cap;
    // 高度设为 0 时仍保留 1px 细线,而不是让标记消失。
    let cap = cap_height.max(1.0);
    // 顶端峰值没有上方空间:贴顶保留标记,而不是让它消失。
    let y1 = (py - gap.max(0.0)).max(plot.y + cap);
    let y0 = (y1 - cap).max(plot.y);
    let radius = rounding.round().clamp(0.0, 2.0);
    fill_rounded(
        frame,
        Rectangle::new(Point::new(x.0, y0), Size::new(x.1 - x.0, y1 - y0)),
        radius,
        color,
    );
}

/// 频谱柱渐变色(调色板驱动)。
fn band_color(pal: &Palette, t: f32) -> Color {
    let c = palette_band_color(pal, t);
    Color::from_rgb8(c[0], c[1], c[2])
}

fn rgb(c: [u8; 3]) -> Color {
    Color::from_rgb8(c[0], c[1], c[2])
}

fn grid_stroke(color: Color) -> canvas::Stroke<'static> {
    canvas::Stroke::default().with_width(1.0).with_color(color)
}

fn fill_rounded(frame: &mut CanvasFrame, rect: Rectangle, radius: f32, color: Color) {
    let radius = radius.clamp(0.0, rect.width.min(rect.height) * 0.5);
    let path = canvas::Path::rounded_rectangle(rect.position(), rect.size(), radius.into());
    frame.fill(&path, color);
}

/// 单行文本尺寸估计(CJK ≈ 1em,其余 ≈ 0.6em;浮层留白用,无需精确)。
fn estimate_text_size(content: &str, size: f32) -> (f32, f32) {
    let units: f32 = content
        .chars()
        .map(|c| if c.is_ascii() { 0.6 } else { 1.05 })
        .sum();
    (units * size, size * 1.4)
}

// ---------------------------------------------------------------------------
// 浮层 widget(应用菜单栏 / 设置窗)
// ---------------------------------------------------------------------------

fn chip_style() -> container::Style {
    container::Style {
        background: Some(CHIP_BG.into()),
        border: iced::border::rounded(4),
        ..Default::default()
    }
}

fn panel_style() -> container::Style {
    container::Style {
        background: Some(PANEL_BG.into()),
        border: iced::border::rounded(8).color(Color::from_rgba(1.0, 1.0, 1.0, 0.08)),
        ..Default::default()
    }
}

/// 全窗半透明遮罩:左键关闭上层浮层(右键留给画布)。
fn dim_overlay(message: Message) -> Element<'static, Message> {
    mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill)).style(|_theme: &Theme| {
            container::Style {
                background: Some(DIM_OVERLAY.into()),
                ..Default::default()
            }
        }),
    )
    .on_press(message)
    .into()
}

/// 应用菜单栏(非 macOS)的几何常量:栏高 / 菜单项宽 / 栏底色。
#[cfg(not(target_os = "macos"))]
const MENU_BAR_HEIGHT: f32 = 28.0;
#[cfg(not(target_os = "macos"))]
const MENU_TAB_WIDTH: f32 = 56.0;
#[cfg(not(target_os = "macos"))]
const MENU_BAR_BG: Color = Color::from_rgb(0.07, 0.08, 0.11);

/// 应用菜单栏(非 macOS):声源 / 设置 两个下拉菜单 + 特效直达按钮,
/// 末尾为「关于」下拉。全屏时整体隐藏(见 view),保持纯净画面。
#[cfg(not(target_os = "macos"))]
fn menu_bar(app: &SpectrumApp) -> Element<'_, Message> {
    let mut bar = row![].spacing(2);
    let source_open = app.open_menu == Some(MenuTab::Source);
    bar = bar.push(menu_tab("声源", source_open, Message::ToggleMenu(MenuTab::Source)));
    // 特效:直达按钮(高亮 = 特效窗体已打开;ADR-0005:原设置窗并入)。
    bar = bar.push(menu_tab("特效", app.effects_open, Message::OpenEffects));
    // 设置:原「视图」下拉(全屏/置顶/开机启动等),排在「关于」前。
    let settings_open = app.open_menu == Some(MenuTab::Settings);
    bar = bar.push(menu_tab(
        "设置",
        settings_open,
        Message::ToggleMenu(MenuTab::Settings),
    ));
    // 关于:末尾下拉(子项:关于 / 检测更新);有待装新版时右上角红点。
    let about_open = app.open_menu == Some(MenuTab::About);
    let about_tab = menu_tab("关于", about_open, Message::ToggleMenu(MenuTab::About));
    let about_element: Element<'static, Message> = if app.update_pending() {
        // 红点:内层 7px 圆点自身涂红;外层透明容器定位到右上角。
        let dot = container(Space::new().width(Length::Fixed(7.0)).height(Length::Fixed(7.0)))
            .style(|_theme: &Theme| container::Style {
                background: Some(Color::from_rgb(0.95, 0.30, 0.28).into()),
                border: iced::border::rounded(3.5),
                ..Default::default()
            });
        stack![
            about_tab,
            container(dot)
                .width(Length::Fixed(MENU_TAB_WIDTH))
                .height(Length::Fixed(MENU_BAR_HEIGHT - 6.0))
                .align_x(Horizontal::Right)
                .align_y(Vertical::Top)
                .padding(Padding { top: 1.0, right: 9.0, ..Padding::ZERO }),
        ]
        .into()
    } else {
        about_tab.into()
    };
    bar = bar.push(about_element);

    container(bar)
        .width(Length::Fill)
        .height(Length::Fixed(MENU_BAR_HEIGHT))
        .padding(Padding {
            top: 2.0,
            right: 6.0,
            bottom: 2.0,
            left: 6.0,
        })
        .style(|_theme: &Theme| container::Style {
            background: Some(MENU_BAR_BG.into()),
            ..Default::default()
        })
        .into()
}

/// 菜单栏顶层按钮(展开时高亮)。
#[cfg(not(target_os = "macos"))]
fn menu_tab(label: &str, open: bool, on_press: Message) -> button::Button<'static, Message> {
    button(
        text(label.to_string())
            .size(12.0)
            .color(if open { Color::WHITE } else { BTN_FG })
            .font(CJK_FONT),
    )
    .on_press(on_press)
    .width(Length::Fixed(MENU_TAB_WIDTH))
    .height(Length::Fixed(MENU_BAR_HEIGHT - 6.0))
    .style(move |_theme, _status| button::Style {
        background: Some(if open { SELECTED_BG } else { Color::TRANSPARENT }.into()),
        text_color: if open { Color::WHITE } else { BTN_FG },
        border: iced::border::rounded(4),
        ..Default::default()
    })
}

/// 菜单栏下拉面板(非 macOS):锚定在对应菜单项正下方。
#[cfg(not(target_os = "macos"))]
fn dropdown(app: &SpectrumApp, tab: MenuTab) -> Element<'_, Message> {
    let s = &app.settings;
    let items: Vec<Element<'_, Message>> = match tab {
        MenuTab::Settings => vec![
            toggle_item("全屏", app.fullscreen, Message::ToggleFullscreen),
            toggle_item("冻结显示", app.frozen, Message::ToggleFrozen),
            toggle_item("性能信息", s.show_hud, Message::ToggleHud),
            section_label("窗口").into(),
            toggle_item(
                "启动时全屏",
                s.default_fullscreen,
                Message::ToggleFullscreenDefault
            ),
            toggle_item("窗口置顶", s.always_on_top, Message::ToggleAlwaysOnTop),
            toggle_item(
                "记住窗口大小和位置",
                s.remember_window,
                Message::ToggleRememberWindow
            ),
            section_label("系统").into(),
            toggle_item("开机启动", s.launch_at_login, Message::ToggleLaunchAtLogin),
            toggle_item("阻止休眠", s.prevent_sleep, Message::TogglePreventSleep),
        ],
        MenuTab::About => {
            let mut items: Vec<Element<'_, Message>> = vec![
                choice("关于", app.about_open, Message::OpenAbout).into(),
                choice("检测更新", app.update_open, Message::OpenUpdate).into(),
            ];
            if app.update_pending() {
                items.push(hint_text("有可用新版本"));
            }
            items
        }
        MenuTab::Source => {
            let mut items: Vec<Element<'_, Message>> = Vec::new();
            let note = app.system.loopback_note();
            if app.loopback_devices.is_empty() && app.input_devices.is_empty() {
                items.push(text(note).size(12.0).color(SECTION_FG).into());
            } else {
                for (devices, label) in [
                    (&app.loopback_devices, "Loopback"),
                    (&app.input_devices, "Input"),
                ] {
                    if devices.is_empty() {
                        continue;
                    }
                    items.push(section_label(label).into());
                    for info in devices {
                        let selected = app.source.as_ref().is_some_and(|s| s.id == info.id);
                        items.push(source_item(info, selected, 13.0));
                    }
                }
            }
            items
        }
    };

    let panel = container(column(items).spacing(4).width(Length::Fixed(200.0)))
        .padding(10.0)
        .style(|_theme: &Theme| panel_style());

    // 锚点:菜单栏正下方,左缘对齐对应菜单项(声源/设置/关于顺次排布,
    // 「特效」为直达按钮无下拉,但其占位宽度计入「设置」偏移)。
    let left = match tab {
        MenuTab::Source => 6.0,
        MenuTab::Settings => 6.0 + 2.0 * MENU_TAB_WIDTH,
        MenuTab::About => 6.0 + 3.0 * MENU_TAB_WIDTH,
    };
    container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Left)
        .align_y(Vertical::Top)
        .padding(Padding {
            top: MENU_BAR_HEIGHT,
            left,
            ..Padding::ZERO
        })
        .into()
}

/// 「频段数」滑杆行(特效窗体频谱 tab;1—255 连续可调,步进 1,右侧实时读数)。
fn band_count_row(s: &Settings) -> Element<'static, Message> {
    row![
        slider(
            BAND_COUNT_MIN as f32..=BAND_COUNT_MAX as f32,
            s.band_count as f32,
            |v| Message::SetBandCount(v.round() as usize),
        )
        .step(1.0_f32)
        .width(Length::Fill),
        text(s.band_count.to_string())
            .size(11.0)
            .color(SECTION_FG)
            .font(Font::MONOSPACE)
            .width(Length::Fixed(26.0)),
    ]
    .spacing(6)
    .align_y(Vertical::Center)
    .into()
}

/// 「衰减速度」档位行(特效窗体频谱 tab;五档并排,名称见 [`DecayChoice::name`])。
fn decay_row(s: &Settings) -> Element<'static, Message> {
    row(DecayChoice::ALL
        .iter()
        .map(|&d| mini_choice(d.name(), s.decay == d, Message::SetDecay(d)).into())
        .collect::<Vec<_>>())
    .spacing(3)
    .into()
}

/// 紧凑单选按钮(档位行用;比 [`choice`] 窄,容纳 5—6 档并排)。
fn mini_choice<'a>(label: &str, selected: bool, on_press: Message) -> button::Button<'a, Message> {
    button(
        text(label.to_string())
            .size(11.0)
            .color(if selected { Color::WHITE } else { BTN_FG }),
    )
    .on_press(on_press)
    .padding([3, 5])
    .style(move |_theme, _status| button::Style {
        background: Some(if selected { SELECTED_BG } else { BTN_BG }.into()),
        text_color: if selected { Color::WHITE } else { BTN_FG },
        border: iced::border::rounded(3),
        ..Default::default()
    })
}

fn section_label(label: &str) -> text::Text<'static> {
    text(label.to_string())
        .size(11.0)
        .color(SECTION_FG)
        .font(CJK_FONT)
}

/// 单选项按钮(选中高亮;等价 egui 的 selectable_value)。
fn choice<'a>(label: &str, selected: bool, on_press: Message) -> button::Button<'a, Message> {
    button(
        text(label.to_string())
            .size(13.0)
            .color(if selected { Color::WHITE } else { BTN_FG }),
    )
    .on_press(on_press)
    .padding([4.0, 10.0])
    .style(move |_theme, _status| button::Style {
        background: Some(if selected { SELECTED_BG } else { BTN_BG }.into()),
        text_color: if selected { Color::WHITE } else { BTN_FG },
        border: iced::border::rounded(4),
        ..Default::default()
    })
}

/// 开关项按钮(✓ 前缀标记当前态)。
fn toggle_item(label: &str, on: bool, on_press: Message) -> Element<'static, Message> {
    let label = if on {
        format!("✓ {label}")
    } else {
        format!("　 {label}")
    };
    button(text(label).size(13.0).color(BTN_FG).font(CJK_FONT))
        .on_press(on_press)
        .padding([4.0, 8.0])
        .width(Length::Fill)
        .style(|_theme, _status| flat_button())
        .into()
}

/// 声源条目(选中高亮；设备名前缀图标)；消息携带克隆，无借用。
fn source_item(info: &DeviceInfo, selected: bool, size: f32) -> Element<'static, Message> {
    let icon = match info.kind {
        SourceKind::Loopback => "🔊",
        SourceKind::Input => "🎤",
    };
    button(
        text(format!("{icon} {}", info.name))
            .size(size)
            .color(if selected { Color::WHITE } else { BTN_FG })
            .font(CJK_FONT),
    )
    .on_press(Message::SelectSource(info.clone()))
    .padding([4.0, 8.0])
    .width(Length::Fill)
    .style(move |_theme, _status| button::Style {
        background: Some(if selected { SELECTED_BG } else { BTN_BG }.into()),
        text_color: if selected { Color::WHITE } else { BTN_FG },
        border: iced::border::rounded(4),
        ..Default::default()
    })
    .into()
}

fn flat_button() -> button::Style {
    button::Style {
        background: Some(BTN_BG.into()),
        text_color: BTN_FG,
        border: iced::border::rounded(4),
        ..Default::default()
    }
}

/// 「关于与更新」窗口内容(菜单栏末尾「关于」/ macOS 原生菜单打开
/// [`about_overlay`]):程序说明 + 更新检测/下载进度。
/// 解码内嵌程序图标为图片句柄(「关于」窗口展示;失败返回 None)。
fn decode_icon() -> Option<ImageHandle> {
    let icon = image::load_from_memory(APP_ICON_PNG).ok()?;
    let rgba = icon.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(ImageHandle::from_rgba(w, h, rgba.into_raw()))
}

/// 「关于」窗口(菜单栏「关于」下拉子项 / macOS 原生菜单打开):图标 +
/// 名称-版本号纵向水平居中,下方开源地址与程序说明。
fn about_overlay(app: &SpectrumApp) -> Element<'_, Message> {
    let icon: Element<'static, Message> = match &app.about_icon {
        Some(handle) => iced::widget::image(handle.clone())
            .width(Length::Fixed(64.0))
            .height(Length::Fixed(64.0))
            .into(),
        None => Space::new().height(Length::Fixed(8.0)).into(),
    };
    let col = column![
        icon,
        row![
            text("audio-spectrum").size(16.0).color(Color::WHITE),
            text(format!("v{}", updater::CURRENT_VERSION))
                .size(12.0)
                .color(SECTION_FG)
                .font(Font::MONOSPACE),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
        iced::widget::rule::horizontal(1.0),
        text(format!("开源主页:github.com/{}", updater::UPDATE_REPO))
            .size(11.0)
            .color(SECTION_FG)
            .font(Font::MONOSPACE),
        text(
            "跨平台实时音频频谱显示:系统回环(Loopback)或麦克风采集,"
                .to_string()
                + "频谱柱 / 语谱图双视图,支持 Peak Hold、主题配色与背景图自定义。",
        )
        .size(12.0)
        .color(BTN_FG),
    ]
    .spacing(10)
    .align_x(Horizontal::Center)
    .width(Length::Fixed(320.0));
    window_card(col)
}

/// 「检测更新」窗口:当前版本 → 最新版本、更新按钮、下载进度(完成后
/// 自动换装重启);启动已自动检测,亦可手动重新检查。
fn update_overlay(app: &SpectrumApp) -> Element<'_, Message> {
    let check_button = |label: &str| {
        button(text(label.to_string()).size(12.0).color(BTN_FG).font(CJK_FONT))
            .on_press(Message::CheckUpdates)
            .padding([3.0, 10.0])
            .style(|_theme, _status| flat_button())
    };
    let primary_button = |label: &str, on_press: Message| {
        button(text(label.to_string()).size(13.0).color(Color::WHITE).font(CJK_FONT))
            .on_press(on_press)
            .padding([4.0, 18.0])
            .style(|_theme, _status| button::Style {
                background: Some(SELECTED_BG.into()),
                text_color: Color::WHITE,
                border: iced::border::rounded(4),
                ..Default::default()
            })
    };
    let mut col = column![section_label("软件更新")].spacing(10).width(Length::Fixed(300.0));
    match &app.update_phase {
        UpdatePhase::Idle => {
            col = col.push(hint_text("尚未检查更新"));
            col = col.push(check_button("检查更新"));
        }
        UpdatePhase::Checking => {
            col = col.push(hint_text("正在检查更新…"));
        }
        UpdatePhase::UpToDate(label) => {
            col = col.push(
                text("✓ 已是最新")
                    .size(13.0)
                    .color(BTN_FG)
                    .font(CJK_FONT),
            );
            col = col.push(hint_text(format!("线上版本 {label}")));
            col = col.push(check_button("重新检查"));
        }
        UpdatePhase::Available(info) => {
            // 当前版本号 → 最新版本号,水平居中。
            col = col.push(
                row![
                    text(format!("v{}", updater::CURRENT_VERSION))
                        .size(14.0)
                        .color(BTN_FG)
                        .font(Font::MONOSPACE),
                    text("→").size(14.0).color(SECTION_FG),
                    text(info.label.clone())
                        .size(14.0)
                        .color(Color::WHITE)
                        .font(Font::MONOSPACE),
                ]
                .spacing(10)
                .align_y(Vertical::Center),
            );
            col = col.push(hint_text(format!(
                "发现新版本,包大小约 {}",
                fmt_bytes(info.asset_size)
            )));
            col = col.push(primary_button("更新", Message::DownloadUpdate));
        }
        UpdatePhase::Downloading { received, total } => {
            let (received, total) = (*received, *total);
            let pct = if total > 0 {
                format!("({:.0}%)", received as f32 / total as f32 * 100.0)
            } else {
                String::new()
            };
            col = col.push(
                iced::widget::progress_bar(
                    0.0..=total.max(1) as f32,
                    received.min(total.max(1)) as f32,
                )
                .girth(Length::Fixed(6.0)),
            );
            col = col.push(hint_text(format!(
                "{:.1} / {:.1} MB {} · 完成后自动重启",
                received as f32 / 1_048_576.0,
                total as f32 / 1_048_576.0,
                pct
            )));
        }
        UpdatePhase::Failed(e) => {
            col = col.push(
                text(format!("更新失败:{e}"))
                    .size(12.0)
                    .color(Color::from_rgb(0.95, 0.55, 0.45)),
            );
            col = col.push(check_button("重试"));
        }
    }
    window_card(col)
}

/// 居中弹窗卡片(「关于」/「检测更新」共用;样式与设置窗一致)。
fn window_card(content: impl Into<Element<'static, Message>>) -> Element<'static, Message> {
    container(
        container(content)
            .padding(16.0)
            .style(|_theme: &Theme| panel_style()),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Horizontal::Center)
    .align_y(Vertical::Center)
    .into()
}

fn hint_text(s: impl Into<String>) -> Element<'static, Message> {
    text(s.into()).size(11.0).color(SECTION_FG).into()
}

/// 字节数人读格式(B / KB / MB,一位小数)。
fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f32 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f32 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 特效窗体(ADR-0005):tab 化的全部视图配置 + 通用页;选 tab 即切视图
/// (Q9),与 S 键/菜单同源联动。原设置窗并入(「通用」页收背景图)。
fn effects_panel(app: &SpectrumApp) -> Element<'_, Message> {
    let s = &app.settings;
    let current_tab = if app.effects_general {
        EffectsTab::General
    } else {
        EffectsTab::View(s.view)
    };
    let mut tabs = row![].spacing(2);
    for &v in ViewMode::ALL {
        let tab = EffectsTab::View(v);
        tabs = tabs.push(tab_button(v.name(), tab == current_tab, Message::SelectTab(tab)));
    }
    tabs = tabs.push(tab_button(
        "通用",
        current_tab == EffectsTab::General,
        Message::SelectTab(EffectsTab::General),
    ));

    let content: Element<'static, Message> = match current_tab {
        EffectsTab::General => general_page(app),
        EffectsTab::View(ViewMode::Spectrum) => spectrum_page(app),
        EffectsTab::View(ViewMode::Spectrogram) => column![hint_text(
            "语谱图暂无可调项:12 秒滚动历史 · 20ms 列聚合(复用频谱的 Band 对数轴)"
        )]
        .spacing(6)
        .into(),
        EffectsTab::View(ViewMode::Waveform) => waveform_page(app),
        EffectsTab::View(ViewMode::Particles) => particles_page(app),
        EffectsTab::View(ViewMode::Glow) => glow_page(app),
        EffectsTab::View(ViewMode::Disc) => disc_page(s),
        EffectsTab::View(ViewMode::Static) => column![hint_text(
            "静态画面:仅显示通用背景图或背景色;均在「通用」页配置(模糊也在那)"
        )]
        .spacing(6)
        .into(),
        EffectsTab::View(ViewMode::Screensaver) => screensaver_page(s),
    };

    let height = (app.window_size.height - 48.0).clamp(240.0, 560.0);
    let panel = container(
        column![
            tabs,
            iced::widget::rule::horizontal(1.0),
            scrollable(container(content).width(Length::Fixed(404.0)))
                .width(Length::Fixed(420.0))
                .height(Length::Fill),
        ]
        .spacing(8),
    )
    .padding(12.0)
    .height(Length::Fixed(height))
    .style(|_theme: &Theme| panel_style());

    container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center)
        .into()
}

/// 特效窗体的 tab 按钮(选中高亮)。
fn tab_button(label: &str, selected: bool, on_press: Message) -> button::Button<'static, Message> {
    button(
        text(label.to_string())
            .size(12.0)
            .color(if selected { Color::WHITE } else { BTN_FG })
            .font(CJK_FONT),
    )
    .on_press(on_press)
    .padding([4.0, 7.0])
    .style(move |_theme, _status| button::Style {
        background: Some(if selected { SELECTED_BG } else { BTN_BG }.into()),
        text_color: if selected { Color::WHITE } else { BTN_FG },
        border: iced::border::rounded(4),
        ..Default::default()
    })
}

/// 「通用」页:背景图与背景色(静态视图即本页配置的背景独占画面)。
fn general_page(app: &SpectrumApp) -> Element<'static, Message> {
    let s = &app.settings;
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("通用")].spacing(6);
    col = col.push(section_label("背景图"));
    let mut bg_row = row![
        button(text("选择图片…").size(13.0).color(BTN_FG).font(CJK_FONT))
            .on_press(Message::PickBackground)
            .padding([4.0, 8.0])
            .style(|_theme, _status| flat_button()),
    ]
    .spacing(6);
    if s.background.path.is_some() {
        bg_row = bg_row.push(
            button(text("清除背景").size(13.0).color(BTN_FG).font(CJK_FONT))
                .on_press(Message::ClearBackground)
                .padding([4.0, 8.0])
                .style(|_theme, _status| flat_button()),
        );
    }
    col = col.push(bg_row);
    col = col.push(
        row![
            text("缩放")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            choice(
                "填满",
                s.background.mode == ScaleMode::Fill,
                Message::SetScaleMode(ScaleMode::Fill)
            ),
            choice(
                "适应",
                s.background.mode == ScaleMode::Fit,
                Message::SetScaleMode(ScaleMode::Fit)
            ),
            choice(
                "平铺",
                s.background.mode == ScaleMode::Tile,
                Message::SetScaleMode(ScaleMode::Tile)
            ),
        ]
        .spacing(4),
    );
    col = col.push(slider_row(
        "不透明度",
        s.background.opacity,
        0.05..=1.0,
        0.05,
        Message::SetOpacity,
    ));
    col = col.push(slider_row(
        "模糊",
        s.background.blur_px as f32,
        0.0..=24.0,
        1.0,
        Message::SetBlur,
    ));
    col = col.push(section_label("背景色"));
    col = col.push(hint_text("未设图片时的底色;设了图片则图片优先"));
    col = col.push(color_picker_row(
        app,
        "背景色",
        PickerId::Background,
        s.background.color.unwrap_or(resolve_palette(&s.style).bg),
        Message::EffectColorReset(EffectColorSlot::Background),
    ));
    col.into()
}

/// 「频谱」页:原设置窗的显示项 + 原菜单的频段数/衰减速度(归拢,ADR-0005)。
fn spectrum_page(app: &SpectrumApp) -> Element<'static, Message> {
    let s = &app.settings;
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("频谱")].spacing(6);
    col = col.push(
        checkbox(s.style.peak_detached)
            .label("峰值保持点")
            .on_toggle(|_| Message::TogglePeakDetached),
    );
    col = col.push(slider_row(
        "保持点高度",
        s.style.peak_cap_height,
        0.0..=10.0,
        1.0,
        Message::SetPeakCapHeight,
    ));
    col = col.push(slider_row(
        "保持点间隔",
        s.style.peak_gap,
        1.0..=10.0,
        1.0,
        Message::SetPeakGap,
    ));
    col = col.push(
        checkbox(s.style.grid)
            .label("显示网格")
            .on_toggle(|_| Message::ToggleGrid),
    );
    col = col.push(section_label("类型"));
    col = col.push(
        row![
            text("布局")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            spectrum_type_row(s.style.spectrum_type),
        ]
        .spacing(4),
    );
    col = col.push(
        checkbox(s.style.blocks)
            .label("方块显示（条形/圆形均生效）")
            .on_toggle(|_| Message::ToggleBlocks),
    );
    col = col.push(slider_row(
        "方块高度",
        s.style.block_height,
        1.0..=50.0,
        1.0,
        Message::SetBlockHeight,
    ));
    col = col.push(slider_row(
        "方块间隔",
        s.style.block_gap,
        1.0..=50.0,
        1.0,
        Message::SetBlockGap,
    ));
    col = col.push(section_label("样式"));
    col = col.push(
        row![
            text("主题")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            for_row_presets(s.style.theme),
        ]
        .spacing(4),
    );
    let pal = resolve_palette(&s.style);
    col = col.push(color_picker_row(
        app,
        "渐变底色",
        PickerId::Spectrum(ColorSlot::BarBottom),
        pal.bar_bottom,
        Message::ColorSlotReset(ColorSlot::BarBottom),
    ));
    col = col.push(color_picker_row(
        app,
        "渐变顶色",
        PickerId::Spectrum(ColorSlot::BarTop),
        pal.bar_top,
        Message::ColorSlotReset(ColorSlot::BarTop),
    ));
    col = col.push(color_picker_row(
        app,
        "背景色",
        PickerId::Spectrum(ColorSlot::Bg),
        pal.bg,
        Message::ColorSlotReset(ColorSlot::Bg),
    ));
    col = col.push(color_picker_row(
        app,
        "峰值色",
        PickerId::Spectrum(ColorSlot::Peak),
        pal.peak,
        Message::ColorSlotReset(ColorSlot::Peak),
    ));
    col = col.push(color_picker_row(
        app,
        "峰值保持点色",
        PickerId::Spectrum(ColorSlot::PeakHold),
        pal.peak_hold,
        Message::ColorSlotReset(ColorSlot::PeakHold),
    ));
    col = col.push(slider_row(
        "柱条圆角",
        s.style.corner_radius,
        0.0..=8.0,
        0.5,
        Message::SetCornerRadius,
    ));
    col = col.push(slider_row(
        "柱条间隙",
        s.style.bar_gap,
        0.0..=0.45,
        0.05,
        Message::SetBarGap,
    ));
    col = col.push(section_label("频段数"));
    col = col.push(band_count_row(s));
    col = col.push(section_label("衰减速度"));
    col = col.push(decay_row(s));
    col.into()
}

/// 「波形」页。
fn waveform_page(app: &SpectrumApp) -> Element<'static, Message> {
    let w = &app.settings.waveform;
    let pal = resolve_palette(&app.settings.style);
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("波形")].spacing(6);
    col = col.push(
        row![
            text("风格")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            choice("单线", !w.mirrored, Message::SetWaveformMirrored(false)),
            choice("镜像填充", w.mirrored, Message::SetWaveformMirrored(true)),
        ]
        .spacing(4),
    );
    col = col.push(slider_row(
        "线宽",
        w.line_width,
        0.5..=8.0,
        0.5,
        Message::SetWaveformWidth,
    ));
    col = col.push(slider_row(
        "增益",
        w.gain,
        1.0..=16.0,
        0.5,
        Message::SetWaveformGain,
    ));
    let mut colors = row![].spacing(4);
    for &c in WaveformColor::ALL {
        colors = colors.push(choice(c.name(), w.color == c, Message::SetWaveformColor(c)));
    }
    col = col.push(
        row![
            text("颜色")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            colors,
        ]
        .spacing(4),
    );
    col = col.push(color_picker_row(
        app,
        "自定义色",
        PickerId::Waveform,
        w.custom_color.unwrap_or(pal.bar_top),
        Message::EffectColorReset(EffectColorSlot::Waveform),
    ));
    col.into()
}

/// 「粒子」页。
fn particles_page(app: &SpectrumApp) -> Element<'static, Message> {
    let p = &app.settings.particles;
    let pal = resolve_palette(&app.settings.style);
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("粒子")].spacing(6);
    col = col.push(slider_row(
        "数量",
        p.count as f32,
        20.0..=400.0,
        10.0,
        |v| Message::SetParticleCount(v.round() as u32),
    ));
    col = col.push(slider_row(
        "大小",
        p.size,
        1.0..=100.0,
        1.0,
        Message::SetParticleSize,
    ));
    col = col.push(slider_row(
        "拖尾",
        p.trail,
        0.0..=3.0,
        0.1,
        Message::SetParticleTrail,
    ));
    let mut emissions = row![].spacing(4);
    for &e in ParticleEmission::ALL {
        emissions = emissions.push(choice(
            e.name(),
            p.emission == e,
            Message::SetParticleEmission(e),
        ));
    }
    col = col.push(
        row![
            text("发射")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            emissions,
        ]
        .spacing(4),
    );
    col = col.push(color_picker_row(
        app,
        "自定义色",
        PickerId::Particles,
        p.custom_color.unwrap_or(pal.bar_top),
        Message::EffectColorReset(EffectColorSlot::Particles),
    ));
    col.into()
}

/// 「光效」页。
fn glow_page(app: &SpectrumApp) -> Element<'static, Message> {
    let g = &app.settings.glow;
    let pal = resolve_palette(&app.settings.style);
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("光效律动")].spacing(6);
    col = col.push(slider_row(
        "光斑数量",
        g.count as f32,
        1.0..=64.0,
        1.0,
        |v| Message::SetGlowCount(v.round() as u32),
    ));
    let mut layouts = row![].spacing(4);
    for &l in GlowLayout::ALL {
        layouts = layouts.push(choice(l.name(), g.layout == l, Message::SetGlowLayout(l)));
    }
    col = col.push(
        row![
            text("布局")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            layouts,
        ]
        .spacing(4),
    );
    col = col.push(slider_row(
        "脉动强度",
        g.intensity,
        0.0..=1.0,
        0.05,
        Message::SetGlowIntensity,
    ));
    col = col.push(
        checkbox(g.hue_drift)
            .label("色相漂移")
            .on_toggle(Message::SetGlowHueDrift),
    );
    col = col.push(color_picker_row(
        app,
        "自定义色",
        PickerId::Glow,
        g.custom_color.unwrap_or(pal.bar_top),
        Message::EffectColorReset(EffectColorSlot::Glow),
    ));
    col.into()
}

/// 颜色选择器行:色块按钮(点击弹出 iced_aw ColorPicker)+ ↺ 重置。
fn color_picker_row(
    app: &SpectrumApp,
    label: &str,
    id: PickerId,
    current: [u8; 3],
    on_reset: Message,
) -> Element<'static, Message> {
    let open = app.picker_open == Some(id);
    let shown = if open { app.picker_working } else { current };
    let underlay = row![
        text(label.to_string())
            .size(13.0)
            .color(BTN_FG)
            .font(CJK_FONT)
            .width(Length::Fixed(64.0)),
        button(
            container(
                Space::new()
                    .width(Length::Fixed(30.0))
                    .height(Length::Fixed(20.0)),
            )
            .style(move |_theme: &Theme| container::Style {
                background: Some(rgb(current).into()),
                border: iced::border::rounded(4)
                    .width(1)
                    .color(Color::from_rgba(1.0, 1.0, 1.0, 0.25)),
                ..Default::default()
            }),
        )
        .on_press(Message::ToggleColorPicker(id))
        .padding(0.0)
        .style(|_theme, _status| button::Style {
            background: None,
            border: iced::border::rounded(4),
            ..Default::default()
        }),
        button(text("↺").size(12.0).color(BTN_FG))
            .on_press(on_reset)
            .padding([2.0, 6.0])
            .style(|_theme, _status| flat_button()),
    ]
    .spacing(6)
    .align_y(Vertical::Center);
    ColorPicker::new(
        open,
        iced::Color::from_rgb8(shown[0], shown[1], shown[2]),
        underlay,
        Message::CancelColorPicker,
        Message::SubmitColorPicker,
    )
    .into()
}

/// 「CD」页。
fn disc_page(s: &Settings) -> Element<'static, Message> {
    let d = &s.disc;
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("CD 唱片")].spacing(6);
    col = col.push(slider_row(
        "转速",
        d.speed,
        0.0..=3.0,
        0.1,
        Message::SetDiscSpeed,
    ));
    col = col.push(slider_row(
        "律动幅度",
        d.pulse,
        0.0..=1.0,
        0.05,
        Message::SetDiscPulse,
    ));
    col = col.push(
        checkbox(d.tonearm)
            .label("显示唱针")
            .on_toggle(|_| Message::ToggleDiscTonearm),
    );
    col = col.push(
        row![
            text("封面形状")
                .size(13.0)
                .color(BTN_FG)
                .font(CJK_FONT)
                .width(Length::Fixed(64.0)),
            choice("圆形", d.round, Message::SetDiscRound(true)),
            choice("方形", !d.round, Message::SetDiscRound(false)),
        ]
        .spacing(4),
    );
    col = col.push(section_label("封面"));
    col = col.push(
        row![
            button(text("选择图片…").size(13.0).color(BTN_FG).font(CJK_FONT))
                .on_press(Message::PickCover)
                .padding([4.0, 8.0])
                .style(|_theme, _status| flat_button()),
            button(text("恢复默认").size(13.0).color(BTN_FG).font(CJK_FONT))
                .on_press(Message::ClearCover)
                .padding([4.0, 8.0])
                .style(|_theme, _status| flat_button()),
        ]
        .spacing(6),
    );
    let cover_label = d
        .cover
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "内置默认唱片图".to_string());
    col = col.push(hint_text(format!("当前:{cover_label}")));
    col.into()
}

/// 「屏保」页:六风格单选,无参数(Q13/Q17)。
fn screensaver_page(s: &Settings) -> Element<'static, Message> {
    let cur = s.screensaver.style;
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("屏保")].spacing(6);
    col = col.push(hint_text("不监听音源,纯氛围动画;六选一"));
    for group in ScreensaverStyle::ALL.chunks(3) {
        let mut r = row![].spacing(4);
        for &st in group {
            r = r.push(mini_choice(
                st.name(),
                cur == st,
                Message::SetScreensaverStyle(st),
            ));
        }
        col = col.push(r);
    }
    col.into()
}

fn spectrum_type_row(current: SpectrumType) -> row::Row<'static, Message> {
    row(SpectrumType::ALL
        .iter()
        .map(|&t| {
            choice(
                t.name(),
                t == current,
                Message::SetSpectrumType(t),
            )
            .into()
        })
        .collect::<Vec<_>>())
    .spacing(4)
}

fn for_row_presets(current: ThemePreset) -> row::Row<'static, Message> {
    row(ThemePreset::ALL
        .iter()
        .map(|&p| choice(p.name(), p == current, Message::SetTheme(p)).into())
        .collect::<Vec<_>>())
    .spacing(4)
}

fn slider_row(
    label: &str,
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    message: fn(f32) -> Message,
) -> Element<'static, Message> {
    row![
        text(label.to_string())
            .size(13.0)
            .color(BTN_FG)
            .font(CJK_FONT)
            .width(Length::Fixed(64.0)),
        slider(range, value, message).step(step).width(Length::Fill),
        text(format!("{value:.2}"))
            .size(12.0)
            .color(SECTION_FG)
            .font(Font::MONOSPACE)
            .width(Length::Fixed(36.0)),
    ]
    .spacing(6)
    .align_y(Vertical::Center)
    .into()
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 备用 CJK 字体字节(注册进 cosmic-text 字体库,保证字形回退可得;
/// 主家族见 [`CJK_FONT`])。
fn cjk_font_bytes() -> Option<Vec<u8>> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/Library/Fonts/Arial Unicode.ttf",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/simhei.ttf",
            "C:/Windows/Fonts/simsun.ttc",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ]
    };
    candidates.iter().find_map(|p| std::fs::read(p).ok())
}

fn main() -> iced::Result {
    pin_windows_wgpu_backend();
    let settings = load_settings();
    // 开机自启动为开启态时启动即重写一次系统项:exe 被移动/换装后路径自愈。
    if settings.launch_at_login {
        autostart::set_enabled(true);
    }
    // 阻止休眠为开启态时启动即恢复抑制(开关随设置持久化)。
    if settings.prevent_sleep {
        keep_awake::set_enabled(true);
    }
    // 清理上次自更新换装的残留(旧版 .old 与临时目录)。
    updater::cleanup_stale();

    let (size, position) = match settings.window_rect {
        Some([x, y, w, h]) => (
            Size::new(w.max(320.0), h.max(240.0)),
            window::Position::Specific(Point::new(x, y)),
        ),
        None => (Size::new(920.0, 440.0), window::Position::default()),
    };
    let mut window = window::Settings {
        size,
        position,
        min_size: Some(Size::new(320.0, 240.0)),
        level: if settings.always_on_top {
            window::Level::AlwaysOnTop
        } else {
            window::Level::Normal
        },
        // 启动全屏不在创建时应用(首帧后经 Frame 消息 set_mode;
        // 部分虚拟显示环境下「启动即全屏」会毒化 wgpu 初始化)。
        fullscreen: false,
        ..Default::default()
    };
    if let Ok(icon) = image::load_from_memory(APP_ICON_PNG) {
        let rgba = icon.to_rgba8();
        let (w, h) = rgba.dimensions();
        if let Ok(icon) = window::icon::from_rgba(rgba.into_raw(), w, h) {
            window.icon = Some(icon);
        }
    }

    let boot = move || SpectrumApp::new(settings.clone());

    let mut app = iced::application(boot, update, view)
        .title("audio-spectrum")
        .window(window)
        .theme(|_state: &SpectrumApp| Theme::Dark)
        .style(|_state: &SpectrumApp, _theme: &Theme| iced::theme::Style {
            background_color: Color::BLACK,
            text_color: Color::WHITE,
        })
        .subscription(subscription)
        .default_font(CJK_FONT);

    if let Some(bytes) = cjk_font_bytes() {
        app = app.font(bytes);
    }

    let result = app.run();
    // 正常退出时解除休眠抑制(Linux 需 kill 掉 systemd-inhibit 子进程;
    // Windows/macOS 的抑制本随进程消亡,复位仅为对称)。
    keep_awake::set_enabled(false);
    result
}
