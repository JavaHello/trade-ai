use anyhow::{Context, anyhow};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, ClientBuilder};
use reqwest_websocket::{Message, RequestBuilderExt, WebSocket};
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

use crate::command::{Command, PricePoint};

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeMessage {
    pub id: Option<String>,
    pub op: String,
    pub args: Vec<SubscribeArgs>,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeArgs {
    pub channel: String,
    pub inst_id: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkPriceMessage {
    pub arg: MarkPriceArg,
    pub data: Vec<MarkPriceData>,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkPriceArg {
    pub channel: String,
    pub inst_id: String,
}
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkPriceData {
    pub inst_id: String,
    pub inst_type: String,
    pub mark_px: String,
    pub ts: String,
}

pub struct OkxWsClient {
    client: Client,
    tx: broadcast::Sender<Command>,
}

const PUBLIC_WS_ENDPOINT: &str = "wss://ws.okx.com:8443/ws/v5/public";
const MARK_PRICE_CANDLES_ENDPOINT: &str = "https://www.okx.com/api/v5/market/mark-price-candles";
const MAX_CANDLE_LIMIT: usize = 300;
const BAR_OPTIONS: &[(u64, &str)] = &[
    (60, "1m"),
    (180, "3m"),
    (300, "5m"),
    (900, "15m"),
    (1_800, "30m"),
    (3_600, "1H"),
    (7_200, "2H"),
    (14_400, "4H"),
    (21_600, "6H"),
    (43_200, "12H"),
    (86_400, "1D"),
    (172_800, "2D"),
    (259_200, "3D"),
    (604_800, "1W"),
];

impl OkxWsClient {
    pub async fn new(btx: broadcast::Sender<Command>) -> Result<OkxWsClient, anyhow::Error> {
        Ok(OkxWsClient {
            client: build_http_client()?,
            tx: btx,
        })
    }

    fn emit_error(&self, message: impl Into<String>) {
        let _ = self.tx.send(Command::Error(message.into()));
    }

    async fn connect(&self) -> Result<WebSocket, anyhow::Error> {
        let response = self.client.get(PUBLIC_WS_ENDPOINT).upgrade().send().await?;

        Ok(response.into_websocket().await?)
    }

    pub async fn subscribe_mark_price(&self, inst_ids: &[String]) -> Result<(), anyhow::Error> {
        if inst_ids.is_empty() {
            return Err(anyhow!("no instrument ids specified"));
        }
        let sub_msg = SubscribeMessage {
            id: None,
            op: "subscribe".to_string(),
            args: inst_ids
                .iter()
                .map(|inst_id| SubscribeArgs {
                    channel: "mark-price".to_string(),
                    inst_id: inst_id.to_string(),
                })
                .collect(),
        };
        let subscribe_payload = serde_json::to_string(&sub_msg)?;
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(32);

        loop {
            match self.connect().await {
                Ok(websocket) => {
                    backoff = Duration::from_secs(1);
                    let (mut ws_tx, mut ws_rx) = websocket.split();

                    if let Err(err) = ws_tx.send(Message::Text(subscribe_payload.clone())).await {
                        self.emit_error(format!("failed to send subscribe request: {err}"));
                    } else {
                        while let Some(result) = ws_rx.next().await {
                            match result {
                                Ok(Message::Text(text)) => {
                                    if let Ok(msg) = serde_json::from_str::<MarkPriceMessage>(&text)
                                    {
                                        for data in msg.data {
                                            let inst_id = data.inst_id;
                                            let mark_px: f64 = data.mark_px.parse().unwrap_or(0.0);
                                            let ts: i64 = data.ts.parse().unwrap_or(0);
                                            let precision = decimal_places(&data.mark_px);
                                            let _ = self.tx.send(Command::MarkPriceUpdate(
                                                inst_id, mark_px, ts, precision,
                                            ));
                                        }
                                    }
                                }
                                Ok(Message::Ping(payload)) => {
                                    if let Err(err) = ws_tx.send(Message::Pong(payload)).await {
                                        self.emit_error(format!("failed to reply pong: {err}"));
                                        break;
                                    }
                                }
                                Ok(Message::Close { code, reason }) => {
                                    self.emit_error(format!(
                                        "websocket closed by server: code={code}, reason={reason:?}"
                                    ));
                                    break;
                                }
                                Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                                Err(err) => {
                                    self.emit_error(format!("websocket read error: {err}"));
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    self.emit_error(format!("failed to connect to okx websocket: {err}"));
                }
            }

            sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct CandleResponse {
    code: String,
    msg: String,
    data: Vec<Vec<String>>,
}

pub async fn bootstrap_history(
    inst_ids: &[String],
    window: Duration,
    tx: broadcast::Sender<Command>,
) -> Result<Vec<PricePoint>, anyhow::Error> {
    if inst_ids.is_empty() {
        return Ok(Vec::new());
    }
    let client = build_http_client()?;
    let mut aggregated: Vec<PricePoint> = Vec::new();
    for inst_id in inst_ids {
        match fetch_history_for_inst(&client, inst_id, window).await {
            Ok(mut points) => aggregated.append(&mut points),
            Err(err) => {
                let _ = tx.send(Command::Error(format!(
                    "failed to load history for {inst_id}: {err}"
                )));
            }
        }
    }
    if aggregated.is_empty() {
        return Ok(Vec::new());
    }
    Ok(aggregated)
}

async fn fetch_history_for_inst(
    client: &Client,
    inst_id: &str,
    window: Duration,
) -> Result<Vec<PricePoint>, anyhow::Error> {
    let (bar, bar_duration) = choose_bar(window);
    let required_points =
        ((window.as_secs_f64() / bar_duration.as_secs_f64()).ceil() as usize).max(1);
    let fetch_limit = required_points
        .saturating_mul(2)
        .max(required_points)
        .min(MAX_CANDLE_LIMIT);
    let limit_param = fetch_limit.to_string();
    let response = client
        .get(MARK_PRICE_CANDLES_ENDPOINT)
        .query(&[
            ("instId", inst_id),
            ("bar", bar),
            ("limit", limit_param.as_str()),
        ])
        .send()
        .await
        .with_context(|| format!("requesting history for {inst_id}"))?
        .error_for_status()
        .with_context(|| format!("history response status for {inst_id}"))?
        .json::<CandleResponse>()
        .await
        .with_context(|| format!("decoding history for {inst_id}"))?;
    if response.code != "0" {
        return Err(anyhow!(
            "okx history error for {} (code {}): {}",
            inst_id,
            response.code,
            response.msg
        ));
    }
    let cutoff = cutoff_timestamp(window);
    let mut points = Vec::new();
    for candle in response.data {
        if let Some(point) = candle_to_point(inst_id, &candle, cutoff) {
            points.push(point);
        }
    }
    points.sort_by_key(|point| point.ts);
    Ok(points)
}

fn candle_to_point(inst_id: &str, candle: &[String], cutoff: i64) -> Option<PricePoint> {
    if candle.len() < 5 {
        return None;
    }
    let ts = candle.get(0)?.parse::<i64>().ok()?;
    if ts < cutoff {
        return None;
    }
    let close_str = candle.get(4)?;
    let close = close_str.parse::<f64>().ok()?;
    let precision = decimal_places(close_str);
    Some(PricePoint {
        inst_id: inst_id.to_string(),
        mark_px: close,
        ts,
        precision,
    })
}

fn choose_bar(window: Duration) -> (&'static str, Duration) {
    for (secs, label) in BAR_OPTIONS {
        let interval = Duration::from_secs(*secs);
        if window.as_secs_f64() <= interval.as_secs_f64() * MAX_CANDLE_LIMIT as f64 {
            return (label, interval);
        }
    }
    let (secs, label) = BAR_OPTIONS.last().copied().unwrap_or((60, "1m"));
    (label, Duration::from_secs(secs))
}

fn cutoff_timestamp(window: Duration) -> i64 {
    let now_ms = Utc::now().timestamp_millis();
    let window_ms = window.as_millis().min(i64::MAX as u128) as i64;
    now_ms.saturating_sub(window_ms)
}

fn decimal_places(value: &str) -> usize {
    value
        .split('.')
        .nth(1)
        .map(|fraction| fraction.len())
        .unwrap_or(0)
}

fn build_http_client() -> Result<Client, anyhow::Error> {
    Ok(ClientBuilder::new()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()?)
}
