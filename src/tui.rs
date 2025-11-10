use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::GraphType;
use ratatui::widgets::{Axis, Block, Chart, Dataset, Paragraph};
use tokio::sync::broadcast;

use crate::command::Command;

const COLOR_PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Green,
    Color::LightBlue,
    Color::Red,
    Color::LightMagenta,
    Color::LightCyan,
];
const EMPTY_SERIES: &[(f64, f64)] = &[];

#[derive(Clone, Debug)]
struct AxisInfo {
    inst_id: String,
    color: Color,
    min: f64,
    mid: f64,
    max: f64,
}

pub struct TuiApp {
    inst_ids: Vec<String>,
    colors: HashMap<String, Color>,
    data: HashMap<String, Vec<(f64, f64)>>,
    window: [f64; 2],
    last_draw: Instant,
    min_redraw_gap: Duration,
    retention: Duration,
    latest_prices: HashMap<String, f64>,
    price_precision: HashMap<String, usize>,
    last_update: Option<String>,
    status_message: Option<String>,
    normalize: bool,
    y_zoom: f64,
    multi_axis: bool,
}
impl TuiApp {
    pub fn new(inst_ids: &[String]) -> TuiApp {
        let min_redraw_gap = Duration::from_millis(100);
        let retention = Duration::from_secs(5 * 60);
        let inst_ids = if inst_ids.is_empty() {
            vec!["BTC-USDT-SWAP".to_string()]
        } else {
            inst_ids.to_vec()
        };
        let mut data = HashMap::new();
        let mut colors = HashMap::new();
        for (idx, inst_id) in inst_ids.iter().enumerate() {
            data.insert(inst_id.clone(), Vec::new());
            colors.insert(inst_id.clone(), COLOR_PALETTE[idx % COLOR_PALETTE.len()]);
        }
        TuiApp {
            inst_ids,
            colors,
            data,
            window: [0.0, 100.0],
            last_draw: Instant::now() - min_redraw_gap,
            min_redraw_gap,
            retention,
            latest_prices: HashMap::new(),
            price_precision: HashMap::new(),
            last_update: None,
            status_message: None,
            normalize: false,
            y_zoom: 1.0,
            multi_axis: false,
        }
    }
    pub fn dispose(&self) {
        ratatui::restore();
    }

    pub async fn run(&mut self, rx: &mut broadcast::Receiver<Command>) -> Result<()> {
        color_eyre::install()?;
        let mut terminal = ratatui::init();
        let mut input_tick = tokio::time::interval(self.min_redraw_gap);
        loop {
            tokio::select! {
                biased;
                _ = input_tick.tick() => {
                    if self.poll_input()? {
                        return Ok(());
                    }
                }
                result = rx.recv() => {
                    match result {
                        Ok(Command::MarkPriceUpdate(inst_id, mark_px, ts, precision)) => {
                            self.status_message = None;
                            self.on_tick(&inst_id, mark_px, ts, precision);
                            if self.last_draw.elapsed() >= self.min_redraw_gap {
                                terminal.draw(|frame| self.render(frame))?;
                                self.last_draw = Instant::now();
                            }
                        }
                        Ok(Command::Error(message)) => {
                            self.status_message = Some(message);
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Ok(Command::Exit) => {
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        }
        Ok(())
    }
    fn on_tick(&mut self, inst_id: &str, mark_px: f64, ts: i64, precision: usize) {
        if !self.inst_ids.iter().any(|id| id == inst_id) {
            self.inst_ids.push(inst_id.to_string());
        }
        if !self.colors.contains_key(inst_id) {
            let idx = self.colors.len();
            self.colors.insert(
                inst_id.to_string(),
                COLOR_PALETTE[idx % COLOR_PALETTE.len()],
            );
        }
        let x = ts as f64;
        let retention_ms = self.retention.as_millis() as i64;
        let cutoff = (ts - retention_ms).max(0) as f64;
        let series = self
            .data
            .entry(inst_id.to_string())
            .or_insert_with(Vec::new);
        series.push((x, mark_px));
        series.retain(|(timestamp, _)| *timestamp >= cutoff);
        self.latest_prices.insert(inst_id.to_string(), mark_px);
        self.update_precision(inst_id, precision);
        self.last_update = Some(inst_id.to_string());
        self.update_window();
    }
    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if self.status_message.is_some() && area.height >= 4 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(3)])
                .split(area);
            self.render_chart(frame, chunks[0]);
            self.render_status(frame, chunks[1]);
        } else {
            self.render_chart(frame, area);
            if self.status_message.is_some() {
                self.render_status(frame, area);
            }
        }
    }
    fn render_chart(&self, frame: &mut Frame, area: Rect) {
        let multi_axis_active = self.multi_axis_active();
        let (chart_area, axis_area) = if multi_axis_active && area.width > 20 {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(18)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        let x_mid = f64::midpoint(self.window[0], self.window[1]);
        let x_labels = vec![
            Span::styled(
                Self::format_timestamp_label(self.window[0]),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(Self::format_timestamp_label(x_mid)),
            Span::styled(
                Self::format_timestamp_label(self.window[1]),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let mut views: Vec<(&str, Cow<'_, [(f64, f64)]>, Color)> = Vec::new();
        let mut axis_infos: Vec<AxisInfo> = Vec::new();
        let mut raw_min_y = f64::INFINITY;
        let mut raw_max_y = f64::NEG_INFINITY;
        for inst_id in &self.inst_ids {
            let color = self.color_for(inst_id);
            let source = self
                .data
                .get(inst_id)
                .map(|series| series.as_slice())
                .unwrap_or(EMPTY_SERIES);
            let (points, axis_info) = self.series_view(inst_id, source, multi_axis_active, color);
            if let Some(info) = axis_info {
                axis_infos.push(info);
            }
            for (_, y) in points.iter() {
                if y.is_finite() {
                    raw_min_y = raw_min_y.min(*y);
                    raw_max_y = raw_max_y.max(*y);
                }
            }
            views.push((inst_id.as_str(), points, color));
        }
        if views.is_empty() {
            views.push(("N/A", Cow::Borrowed(EMPTY_SERIES), Color::White));
        }
        let (label_min_y, label_max_y, bounds_min_y, bounds_max_y) =
            if !raw_min_y.is_finite() || !raw_max_y.is_finite() {
                (0.0, 1.0, 0.0, 1.0)
            } else if (raw_max_y - raw_min_y).abs() < f64::EPSILON {
                let padding = (raw_max_y.abs() * 0.05).max(1.0);
                (
                    raw_min_y,
                    raw_max_y,
                    raw_min_y - padding,
                    raw_max_y + padding,
                )
            } else {
                let padding = (raw_max_y - raw_min_y) * 0.05;
                (
                    raw_min_y,
                    raw_max_y,
                    raw_min_y - padding,
                    raw_max_y + padding,
                )
            };
        let y_mid = f64::midpoint(label_min_y, label_max_y);
        let y_labels = vec![
            Span::styled(
                self.format_axis_value(label_min_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.format_axis_value(y_mid)),
            Span::styled(
                self.format_axis_value(label_max_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let x_bounds = Self::normalize_bounds(self.window);
        let zoomed_bounds = self.apply_y_zoom(bounds_min_y, bounds_max_y);
        let y_bounds = Self::normalize_bounds(zoomed_bounds);
        let datasets: Vec<Dataset> = views
            .iter()
            .map(|(inst_id, points, color)| {
                Dataset::default()
                    .name(self.legend_label(inst_id))
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(*color))
                    .data(points.as_ref())
            })
            .collect();
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(self.chart_title_line()))
            .x_axis(
                Axis::default()
                    .title("Time")
                    .style(Style::default().fg(Color::Gray))
                    .labels(x_labels)
                    .labels_alignment(Alignment::Left)
                    .bounds(x_bounds),
            )
            .y_axis(
                Axis::default()
                    .title(self.axis_title())
                    .style(Style::default().fg(Color::Gray))
                    .labels(y_labels)
                    .bounds(y_bounds),
            );

        frame.render_widget(chart, chart_area);
        if let (Some(axis_area), true) = (axis_area, multi_axis_active) {
            self.render_multi_axis(frame, axis_area, &axis_infos);
        }
    }
    fn chart_title_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            self.chart_title_text(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        for badge in self.mode_badges() {
            spans.push(Span::raw(" "));
            spans.push(badge);
        }
        Line::from(spans)
    }

    fn chart_title_text(&self) -> String {
        let base = if self.normalize {
            "Mark Δ%"
        } else if self.multi_axis_active() {
            "Mark Price (Multi Y)"
        } else {
            "Mark Price"
        };
        if self.inst_ids.is_empty() {
            base.to_string()
        } else {
            format!("{base} [{}]", self.inst_ids.join(", "))
        }
    }

    fn mode_badges(&self) -> Vec<Span<'static>> {
        let mut badges = Vec::new();
        if self.normalize {
            badges.push(Span::styled(
                "[归一化]",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if self.multi_axis {
            let (label, style) = if self.multi_axis_active() {
                (
                    "[多 Y 轴]",
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("[多 Y 轴待用]", Style::default().fg(Color::DarkGray))
            };
            badges.push(Span::styled(label, style));
        }
        badges
    }

    fn axis_title(&self) -> String {
        if let Some(inst_id) = &self.last_update {
            if let Some(value) = self.latest_display_value(inst_id) {
                if self.normalize {
                    return format!("Δ% {} {}", inst_id, self.format_percent(value));
                } else if !self.multi_axis_active() {
                    return format!("{} {}", inst_id, self.format_price_for(inst_id, value));
                }
            }
        }
        if self.normalize {
            "Δ% (相对首个采样点)".to_string()
        } else if self.multi_axis_active() {
            "Scaled (0-1)".to_string()
        } else {
            "Mark Price".to_string()
        }
    }

    fn legend_label(&self, inst_id: &str) -> String {
        if let Some(value) = self.latest_display_value(inst_id) {
            format!("{} {}", inst_id, self.format_value(inst_id, value))
        } else {
            inst_id.to_string()
        }
    }

    fn color_for(&self, inst_id: &str) -> Color {
        self.colors.get(inst_id).copied().unwrap_or(Color::White)
    }

    fn update_window(&mut self) {
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        for series in self.data.values() {
            if let Some((first_x, _)) = series.first() {
                min_x = min_x.min(*first_x);
            }
            if let Some((last_x, _)) = series.last() {
                max_x = max_x.max(*last_x);
            }
        }
        if min_x.is_finite() && max_x.is_finite() {
            self.window = [min_x, max_x];
        } else {
            self.window = [0.0, 100.0];
        }
    }
    fn render_status(&self, frame: &mut Frame, area: Rect) {
        if let Some(message) = &self.status_message {
            let block = Block::bordered().title("Status");
            let status = Paragraph::new(message.as_str())
                .style(Style::default().fg(Color::Yellow))
                .alignment(Alignment::Left)
                .block(block);
            frame.render_widget(status, area);
        }
    }

    fn render_multi_axis(&self, frame: &mut Frame, area: Rect, infos: &[AxisInfo]) {
        if infos.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        for info in infos {
            lines.push(Line::from(vec![Span::styled(
                info.inst_id.as_str(),
                Style::default().fg(info.color).add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(format!(
                "↑ {}",
                self.format_price_for(&info.inst_id, info.max)
            )));
            lines.push(Line::from(format!(
                "• {}",
                self.format_price_for(&info.inst_id, info.mid)
            )));
            lines.push(Line::from(format!(
                "↓ {}",
                self.format_price_for(&info.inst_id, info.min)
            )));
            lines.push(Line::from(" "));
        }
        let block = Block::bordered().title("Y Axis");
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn update_precision(&mut self, inst_id: &str, precision: usize) {
        self.price_precision
            .entry(inst_id.to_string())
            .and_modify(|existing| {
                if (*existing == 0 && precision > 0) || precision > *existing {
                    *existing = precision;
                }
            })
            .or_insert(precision);
    }

    fn normalized_series(series: &[(f64, f64)]) -> Vec<(f64, f64)> {
        if series.is_empty() {
            return Vec::new();
        }
        let base = series[0].1;
        if base.abs() < f64::EPSILON {
            return Vec::new();
        }
        series
            .iter()
            .map(|(x, y)| (*x, ((*y / base) - 1.0) * 100.0))
            .collect()
    }

    fn scaled_series(series: &[(f64, f64)], min: f64, max: f64) -> Vec<(f64, f64)> {
        if series.is_empty() {
            return Vec::new();
        }
        let range = max - min;
        if range.abs() < f64::EPSILON {
            return series.iter().map(|(x, _)| (*x, 0.5)).collect();
        }
        series
            .iter()
            .map(|(x, y)| (*x, (y - min) / range))
            .collect()
    }

    fn series_bounds(series: &[(f64, f64)]) -> Option<(f64, f64)> {
        series.iter().fold(None, |acc, (_, y)| {
            if !y.is_finite() {
                acc
            } else {
                Some(match acc {
                    Some((min, max)) => (min.min(*y), max.max(*y)),
                    None => (*y, *y),
                })
            }
        })
    }

    fn series_view<'a>(
        &self,
        inst_id: &str,
        source: &'a [(f64, f64)],
        multi_axis_active: bool,
        color: Color,
    ) -> (Cow<'a, [(f64, f64)]>, Option<AxisInfo>) {
        if source.is_empty() {
            return (Cow::Borrowed(source), None);
        }
        if self.normalize {
            return (Cow::Owned(Self::normalized_series(source)), None);
        }
        if multi_axis_active {
            if let Some((min, max)) = Self::series_bounds(source) {
                let scaled = Self::scaled_series(source, min, max);
                let info = AxisInfo {
                    inst_id: inst_id.to_string(),
                    color,
                    min,
                    mid: (min + max) / 2.0,
                    max,
                };
                return (Cow::Owned(scaled), Some(info));
            }
        }
        (Cow::Borrowed(source), None)
    }

    fn latest_display_value(&self, inst_id: &str) -> Option<f64> {
        if self.normalize {
            self.normalized_latest_value(inst_id)
        } else {
            self.latest_prices.get(inst_id).copied()
        }
    }

    fn normalized_latest_value(&self, inst_id: &str) -> Option<f64> {
        let series = self.data.get(inst_id)?;
        let (_, first) = series.first()?;
        let (_, last) = series.last()?;
        if first.abs() < f64::EPSILON {
            None
        } else {
            Some(((last / first) - 1.0) * 100.0)
        }
    }

    fn price_precision_for(&self, inst_id: &str) -> usize {
        self.price_precision
            .get(inst_id)
            .copied()
            .unwrap_or_else(|| self.price_precision())
    }

    fn format_price_for(&self, inst_id: &str, value: f64) -> String {
        let precision = self.price_precision_for(inst_id);
        format!("{value:.prec$}", value = value, prec = precision)
    }

    fn format_value(&self, inst_id: &str, value: f64) -> String {
        if self.normalize {
            self.format_percent(value)
        } else {
            self.format_price_for(inst_id, value)
        }
    }

    fn format_axis_value(&self, value: f64) -> String {
        if self.normalize {
            self.format_percent(value)
        } else if self.multi_axis_active() {
            format!("{value:.2}")
        } else {
            self.format_axis_price(value)
        }
    }

    fn format_percent(&self, value: f64) -> String {
        format!("{value:+.2}%", value = value)
    }

    fn apply_y_zoom(&self, min: f64, max: f64) -> [f64; 2] {
        if !min.is_finite() || !max.is_finite() {
            return [min, max];
        }
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        if (max - min).abs() < f64::EPSILON {
            return [min - 1.0, max + 1.0];
        }
        let center = (min + max) / 2.0;
        let half_range = (max - min) / 2.0;
        let zoom = self.y_zoom.clamp(0.05, 100.0);
        let adjusted_half = (half_range / zoom).max(half_range * 0.01);
        [center - adjusted_half, center + adjusted_half]
    }

    fn multi_axis_active(&self) -> bool {
        self.multi_axis && !self.normalize
    }

    fn poll_input(&mut self) -> Result<bool> {
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        return Ok(true);
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.normalize = !self.normalize;
                        self.y_zoom = 1.0;
                        self.status_message = Some(match (self.normalize, self.multi_axis) {
                            (true, true) => {
                                "已切换到相对涨跌幅模式，多 Y 轴将在退出后恢复 (N)".to_string()
                            }
                            (true, false) => "已切换到相对涨跌幅模式 (N)".to_string(),
                            (false, true) => "已切换到绝对价格模式，多 Y 轴已恢复 (N)".to_string(),
                            (false, false) => "已切换到绝对价格模式 (N)".to_string(),
                        });
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        self.multi_axis = !self.multi_axis;
                        self.y_zoom = 1.0;
                        self.status_message = Some(match (self.multi_axis, self.normalize) {
                            (true, true) => {
                                "已预开启多 Y 轴模式，退出相对涨跌幅模式后即刻生效 (M)".to_string()
                            }
                            (true, false) => "已开启多 Y 轴模式 (M)".to_string(),
                            (false, true) => {
                                "已关闭多 Y 轴模式，当前仍处于相对涨跌幅模式 (M)".to_string()
                            }
                            (false, false) => "已关闭多 Y 轴模式 (M)".to_string(),
                        });
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        self.y_zoom = (self.y_zoom * 1.25).min(100.0);
                        self.status_message =
                            Some(format!("放大纵坐标 (Zoom {:.2}x)", self.y_zoom));
                    }
                    KeyCode::Char('-') => {
                        self.y_zoom = (self.y_zoom / 1.25).max(0.05);
                        self.status_message =
                            Some(format!("缩小纵坐标 (Zoom {:.2}x)", self.y_zoom));
                    }
                    KeyCode::Char('0') => {
                        self.y_zoom = 1.0;
                        self.status_message = Some("已重置纵坐标 (0)".to_string());
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(true);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(false)
    }

    fn format_timestamp_label(ts_ms: f64) -> String {
        let rounded = ts_ms.round() as i64;
        if rounded <= 0 {
            return "--:--:--".to_string();
        }
        let secs = rounded / 1000;
        let nanos = ((rounded % 1000).abs() as u32) * 1_000_000;
        Local
            .timestamp_opt(secs, nanos)
            .single()
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "--:--:--".to_string())
    }

    fn normalize_bounds(bounds: [f64; 2]) -> [f64; 2] {
        let (min, max) = if bounds[0] <= bounds[1] {
            (bounds[0], bounds[1])
        } else {
            (bounds[1], bounds[0])
        };
        if (max - min).abs() < f64::EPSILON {
            [min, min + 1.0]
        } else {
            [min, max]
        }
    }

    fn price_precision(&self) -> usize {
        self.price_precision.values().copied().max().unwrap_or(2)
    }

    fn format_axis_price(&self, value: f64) -> String {
        let precision = self.price_precision();
        format!("{value:.prec$}", value = value, prec = precision)
    }
}
