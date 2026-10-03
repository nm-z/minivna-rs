use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, FontWeight, PathBuilder, Render, Rgba, Timer,
    Window, WindowBounds, WindowDecorations, WindowOptions, canvas, div, point, prelude::*, px,
    rgb, size,
};
use serde_json::Value;

const WINDOW_WIDTH: f32 = 1_360.0;
const WINDOW_HEIGHT: f32 = 860.0;
const Y_AXIS_WIDTH: f32 = 92.0;
const AXIS_TICKS: usize = 5;
const MAX_TRACE_POINTS: usize = 1_200;

#[derive(Clone, Copy, Debug)]
struct PlotPoint {
    x: f32,
    y: f32,
}

#[derive(Clone)]
struct GraphSnapshot {
    frequency_start_hz: u64,
    frequency_stop_hz: u64,
    loss: Arc<Vec<PlotPoint>>,
    phase: Arc<Vec<PlotPoint>>,
    resistance: Arc<Vec<PlotPoint>>,
    swr: Arc<Vec<PlotPoint>>,
    reactance: Arc<Vec<PlotPoint>>,
    impedance: Arc<Vec<PlotPoint>>,
    theta: Arc<Vec<PlotPoint>>,
    finished: bool,
    close_requested: bool,
}

struct GraphState {
    snapshot: GraphSnapshot,
    total_points: usize,
    sample_stride: usize,
}

#[derive(Clone)]
pub(crate) struct GraphTelemetry {
    state: Arc<Mutex<GraphState>>,
}

impl GraphTelemetry {
    pub(crate) fn new() -> Self {
        let empty = Arc::new(Vec::new());
        Self {
            state: Arc::new(Mutex::new(GraphState {
                snapshot: GraphSnapshot {
                    frequency_start_hz: 0,
                    frequency_stop_hz: 0,
                    loss: Arc::clone(&empty),
                    phase: Arc::clone(&empty),
                    resistance: Arc::clone(&empty),
                    swr: Arc::clone(&empty),
                    reactance: Arc::clone(&empty),
                    impedance: Arc::clone(&empty),
                    theta: empty,
                    finished: false,
                    close_requested: false,
                },
                total_points: 0,
                sample_stride: 1,
            })),
        }
    }

    pub(crate) fn observe(&self, value: &Value) {
        let Some(event) = value.get("event").and_then(Value::as_str) else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match event {
            "scan_started" => {
                let total_points = value_usize(&value["points"]).unwrap_or(0);
                state.total_points = total_points;
                state.sample_stride = total_points.div_ceil(MAX_TRACE_POINTS).max(1);
                state.snapshot.frequency_start_hz = value_u64(&value["start_hz"]).unwrap_or(0);
                state.snapshot.frequency_stop_hz = value_u64(&value["stop_hz"]).unwrap_or(0);
                state.snapshot.loss = Arc::new(Vec::new());
                state.snapshot.phase = Arc::new(Vec::new());
                state.snapshot.resistance = Arc::new(Vec::new());
                state.snapshot.swr = Arc::new(Vec::new());
                state.snapshot.reactance = Arc::new(Vec::new());
                state.snapshot.impedance = Arc::new(Vec::new());
                state.snapshot.theta = Arc::new(Vec::new());
                state.snapshot.finished = false;
                state.snapshot.close_requested = false;
            }
            "scan_sample" => {
                let point_number = value_usize(&value["point"]).unwrap_or(0);
                let total_points = value_usize(&value["total_points"])
                    .unwrap_or(state.total_points)
                    .max(1);
                let keep = point_number <= 1
                    || point_number == total_points
                    || point_number
                        .saturating_sub(1)
                        .is_multiple_of(state.sample_stride);
                if !keep {
                    return;
                }
                let start_hz = state.snapshot.frequency_start_hz;
                let stop_hz = state.snapshot.frequency_stop_hz;
                let frequency_hz = value_u64(&value["frequency_hz"]).unwrap_or(start_hz);
                let span_hz = stop_hz.saturating_sub(start_hz).max(1) as f64;
                let x = (frequency_hz.saturating_sub(start_hz) as f64 / span_hz) as f32;
                push_value(&mut state.snapshot.loss, x, &value["loss_db"]);
                push_value(&mut state.snapshot.phase, x, &value["phase_deg"]);
                push_value(&mut state.snapshot.resistance, x, &value["resistance_ohm"]);
                push_value(&mut state.snapshot.swr, x, &value["swr"]);
                push_value(&mut state.snapshot.reactance, x, &value["reactance_ohm"]);
                push_value(&mut state.snapshot.impedance, x, &value["impedance_ohm"]);
                push_value(&mut state.snapshot.theta, x, &value["theta_deg"]);
            }
            "scan_completed" => state.snapshot.finished = true,
            "cancellation_requested" | "scan_cancelled" | "scan_failed" => {
                state.snapshot.finished = true;
                state.snapshot.close_requested = true;
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> GraphSnapshot {
        self.state
            .lock()
            .map(|state| state.snapshot.clone())
            .unwrap_or_else(|_| failed_snapshot())
    }

    fn finish_and_close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.snapshot.finished = true;
            state.snapshot.close_requested = true;
        }
    }
}

pub(crate) fn run<F>(telemetry: GraphTelemetry, interrupted: Arc<AtomicBool>, task: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let worker_telemetry = telemetry.clone();
    let worker_result = Arc::new(Mutex::new(None::<Result<()>>));
    let result_slot = Arc::clone(&worker_result);
    let worker = thread::Builder::new()
        .name("minivna-gui-scan".to_owned())
        .spawn(move || {
            let result = task();
            worker_telemetry.finish_and_close();
            if let Ok(mut slot) = result_slot.lock() {
                *slot = Some(result);
            }
        })
        .context("failed to start minivna GUI scan worker")?;

    let ui_error = Arc::new(Mutex::new(None::<String>));
    let app_telemetry = telemetry.clone();
    let app_interrupted = Arc::clone(&interrupted);
    let app_error = Arc::clone(&ui_error);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        let window_telemetry = app_telemetry.clone();
        let window_interrupted = Arc::clone(&app_interrupted);
        let result = cx.open_window(
            WindowOptions {
                titlebar: None,
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            move |window, cx| {
                let close_interrupted = Arc::clone(&window_interrupted);
                window.on_window_should_close(cx, move |_, _| {
                    close_interrupted.store(true, Ordering::Relaxed);
                    true
                });
                cx.new(|cx| {
                    LiveGraph::new(
                        window_telemetry.clone(),
                        Arc::clone(&window_interrupted),
                        window,
                        cx,
                    )
                })
            },
        );
        if let Err(error) = result {
            app_interrupted.store(true, Ordering::Relaxed);
            if let Ok(mut slot) = app_error.lock() {
                *slot = Some(format!("failed to open minivna GUI window: {error:#}"));
            }
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });

    interrupted.store(true, Ordering::Relaxed);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("minivna GUI scan worker panicked"))?;
    if let Some(error) = ui_error.lock().ok().and_then(|slot| slot.clone()) {
        bail!(error);
    }
    worker_result
        .lock()
        .map_err(|_| anyhow::anyhow!("minivna GUI result lock poisoned"))?
        .take()
        .ok_or_else(|| anyhow::anyhow!("minivna GUI scan worker exited without a result"))?
}

struct LiveGraph {
    telemetry: GraphTelemetry,
    interrupted: Arc<AtomicBool>,
    focus: FocusHandle,
    visible: [bool; TraceKind::COUNT],
}

impl LiveGraph {
    fn new(
        telemetry: GraphTelemetry,
        interrupted: Arc<AtomicBool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        let key_interrupted = Arc::clone(&interrupted);
        cx.observe_keystrokes(move |_, event, _, cx| {
            if event.keystroke.key == "c" && event.keystroke.modifiers.control {
                key_interrupted.store(true, Ordering::Relaxed);
                cx.quit();
            }
        })
        .detach();
        cx.spawn(async move |view, cx| {
            loop {
                Timer::after(Duration::from_millis(50)).await;
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            telemetry,
            interrupted,
            focus,
            visible: [true; TraceKind::COUNT],
        }
    }
}

impl Render for LiveGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.telemetry.snapshot();
        if snapshot.close_requested || self.interrupted.load(Ordering::Relaxed) {
            window.remove_window();
        }
        let series = TraceKind::ALL
            .into_iter()
            .filter(|kind| self.visible[kind.index()])
            .map(|kind| PlotSeries {
                kind,
                points: kind.points(&snapshot),
            })
            .collect::<Vec<_>>();
        let scales = axis_scales(&series);
        let y_axis_width = Y_AXIS_WIDTH * scales.len() as f32;

        let mut legend = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_4()
            .h(px(34.0));
        for kind in TraceKind::ALL {
            let index = kind.index();
            let selected = self.visible[index];
            let checkbox = div()
                .w(px(11.0))
                .h(px(11.0))
                .border_1()
                .border_color(kind.color())
                .bg(if selected {
                    kind.color()
                } else {
                    rgb(0x000000)
                });
            legend = legend.child(
                div()
                    .id(("trace", index))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(kind.color())
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.72))
                    .child(checkbox)
                    .child(kind.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.visible[index] = !this.visible[index];
                        cx.notify();
                    })),
            );
        }

        let mut y_axes = div().w(px(y_axis_width)).h_full().flex();
        for scale in &scales {
            let mut labels = div()
                .w(px(Y_AXIS_WIDTH))
                .h_full()
                .flex()
                .flex_col()
                .items_end()
                .justify_between()
                .pr_2()
                .text_xs();
            for index in 0..AXIS_TICKS {
                let fraction = index as f32 / (AXIS_TICKS - 1) as f32;
                let value = scale.maximum - (scale.maximum - scale.minimum) * fraction;
                labels = labels.child(format_axis_value(value, scale.unit));
            }
            y_axes = y_axes.child(labels);
        }

        let plot_series = series.clone();
        let plot_scales = scales.clone();
        let plot = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                paint_plot(bounds, &plot_scales, &plot_series, window);
            },
        )
        .flex_1()
        .h_full();

        let frequency_axis =
            FrequencyAxis::for_range(snapshot.frequency_start_hz, snapshot.frequency_stop_hz);
        let mut x_labels = div().flex().flex_1().justify_between().pt_1().text_xs();
        for index in 0..AXIS_TICKS {
            let frequency = interpolate_frequency(
                snapshot.frequency_start_hz,
                snapshot.frequency_stop_hz,
                index,
            );
            x_labels = x_labels.child(frequency_axis.format(frequency));
        }

        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .p_3()
            .bg(rgb(0x000000))
            .text_color(rgb(0xffffff))
            .font_family("JetBrains Mono")
            .font_weight(FontWeight::SEMIBOLD)
            .child(legend)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(300.0))
                    .child(y_axes)
                    .child(plot),
            )
            .child(
                div()
                    .flex()
                    .child(div().w(px(y_axis_width)))
                    .child(x_labels),
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AxisUnit {
    Decibels,
    Degrees,
    Ohms,
    Swr,
}

impl AxisUnit {
    const ALL: [Self; 4] = [Self::Decibels, Self::Degrees, Self::Ohms, Self::Swr];
}

#[derive(Clone, Copy, Debug)]
struct AxisScale {
    unit: AxisUnit,
    minimum: f32,
    maximum: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
enum TraceKind {
    ReturnLoss,
    Phase,
    Resistance,
    Swr,
    Reactance,
    Impedance,
    Theta,
}

impl TraceKind {
    const COUNT: usize = 7;
    const ALL: [Self; Self::COUNT] = [
        Self::ReturnLoss,
        Self::Phase,
        Self::Resistance,
        Self::Swr,
        Self::Reactance,
        Self::Impedance,
        Self::Theta,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ReturnLoss => "Return Loss (dB)",
            Self::Phase => "Phase (°)",
            Self::Resistance => "Rs (Ω)",
            Self::Swr => "SWR",
            Self::Reactance => "Xs (Ω)",
            Self::Impedance => "|Z| (Ω)",
            Self::Theta => "Theta (°)",
        }
    }

    const fn axis_unit(self) -> AxisUnit {
        match self {
            Self::ReturnLoss => AxisUnit::Decibels,
            Self::Phase | Self::Theta => AxisUnit::Degrees,
            Self::Resistance | Self::Reactance | Self::Impedance => AxisUnit::Ohms,
            Self::Swr => AxisUnit::Swr,
        }
    }

    fn points(self, snapshot: &GraphSnapshot) -> Arc<Vec<PlotPoint>> {
        match self {
            Self::ReturnLoss => Arc::clone(&snapshot.loss),
            Self::Phase => Arc::clone(&snapshot.phase),
            Self::Resistance => Arc::clone(&snapshot.resistance),
            Self::Swr => Arc::clone(&snapshot.swr),
            Self::Reactance => Arc::clone(&snapshot.reactance),
            Self::Impedance => Arc::clone(&snapshot.impedance),
            Self::Theta => Arc::clone(&snapshot.theta),
        }
    }

    const fn color(self) -> Rgba {
        match self {
            Self::ReturnLoss => hex(0x00ae6b),
            Self::Phase => hex(0xf2283c),
            Self::Resistance => hex(0x277dff),
            Self::Swr => hex(0xffc200),
            Self::Reactance => hex(0xd72e82),
            Self::Impedance => hex(0x875afb),
            Self::Theta => hex(0xff7a00),
        }
    }
}

#[derive(Clone)]
struct PlotSeries {
    kind: TraceKind,
    points: Arc<Vec<PlotPoint>>,
}

fn paint_plot(
    bounds: Bounds<gpui::Pixels>,
    scales: &[AxisScale],
    series: &[PlotSeries],
    window: &mut Window,
) {
    let mut grid = PathBuilder::stroke(px(1.0)).dash_array(&[px(2.0), px(5.0)]);
    for index in 0..AXIS_TICKS {
        let fraction = index as f32 / (AXIS_TICKS - 1) as f32;
        let x = bounds.left() + bounds.size.width * fraction;
        let y = bounds.top() + bounds.size.height * fraction;
        grid.move_to(point(x, bounds.top()));
        grid.line_to(point(x, bounds.bottom()));
        grid.move_to(point(bounds.left(), y));
        grid.line_to(point(bounds.right(), y));
    }
    if let Ok(path) = grid.build() {
        window.paint_path(path, white(0.16));
    }

    let mut axes = PathBuilder::stroke(px(1.4));
    axes.move_to(point(bounds.left(), bounds.top()));
    axes.line_to(point(bounds.left(), bounds.bottom()));
    axes.line_to(point(bounds.right(), bounds.bottom()));
    if let Ok(path) = axes.build() {
        window.paint_path(path, rgb(0xffffff));
    }

    for series in series {
        let Some(scale) = scales
            .iter()
            .find(|scale| scale.unit == series.kind.axis_unit())
        else {
            continue;
        };
        let span = (scale.maximum - scale.minimum).max(f32::EPSILON);
        let mut path = PathBuilder::stroke(px(1.8));
        let mut started = false;
        for sample in series.points.iter() {
            if !sample.x.is_finite() || !sample.y.is_finite() {
                continue;
            }
            let x = bounds.left() + bounds.size.width * sample.x.clamp(0.0, 1.0);
            let normalized = ((sample.y - scale.minimum) / span).clamp(0.0, 1.0);
            let y = bounds.bottom() - bounds.size.height * normalized;
            if started {
                path.line_to(point(x, y));
            } else {
                path.move_to(point(x, y));
                started = true;
            }
        }
        if started && let Ok(path) = path.build() {
            window.paint_path(path, series.kind.color());
        }
    }
}

fn push_value(points: &mut Arc<Vec<PlotPoint>>, x: f32, value: &Value) {
    let Some(y) = value_f64(value).map(|value| value as f32) else {
        return;
    };
    if x.is_finite() && y.is_finite() {
        Arc::make_mut(points).push(PlotPoint { x, y });
    }
}

fn value_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn value_u64(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn value_usize(value: &Value) -> Option<usize> {
    usize::try_from(value_u64(value)?).ok()
}

fn axis_scales(series: &[PlotSeries]) -> Vec<AxisScale> {
    AxisUnit::ALL
        .into_iter()
        .filter(|unit| series.iter().any(|series| series.kind.axis_unit() == *unit))
        .map(|unit| {
            let (minimum, maximum) = data_range(series, unit);
            AxisScale {
                unit,
                minimum,
                maximum,
            }
        })
        .collect()
}

fn data_range(series: &[PlotSeries], unit: AxisUnit) -> (f32, f32) {
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for point in series
        .iter()
        .filter(|series| series.kind.axis_unit() == unit)
        .flat_map(|series| series.points.iter())
    {
        if point.y.is_finite() {
            minimum = minimum.min(point.y);
            maximum = maximum.max(point.y);
        }
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return (0.0, 1.0);
    }
    if (maximum - minimum).abs() < 1.0e-3 {
        return (minimum - 1.0, maximum + 1.0);
    }
    let padding = (maximum - minimum) * 0.06;
    (minimum - padding, maximum + padding)
}

fn format_number(value: f32) -> String {
    let absolute = value.abs();
    if absolute >= 100_000.0 || (absolute > 0.0 && absolute < 0.001) {
        format!("{value:.3e}")
    } else if absolute >= 1_000.0 {
        format!("{value:.0}")
    } else if absolute >= 100.0 {
        format!("{value:.1}")
    } else if absolute >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

fn format_axis_value(value: f32, unit: AxisUnit) -> String {
    let number = format_number(value);
    match unit {
        AxisUnit::Decibels => format!("{number} dB"),
        AxisUnit::Degrees => format!("{number}°"),
        AxisUnit::Ohms => format!("{number} Ω"),
        AxisUnit::Swr => format!("{number}:1"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrequencyUnit {
    Hertz,
    Kilohertz,
    Megahertz,
    Gigahertz,
}

impl FrequencyUnit {
    const fn for_frequency(frequency_hz: u64) -> Self {
        if frequency_hz >= 1_000_000_000 {
            Self::Gigahertz
        } else if frequency_hz >= 1_000_000 {
            Self::Megahertz
        } else if frequency_hz >= 1_000 {
            Self::Kilohertz
        } else {
            Self::Hertz
        }
    }

    const fn divisor(self) -> f64 {
        match self {
            Self::Hertz => 1.0,
            Self::Kilohertz => 1_000.0,
            Self::Megahertz => 1_000_000.0,
            Self::Gigahertz => 1_000_000_000.0,
        }
    }

    const fn suffix(self) -> &'static str {
        match self {
            Self::Hertz => "Hz",
            Self::Kilohertz => "kHz",
            Self::Megahertz => "MHz",
            Self::Gigahertz => "GHz",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FrequencyAxis {
    unit: FrequencyUnit,
    decimal_places: usize,
}

impl FrequencyAxis {
    fn for_range(start_hz: u64, stop_hz: u64) -> Self {
        let unit = FrequencyUnit::for_frequency(start_hz.max(stop_hz));
        let step = stop_hz.abs_diff(start_hz) as f64 / (AXIS_TICKS - 1) as f64 / unit.divisor();
        Self {
            unit,
            decimal_places: decimal_places_for_step(step),
        }
    }

    fn format(self, frequency_hz: u64) -> String {
        let scaled = frequency_hz as f64 / self.unit.divisor();
        let mut number = format!("{:.*}", self.decimal_places, scaled);
        if number.contains('.') {
            while number.ends_with('0') {
                number.pop();
            }
            if number.ends_with('.') {
                number.pop();
            }
        }
        format!("{number} {}", self.unit.suffix())
    }
}

fn decimal_places_for_step(step: f64) -> usize {
    if !step.is_finite() || step <= 0.0 {
        return 0;
    }
    (2 - step.log10().floor() as i32).clamp(0, 6) as usize
}

fn interpolate_frequency(start: u64, stop: u64, index: usize) -> u64 {
    let span = stop.saturating_sub(start) as u128;
    let offset = span * index as u128 / (AXIS_TICKS - 1) as u128;
    start.saturating_add(offset as u64)
}

fn failed_snapshot() -> GraphSnapshot {
    let empty = Arc::new(Vec::new());
    GraphSnapshot {
        frequency_start_hz: 0,
        frequency_stop_hz: 0,
        loss: Arc::clone(&empty),
        phase: Arc::clone(&empty),
        resistance: Arc::clone(&empty),
        swr: Arc::clone(&empty),
        reactance: Arc::clone(&empty),
        impedance: Arc::clone(&empty),
        theta: empty,
        finished: true,
        close_requested: true,
    }
}

const fn white(alpha: f32) -> Rgba {
    Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: alpha,
    }
}

const fn hex(value: u32) -> Rgba {
    Rgba {
        r: ((value >> 16) & 0xff) as f32 / 255.0,
        g: ((value >> 8) & 0xff) as f32 / 255.0,
        b: (value & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scan_events_feed_all_seven_traces() {
        let graph = GraphTelemetry::new();
        graph.observe(&json!({
            "event": "scan_started",
            "start_hz": 45_000_000,
            "stop_hz": 60_000_000,
            "points": 1200
        }));
        graph.observe(&json!({
            "event": "scan_sample",
            "point": 1,
            "total_points": 1200,
            "frequency_hz": 45_000_000,
            "loss_db": -12.0,
            "phase_deg": 4.0,
            "resistance_ohm": 50.0,
            "swr": 1.1,
            "reactance_ohm": 2.0,
            "impedance_ohm": 50.04,
            "theta_deg": 2.3
        }));
        let snapshot = graph.snapshot();
        assert_eq!(snapshot.loss.len(), 1);
        assert_eq!(snapshot.phase.len(), 1);
        assert_eq!(snapshot.resistance.len(), 1);
        assert_eq!(snapshot.swr.len(), 1);
        assert_eq!(snapshot.reactance.len(), 1);
        assert_eq!(snapshot.impedance.len(), 1);
        assert_eq!(snapshot.theta.len(), 1);
    }

    #[test]
    fn finished_gui_task_requests_window_close() {
        let graph = GraphTelemetry::new();
        graph.finish_and_close();

        let snapshot = graph.snapshot();
        assert!(snapshot.finished);
        assert!(snapshot.close_requested);
    }

    #[test]
    fn frequency_ticks_cover_the_scan() {
        assert_eq!(interpolate_frequency(1_000_000, 5_000_000, 0), 1_000_000);
        assert_eq!(interpolate_frequency(1_000_000, 5_000_000, 4), 5_000_000);
    }

    #[test]
    fn frequency_ticks_use_readable_si_units() {
        let mhz = FrequencyAxis::for_range(1_000_000, 20_000_000);
        assert_eq!(mhz.format(1_000_000), "1 MHz");
        assert_eq!(mhz.format(5_750_000), "5.75 MHz");
        assert_eq!(mhz.format(20_000_000), "20 MHz");

        let khz = FrequencyAxis::for_range(1_000, 20_000);
        assert_eq!(khz.format(5_750), "5.75 kHz");

        let hz = FrequencyAxis::for_range(100, 900);
        assert_eq!(hz.format(500), "500 Hz");

        let ghz = FrequencyAxis::for_range(2_400_000_000, 2_800_000_000);
        assert_eq!(ghz.format(2_500_000_000), "2.5 GHz");
    }

    #[test]
    fn enabled_trace_families_receive_separate_unit_scales() {
        let point = Arc::new(vec![PlotPoint { x: 0.0, y: 1.0 }]);
        let series = vec![
            PlotSeries {
                kind: TraceKind::ReturnLoss,
                points: Arc::clone(&point),
            },
            PlotSeries {
                kind: TraceKind::Phase,
                points: Arc::clone(&point),
            },
            PlotSeries {
                kind: TraceKind::Resistance,
                points: Arc::clone(&point),
            },
            PlotSeries {
                kind: TraceKind::Swr,
                points: point,
            },
        ];

        let units = axis_scales(&series)
            .into_iter()
            .map(|scale| scale.unit)
            .collect::<Vec<_>>();
        assert_eq!(
            units,
            vec![
                AxisUnit::Decibels,
                AxisUnit::Degrees,
                AxisUnit::Ohms,
                AxisUnit::Swr
            ]
        );
    }

    #[test]
    fn y_axis_values_include_the_measurement_unit() {
        assert_eq!(format_axis_value(-15.5, AxisUnit::Decibels), "-15.50 dB");
        assert_eq!(format_axis_value(118.33, AxisUnit::Degrees), "118.3°");
        assert_eq!(format_axis_value(50.0, AxisUnit::Ohms), "50.00 Ω");
        assert_eq!(format_axis_value(1.5, AxisUnit::Swr), "1.50:1");
    }
}
