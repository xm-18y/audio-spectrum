#![forbid(unsafe_code)]
//! music-spec-app — the egui shell of music-spec: Source picking, Reconnect,
//! and the Spectrum canvas (log-frequency axis, gradient Bands, Peak Hold).
//!
//! Threading model (ADR-0002): the audio callback writes the ring buffer
//! inside `music-spec-capture`; this UI thread pops samples, runs the FFT and
//! paints, at a constant repaint rate.

use eframe::egui;
use egui::{
    Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, TextureHandle, TextureOptions,
};
use music_spec_capture::{CaptureError, CaptureStream, CaptureSystem, DeviceInfo, SourceKind};
use music_spec_core::{
    Analyzer, DB_CEILING, DB_FLOOR, DecaySpeed, FRAME_SIZE, FREQ_MIN_HZ, SPECTRO_COLUMN_COUNT,
    Smoother, Spectrograph,
};

/// Default number of displayed Bands (settings offer 64/96).
const DEFAULT_BANDS: usize = 96;

/// Which canvas is active; `S` toggles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewMode {
    Spectrum,
    Spectrogram,
}
/// `ln(FREQ_MAX_HZ / FREQ_MIN_HZ)` = ln(1000) — the log-frequency axis span.
const LOG_SPAN: f32 = 6.907_755;

const FREQ_TICKS: &[f32] = &[
    63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
const DB_TICKS: &[f32] = &[0.0, -20.0, -40.0, -60.0];

/// Embedded at compile time so the binary has no runtime asset dependency.
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/music-spec-256.png");

const BG: Color32 = Color32::from_rgb(12, 14, 19);
const GRID: Color32 = Color32::from_rgb(36, 41, 52);
const AXIS_TEXT: Color32 = Color32::from_rgb(108, 114, 130);
const PEAK_COLOR: Color32 = Color32::from_rgba_unmultiplied_const(232, 238, 248, 210);
const STATUS_OK: Color32 = Color32::from_rgb(96, 200, 128);
const STATUS_WARN: Color32 = Color32::from_rgb(232, 180, 96);

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([920.0, 440.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "music-spec",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(SpectrumApp::new()))
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
    /// Performance HUD: EMA of the whole per-frame cost (ms).
    frame_cost_ms: f64,
    /// Performance HUD: EMA of the repaint interval (ms).
    frame_interval_ms: f64,
    last_frame_at: Option<f64>,
    /// Active canvas (S 键切换)。
    view: ViewMode,
    /// 冻结显示(Space 键切换):分析继续,画面与滚动停止。
    frozen: bool,
    always_on_top: bool,
    analyzer: Analyzer,
    smoother: Smoother,
    spectrograph: Spectrograph,
    spectro_texture: Option<TextureHandle>,
    /// 极简三件套设置:频段数 / Peak Hold / 衰减速度。
    band_count: usize,
    peak_hold: bool,
    decay: DecaySpeed,
    /// Latest per-Band dBFS from the analyzer; smoothing input.
    target: Vec<f32>,
    /// Latest smoothed display values.
    last_bands: Vec<f32>,
    last_peaks: Vec<f32>,
    /// Samples popped from the stream since the last analyzed frame.
    staging: Vec<f32>,
}

impl SpectrumApp {
    fn new() -> SpectrumApp {
        let system = CaptureSystem::new();
        // Design consensus: open Loopback (whatever the system is playing)
        // at startup; fall back to the default Input device.
        let source = system
            .default_source(SourceKind::Loopback)
            .or_else(|| system.default_source(SourceKind::Input));
        let status = match &source {
            Some(s) => format!("打开 {}…", s.name),
            None => "未找到可用声源,请手动选择".to_string(),
        };
        let band_count = DEFAULT_BANDS;
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
            view: ViewMode::Spectrum,
            frozen: false,
            always_on_top: false,
            analyzer: Analyzer::new(band_count),
            smoother: Smoother::new(band_count, true),
            spectrograph: Spectrograph::new(band_count),
            spectro_texture: None,
            band_count,
            peak_hold: true,
            decay: DecaySpeed::Medium,
            target: vec![DB_FLOOR; band_count],
            last_bands: vec![DB_FLOOR; band_count],
            last_peaks: vec![DB_FLOOR; band_count],
            staging: Vec::new(),
        }
    }

    /// 重建分析管线(频段数变化后);显示状态清零重来。
    fn rebuild_analysis(&mut self) {
        self.analyzer = Analyzer::new(self.band_count);
        self.smoother = Smoother::new(self.band_count, self.peak_hold);
        self.smoother.set_decay(self.decay);
        self.spectrograph = Spectrograph::new(self.band_count);
        self.spectro_texture = None; // 纹理尺寸随 Band 数变化,重建
        self.target = vec![DB_FLOOR; self.band_count];
        self.last_bands = vec![DB_FLOOR; self.band_count];
        self.last_peaks = vec![DB_FLOOR; self.band_count];
    }

    /// Source lifecycle: detect dead streams, schedule and perform Reconnects.
    fn tick(&mut self, now: f64) {
        if self.stream.as_ref().is_some_and(|s| s.is_dead()) {
            self.stream = None;
            self.staging.clear();
            self.status = "设备断开，正在重连…".to_string();
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
                            self.status = format!("设备已断开，切换到 {}", next.name);
                            self.source = Some(next);
                        } else {
                            self.status = "设备无响应，继续重试…".to_string();
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

    /// The user picked a new Source in the combo box.
    fn switch_source(&mut self) {
        self.stream = None;
        self.reconnect_at = None;
        self.staging.clear();
        self.status = "打开声源…".to_string();
    }
}

impl eframe::App for SpectrumApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 常驻重绘(设计共识):spectrum animation never sleeps.
        ui.ctx().request_repaint();
        let frame_start = std::time::Instant::now();

        let now = ui.input(|i| i.time);

        // 键盘:S 切换 频谱/语谱,Space 冻结/解冻(设计共识)。
        ui.input(|i| {
            if i.key_pressed(egui::Key::S) {
                self.view = match self.view {
                    ViewMode::Spectrum => ViewMode::Spectrogram,
                    ViewMode::Spectrogram => ViewMode::Spectrum,
                };
            }
            if i.key_pressed(egui::Key::Space) {
                self.frozen = !self.frozen;
            }
        });

        self.tick(now);
        self.pump_audio();
        // 冻结时分析继续(环缓冲不积压),但显示与语谱滚动停住。
        if !self.frozen {
            self.spectrograph.push_frame(now, &self.target);
            let smoothed = self.smoother.process(&self.target);
            self.last_bands = smoothed.bands;
            self.last_peaks = smoothed.peaks;
        }

        let loop_note = self.system.loopback_note().to_owned();
        let status = self.status.clone();
        let running = self.stream.is_some();
        let loopback_devices = self.loopback_devices.clone();
        let input_devices = self.input_devices.clone();

        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("music-spec");
                ui.separator();

                let selected = self
                    .source
                    .as_ref()
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| "选择声源…".to_string());
                egui::ComboBox::from_id_salt("source")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        ui.weak("Loopback(系统回环)");
                        if loopback_devices.is_empty() {
                            ui.weak(format!("    {loop_note}"));
                        }
                        for info in &loopback_devices {
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
                        ui.weak("Input(麦克风/线路输入)");
                        for info in &input_devices {
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
                    });

                // 视图切换(设计共识:频谱 | 语谱 分段按钮 + S 键)。
                ui.separator();
                ui.selectable_value(&mut self.view, ViewMode::Spectrum, "频谱");
                ui.selectable_value(&mut self.view, ViewMode::Spectrogram, "语谱");

                if ui.toggle_value(&mut self.always_on_top, "置顶").changed() {
                    let level = if self.always_on_top {
                        egui::WindowLevel::AlwaysOnTop
                    } else {
                        egui::WindowLevel::Normal
                    };
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                }

                // 极简三件套设置(设计共识)。
                ui.menu_button("⚙", |ui| {
                    ui.label("频段数");
                    ui.horizontal(|ui| {
                        for n in [64usize, 96] {
                            if ui
                                .selectable_value(&mut self.band_count, n, format!("{n}"))
                                .changed()
                            {
                                self.rebuild_analysis();
                            }
                        }
                    });
                    ui.separator();
                    if ui
                        .checkbox(&mut self.peak_hold, "Peak Hold 峰值保持")
                        .changed()
                    {
                        self.smoother.set_peak_hold(self.peak_hold);
                    }
                    ui.separator();
                    ui.label("衰减速度");
                    egui::ComboBox::from_id_salt("decay")
                        .selected_text(decay_name(self.decay))
                        .show_ui(ui, |ui| {
                            for d in [DecaySpeed::Slow, DecaySpeed::Medium, DecaySpeed::Fast] {
                                if ui
                                    .selectable_value(&mut self.decay, d, decay_name(d))
                                    .changed()
                                {
                                    self.smoother.set_decay(d);
                                }
                            }
                        });
                });

                if self.frozen {
                    ui.colored_label(STATUS_WARN, "⏸ 已冻结");
                    ui.separator();
                }
                if running {
                    ui.colored_label(STATUS_OK, status);
                } else {
                    ui.colored_label(STATUS_WARN, status);
                }

                // Performance HUD: live evidence for the design budget
                // (60 fps, < 5% single-core CPU; see README).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!(
                        "{:.2}ms/帧 · {:.0}fps · ≈{:.1}% CPU",
                        self.frame_cost_ms,
                        1000.0 / self.frame_interval_ms.max(0.001),
                        100.0 * self.frame_cost_ms / self.frame_interval_ms
                    ));
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let plot = self.plot_rect(ui);
            match self.view {
                ViewMode::Spectrum => self.draw_spectrum(ui, plot),
                ViewMode::Spectrogram => self.draw_spectrogram(ui, plot),
            }
            self.draw_hover(ui, plot);
        });

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

impl SpectrumApp {
    /// 中央画布的绘图区(留出坐标轴边距)。
    fn plot_rect(&self, ui: &egui::Ui) -> Rect {
        let full = ui.available_rect_before_wrap();
        Rect::from_min_max(
            Pos2::new(full.left() + 10.0, full.top() + 8.0),
            Pos2::new(full.right() - 10.0, full.bottom() - 20.0),
        )
    }

    fn draw_spectrum(&self, ui: &mut egui::Ui, plot: Rect) {
        let painter = ui.painter_at(ui.available_rect_before_wrap());
        painter.rect_filled(plot, 2.0, BG);

        let (l, r) = (plot.left(), plot.right());
        let (top, bottom) = (plot.top(), plot.bottom());
        let (w, h) = (r - l, bottom - top);

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

        for i in 0..self.analyzer.num_bands() {
            let (f_lo, f_hi) = self.analyzer.band_edges(i);
            let x0 = freq_to_x(f_lo, l, w) + 0.75;
            let x1 = freq_to_x(f_hi, l, w) - 0.75;
            if x1 - x0 < 0.5 {
                continue;
            }
            let bar_top = db_to_y(self.last_bands[i], top, h);
            if bottom - bar_top > 0.5 {
                // Vertical gradient (blue base → red top) as a small mesh.
                let mut mesh = Mesh::default();
                const SEGMENTS: usize = 12;
                for s in 0..=SEGMENTS {
                    let t = s as f32 / SEGMENTS as f32;
                    let y = bottom + (bar_top - bottom) * t;
                    let color = band_color(t);
                    mesh.colored_vertex(Pos2::new(x0, y), color);
                    mesh.colored_vertex(Pos2::new(x1, y), color);
                    if s > 0 {
                        let n = mesh.vertices.len() as u32;
                        mesh.add_triangle(n - 4, n - 3, n - 2);
                        mesh.add_triangle(n - 3, n - 1, n - 2);
                    }
                }
                painter.add(Shape::mesh(mesh));
            }
            let peak = self.last_peaks[i];
            if peak > DB_FLOOR + 0.5 {
                let py = db_to_y(peak, top, h);
                painter.line_segment(
                    [Pos2::new(x0, py), Pos2::new(x1, py)],
                    Stroke::new(1.5, PEAK_COLOR),
                );
            }
        }
    }

    /// 语谱视图:历史 → 纹理(左老右新,底低频顶高频),单个 quad 绘制,
    /// 使 60fps 预算在 600×96 网格规模下依然成立(见 HUD)。
    fn draw_spectrogram(&mut self, ui: &mut egui::Ui, plot: Rect) {
        let painter = ui.painter_at(ui.available_rect_before_wrap());
        painter.rect_filled(plot, 2.0, BG);

        let image = spectro_image(&self.spectrograph, self.band_count);
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
        draw_freq_grid(&painter, plot);
    }

    /// 悬停读数(设计共识:光标跟随):该处的频率与 dB。
    fn draw_hover(&self, ui: &egui::Ui, plot: Rect) {
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if !plot.contains(pos) {
            return;
        }
        let bands = self.band_count;
        let band = (((plot.bottom() - pos.y) / plot.height()) * bands as f32).floor() as usize;
        let band = band.min(bands - 1);
        let freq = self.analyzer.band_center(band);
        let freq_txt = if freq >= 1000.0 {
            format!("{:.1}kHz", freq / 1000.0)
        } else {
            format!("{freq:.0}Hz")
        };
        let value = match self.view {
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

/// 历史 → 纹理:x=时间(左老右新),y=Band(纹理行 0=顶部=高频)。
/// 提取为纯函数以支持无 GPU 环境下的行为测试。
fn spectro_image(sg: &Spectrograph, bands: usize) -> egui::ColorImage {
    // ColorImage 是行主序(y*宽+x):外层=Band 行(顶部=高频),内层=时间列。
    let mut pixels = Vec::with_capacity(SPECTRO_COLUMN_COUNT * bands);
    for band in (0..bands).rev() {
        for x in 0..SPECTRO_COLUMN_COUNT {
            let age = SPECTRO_COLUMN_COUNT - 1 - x;
            let db = sg.column(age).map(|c| c[band]).unwrap_or(DB_FLOOR);
            pixels.push(spectro_color(db));
        }
    }
    egui::ColorImage {
        size: [SPECTRO_COLUMN_COUNT, bands],
        source_size: egui::vec2(SPECTRO_COLUMN_COUNT as f32, bands as f32),
        pixels,
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

/// 语谱像素色:强度 t 混合背景色与频谱柱渐变色——静音 ≈ 背景,与
/// Spectrum 同色系(设计共识)。
fn spectro_color(db: f32) -> Color32 {
    let t = ((db - DB_FLOOR) / (DB_CEILING - DB_FLOOR)).clamp(0.0, 1.0);
    let hot = band_color(t);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(
        mix(BG.r(), hot.r()),
        mix(BG.g(), hot.g()),
        mix(BG.b(), hot.b()),
    )
}

fn decay_name(d: DecaySpeed) -> &'static str {
    match d {
        DecaySpeed::Slow => "慢",
        DecaySpeed::Medium => "中",
        DecaySpeed::Fast => "快",
    }
}

/// Bar color by height fraction: blue at the base, red at the top.
fn band_color(t: f32) -> Color32 {
    let hue = 0.68 * (1.0 - t);
    let (r, g, b) = hsv_to_rgb(hue, 0.85, 1.0);
    Color32::from_rgb(r, g, b)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let sector = (h * 6.0).floor();
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (sector as i32) % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// egui ships no CJK glyphs; install the platform's system CJK font so the
/// status/picker strings render.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 响亮事件必须落在纹理的「最新列」上,静默历史保持背景色。
    /// 这是语谱渲染数据链(历史→像素→ColorImage)的无 GPU 验证。
    #[test]
    fn spectro_image_lights_the_newest_column() {
        let bands = 96;
        let mut sg = Spectrograph::new(bands);
        // 静默一段,再响一段(满幅),使历史出现对比。
        sg.push_frame(0.0, &vec![DB_FLOOR; bands]);
        sg.push_frame(0.05, &vec![0.0; bands]);
        sg.push_frame(0.10, &vec![0.0; bands]);
        let image = spectro_image(&sg, bands);

        assert_eq!(image.size, [SPECTRO_COLUMN_COUNT, bands]);
        assert_eq!(image.pixels.len(), SPECTRO_COLUMN_COUNT * bands);

        // 右上角:x=最新列,y=0=最高频 Band(行主序)。
        let newest_top = image.pixels[SPECTRO_COLUMN_COUNT - 1];
        assert_ne!(newest_top, spectro_color(DB_FLOOR), "最新列应有能量");
        // 最老列(左边缘)仍为背景。
        let oldest_top = image.pixels[0];
        assert_eq!(oldest_top, spectro_color(DB_FLOOR), "静默历史应保持背景色");
    }
}
