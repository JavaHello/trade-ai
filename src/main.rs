mod config;
use std::time::Duration;

use clap::Parser;
use color_eyre::Result;
use crossterm::event::{self, KeyCode};
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::{self, Marker};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, LegendPosition};
use ratatui::{DefaultTerminal, Frame};
use reqwest::Client;
use reqwest_websocket::{Message, RequestBuilderExt};
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::{Instant, interval};

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
}
impl App {
    fn new(inst_id: &str) -> App {
        App {
            inst_id: inst_id.to_string(),
            data: vec![],
            window: [0.0, 100.0],
        }
    }

    async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        rx: &mut mpsc::Receiver<(f64, i64)>,
    ) -> Result<()> {
        while let Some(message) = rx.recv().await {
            if event::poll(Duration::from_millis(100))? {
                if event::read()?
                    .as_key_press_event()
                    .is_some_and(|key| key.code == KeyCode::Char('q'))
                {
                    return Ok(());
                }
            }
            self.on_tick(message.0, message.1);
            terminal.draw(|frame| self.render(frame))?;
        }
        Ok(())
    }
    fn on_tick(&mut self, mark_px: f64, ts: i64) {
        let x = ts as f64;
        let y = mark_px;
        self.data.push((x, y));
        if self.data.len() > 100 {
            self.data.remove(0);
        }
        if let Some((min_x, _)) = self.data.first() {
            if let Some((max_x, _)) = self.data.last() {
                self.window = [*min_x, *max_x];
            }
        }
    }
    fn render(&self, frame: &mut Frame) {
        let chunks = Layout::default()
            .constraints([Constraint::Percentage(100)].as_ref())
            .margin(1)
            .split(frame.size());
        self.render_chart(frame, chunks[0]);
    }
    fn render_chart(&self, frame: &mut Frame, area: Rect) {
        let x_labels = vec![
            Span::styled(
                format!("{}", self.window[0]),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{}", f64::midpoint(self.window[0], self.window[1]))),
            Span::styled(
                format!("{}", self.window[1]),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let datasets = vec![
            Dataset::default()
                .name(self.inst_id.as_str())
                .marker(symbols::Marker::Dot)
                .style(Style::default().fg(Color::Cyan))
                .data(&self.data),
        ];

        let chart = Chart::new(datasets)
            .block(Block::bordered())
            .x_axis(
                Axis::default()
                    .title("Time")
                    .style(Style::default().fg(Color::Gray))
                    .labels(x_labels)
                    .bounds(self.window),
            )
            .y_axis(
                Axis::default()
                    .title("Mark Price")
                    .style(Style::default().fg(Color::Gray))
                    .labels(vec![
                        Span::styled(
                            format!(
                                "{:.2}",
                                self.data
                                    .iter()
                                    .map(|(_, y)| *y)
                                    .fold(f64::INFINITY, f64::min)
                            ),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            "{:.2}",
                            (self
                                .data
                                .iter()
                                .map(|(_, y)| *y)
                                .fold(f64::INFINITY, f64::min)
                                + self
                                    .data
                                    .iter()
                                    .map(|(_, y)| *y)
                                    .fold(f64::NEG_INFINITY, f64::max))
                                / 2.0
                        )),
                        Span::styled(
                            format!(
                                "{:.2}",
                                self.data
                                    .iter()
                                    .map(|(_, y)| *y)
                                    .fold(f64::NEG_INFINITY, f64::max)
                            ),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ])
                    .bounds([
                        self.data
                            .iter()
                            .map(|(_, y)| *y)
                            .fold(f64::INFINITY, f64::min),
                        self.data
                            .iter()
                            .map(|(_, y)| *y)
                            .fold(f64::NEG_INFINITY, f64::max),
                    ]),
            );

        frame.render_widget(chart, area);
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
            while let Some(message) = rx.try_next().await.unwrap() {
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
                            ttx.send((mark_px, ts)).await.unwrap();

                            if mark_px >= param.upper_threshold {
                                let alert_msg = format!(
                                    "告警: {} 标记价格 {:.2} 超过上限 {:.2}",
                                    inst_id, mark_px, param.upper_threshold
                                );
                                mtx.send(alert_msg).await.unwrap();
                            } else if mark_px <= param.lower_threshold {
                                let alert_msg = format!(
                                    "告警: {} 标记价格 {:.2} 低于下限 {:.2}",
                                    inst_id, mark_px, param.lower_threshold
                                );
                                mtx.send(alert_msg).await.unwrap();
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
