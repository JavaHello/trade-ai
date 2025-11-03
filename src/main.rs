mod config;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use clap::Parser;
use color_eyre::Result;
use crossterm::event::{self, KeyCode};
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::Span;
use ratatui::widgets::{Axis, Block, Chart, Dataset};
use ratatui::{DefaultTerminal, Frame};
use reqwest::Client;
use reqwest_websocket::{Message, RequestBuilderExt};
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::interval;

// {
//   "id": "1512",
//     "op": "subscribe",
//     "args": [{
//         "channel": "mark-price",
//         "instId": "BTC-USDT"
//     }]
// }
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeMessage {
    id: Option<String>,
    op: String,
    args: Vec<SubscribeArgs>,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeArgs {
    channel: String,
    inst_id: String,
}

// {"arg":{"channel":"mark-price","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","instType":"SWAP","markPx":"110562.0","ts":"1762079224447"}]}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MarkPriceMessage {
    arg: MarkPriceArg,
    data: Vec<MarkPriceData>,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MarkPriceArg {
    channel: String,
    inst_id: String,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MarkPriceData {
    inst_id: String,
    inst_type: String,
    mark_px: String,
    ts: String,
}

#[cfg(target_os = "linux")]
async fn linux_notify(msg: &str, inst_id: &str) -> Result<(), anyhow::Error> {
    use notify_rust::{CloseReason, Hint, Notification};
    use tokio::process::Command;
    Notification::new()
        .summary("价格监控")
        .body(msg)
        .hint(Hint::Urgency(notify_rust::Urgency::Critical))
        .action("open", "okx")
        .show()
        .unwrap()
        .on_close(|_: CloseReason| {
            let _ = Command::new("xdg-open")
                .arg(format!(
                    "https://www.okx.com/zh-hans/trade-swap/{}",
                    inst_id.to_lowercase()
                ))
                .spawn();
        });
    Ok(())
}

struct App {
    inst_id: String,
    data: Vec<(f64, f64)>,
    window: [f64; 2],
    last_draw: Instant,
    min_redraw_gap: Duration,
    retention: Duration,
    latest_price: f64,
}
impl App {
    fn new(inst_id: &str) -> App {
        let min_redraw_gap = Duration::from_millis(100);
        let retention = Duration::from_secs(5 * 60);
        App {
            inst_id: inst_id.to_string(),
            data: vec![],
            window: [0.0, 100.0],
            last_draw: Instant::now() - min_redraw_gap,
            min_redraw_gap,
            retention,
            latest_price: 0.0,
        }
    }

    async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        rx: &mut mpsc::Receiver<(f64, i64)>,
    ) -> Result<()> {
        while let Some(message) = rx.recv().await {
            let timeout = self.min_redraw_gap.saturating_sub(self.last_draw.elapsed());
            while event::poll(timeout)? {
                match event::read()? {
                    event::Event::Key(key_event) if key_event.code == KeyCode::Char('q') => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
            self.on_tick(message.0, message.1);
            if self.last_draw.elapsed() >= self.min_redraw_gap {
                terminal.draw(|frame| self.render(frame))?;
                self.last_draw = Instant::now();
            }
        }
        Ok(())
    }
    fn on_tick(&mut self, mark_px: f64, ts: i64) {
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
        self.render_chart(frame, frame.area());
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
                format!("{:.2}", label_min_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{:.2}", y_mid)),
            Span::styled(
                format!("{:.2}", label_max_y),
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
                    .title(format!("{:.2}", self.latest_price))
                    .style(Style::default().fg(Color::Gray))
                    .labels(y_labels)
                    .bounds(y_bounds),
            );

        frame.render_widget(chart, area);
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
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let param = config::CliParams::parse();
    let (mtx, mut mrx) = mpsc::channel::<String>(1);
    let mut ticker = interval(Duration::from_secs(10));
    ticker.tick().await;
    let tsender = mtx.clone();
    task::spawn(async move {
        loop {
            ticker.tick().await;
            // reset send flag
            let _ = tsender.send("reset".to_string()).await;
        }
    });

    #[allow(unused)]
    let np = param.clone();
    task::spawn(async move {
        let mut send_flag = false;
        while let Some(msg) = mrx.recv().await {
            if msg == "reset" {
                send_flag = false;
                continue;
            }
            if !send_flag {
                send_flag = true;
                #[cfg(target_os = "linux")]
                {
                    let _ = linux_notify(&msg, &np.inst_id).await;
                }
            }
        }
    });
    // println!("AppContext initialized: {:?}", ctx);
    let websocket = Client::default()
        .get("wss://ws.okx.com:8443/ws/v5/public")
        .upgrade()
        .send()
        .await?
        .into_websocket()
        .await?;

    let (mut tx, mut rx) = websocket.split();
    let pa = param.clone();

    let (ttx, mut trx) = mpsc::channel::<(f64, i64)>(1);
    task::spawn(async move {
        color_eyre::install().unwrap();
        let mut terminal = ratatui::init();
        let app = App::new(&pa.inst_id);
        app.run(&mut terminal, &mut trx).await.unwrap();
        ratatui::restore();
    });

    futures_util::future::join(
        async move {
            let sub_msg = SubscribeMessage {
                id: None,
                op: "subscribe".to_string(),
                args: vec![SubscribeArgs {
                    channel: "mark-price".to_string(),
                    inst_id: param.inst_id.clone(),
                }],
            };
            let msg_text = serde_json::to_string(&sub_msg).unwrap();
            tx.send(Message::Text(msg_text)).await.unwrap();
        },
        async move {
            'outer: loop {
                let Some(message) = (match rx.try_next().await {
                    Ok(msg) => msg,
                    Err(_err) => {
                        break 'outer;
                    }
                }) else {
                    break 'outer;
                };
                if let Message::Text(text) = message {
                    if let Ok(mark_price_msg) = serde_json::from_str::<MarkPriceMessage>(&text) {
                        for data in mark_price_msg.data {
                            // println!(
                            //     "币种: {}, 标记价格: {}, 时间戳: {}",
                            //     data.inst_id, data.mark_px, data.ts
                            // );
                            let inst_id = data.inst_id;
                            let mark_px: f64 = data.mark_px.parse().unwrap_or(0.0);
                            let ts: i64 = data.ts.parse().unwrap_or(0);
                            if ttx.send((mark_px, ts)).await.is_err() {
                                break 'outer;
                            }

                            if mark_px >= param.upper_threshold {
                                let alert_msg = format!(
                                    "告警: {} 标记价格 {:.2} 超过上限 {:.2}",
                                    inst_id, mark_px, param.upper_threshold
                                );
                                if mtx.send(alert_msg).await.is_err() {
                                    break 'outer;
                                }
                            } else if mark_px <= param.lower_threshold {
                                let alert_msg = format!(
                                    "告警: {} 标记价格 {:.2} 低于下限 {:.2}",
                                    inst_id, mark_px, param.lower_threshold
                                );
                                if mtx.send(alert_msg).await.is_err() {
                                    break 'outer;
                                }
                            }
                        }
                    } else {
                        // println!("Received non-mark-price message: {}", text);
                    }
                }
            }
        },
    )
    .await;
    Ok(())
}
