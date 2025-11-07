use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::Span;
use ratatui::widgets::{Axis, Block, Chart, Dataset, Paragraph};
use tokio::sync::broadcast;

use crate::command::Command;

pub struct TuiApp {
    inst_id: String,
    data: Vec<(f64, f64)>,
    window: [f64; 2],
    last_draw: Instant,
    min_redraw_gap: Duration,
    retention: Duration,
    latest_price: f64,
    price_precision: Option<usize>,
    status_message: Option<String>,
}
impl TuiApp {
    pub fn new(inst_id: &str) -> TuiApp {
        let min_redraw_gap = Duration::from_millis(100);
        let retention = Duration::from_secs(5 * 60);
        TuiApp {
            inst_id: inst_id.to_string(),
            data: vec![],
            window: [0.0, 100.0],
            last_draw: Instant::now() - min_redraw_gap,
            min_redraw_gap,
            retention,
            latest_price: 0.0,
            price_precision: None,
            status_message: None,
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
                        Ok(Command::MarkPriceUpdate(_inst_id, mark_px, ts, precision)) => {
                            self.status_message = None;
                            self.on_tick(mark_px, ts, precision);
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
    fn on_tick(&mut self, mark_px: f64, ts: i64, precision: usize) {
        if self.price_precision.is_none() || (self.price_precision == Some(0) && precision > 0) {
            self.price_precision = Some(precision);
        }
        let x = ts as f64;
        let y = mark_px;
        self.latest_price = mark_px;
        self.data.push((x, y));
        let retention_ms = self.retention.as_millis() as i64;
        let cutoff = (ts - retention_ms).max(0) as f64;
        self.data.retain(|(timestamp, _)| *timestamp >= cutoff);
        if let Some((min_x, _)) = self.data.first() {
            if let Some((max_x, _)) = self.data.last() {
                self.window = [*min_x, *max_x];
            }
        } else {
            self.window = [0.0, 100.0];
        }
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
        let (raw_min_y, raw_max_y) = self
            .data
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), (_, y)| {
                (min.min(*y), max.max(*y))
            });
        let (label_min_y, label_max_y, bounds_min_y, bounds_max_y) =
            if self.data.is_empty() || !raw_min_y.is_finite() || !raw_max_y.is_finite() {
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
                self.format_price(label_min_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.format_price(y_mid)),
            Span::styled(
                self.format_price(label_max_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let x_bounds = Self::normalize_bounds(self.window);
        let y_bounds = Self::normalize_bounds([bounds_min_y, bounds_max_y]);
        let datasets = vec![
            Dataset::default()
                .marker(symbols::Marker::Dot)
                .style(Style::default().fg(Color::Cyan))
                .data(&self.data),
        ];
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(self.inst_id.as_str()))
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
                    // .title("Mark Price")
                    .title(self.format_price(self.latest_price))
                    .style(Style::default().fg(Color::Gray))
                    .labels(y_labels)
                    .bounds(y_bounds),
            );

        frame.render_widget(chart, area);
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

    fn poll_input(&self) -> Result<bool> {
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        return Ok(true);
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
        self.price_precision.unwrap_or(2)
    }

    fn format_price(&self, value: f64) -> String {
        let precision = self.price_precision();
        format!("{value:.prec$}", value = value, prec = precision)
    }
}
