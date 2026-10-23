#![forbid(unsafe_code)]
//! music-spec-app — the egui shell of music-spec: Source picking, Reconnect,
//! and the Spectrum canvas (log-frequency axis, gradient Bands, Peak Hold).
//!
//! Threading model (ADR-0002): the audio callback writes the ring buffer
//! inside `music-spec-capture`; this UI thread pops samples, runs the FFT and
//! paints, at a constant repaint rate.

use eframe::egui;
use egui::{Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke};
use music_spec_capture::{CaptureError, CaptureStream, CaptureSystem, DeviceInfo, SourceKind};
use music_spec_core::{Analyzer, DB_CEILING, DB_FLOOR, FRAME_SIZE, FREQ_MIN_HZ, Smoother};

/// Number of displayed Bands (design consensus: 64–96).
const NUM_BANDS: usize = 96;
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
    analyzer: Analyzer,
    smoother: Smoother,
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
            analyzer: Analyzer::new(NUM_BANDS),
            smoother: Smoother::new(NUM_BANDS, true),
            target: vec![DB_FLOOR; NUM_BANDS],
            last_bands: vec![DB_FLOOR; NUM_BANDS],
            last_peaks: vec![DB_FLOOR; NUM_BANDS],
            staging: Vec::new(),
        }
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
        self.tick(now);
        self.pump_audio();
        let smoothed = self.smoother.process(&self.target);
        self.last_bands = smoothed.bands;
        self.last_peaks = smoothed.peaks;

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
            self.draw_spectrum(ui);
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
    fn draw_spectrum(&self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let plot = Rect::from_min_max(
            Pos2::new(full.left() + 10.0, full.top() + 8.0),
            Pos2::new(full.right() - 10.0, full.bottom() - 20.0),
        );
        let painter = ui.painter_at(full);
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
        for &f in FREQ_TICKS {
            let x = freq_to_x(f, l, w);
            painter.line_segment(
                [Pos2::new(x, top), Pos2::new(x, bottom)],
                Stroke::new(1.0, Color32::from_rgb(30, 34, 43)),
            );
            let label = if f >= 1000.0 {
                format!("{}k", (f / 1000.0) as i32)
            } else {
                format!("{}", f as i32)
            };
            painter.text(
                Pos2::new(x, bottom + 3.0),
                Align2::CENTER_TOP,
                label,
                FontId::monospace(10.0),
                AXIS_TEXT,
            );
        }

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
