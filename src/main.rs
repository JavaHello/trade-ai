mod config;
use std::time::Duration;

use clap::Parser;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use notify_rust::Notification;
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

    task::spawn(async move {
        let mut send_flag = false;
        while let Some(msg) = mrx.recv().await {
            if msg == "reset" {
                send_flag = false;
                continue;
            }
            if !send_flag {
                Notification::new()
                    .summary("价格监控")
                    .body(&msg)
                    .show()
                    .unwrap();
                send_flag = true;
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
                            println!(
                                "币种: {}, 标记价格: {}, 时间戳: {}",
                                data.inst_id, data.mark_px, data.ts
                            );
                            let inst_id = data.inst_id;
                            let mark_px: f64 = data.mark_px.parse().unwrap_or(0.0);

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
                        println!("Received non-mark-price message: {}", text);
                    }
                }
            }
        },
    )
    .await;
    Ok(())
}
