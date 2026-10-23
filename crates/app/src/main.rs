#![forbid(unsafe_code)]
//! music-spec-app — the egui shell of music-spec.
//!
//! v0.3.0 布局契约(设计共识):频谱内容占据整个窗体,一切配置收进
//! 原生菜单栏(macOS)/右键菜单承担;全屏时 ESC 或右键退出。设置经
//! `music_spec_app` lib 持久化(ron/惯例目录/300ms 防抖)。
//!
//! Threading model (ADR-0002): the audio callback writes the ring buffer
//! inside `music-spec-capture`; this UI thread pops samples, runs the FFT and
//! paints, at a constant repaint rate.

use eframe::egui;
use egui::{
    Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, TextureHandle, TextureOptions,
    TextureWrapMode,
};
use music_spec_app::{
    DecayChoice, Palette, ScaleMode, Settings, ThemePreset, ViewMode,
    band_color as palette_band_color, blur_rgba, fit_rects, load as load_settings, resolve_palette,
    save as save_settings, spectro_pixels,
};
#[cfg(target_os = "macos")]
use music_spec_app::{MenuCommand, NativeMenu};
use music_spec_capture::{CaptureError, CaptureStream, CaptureSystem, DeviceInfo, SourceKind};
use music_spec_core::{
    Analyzer, DB_CEILING, DB_FLOOR, DecaySpeed, FRAME_SIZE, FREQ_MIN_HZ, SPECTRO_COLUMN_COUNT,
    Smoother, Spectrograph,
};

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// `ln(FREQ_MAX_HZ / FREQ_MIN_HZ)` = ln(1000) — the log-frequency axis span.
const LOG_SPAN: f32 = 6.907_755;

const FREQ_TICKS: &[f32] = &[
    63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
const DB_TICKS: &[f32] = &[0.0, -20.0, -40.0, -60.0];

/// Embedded at compile time so the binary has no runtime asset dependency.
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/music-spec-256.png");

const GRID: Color32 = Color32::from_rgb(36, 41, 52);
const AXIS_TEXT: Color32 = Color32::from_rgb(108, 114, 130);
const STATUS_OK: Color32 = Color32::from_rgb(96, 200, 128);
const STATUS_WARN: Color32 = Color32::from_rgb(232, 180, 96);
/// 设置防抖落盘间隔。
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

fn main() -> eframe::Result {
    let settings = load_settings();

    let mut viewport = egui::ViewportBuilder::default().with_inner_size([920.0, 440.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    if let Some([x, y, w, h]) = settings.window_rect {
        viewport = viewport
            .with_inner_size([w.max(320.0), h.max(240.0)])
            .with_position([x, y]);
    }
    if settings.always_on_top {
        viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnTop);
    }
    if settings.default_fullscreen {
        viewport = viewport.with_fullscreen(true);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "music-spec",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(SpectrumApp::new(settings)))
        }),
    )
}

struct SpectrumApp {
    system: CaptureSystem,
    /// The selected Source; the stream below is (re)opened for it.
    source: Option<DeviceInfo>,
    /// `Some` = capturing; `None` + `reconnect_at` = Reconnect in progress.
    stream: Option<CaptureStream>,
    reconnect_at: Option<f64>,
    status: String,
    /// Device list cache, refreshed once per second (enumeration is not free).
    loopback_devices: Vec<DeviceInfo>,
    input_devices: Vec<DeviceInfo>,
    last_device_refresh: f64,
    frame_cost_ms: f64,
    frame_interval_ms: f64,
    last_frame_at: Option<f64>,
    /// 全部持久化设置(唯一事实源)。
    settings: Settings,
    /// 设置窗开关(原生菜单 ⌘, / 右键菜单打开)。
    settings_open: bool,
    /// macOS 原生菜单(首帧安装)。
    #[cfg(target_os = "macos")]
    native_menu: Option<NativeMenu>,
    /// 冻结显示(Space):分析继续,画面与滚动停止。瞬态,不持久化。
    frozen: bool,
    /// 视口是否全屏(每帧从视口信息同步,兼容系统绿色按钮)。
    fullscreen: bool,
    /// 防 Dirty 标记:设置变更时间,防抖后落盘。
    settings_dirty: Option<Instant>,
    /// 启动时的窗口矩形,避免启动即触发一轮「位置变化」保存。
    startup_rect: Option<[f32; 4]>,
    analyzer: Analyzer,
    smoother: Smoother,
    spectrograph: Spectrograph,
    spectro_texture: Option<TextureHandle>,
    target: Vec<f32>,
    last_bands: Vec<f32>,
    last_peaks: Vec<f32>,
    staging: Vec<f32>,
    /// 背景缓存:解码原图(供模糊参数变化时重烘焙)与成品纹理。
    bg_raw: Option<(Vec<[u8; 4]>, usize, usize)>,
    bg_key: Option<(PathBuf, u32)>,
    bg_texture: Option<TextureHandle>,
    bg_src_size: [f32; 2],
}

impl SpectrumApp {
    fn new(settings: Settings) -> SpectrumApp {
        let system = CaptureSystem::new();
        // Design consensus: open Loopback (whatever the system is playing)
        // at startup; fall back to the default Input device.
        let source = system
            .default_source(SourceKind::Loopback)
            .or_else(|| system.default_source(SourceKind::Input));
        let status = match &source {
            Some(s) => format!("打开 {}…", s.name),
            None => "未找到可用声源,请从菜单栏选择".to_string(),
        };
        let band_count = settings.band_count;
        let decay: DecaySpeed = settings.decay.into();
        let mut smoother = Smoother::new(band_count, settings.peak_hold);
        smoother.set_decay(decay);
        SpectrumApp {
            system,
            source,
            stream: None,
            reconnect_at: None,
            status,
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
            frozen: false,
            fullscreen: false,
            settings_dirty: None,
            startup_rect: None,
            analyzer: Analyzer::new(band_count),
            smoother,
            spectrograph: Spectrograph::new(band_count),
            spectro_texture: None,
            target: vec![DB_FLOOR; band_count],
            last_bands: vec![DB_FLOOR; band_count],
            last_peaks: vec![DB_FLOOR; band_count],
            staging: Vec::new(),
            bg_raw: None,
            bg_key: None,
            bg_texture: None,
            bg_src_size: [1.0, 1.0],
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
        self.spectro_texture = None; // 纹理尺寸随 Band 数变化,重建
        self.target = vec![DB_FLOOR; band_count];
        self.last_bands = vec![DB_FLOOR; band_count];
        self.last_peaks = vec![DB_FLOOR; band_count];
    }

    /// Source lifecycle: detect dead streams, schedule and perform Reconnects.
    fn tick(&mut self, now: f64) {
        if self.stream.as_ref().is_some_and(|s| s.is_dead()) {
            self.stream = None;
            self.staging.clear();
            self.status = "设备断开,正在重连…".to_string();
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
                self.status = format!("采集中:{}", source.name);
            }
            Err(CaptureError::DeviceGone(_)) => {
                // The device vanished: fall back to the kind's default Source.
                match self.system.default_source(source.kind) {
                    Some(next) => {
                        if next.id != source.id {
                            self.status = format!("设备已断开,切换到 {}", next.name);
                            self.source = Some(next);
                        } else {
                            self.status = "设备无响应,继续重试…".to_string();
                        }
                        self.reconnect_at = Some(now + 0.5);
                    }
                    None => {
                        self.status = "该类声源暂不可用".to_string();
                        self.source = None;
                        self.reconnect_at = Some(now + 2.0);
                    }
                }
            }
            Err(e) => {
                self.status = e.to_string();
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

    /// The user picked a new Source in the menu.
    fn switch_source(&mut self) {
        self.stream = None;
        self.reconnect_at = None;
        self.staging.clear();
        self.status = "打开声源…".to_string();
    }

    /// 确保背景纹理与设置一致(路径或模糊变化时重解码/重烘焙)。
    fn ensure_background(&mut self, ctx: &egui::Context) {
        let want_path = self.settings.background.path.clone();
        let want_blur = self.settings.background.blur_px;
        let want_key = want_path.clone().map(|p| (p, want_blur));
        if want_key == self.bg_key {
            return;
        }
        let old_key = self.bg_key.clone();
        self.bg_key = want_key;
        match &want_path {
            None => {
                self.bg_texture = None;
                self.bg_raw = None;
            }
            Some(path) => {
                // 路径变化 → 解码原图;仅模糊变化 → 复用原图重烘焙。
                let path_changed = old_key.as_ref().map(|(p, _)| p) != want_path.as_ref();
                let need_decode = path_changed || self.bg_raw.is_none();
                if need_decode {
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
                            self.bg_texture = None;
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
                let image = egui::ColorImage {
                    size: [w, h],
                    source_size: egui::vec2(w as f32, h as f32),
                    pixels: pixels
                        .iter()
                        .map(|p| Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
                        .collect(),
                };
                let options = TextureOptions {
                    wrap_mode: if self.settings.background.mode == ScaleMode::Tile {
                        TextureWrapMode::Repeat
                    } else {
                        TextureWrapMode::ClampToEdge
                    },
                    ..Default::default()
                };
                match &mut self.bg_texture {
                    Some(tex) => tex.set(image, options),
                    None => {
                        self.bg_texture = Some(ctx.load_texture("background", image, options));
                    }
                }
            }
        }
    }
}

impl eframe::App for SpectrumApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 常驻重绘(设计共识):spectrum animation never sleeps.
        ui.ctx().request_repaint();
        let frame_start = Instant::now();

        let now = ui.input(|i| i.time);

        // 键盘:S 切换 频谱/语谱,Space 冻结/解冻,⌘,/Ctrl+, 设置(设计共识)。
        ui.input(|i| {
            if i.key_pressed(egui::Key::S) {
                self.settings.view = match self.settings.view {
                    ViewMode::Spectrum => ViewMode::Spectrogram,
                    ViewMode::Spectrogram => ViewMode::Spectrum,
                };
                self.mark_dirty();
            }
            if i.key_pressed(egui::Key::Space) {
                self.frozen = !self.frozen;
            }
            if i.key_pressed(egui::Key::Comma) && i.modifiers.command {
                self.settings_open = true;
            }
            // F11:全屏切换(仅非 macOS;macOS 的 F11 是系统调度中心热键,
            // 全屏走 ⌘F/原生菜单/窗口绿色按钮)。egui 仅在焦点窗口收到按键。
            #[cfg(not(target_os = "macos"))]
            if i.key_pressed(egui::Key::F11) {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!self.fullscreen));
            }
        });

        // 全屏状态同步(兼容系统按钮进入的全屏)+ 退出:
        // ESC 优先关设置窗(非全屏时);全屏时 ESC/右键(macOS)退出。
        self.fullscreen = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if self.fullscreen && (esc || ui.input(|i| i.pointer.secondary_clicked())) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        } else if esc && self.settings_open && !self.fullscreen {
            self.settings_open = false;
        }

        // macOS 原生菜单:首帧安装,每帧轮询命令、同步勾选、重建声源。
        #[cfg(target_os = "macos")]
        {
            if self.native_menu.is_none() {
                self.native_menu = NativeMenu::install();
                if std::env::var_os("MUSIC_SPEC_DEBUG").is_some() {
                    eprintln!(
                        "[menu] install: {}",
                        if self.native_menu.is_some() {
                            "ok"
                        } else {
                            "FAILED"
                        }
                    );
                }
            }
            let commands: Vec<MenuCommand> = match self.native_menu.as_mut() {
                None => Vec::new(),
                Some(menu) => {
                    menu.rebuild_sources(
                        &self.loopback_devices,
                        &self.input_devices,
                        self.source.as_ref().map(|d| d.id.as_str()),
                    );
                    let polled = menu.poll();
                    menu.sync(
                        self.settings.view,
                        self.fullscreen,
                        self.settings.always_on_top,
                        self.frozen,
                        self.settings.show_hud,
                    );
                    polled
                }
            };
            for cmd in commands {
                self.apply_menu_command(cmd, ui.ctx());
            }
        }

        // 窗口几何记忆(防抖):仅在记住开启且非全屏时跟踪。
        if self.settings.remember_window && !self.fullscreen {
            let rect = ui.input(|i| i.viewport().outer_rect);
            if let Some(r) = rect {
                let geom = [r.min.x, r.min.y, r.width(), r.height()];
                if self.startup_rect.is_none() {
                    self.startup_rect = Some(geom); // 首帧基线,避免启动即写
                } else if Some(geom) != self.settings.window_rect && Some(geom) != self.startup_rect
                {
                    self.settings.window_rect = Some(geom);
                    self.mark_dirty();
                }
            }
        }

        self.tick(now);
        self.pump_audio();
        // 冻结时分析继续(环缓冲不积压),但显示与语谱滚动停住。
        if !self.frozen {
            self.spectrograph.push_frame(now, &self.target);
            let smoothed = self.smoother.process(&self.target);
            self.last_bands = smoothed.bands;
            self.last_peaks = smoothed.peaks;
        }

        self.ensure_background(ui.ctx());

        let palette = resolve_palette(&self.settings.style);
        let bg_on = self.bg_texture.is_some();
        let loop_note = self.system.loopback_note().to_owned();
        let status = self.status.clone();
        let running = self.stream.is_some();
        let loopback_devices = self.loopback_devices.clone();
        let input_devices = self.input_devices.clone();

        // 频谱占满整个窗体(设计共识 v0.4.0):无顶栏;
        // 状态与 HUD 为画布左下/右下浮层,配置走原生菜单/右键菜单/设置窗。
        let panel_frame = egui::Frame::new()
            .inner_margin(0)
            .fill(Color32::TRANSPARENT);
        let panel = egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ui, |ui| {
                let plot = self.plot_rect(ui);
                let full = ui.available_rect_before_wrap();
                self.draw_background(ui, full, &palette);
                match self.settings.view {
                    ViewMode::Spectrum => self.draw_spectrum(ui, plot, &palette, bg_on),
                    ViewMode::Spectrogram => self.draw_spectrogram(ui, plot, &palette, bg_on),
                }
                self.draw_hover(ui, plot);
                self.draw_overlays(ui, full, &status, running);
            });

        // 非 macOS:右键菜单作为配置入口(macOS 由原生菜单承担)。
        #[cfg(not(target_os = "macos"))]
        panel.response.context_menu(|ui| {
            self.context_menu(ui, &loopback_devices, &input_devices, &loop_note)
        });
        #[cfg(target_os = "macos")]
        let _ = &panel;

        // 设置窗(⌘, / 原生菜单 / 右键菜单打开;可滚动,不常驻)。
        let mut settings_open = self.settings_open;
        egui::Window::new("设置")
            .open(&mut settings_open)
            .default_size([380.0, 500.0])
            .vscroll(true)
            .show(ui.ctx(), |ui| {
                self.settings_ui(ui, &loopback_devices, &input_devices, &loop_note);
            });
        self.settings_open = settings_open;

        // HUD statistics: EMA over ~60 frames.
        let cost_ms = frame_start.elapsed().as_secs_f64() * 1000.0;
        self.frame_cost_ms += 0.1 * (cost_ms - self.frame_cost_ms);
        if let Some(prev) = self.last_frame_at {
            let interval_ms = (now - prev) * 1000.0;
            if interval_ms > 0.0 && interval_ms < 1000.0 {
                self.frame_interval_ms += 0.1 * (interval_ms - self.frame_interval_ms);
            }
        }
        self.last_frame_at = Some(now);
    }
}

impl SpectrumApp {
    /// 设置窗内容(v0.4.0:窗口承载,可滚动,不常驻)。
    fn settings_ui(
        &mut self,
        ui: &mut egui::Ui,
        loopback_devices: &[DeviceInfo],
        input_devices: &[DeviceInfo],
        loop_note: &str,
    ) {
        {
            // ---- 声源 ----
            ui.weak("声源 · Loopback(系统回环)");
            if loopback_devices.is_empty() {
                ui.weak(format!("    {loop_note}"));
            }
            for info in loopback_devices {
                if ui
                    .selectable_value(
                        &mut self.source,
                        Some(info.clone()),
                        format!("🔊 {}", info.name),
                    )
                    .changed()
                {
                    self.switch_source();
                }
            }
            ui.separator();
            ui.weak("声源 · Input(麦克风/线路输入)");
            for info in input_devices {
                if ui
                    .selectable_value(
                        &mut self.source,
                        Some(info.clone()),
                        format!("🎤 {}", info.name),
                    )
                    .changed()
                {
                    self.switch_source();
                }
            }
            ui.separator();

            // ---- 视图与窗口 ----
            ui.weak("视图与窗口");
            let mut view = self.settings.view;
            ui.selectable_value(&mut view, ViewMode::Spectrum, "频谱");
            ui.selectable_value(&mut view, ViewMode::Spectrogram, "语谱");
            if view != self.settings.view {
                self.settings.view = view;
                self.mark_dirty();
            }
            if ui
                .checkbox(&mut self.settings.default_fullscreen, "启动时全屏")
                .changed()
            {
                self.mark_dirty();
            }
            if ui
                .checkbox(&mut self.settings.always_on_top, "窗口置顶")
                .changed()
            {
                let level = if self.settings.always_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                };
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                self.mark_dirty();
            }
            if ui
                .checkbox(&mut self.settings.remember_window, "记住窗口大小和位置")
                .changed()
            {
                self.mark_dirty();
            }
            ui.separator();

            // ---- 显示 ----
            ui.weak("显示");
            ui.horizontal(|ui| {
                ui.label("频段数");
                for n in [64usize, 96] {
                    if ui
                        .selectable_value(&mut self.settings.band_count, n, format!("{n}"))
                        .changed()
                    {
                        self.rebuild_analysis();
                        self.mark_dirty();
                    }
                }
            });
            if ui
                .checkbox(&mut self.settings.peak_hold, "Peak Hold 峰值保持")
                .changed()
            {
                self.smoother.set_peak_hold(self.settings.peak_hold);
                self.mark_dirty();
            }
            ui.horizontal(|ui| {
                ui.label("衰减速度");
                egui::ComboBox::from_id_salt("decay")
                    .selected_text(decay_name(self.settings.decay))
                    .show_ui(ui, |ui| {
                        for d in [DecayChoice::Slow, DecayChoice::Medium, DecayChoice::Fast] {
                            if ui
                                .selectable_value(&mut self.settings.decay, d, decay_name(d))
                                .changed()
                            {
                                self.smoother.set_decay(d.into());
                                self.mark_dirty();
                            }
                        }
                    });
            });
            if ui
                .checkbox(&mut self.settings.style.grid, "显示网格")
                .changed()
            {
                self.mark_dirty();
            }
            ui.separator();

            // ---- 样式 ----
            ui.weak("样式");
            ui.horizontal(|ui| {
                for preset in ThemePreset::ALL {
                    if ui
                        .selectable_value(&mut self.settings.style.theme, *preset, preset.name())
                        .changed()
                    {
                        self.mark_dirty();
                    }
                }
            });
            let pal = resolve_palette(&self.settings.style);
            if color_override(
                ui,
                "渐变底色",
                pal.bar_bottom,
                &mut self.settings.style.bar_bottom,
            ) {
                self.mark_dirty();
            }
            if color_override(
                ui,
                "渐变顶色",
                pal.bar_top,
                &mut self.settings.style.bar_top,
            ) {
                self.mark_dirty();
            }
            if color_override(ui, "背景色", pal.bg, &mut self.settings.style.bg) {
                self.mark_dirty();
            }
            if color_override(ui, "峰值色", pal.peak, &mut self.settings.style.peak) {
                self.mark_dirty();
            }
            ui.horizontal(|ui| {
                ui.label("柱条圆角");
                if ui
                    .add(egui::Slider::new(
                        &mut self.settings.style.corner_radius,
                        0.0..=8.0,
                    ))
                    .changed()
                {
                    self.mark_dirty();
                }
            });
            ui.horizontal(|ui| {
                ui.label("柱条间隙");
                if ui
                    .add(egui::Slider::new(
                        &mut self.settings.style.bar_gap,
                        0.0..=0.45,
                    ))
                    .changed()
                {
                    self.mark_dirty();
                }
            });
            ui.separator();

            // ---- 背景图 ----
            ui.weak("背景图");
            if ui.button("选择图片…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("图片", &["png", "jpg", "jpeg"])
                    .set_title("选择背景图")
                    .pick_file()
            {
                self.settings.background.path = Some(path);
                self.mark_dirty();
            }
            if self.settings.background.path.is_some() && ui.button("清除背景").clicked() {
                self.settings.background.path = None;
                self.mark_dirty();
            }
            ui.horizontal(|ui| {
                ui.label("缩放");
                egui::ComboBox::from_id_salt("bg_scale")
                    .selected_text(self.settings.background.mode.name())
                    .show_ui(ui, |ui| {
                        for m in [ScaleMode::Fill, ScaleMode::Fit, ScaleMode::Tile] {
                            if ui
                                .selectable_value(&mut self.settings.background.mode, m, m.name())
                                .changed()
                            {
                                self.mark_dirty();
                            }
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("不透明度");
                if ui
                    .add(
                        egui::Slider::new(&mut self.settings.background.opacity, 0.05..=1.0)
                            .text("%"),
                    )
                    .changed()
                {
                    self.mark_dirty();
                }
            });
            ui.horizontal(|ui| {
                ui.label("模糊");
                if ui
                    .add(egui::Slider::new(
                        &mut self.settings.background.blur_px,
                        0..=24,
                    ))
                    .changed()
                {
                    self.mark_dirty();
                }
            });
        }
    }
    /// 应用原生菜单命令(macOS)。
    #[cfg(target_os = "macos")]
    fn apply_menu_command(&mut self, cmd: MenuCommand, ctx: &egui::Context) {
        match cmd {
            MenuCommand::OpenSettings => self.settings_open = true,
            MenuCommand::SetView(v) => {
                self.settings.view = v;
                self.mark_dirty();
            }
            MenuCommand::ToggleFullscreen => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!self.fullscreen));
            }
            MenuCommand::ToggleAlwaysOnTop => {
                self.settings.always_on_top = !self.settings.always_on_top;
                let level = if self.settings.always_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                self.mark_dirty();
            }
            MenuCommand::ToggleFrozen => self.frozen = !self.frozen,
            MenuCommand::ToggleHud => {
                self.settings.show_hud = !self.settings.show_hud;
                self.mark_dirty();
            }
            MenuCommand::SelectSource(info) => {
                self.source = Some(info);
                self.switch_source();
            }
        }
    }

    /// 左下状态浮层 + 右下性能 HUD(默认关;原生菜单可开)。
    fn draw_overlays(&self, ui: &mut egui::Ui, full: Rect, status: &str, running: bool) {
        let painter = ui.painter_at(full);
        let chip = Color32::from_rgba_unmultiplied_const(12, 14, 19, 160);

        let status_text = if self.frozen {
            format!("⏸ 已冻结 · {status}")
        } else {
            status.to_string()
        };
        let status_color = if running { STATUS_OK } else { STATUS_WARN };
        let galley = painter.layout(
            status_text,
            FontId::proportional(11.0),
            status_color,
            f32::INFINITY,
        );
        let rect = Rect::from_min_size(
            Pos2::new(full.left() + 12.0, full.bottom() - galley.size().y - 10.0),
            galley.size(),
        );
        painter.rect_filled(rect.expand(6.0), 4.0, chip);
        painter.galley(rect.min, galley, status_color);

        if self.settings.show_hud {
            let text = format!(
                "{:.2}ms/帧 · {:.0}fps · ≈{:.1}% CPU",
                self.frame_cost_ms,
                1000.0 / self.frame_interval_ms.max(0.001),
                100.0 * self.frame_cost_ms / self.frame_interval_ms
            );
            let galley = painter.layout(text, FontId::monospace(10.0), AXIS_TEXT, f32::INFINITY);
            let rect = Rect::from_min_size(
                Pos2::new(
                    full.right() - galley.size().x - 12.0,
                    full.bottom() - galley.size().y - 10.0,
                ),
                galley.size(),
            );
            painter.rect_filled(rect.expand(6.0), 4.0, chip);
            painter.galley(rect.min, galley, AXIS_TEXT);
        }
    }

    /// 非 macOS 的右键菜单(macOS 由原生菜单栏承担)。
    #[cfg(not(target_os = "macos"))]
    fn context_menu(
        &mut self,
        ui: &mut egui::Ui,
        loopback_devices: &[DeviceInfo],
        input_devices: &[DeviceInfo],
        loop_note: &str,
    ) {
        ui.menu_button("视图", |ui| {
            let mut view = self.settings.view;
            ui.selectable_value(&mut view, ViewMode::Spectrum, "频谱");
            ui.selectable_value(&mut view, ViewMode::Spectrogram, "语谱");
            if view != self.settings.view {
                self.settings.view = view;
                self.mark_dirty();
            }
            ui.separator();
            ui.toggle_value(&mut self.frozen, "冻结显示");
            if ui
                .toggle_value(&mut self.settings.always_on_top, "窗口置顶")
                .changed()
            {
                let level = if self.settings.always_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                };
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                self.mark_dirty();
            }
            if ui
                .toggle_value(&mut self.settings.show_hud, "性能信息")
                .changed()
            {
                self.mark_dirty();
            }
        });
        ui.menu_button("声源", |ui| {
            if loopback_devices.is_empty() && input_devices.is_empty() {
                ui.weak(loop_note);
            }
            for (devices, label) in [(loopback_devices, "Loopback"), (input_devices, "Input")] {
                if devices.is_empty() {
                    continue;
                }
                ui.weak(label);
                for info in devices {
                    if ui
                        .selectable_value(&mut self.source, Some(info.clone()), &info.name)
                        .changed()
                    {
                        self.switch_source();
                    }
                }
            }
        });
        ui.separator();
        if ui.button("设置…").clicked() {
            self.settings_open = true;
        }
    }
}

/// 颜色覆盖槽:预设色打底,取色器一改即覆盖。返回是否变更(调用方标记落盘)。
fn color_override(
    ui: &mut egui::Ui,
    label: &str,
    preset: [u8; 3],
    slot: &mut Option<[u8; 3]>,
) -> bool {
    let mut c = *slot.get_or_insert(preset);
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        if egui::color_picker::color_edit_button_srgb(ui, &mut c).changed() {
            changed = true;
        }
    });
    if changed {
        *slot = Some(c);
    }
    changed
}

// ---------------------------------------------------------------------------
// Spectrum canvas
// ---------------------------------------------------------------------------

fn freq_to_x(freq: f32, left: f32, width: f32) -> f32 {
    let t = (((freq / FREQ_MIN_HZ).ln()) / LOG_SPAN).clamp(0.0, 1.0);
    left + t * width
}

fn db_to_y(db: f32, top: f32, height: f32) -> f32 {
    let t = ((db - DB_FLOOR) / (DB_CEILING - DB_FLOOR)).clamp(0.0, 1.0);
    top + (1.0 - t) * height
}

/// 频谱柱渐变色(调色板驱动)。
fn band_color(pal: &Palette, t: f32) -> Color32 {
    let c = palette_band_color(pal, t);
    Color32::from_rgb(c[0], c[1], c[2])
}

impl SpectrumApp {
    /// 中央画布的绘图区(留出坐标轴边距)。
    fn plot_rect(&self, ui: &egui::Ui) -> Rect {
        let full = ui.available_rect_before_wrap();
        Rect::from_min_max(
            Pos2::new(full.left() + 10.0, full.top() + 8.0),
            Pos2::new(full.right() - 10.0, full.bottom() - 20.0),
        )
    }

    /// 背景:图片(缩放/平铺)+ 亮度罩;无图时铺调色板背景色。
    fn draw_background(&self, ui: &mut egui::Ui, full: Rect, pal: &Palette) {
        let painter = ui.painter_at(full);
        let bg_rgb = Color32::from_rgb(pal.bg[0], pal.bg[1], pal.bg[2]);
        match (&self.bg_texture, self.settings.background.path.is_some()) {
            (Some(tex), true) => {
                let dst = music_spec_app::Rect {
                    pos: [full.min.x, full.min.y],
                    size: [full.width(), full.height()],
                };
                let (d, uv) = fit_rects(self.bg_src_size, dst, self.settings.background.mode);
                let d = Rect::from_min_size(
                    Pos2::new(d.pos[0], d.pos[1]),
                    egui::vec2(d.size[0], d.size[1]),
                );
                let uv = Rect::from_min_max(
                    Pos2::new(uv.pos[0], uv.pos[1]),
                    Pos2::new(uv.pos[0] + uv.size[0], uv.pos[1] + uv.size[1]),
                );
                painter.image(tex.id(), d, uv, Color32::WHITE);
                // 亮度罩:按 (1 - 不透明度) 叠背景色。
                let a = ((1.0 - self.settings.background.opacity).clamp(0.0, 1.0) * 255.0) as u8;
                painter.rect_filled(
                    full,
                    0.0,
                    Color32::from_rgba_unmultiplied(bg_rgb.r(), bg_rgb.g(), bg_rgb.b(), a),
                );
            }
            _ => {
                painter.rect_filled(full, 0.0, bg_rgb);
            }
        }
    }

    fn draw_spectrum(&mut self, ui: &mut egui::Ui, plot: Rect, pal: &Palette, bg_on: bool) {
        let painter = ui.painter_at(ui.available_rect_before_wrap());
        // 画布底:有背景时半透明,无背景时实底。
        let plot_bg = Color32::from_rgba_unmultiplied(
            pal.bg[0],
            pal.bg[1],
            pal.bg[2],
            if bg_on { 150 } else { 255 },
        );
        painter.rect_filled(plot, self.settings.style.corner_radius, plot_bg);

        let (l, r) = (plot.left(), plot.right());
        let (top, bottom) = (plot.top(), plot.bottom());
        let (w, h) = (r - l, bottom - top);

        if self.settings.style.grid {
            for &db in DB_TICKS {
                let y = db_to_y(db, top, h);
                painter.line_segment([Pos2::new(l, y), Pos2::new(r, y)], Stroke::new(1.0, GRID));
                painter.text(
                    Pos2::new(l + 4.0, y - 2.0),
                    Align2::LEFT_BOTTOM,
                    format!("{}dB", db as i32),
                    FontId::monospace(10.0),
                    AXIS_TEXT,
                );
            }
            draw_freq_grid(&painter, plot);
        }

        let gap = self.settings.style.bar_gap;
        let peak_color = Color32::from_rgb(pal.peak[0], pal.peak[1], pal.peak[2]);
        let rounding = self.settings.style.corner_radius;
        for i in 0..self.analyzer.num_bands() {
            let (f_lo, f_hi) = self.analyzer.band_edges(i);
            let x0 = freq_to_x(f_lo, l, w) + 0.5;
            let x1 = freq_to_x(f_hi, l, w) - 0.5;
            let raw_w = x1 - x0;
            if raw_w < 0.5 {
                continue;
            }
            let inset = (raw_w * gap * 0.5).min(raw_w * 0.45);
            let x0 = x0 + inset;
            let x1 = x1 - inset;
            let bar_top = db_to_y(self.last_bands[i], top, h);
            if bottom - bar_top > 0.5 {
                // Vertical gradient (palette base → top) as a small mesh.
                let mut mesh = Mesh::default();
                const SEGMENTS: usize = 12;
                for s in 0..=SEGMENTS {
                    let t = s as f32 / SEGMENTS as f32;
                    let y = bottom + (bar_top - bottom) * t;
                    let color = band_color(pal, t);
                    mesh.colored_vertex(Pos2::new(x0, y), color);
                    mesh.colored_vertex(Pos2::new(x1, y), color);
                    if s > 0 {
                        let n = mesh.vertices.len() as u32;
                        mesh.add_triangle(n - 4, n - 3, n - 2);
                        mesh.add_triangle(n - 3, n - 1, n - 2);
                    }
                }
                painter.add(Shape::mesh(mesh));
                if rounding > 0.0 {
                    // 圆角仅作为轻量顶帽处理,避免整柱复杂形状。
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(x0, bar_top - rounding),
                            Pos2::new(x1, bar_top + rounding),
                        ),
                        rounding,
                        band_color(pal, 1.0),
                    );
                }
            }
            let peak = self.last_peaks[i];
            if peak > DB_FLOOR + 0.5 {
                let py = db_to_y(peak, top, h);
                painter.line_segment(
                    [Pos2::new(x0, py), Pos2::new(x1, py)],
                    Stroke::new(
                        1.5,
                        Color32::from_rgba_unmultiplied(
                            peak_color.r(),
                            peak_color.g(),
                            peak_color.b(),
                            210,
                        ),
                    ),
                );
            }
        }
    }

    /// 语谱视图:历史 → 纹理(左老右新,底低频顶高频),单个 quad 绘制,
    /// 使 60fps 预算在 600×96 网格规模下依然成立(见 HUD)。
    fn draw_spectrogram(&mut self, ui: &mut egui::Ui, plot: Rect, pal: &Palette, bg_on: bool) {
        let painter = ui.painter_at(ui.available_rect_before_wrap());
        let plot_bg = Color32::from_rgba_unmultiplied(
            pal.bg[0],
            pal.bg[1],
            pal.bg[2],
            if bg_on { 150 } else { 255 },
        );
        painter.rect_filled(plot, 2.0, plot_bg);

        let bands = self.settings.band_count;
        let pixels = spectro_pixels(&self.spectrograph, pal, bg_on);
        let image = egui::ColorImage {
            size: [SPECTRO_COLUMN_COUNT, bands],
            source_size: egui::vec2(SPECTRO_COLUMN_COUNT as f32, bands as f32),
            pixels: pixels
                .iter()
                .map(|p| Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
                .collect(),
        };
        let options = TextureOptions::default();
        let handle = match &mut self.spectro_texture {
            Some(h) => {
                h.set(image, options);
                h.clone()
            }
            None => {
                let h = ui.ctx().load_texture("spectrogram", image, options);
                self.spectro_texture = Some(h.clone());
                h
            }
        };
        painter.image(
            handle.id(),
            plot,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        if self.settings.style.grid {
            draw_freq_grid(&painter, plot);
        }
    }

    /// 悬停读数(设计共识:光标跟随):该处的频率与 dB。
    fn draw_hover(&self, ui: &egui::Ui, plot: Rect) {
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if !plot.contains(pos) {
            return;
        }
        let bands = self.settings.band_count;
        let band = (((plot.bottom() - pos.y) / plot.height()) * bands as f32).floor() as usize;
        let band = band.min(bands - 1);
        let freq = self.analyzer.band_center(band);
        let freq_txt = if freq >= 1000.0 {
            format!("{:.1}kHz", freq / 1000.0)
        } else {
            format!("{freq:.0}Hz")
        };
        let value = match self.settings.view {
            ViewMode::Spectrum => self.last_bands[band],
            ViewMode::Spectrogram => {
                let x_frac = (pos.x - plot.left()) / plot.width();
                let age = (x_frac * (SPECTRO_COLUMN_COUNT - 1) as f32).round() as usize;
                let age_from_newest = SPECTRO_COLUMN_COUNT - 1 - age;
                self.spectrograph
                    .column(age_from_newest)
                    .map(|c| c[band])
                    .unwrap_or(DB_FLOOR)
            }
        };
        let painter = ui.painter();
        let galley = painter.layout(
            format!("{freq_txt} · {value:.0}dB"),
            FontId::monospace(11.0),
            AXIS_TEXT,
            f32::INFINITY,
        );
        let rect = Rect::from_min_size(
            pos + egui::vec2(12.0, -16.0 - galley.size().y),
            galley.size(),
        );
        painter.rect_filled(
            rect.expand(4.0),
            3.0,
            Color32::from_rgba_unmultiplied_const(16, 18, 24, 235),
        );
        painter.galley(rect.min, galley, Color32::WHITE);
    }
}

/// 频率轴刻度(两个视图共享)。
fn draw_freq_grid(painter: &egui::Painter, plot: Rect) {
    let (l, w) = (plot.left(), plot.right() - plot.left());
    for &f in FREQ_TICKS {
        let x = freq_to_x(f, l, w);
        painter.line_segment(
            [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
            Stroke::new(1.0, Color32::from_rgb(30, 34, 43)),
        );
        let label = if f >= 1000.0 {
            format!("{}k", (f / 1000.0) as i32)
        } else {
            format!("{}", f as i32)
        };
        painter.text(
            Pos2::new(x, plot.bottom() + 3.0),
            Align2::CENTER_TOP,
            label,
            FontId::monospace(10.0),
            AXIS_TEXT,
        );
    }
}

fn decay_name(d: DecayChoice) -> &'static str {
    match d {
        DecayChoice::Slow => "慢",
        DecayChoice::Medium => "中",
        DecayChoice::Fast => "快",
    }
}

/// egui ships no CJK glyphs; install the platform's system CJK font so the
/// status/menu strings render.
fn install_cjk_fonts(ctx: &egui::Context) {
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
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".into());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".into());
        ctx.set_fonts(fonts);
        return;
    }
}
