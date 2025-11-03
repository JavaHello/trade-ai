use anyhow::Ok;
use futures_util::{
    SinkExt, StreamExt, TryStreamExt,
    stream::{SplitSink, SplitStream},
};
use reqwest::Client;
use reqwest_websocket::{Message, RequestBuilderExt, WebSocket};
use tokio::sync::broadcast;

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
    pub ws_tx: SplitSink<WebSocket, Message>,
    pub ws_rx: SplitStream<WebSocket>,
    pub tx: broadcast::Sender<Command>,
}

impl OkxWsClient {
    pub async fn new(btx: broadcast::Sender<Command>) -> Result<OkxWsClient, anyhow::Error> {
        // println!("AppContext initialized: {:?}", ctx);
        let websocket = Client::default()
            .get("wss://ws.okx.com:8443/ws/v5/public")
            .upgrade()
            .send()
            .await?
            .into_websocket()
            .await?;

        let (tx, rx) = websocket.split();
        Ok(OkxWsClient {
            ws_tx: tx,
            ws_rx: rx,
            tx: btx,
        })
    }

    pub async fn subscribe_mark_price(&mut self, inst_id: &str) -> Result<(), anyhow::Error> {
        let sub_msg = SubscribeMessage {
            id: None,
            op: "subscribe".to_string(),
            args: vec![SubscribeArgs {
                channel: "mark-price".to_string(),
                inst_id: inst_id.to_string(),
            }],
        };
        let msg_text = serde_json::to_string(&sub_msg)?;
        self.ws_tx.send(Message::Text(msg_text)).await?;

        while let Some(message) = self.ws_rx.try_next().await? {
            if let Message::Text(text) = message {
                let msg = serde_json::from_str::<MarkPriceMessage>(&text);
                if msg.is_err() {
                    continue;
                }
                for data in msg.unwrap().data {
                    let inst_id = data.inst_id;
                    let mark_px: f64 = data.mark_px.parse().unwrap_or(0.0);
                    let ts: i64 = data.ts.parse().unwrap_or(0);
                    if self
                        .tx
                        .send(Command::MarkPriceUpdate(inst_id, mark_px, ts))
                        .is_err()
                    {}
                }
            }
        }
        Ok(())
    }
}
