use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, ClientBuilder};
use reqwest_websocket::{Message, RequestBuilderExt, WebSocket};
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

use crate::command::Command;

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

impl OkxWsClient {
    pub async fn new(btx: broadcast::Sender<Command>) -> Result<OkxWsClient, anyhow::Error> {
        Ok(OkxWsClient {
            client: ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()?,
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

    pub async fn subscribe_mark_price(&self, inst_id: &str) -> Result<(), anyhow::Error> {
        let sub_msg = SubscribeMessage {
            id: None,
            op: "subscribe".to_string(),
            args: vec![SubscribeArgs {
                channel: "mark-price".to_string(),
                inst_id: inst_id.to_string(),
            }],
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
                                            let precision = Self::decimal_places(&data.mark_px);
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

impl OkxWsClient {
    fn decimal_places(value: &str) -> usize {
        value
            .split('.')
            .nth(1)
            .map(|fraction| fraction.len())
            .unwrap_or(0)
    }
}
