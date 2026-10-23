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
    BackgroundSettings, DecayChoice, Palette, ScaleMode, Settings, ThemePreset, ViewMode,
    band_color as palette_band_color, blur_rgba, load as load_settings, resolve_palette,
    save as save_settings, spectro_pixels,
};
#[cfg(target_os = "macos")]
use audio_spectrum_app::{MenuCommand, NativeMenu};
use audio_spectrum_capture::{CaptureError, CaptureStream, CaptureSystem, DeviceInfo, SourceKind};
use audio_spectrum_core::{
    Analyzer, DB_CEILING, DB_FLOOR, DecaySpeed, FRAME_SIZE, FREQ_MIN_HZ, SPECTRO_COLUMN_COUNT,
    Smoother, Spectrograph,
};
use iced::alignment::{Horizontal, Vertical};
use iced::keyboard::{self, key::Named};
use iced::widget::image::Handle as ImageHandle;
use iced::widget::{
    Space, button, canvas, checkbox, column, container, mouse_area, row, scrollable, slider, stack,
    text, text_input,
};
use iced::window;
use iced::{
    Color, Element, Font, Length, Padding, Pixels, Point, Rectangle, Size, Subscription, Task,
    Theme,
};

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
    // —— 配置动作(菜单栏/设置窗共用)——
    OpenSettings,
    CloseSettings,
    SetView(ViewMode),
    ToggleFullscreen,
    ToggleAlwaysOnTop,
    ToggleFrozen,
    ToggleHud,
    SelectSource(DeviceInfo),
    SetBandCount(usize),
    TogglePeakHold,
    SetDecay(DecayChoice),
    ToggleGrid,
    SetTheme(ThemePreset),
    ColorSlotInput(ColorSlot, String),
    ColorSlotReset(ColorSlot),
    SetCornerRadius(f32),
    SetBarGap(f32),
    PickBackground,
    ClearBackground,
    SetScaleMode(ScaleMode),
    SetOpacity(f32),
    SetBlur(f32),
    ToggleFullscreenDefault,
    ToggleRememberWindow,
}

/// 应用菜单栏的顶层菜单(非 macOS)。
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuTab {
    View,
    Source,
}

/// 颜色覆盖槽(设置窗的十六进制输入对应 StyleSettings 的四个 Option)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorSlot {
    BarBottom,
    BarTop,
    Bg,
    Peak,
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
    /// 设置窗开关(⌘, / 菜单栏打开)。
    settings_open: bool,
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
    analyzer: Analyzer,
    smoother: Smoother,
    spectrograph: Spectrograph,
    target: Vec<f32>,
    last_bands: Vec<f32>,
    last_peaks: Vec<f32>,
    staging: Vec<f32>,
    /// 背景缓存:解码原图(供模糊参数变化时重烘焙)与成品纹理。
    bg_raw: Option<(Vec<[u8; 4]>, usize, usize)>,
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
    /// 颜色覆盖槽的十六进制输入草稿(与 StyleSettings 的 Option 解析联动)。
    color_drafts: [String; 4],
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
        let decay: DecaySpeed = settings.decay.into();
        let mut smoother = Smoother::new(band_count, settings.peak_hold);
        smoother.set_decay(decay);
        let pal = resolve_palette(&settings.style);
        let drafts = [
            to_hex(pal.bar_bottom),
            to_hex(pal.bar_top),
            to_hex(pal.bg),
            to_hex(pal.peak),
        ];
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
            settings_open: false,
            #[cfg(target_os = "macos")]
            native_menu: None,
            #[cfg(not(target_os = "macos"))]
            open_menu: None,
            frozen: false,
            last_canvas_click: None,
            fullscreen: false,
            last_mode_poll: f64::NEG_INFINITY,
            settings_dirty: None,
            startup_rect: None,
            analyzer: Analyzer::new(band_count),
            smoother,
            spectrograph: Spectrograph::new(band_count),
            target: vec![DB_FLOOR; band_count],
            last_bands: vec![DB_FLOOR; band_count],
            last_peaks: vec![DB_FLOOR; band_count],
            staging: Vec::new(),
            bg_raw: None,
            bg_key: None,
            bg_handle: None,
            bg_src_size: [1.0, 1.0],
            start: Instant::now(),
            main_window: None,
            window_size: Size::new(920.0, 440.0),
            window_pos: None,
            color_drafts: drafts,
        }
    }

    /// 标记设置已变更(300ms 防抖后落盘)。
    fn mark_dirty(&mut self) {
        self.settings_dirty = Some(Instant::now());
    }

    /// 重建分析管线(频段数变化后);显示状态清零重来。
    fn rebuild_analysis(&mut self) {
        let band_count = self.settings.band_count;
        self.analyzer = Analyzer::new(band_count);
        let decay: DecaySpeed = self.settings.decay.into();
        self.smoother = Smoother::new(band_count, self.settings.peak_hold);
        self.smoother.set_decay(decay);
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
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let pixels: Vec<[u8; 4]> = rgba
                                .into_raw()
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|c| [c[0], c[1], c[2], c[3]])
                                .collect();
                            self.bg_raw = Some((pixels, w as usize, h as usize));
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
                let (mut pixels, w, h) = self.bg_raw.clone().expect("bg_raw just set");
                if want_blur > 0 {
                    blur_rgba(&mut pixels, w, h, want_blur as usize);
                }
                self.bg_src_size = [w as f32, h as f32];
                let flat: Vec<u8> = pixels.into_iter().flatten().collect();
                self.bg_handle = Some(ImageHandle::from_rgba(w as u32, h as u32, flat));
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
            // ESC 优先退全屏;其次关设置窗/收起菜单栏下拉。
            keyboard::Key::Named(Named::Escape) => {
                if self.fullscreen {
                    self.do_toggle_fullscreen()
                } else {
                    self.settings_open = false;
                    #[cfg(not(target_os = "macos"))]
                    {
                        self.open_menu = None;
                    }
                    Task::none()
                }
            }
            keyboard::Key::Character(c) if c.eq_ignore_ascii_case("s") => {
                self.settings.view = match self.settings.view {
                    ViewMode::Spectrum => ViewMode::Spectrogram,
                    ViewMode::Spectrogram => ViewMode::Spectrum,
                };
                self.mark_dirty();
                Task::none()
            }
            keyboard::Key::Character(c) if c.eq_ignore_ascii_case(",") && primary => {
                self.settings_open = true;
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

    /// 颜色覆盖槽输入:草稿始终记录,可解析的十六进制即时覆盖预设。
    fn color_input(&mut self, slot: ColorSlot, draft: String) {
        self.color_drafts[slot as usize] = draft;
        if let Some(rgb) = parse_hex(&self.color_drafts[slot as usize]) {
            self.set_color_slot(slot, rgb);
        }
    }

    /// 颜色覆盖槽重置：清除覆盖，草稿回到预设值。
    fn color_reset(&mut self, slot: ColorSlot) {
        match slot {
            ColorSlot::BarBottom => self.settings.style.bar_bottom = None,
            ColorSlot::BarTop => self.settings.style.bar_top = None,
            ColorSlot::Bg => self.settings.style.bg = None,
            ColorSlot::Peak => self.settings.style.peak = None,
        }
        let pal = resolve_palette(&self.settings.style);
        let value = match slot {
            ColorSlot::BarBottom => pal.bar_bottom,
            ColorSlot::BarTop => pal.bar_top,
            ColorSlot::Bg => pal.bg,
            ColorSlot::Peak => pal.peak,
        };
        self.color_drafts[slot as usize] = to_hex(value);
        self.mark_dirty();
    }

    fn set_color_slot(&mut self, slot: ColorSlot, rgb: [u8; 3]) {
        match slot {
            ColorSlot::BarBottom => self.settings.style.bar_bottom = Some(rgb),
            ColorSlot::BarTop => self.settings.style.bar_top = Some(rgb),
            ColorSlot::Bg => self.settings.style.bg = Some(rgb),
            ColorSlot::Peak => self.settings.style.peak = Some(rgb),
        }
        self.mark_dirty();
    }

    /// 应用原生菜单命令(macOS)。
    #[cfg(target_os = "macos")]
    fn apply_menu_command(&mut self, cmd: MenuCommand) -> Task<Message> {
        match cmd {
            MenuCommand::OpenSettings => {
                self.settings_open = true;
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
            MenuCommand::SelectSource(info) => self.do_select_source(info),
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
        Message::SetView(_)
            | Message::ToggleFullscreen
            | Message::ToggleFullscreenDefault
            | Message::ToggleFrozen
            | Message::ToggleAlwaysOnTop
            | Message::ToggleRememberWindow
            | Message::ToggleHud
            | Message::SetBandCount(_)
            | Message::SetDecay(_)
            | Message::SelectSource(_)
            | Message::OpenSettings
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
                        );
                        polled
                    }
                };
                for cmd in commands {
                    tasks.push(app.apply_menu_command(cmd));
                }
            }

            // 全屏状态回读(节流;兼容系统绿色按钮)。
            if let Some(id) = app.main_window
                && now - app.last_mode_poll >= MODE_POLL_SECS
            {
                app.last_mode_poll = now;
                tasks.push(window::mode(id).map(Message::WindowMode));
            }

            app.tick(now);
            app.pump_audio();
            // 冻结时分析继续(环缓冲不积压),但显示与语谱滚动停住。
            if !app.frozen {
                app.spectrograph.push_frame(now, &app.target);
                let smoothed = app.smoother.process(&app.target);
                app.last_bands = smoothed.bands;
                app.last_peaks = smoothed.peaks;
            }
            app.ensure_background();

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
        Message::OpenSettings => {
            app.settings_open = true;
            Task::none()
        }
        Message::CloseSettings => {
            app.settings_open = false;
            Task::none()
        }
        Message::SetView(v) => {
            if app.settings.view != v {
                app.settings.view = v;
                app.mark_dirty();
            }
            Task::none()
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
        Message::SetBandCount(n) => {
            if app.settings.band_count != n {
                app.settings.band_count = n;
                app.rebuild_analysis();
                app.mark_dirty();
            }
            Task::none()
        }
        Message::TogglePeakHold => {
            app.settings.peak_hold = !app.settings.peak_hold;
            app.smoother.set_peak_hold(app.settings.peak_hold);
            app.mark_dirty();
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
        Message::ColorSlotInput(slot, draft) => {
            app.color_input(slot, draft);
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

    // 语谱纹理:每帧重建 RGBA(600×96 ≈ 230KB,预算内;见 README HUD)。
    let spectro = (app.settings.view == ViewMode::Spectrogram).then(|| {
        let pixels = spectro_pixels(&app.spectrograph, &palette, bg_on);
        let flat: Vec<u8> = pixels.into_iter().flatten().collect();
        ImageHandle::from_rgba(
            SPECTRO_COLUMN_COUNT as u32,
            app.settings.band_count as u32,
            flat,
        )
    });

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

    // 右下性能 HUD(默认关;视图菜单可开)。
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

    // 设置窗(⌘, / 菜单栏打开;可滚动,不常驻)。
    if app.settings_open {
        layers.push(dim_overlay(Message::CloseSettings));
        layers.push(settings_panel(app));
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
        let bg = rgb(self.palette.bg);
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
        }
        self.draw_hover(&mut frame, plot, bounds, cursor);

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

        let (l, r) = (plot.x, plot.x + plot.width);
        let (top, bottom) = (plot.y, plot.y + plot.height);
        let (w, h) = (plot.width, plot.height);

        if self.app.settings.style.grid {
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

        let gap = self.app.settings.style.bar_gap;
        let peak_color = Color::from_rgba8(pal.peak[0], pal.peak[1], pal.peak[2], 225.0 / 255.0);
        let rounding = self.app.settings.style.corner_radius;
        for i in 0..self.app.analyzer.num_bands() {
            let (f_lo, f_hi) = self.app.analyzer.band_edges(i);
            let x0 = freq_to_x(f_lo, l, w) + 0.5;
            let x1 = freq_to_x(f_hi, l, w) - 0.5;
            let raw_w = x1 - x0;
            if raw_w < 0.5 {
                continue;
            }
            let inset = (raw_w * gap * 0.5).min(raw_w * 0.45);
            let (x0, x1) = (x0 + inset, x1 - inset);
            let bar_top = db_to_y(self.app.last_bands[i], top, h);
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
            let peak = self.app.last_peaks[i];
            if peak > DB_FLOOR + 0.5 {
                // 峰值保持块:悬于柱子上方——底边对齐峰值电平,衰减慢于
                // 柱体(PEAK_DECAY < RELEASE 视觉),形成经典拖尾帽。
                let py = db_to_y(peak, top, h);
                let radius = rounding.round().clamp(0.0, 2.0);
                fill_rounded(
                    frame,
                    Rectangle::new(Point::new(x0, py - 4.0), Size::new(x1 - x0, 4.0)),
                    radius,
                    peak_color,
                );
            }
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

/// 中央画布的绘图区(留出坐标轴边距)。
/// 绘图区:铺满整个画布(全出血;频率刻度内嵌在绘图区底部内缘)。
fn plot_rect(full: Rectangle) -> Rectangle {
    full
}

fn freq_to_x(freq: f32, left: f32, width: f32) -> f32 {
    let t = (((freq / FREQ_MIN_HZ).ln()) / LOG_SPAN).clamp(0.0, 1.0);
    left + t * width
}

fn db_to_y(db: f32, top: f32, height: f32) -> f32 {
    let t = ((db - DB_FLOOR) / (DB_CEILING - DB_FLOOR)).clamp(0.0, 1.0);
    top + (1.0 - t) * height
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

/// 应用菜单栏(非 macOS):视图 / 声源 两个下拉菜单 + 设置直达按钮。
/// 全屏时整体隐藏(见 view),保持纯净画面。
#[cfg(not(target_os = "macos"))]
fn menu_bar(app: &SpectrumApp) -> Element<'_, Message> {
    let tabs = [(MenuTab::View, "视图"), (MenuTab::Source, "声源")];
    let mut bar = row![].spacing(2);
    for (tab, label) in tabs {
        let open = app.open_menu == Some(tab);
        bar = bar.push(menu_tab(label, open, Message::ToggleMenu(tab)));
    }
    // 设置:直达按钮(高亮 = 设置窗已打开)。
    bar = bar.push(menu_tab("设置", app.settings_open, Message::OpenSettings));

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
        MenuTab::View => vec![
            section_label("视图").into(),
            row![
                choice(
                    "频谱",
                    s.view == ViewMode::Spectrum,
                    Message::SetView(ViewMode::Spectrum)
                ),
                choice(
                    "语谱",
                    s.view == ViewMode::Spectrogram,
                    Message::SetView(ViewMode::Spectrogram)
                ),
            ]
            .spacing(4)
            .into(),
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
            section_label("频段数").into(),
            band_count_row(s),
            section_label("衰减速度").into(),
            decay_row(s),
        ],
        MenuTab::Source => {
            let mut items: Vec<Element<'_, Message>> = Vec::new();
            let note = app.system.loopback_note();
            if app.loopback_devices.is_empty() && app.input_devices.is_empty() {
                items.push(text(note).size(12.0).color(SECTION_FG).into());
            } else {
                for (devices, label, empty_hint) in [
                    (
                        &app.loopback_devices,
                        "Loopback(系统回环)",
                        "未发现输出设备",
                    ),
                    (
                        &app.input_devices,
                        "Input(麦克风/输入)",
                        "未发现录音设备(可能已在系统声音设置中禁用)",
                    ),
                ] {
                    items.push(section_label(label).into());
                    if devices.is_empty() {
                        // 空分组不再隐藏:明确提示原因,便于定位系统设置。
                        items.push(
                            text(empty_hint)
                                .size(12.0)
                                .color(SECTION_FG)
                                .font(CJK_FONT)
                                .into(),
                        );
                        continue;
                    }
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

    // 锚点:菜单栏正下方,左缘对齐对应菜单项。
    let left = match tab {
        MenuTab::View => 6.0,
        MenuTab::Source => 6.0 + MENU_TAB_WIDTH,
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

/// 频段数可选档位。
#[cfg(not(target_os = "macos"))]
const BAND_CHOICES: &[usize] = &[32, 48, 64, 96, 128, 192];

/// 「频段数」档位行(视图菜单内;紧凑多档并排)。
#[cfg(not(target_os = "macos"))]
fn band_count_row(s: &Settings) -> Element<'static, Message> {
    row(BAND_CHOICES
        .iter()
        .map(|&n| mini_choice(&n.to_string(), s.band_count == n, Message::SetBandCount(n)).into())
        .collect::<Vec<_>>())
    .spacing(3)
    .into()
}

/// 「衰减速度」档位行(视图菜单内;五档并排,名称见 [`DecayChoice::name`])。
#[cfg(not(target_os = "macos"))]
fn decay_row(s: &Settings) -> Element<'static, Message> {
    row(DecayChoice::ALL
        .iter()
        .map(|&d| mini_choice(d.name(), s.decay == d, Message::SetDecay(d)).into())
        .collect::<Vec<_>>())
    .spacing(3)
    .into()
}

/// 紧凑单选按钮(档位行用;比 [`choice`] 窄,容纳 5—6 档并排)。
#[cfg(not(target_os = "macos"))]
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

/// 设置窗(v0.4.3:窗口承载,可滚动,不常驻;居中覆盖整个画布。
/// 声源选择收在菜单栏「声源」下拉/macOS 原生菜单,不在设置窗重复)。
fn settings_panel(app: &SpectrumApp) -> Element<'_, Message> {
    let s = &app.settings;
    // 视图/窗口/频段/衰减项在视图菜单(非 macOS)/原生菜单(macOS);
    // 此处只保留音色类微调:Peak Hold、网格、样式、背景图。
    let mut col: iced::widget::Column<'static, Message> =
        column![section_label("显示")].spacing(6);
    col = col.push(
        checkbox(s.peak_hold)
            .label("Peak Hold 峰值保持")
            .on_toggle(|_| Message::TogglePeakHold),
    );
    col = col.push(
        checkbox(s.style.grid)
            .label("显示网格")
            .on_toggle(|_| Message::ToggleGrid),
    );
    col = col.push(iced::widget::rule::horizontal(1.0));

    // ---- 样式 ----
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
    col = col.push(color_row(
        "渐变底色",
        ColorSlot::BarBottom,
        pal.bar_bottom,
        &app.color_drafts[0],
    ));
    col = col.push(color_row(
        "渐变顶色",
        ColorSlot::BarTop,
        pal.bar_top,
        &app.color_drafts[1],
    ));
    col = col.push(color_row(
        "背景色",
        ColorSlot::Bg,
        pal.bg,
        &app.color_drafts[2],
    ));
    col = col.push(color_row(
        "峰值色",
        ColorSlot::Peak,
        pal.peak,
        &app.color_drafts[3],
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
    col = col.push(iced::widget::rule::horizontal(1.0));

    // ---- 背景图 ----
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

    // 面板:固定宽,高度随窗口(留边),内容超出滚动。
    let height = (app.window_size.height - 48.0).clamp(240.0, 560.0);
    let panel = container(
        scrollable(
            container(col.spacing(6).width(Length::Fill).padding(Padding {
                top: 0.0,
                right: 4.0,
                bottom: 0.0,
                left: 0.0,
            }))
            .width(Length::Fixed(400.0)),
        )
        .width(Length::Fixed(416.0))
        .height(Length::Fixed(height)),
    )
    .padding(12.0)
    .style(|_theme: &Theme| panel_style());

    container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center)
        .into()
}

fn for_row_presets(current: ThemePreset) -> row::Row<'static, Message> {
    row(ThemePreset::ALL
        .iter()
        .map(|&p| choice(p.name(), p == current, Message::SetTheme(p)).into())
        .collect::<Vec<_>>())
    .spacing(4)
}

/// 颜色覆盖槽:预设色打底,色板 + 十六进制输入一改即覆盖,↺ 恢复预设。
fn color_row(
    label: &str,
    slot: ColorSlot,
    preset: [u8; 3],
    draft: &str,
) -> Element<'static, Message> {
    row![
        text(label.to_string())
            .size(13.0)
            .color(BTN_FG)
            .font(CJK_FONT)
            .width(Length::Fixed(64.0)),
        container(
            Space::new()
                .width(Length::Fixed(24.0))
                .height(Length::Fixed(16.0))
        )
        .style(move |_theme: &Theme| container::Style {
            background: Some(rgb(preset).into()),
            border: iced::border::rounded(3),
            ..Default::default()
        }),
        text_input("", draft)
            .size(13.0)
            .width(Length::Fixed(92.0))
            .on_input(move |s| Message::ColorSlotInput(slot, s)),
        button(text("↺").size(12.0).color(BTN_FG))
            .on_press(Message::ColorSlotReset(slot))
            .padding([2.0, 6.0])
            .style(|_theme, _status| flat_button()),
    ]
    .spacing(6)
    .align_y(Vertical::Center)
    .into()
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

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(t, 16).ok()?;
    Some([(v >> 16) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8])
}

fn to_hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

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
    let settings = load_settings();

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
        fullscreen: settings.default_fullscreen,
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

    app.run()
}
