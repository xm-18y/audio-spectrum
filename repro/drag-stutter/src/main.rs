//! 最小复现:iced 0.14 + winit 0.30.13 在 macOS 26 上拖动标题栏时
//! 「起始瞬间卡顿、移动后消失」的对照工程。
//!
//! 帧循环与 audio-spectrum 完全同构:canvas 在每次 RedrawRequested 发布
//! `Message::Frame`,update 推进动画 → view 重建 → 重绘 → 下一帧,自维持。
//!
//! 窗口内实时显示帧间隔统计;尖峰(>40ms)同步打到 stderr,便于把窗口
//! 冻结时刻与日志对齐。注意:按下标题栏的瞬间应用收不到任何事件
//! (标题栏不在 winit 的 content view 里),起始卡顿表现为 RedrawRequested
//! 之间的长间隔,而非某个事件处理慢。
//!
//! 按键:
//! - Space  暂停/恢复帧循环(对照:静止窗口拖动是否同样卡;
//!   若暂停后拖动同样有起始顿挫,则与帧循环无关)
//! - 1/2    切换「纯色」/「重负载画布」(对照:单帧成本放大效应)
//! - ESC    退出

use iced::keyboard::{self, key::Named};
use iced::widget::{canvas, column, container, text};
use iced::{Color, Element, Point, Subscription, Task, Theme, window};

use std::time::Instant;

/// 超过此帧间隔视为尖峰(60fps 预算 16.7ms;给 2 帧余量)。
const SPIKE_MS: f32 = 40.0;

fn main() -> iced::Result {
    iced::application(State::default, update, view)
        .title("drag-stutter-repro")
        .theme(|_state: &State| Theme::Dark)
        .style(|_state: &State, _theme: &Theme| iced::theme::Style {
            background_color: Color::BLACK,
            text_color: Color::WHITE,
        })
        .subscription(subscription)
        .run()
}

#[derive(Debug, Clone)]
enum Message {
    /// 帧心跳(canvas 在每次重绘请求时发布,与主工程一致)。
    Frame,
    Key(keyboard::Event),
    /// 主窗口 Id(Opened 事件确定)与窗口事件。
    Window(Option<window::Id>, window::Event),
}

#[derive(Default)]
struct State {
    /// 帧循环开关(Space)。
    running: bool,
    /// 画布负载档位(1 = 纯色,2 = 重负载)。
    heavy: bool,
    /// 上一帧时刻。
    last: Option<Instant>,
    /// 帧间隔 EMA 与自暂停以来的最大值。
    ema_ms: f32,
    max_ms: f32,
    /// 尖峰计数与最近一次尖峰详情(窗口内展示)。
    spikes: u32,
    last_spike: Option<String>,
    /// 动画相位(证明帧循环活着;暂停时定格)。
    phase: f32,
    /// 主窗口 Id(Opened 事件确定;ESC 关窗用)。
    main_window: Option<window::Id>,
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Frame => {
            let now = Instant::now();
            if let Some(prev) = state.last {
                let ms = (now - prev).as_secs_f32() * 1000.0;
                state.ema_ms += 0.1 * (ms - state.ema_ms);
                if ms > state.max_ms {
                    state.max_ms = ms;
                }
                if ms > SPIKE_MS {
                    state.spikes += 1;
                    state.last_spike = Some(format!("{ms:.1}ms"));
                    eprintln!("[spike] {ms:.1}ms (max {:.1}ms, #{})", state.max_ms, state.spikes);
                }
            }
            state.last = Some(now);
            if state.running {
                state.phase += 0.02;
            }
            Task::none()
        }
        Message::Key(event) => {
            let keyboard::Event::KeyPressed { key, repeat: false, .. } = event else {
                return Task::none();
            };
            match key {
                keyboard::Key::Named(Named::Space) => {
                    state.running = !state.running;
                    state.max_ms = 0.0;
                    state.spikes = 0;
                    state.last = None;
                    eprintln!("[loop] {}", if state.running { "resumed" } else { "paused" });
                    Task::none()
                }
                keyboard::Key::Character(c) if c == "1" => {
                    state.heavy = false;
                    Task::none()
                }
                keyboard::Key::Character(c) if c == "2" => {
                    state.heavy = true;
                    Task::none()
                }
                keyboard::Key::Named(Named::Escape) => match state.main_window {
                    Some(id) => window::close(id),
                    None => Task::none(),
                },
                _ => Task::none(),
            }
        }
        Message::Window(id, event) => {
            if state.main_window.is_none()
                && matches!(event, window::Event::Opened { .. })
            {
                state.main_window = id;
            }
            Task::none()
        }
    }
}

fn subscription(_state: &State) -> Subscription<Message> {
    Subscription::batch([
        iced::keyboard::listen().map(Message::Key),
        window::events().map(|(id, event)| match event {
            window::Event::RedrawRequested(_) => Message::Frame,
            other => Message::Window(Some(id), other),
        }),
    ])
}

fn view(state: &State) -> Element<'_, Message> {
    let stats = format!(
        "ema {:>5.1}ms  max {:>6.1}ms  spikes {}\nmode: {}  loop: {}",
        state.ema_ms,
        state.max_ms,
        state.spikes,
        if state.heavy { "heavy canvas" } else { "flat fill" },
        if state.running { "running" } else { "paused" },
    );
    column![
        canvas(Plot { state }).width(iced::Length::Fill).height(iced::Length::Fill),
        container(text(stats).size(14.0).font(iced::Font::MONOSPACE))
            .padding(10.0),
    ]
    .width(iced::Length::Fill)
    .height(iced::Length::Fill)
    .into()
}

/// 与主工程同构的自维持帧循环画布。
struct Plot<'a> {
    state: &'a State,
}

impl canvas::Program<Message> for Plot<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &canvas::Event,
        _bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            // 常驻重绘:仅帧循环开启时自维持。
            canvas::Event::Window(window::Event::RedrawRequested(_)) => {
                self.state.running.then_some(canvas::Action::publish(Message::Frame))
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        if !self.state.heavy {
            frame.fill_rectangle(Point::ORIGIN, bounds.size(), Color::from_rgb8(24, 28, 36));
        } else {
            // 重负载:数千条线段,单帧成本接近主工程的频谱画布。
            frame.fill_rectangle(Point::ORIGIN, bounds.size(), Color::BLACK);
            let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
            let radius = bounds.width.min(bounds.height) * 0.45;
            for i in 0..1500 {
                let a = self.state.phase + i as f32 * 0.0042;
                let r = radius * (0.3 + 0.7 * (a * 0.5).sin().abs());
                frame.stroke(
                    &canvas::Path::line(center, Point::new(center.x + a.cos() * r, center.y + a.sin() * r)),
                    canvas::Stroke::default().with_color(Color::from_rgb8(60, 140, 220)).with_width(1.2),
                );
            }
        }
        vec![frame.into_geometry()]
    }
}
