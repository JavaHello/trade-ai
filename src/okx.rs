use std::collections::{HashMap, HashSet};

use anyhow::{Context, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{SecondsFormat, Utc};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use reqwest::{Client, ClientBuilder};
use reqwest_websocket::{Message, RequestBuilderExt, WebSocket};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Sha256;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::{Duration, interval, sleep};

use crate::command::{
    AccountBalance, AccountBalanceDelta, AccountSnapshot, CancelOrderRequest, CancelResponse,
    Command, PendingOrderInfo, PositionInfo, PricePoint, SetLeverageRequest, TradeEvent, TradeFill,
    TradeOrderKind, TradeRequest, TradeResponse, TradeSide, TradingCommand,
};
use crate::config::TradingConfig;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OkxResponse<T> {
    pub code: String,
    pub msg: String,
    pub data: T,
}

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

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkPriceResponse {
    code: String,
    msg: String,
    pub data: Vec<MarkPriceData>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LongShortRatioEntry {
    ts: String,
    ratio: String,
}

#[derive(Debug, Clone)]
pub struct LongShortRatio {
    pub ts: i64,
    pub ratio: f64,
}

pub struct OkxWsClient {
    client: Client,
    tx: broadcast::Sender<Command>,
}

pub struct OkxTradingClient {
    client: Client,
    tx: broadcast::Sender<Command>,
    config: TradingConfig,
}

pub struct OkxPrivateWsClient {
    client: Client,
    tx: broadcast::Sender<Command>,
    config: TradingConfig,
}

pub struct OkxBusinessWsClient {
    client: Client,
    tx: broadcast::Sender<Command>,
    config: TradingConfig,
}

const PUBLIC_WS_ENDPOINT: &str = "wss://ws.okx.com:8443/ws/v5/public";
const PRIVATE_WS_ENDPOINT: &str = "wss://ws.okx.com:8443/ws/v5/private";
const BUSINESS_WS_ENDPOINT: &str = "wss://ws.okx.com:8443/ws/v5/business";
const OKX_API_BASE: &str = "https://www.okx.com";
const MARK_PRICE_CANDLES_ENDPOINT: &str = "https://www.okx.com/api/v5/market/mark-price-candles";
const MARK_PRICE_ENDPOINT: &str = "https://www.okx.com/api/v5/public/mark-price";
const INSTRUMENTS_ENDPOINT: &str = "/api/v5/account/instruments";
const ACCOUNT_LEVERAGE_ENDPOINT: &str = "/api/v5/account/leverage-info";
const TRADE_ORDER_ENDPOINT: &str = "/api/v5/trade/order";
const TRADE_ORDER_ALGO_ENDPOINT: &str = "/api/v5/trade/order-algo";
const CANCEL_ORDER_ENDPOINT: &str = "/api/v5/trade/cancel-order";
const CANCEL_ALGO_ORDER_ENDPOINT: &str = "/api/v5/trade/cancel-algos";
const SET_LEVERAGE_ENDPOINT: &str = "/api/v5/account/set-leverage";
const ACCOUNT_BALANCE_ENDPOINT: &str = "/api/v5/account/balance";
const POSITIONS_ENDPOINT: &str = "/api/v5/account/positions";
const ORDERS_PENDING_ENDPOINT: &str = "/api/v5/trade/orders-pending";
const ORDERS_ALGO_PENDING_ENDPOINT: &str = "/api/v5/trade/orders-algo-pending";
const LONG_SHORT_ACCOUNT_RATIO_ENDPOINT: &str =
    "api/v5/rubik/stat/contracts/long-short-account-ratio";

const MAX_CANDLE_LIMIT: usize = 300;
type HmacSha256 = Hmac<Sha256>;
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

impl OkxTradingClient {
    pub fn new(
        config: TradingConfig,
        tx: broadcast::Sender<Command>,
    ) -> Result<Self, anyhow::Error> {
        Ok(OkxTradingClient {
            client: build_http_client()?,
            tx,
            config,
        })
    }

    pub async fn run(self, mut rx: mpsc::Receiver<TradingCommand>) -> Result<(), anyhow::Error> {
        while let Some(command) = rx.recv().await {
            match command {
                TradingCommand::Place(request) => {
                    let response = match self.place_order(&request).await {
                        Ok(result) => result,
                        Err(err) => TradeResponse {
                            inst_id: request.inst_id.clone(),
                            side: request.side,
                            price: request.price,
                            size: request.size,
                            order_id: None,
                            message: format!("OKX 下单失败: {err}"),
                            success: false,
                            operator: request.operator.clone(),
                            pos_side: request.pos_side.clone(),
                            leverage: request.leverage,
                            kind: request.kind,
                        },
                    };
                    if !response.success {
                        let side_label = match response.side {
                            TradeSide::Buy => "买入",
                            TradeSide::Sell => "卖出",
                        };
                        let message = format!(
                            "{inst} {side} 委托失败: {msg}",
                            inst = response.inst_id,
                            side = side_label,
                            msg = response.message
                        );
                        let _ = self.tx.send(Command::Error(message));
                    }
                    let _ = self
                        .tx
                        .send(Command::TradeResult(TradeEvent::Order(response)));
                }
                TradingCommand::Cancel(request) => {
                    let response = match self.cancel_order(&request).await {
                        Ok(result) => result,
                        Err(err) => CancelResponse {
                            inst_id: request.inst_id.clone(),
                            ord_id: request.ord_id.clone(),
                            message: format!("OKX 撤单失败: {err}"),
                            success: false,
                            operator: request.operator.clone(),
                            pos_side: request.pos_side.clone(),
                        },
                    };
                    if !response.success {
                        let message = format!(
                            "{inst} 撤单失败: {msg}",
                            inst = response.inst_id,
                            msg = response.message
                        );
                        let _ = self.tx.send(Command::Error(message));
                    }
                    let _ = self
                        .tx
                        .send(Command::TradeResult(TradeEvent::Cancel(response)));
                }
                TradingCommand::SetLeverage(request) => match self.set_leverage(&request).await {
                    Ok(_) => {
                        let message =
                            format!("杠杆已调整至 {}x", format_leverage_display(request.lever));
                        let _ = self
                            .tx
                            .send(Command::Notify(request.inst_id.clone(), message));
                    }
                    Err(err) => {
                        let _ = self.tx.send(Command::Error(format!("调整杠杆失败: {err}")));
                    }
                },
            }
        }
        Ok(())
    }

    async fn place_order(&self, request: &TradeRequest) -> Result<TradeResponse, anyhow::Error> {
        match request.kind {
            TradeOrderKind::Regular => self.place_regular_order(request).await,
            TradeOrderKind::TakeProfit | TradeOrderKind::StopLoss => {
                self.place_strategy_order(request).await
            }
        }
    }

    async fn place_regular_order(
        &self,
        request: &TradeRequest,
    ) -> Result<TradeResponse, anyhow::Error> {
        let payload = TradeOrderRequest::from_request(request, &self.config.td_mode);
        let body = serde_json::to_string(&payload)?;
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "POST",
            TRADE_ORDER_ENDPOINT,
            &body,
        )?;
        let response = self
            .client
            .post(format!("{OKX_API_BASE}{TRADE_ORDER_ENDPOINT}"))
            .header("OK-ACCESS-KEY", &self.config.api_key)
            .header("OK-ACCESS-PASSPHRASE", &self.config.passphrase)
            .header("OK-ACCESS-TIMESTAMP", &timestamp)
            .header("OK-ACCESS-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| "sending order to OKX")?
            .error_for_status()
            .with_context(|| "OKX returned non-success status")?
            .json::<TradeOrderResponse>()
            .await
            .with_context(|| "decoding OKX order response")?;
        Ok(build_regular_trade_response(request, response))
    }

    async fn place_strategy_order(
        &self,
        request: &TradeRequest,
    ) -> Result<TradeResponse, anyhow::Error> {
        let payload = AlgoOrderRequest::from_request(request, &self.config.td_mode);
        let body = serde_json::to_string(&payload)?;
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "POST",
            TRADE_ORDER_ALGO_ENDPOINT,
            &body,
        )?;
        let response = self
            .client
            .post(format!("{OKX_API_BASE}{TRADE_ORDER_ALGO_ENDPOINT}"))
            .header("OK-ACCESS-KEY", &self.config.api_key)
            .header("OK-ACCESS-PASSPHRASE", &self.config.passphrase)
            .header("OK-ACCESS-TIMESTAMP", &timestamp)
            .header("OK-ACCESS-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| "sending algo order to OKX")?
            // .error_for_status()
            // .with_context(|| "OKX returned non-success status for algo order")?
            .json::<AlgoOrderResponse>()
            .await
            .with_context(|| "decoding OKX algo order response")?;
        Ok(build_algo_trade_response(request, response))
    }

    async fn cancel_order(
        &self,
        request: &CancelOrderRequest,
    ) -> Result<CancelResponse, anyhow::Error> {
        match request.kind {
            TradeOrderKind::Regular => self.cancel_regular_order(request).await,
            TradeOrderKind::TakeProfit | TradeOrderKind::StopLoss => {
                self.cancel_algo_order(request).await
            }
        }
    }

    async fn cancel_regular_order(
        &self,
        request: &CancelOrderRequest,
    ) -> Result<CancelResponse, anyhow::Error> {
        let payload = CancelOrderPayload::from_request(request);
        let body = serde_json::to_string(&payload)?;
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "POST",
            CANCEL_ORDER_ENDPOINT,
            &body,
        )?;
        let response = self
            .client
            .post(format!("{OKX_API_BASE}{CANCEL_ORDER_ENDPOINT}"))
            .header("OK-ACCESS-KEY", &self.config.api_key)
            .header("OK-ACCESS-PASSPHRASE", &self.config.passphrase)
            .header("OK-ACCESS-TIMESTAMP", &timestamp)
            .header("OK-ACCESS-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| "sending cancel to OKX")?
            .error_for_status()
            .with_context(|| "OKX returned non-success status for cancel")?
            .json::<CancelOrderResponse>()
            .await
            .with_context(|| "decoding OKX cancel response")?;
        Ok(build_cancel_response(request, response))
    }

    async fn cancel_algo_order(
        &self,
        request: &CancelOrderRequest,
    ) -> Result<CancelResponse, anyhow::Error> {
        let payload = CancelAlgoPayload::new(request);
        let body = serde_json::to_string(&payload)?;
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "POST",
            CANCEL_ALGO_ORDER_ENDPOINT,
            &body,
        )?;
        let response = self
            .client
            .post(format!("{OKX_API_BASE}{CANCEL_ALGO_ORDER_ENDPOINT}"))
            .header("OK-ACCESS-KEY", &self.config.api_key)
            .header("OK-ACCESS-PASSPHRASE", &self.config.passphrase)
            .header("OK-ACCESS-TIMESTAMP", &timestamp)
            .header("OK-ACCESS-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| "sending algo cancel to OKX")?
            .error_for_status()
            .with_context(|| "OKX returned non-success status for algo cancel")?
            .json::<CancelAlgoResponse>()
            .await
            .with_context(|| "decoding OKX algo cancel response")?;
        Ok(build_algo_cancel_response(request, response))
    }

    async fn set_leverage(&self, request: &SetLeverageRequest) -> Result<(), anyhow::Error> {
        let payload = SetLeveragePayload::from_request(request, &self.config.td_mode);
        let body = serde_json::to_string(&payload)?;
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "POST",
            SET_LEVERAGE_ENDPOINT,
            &body,
        )?;
        let response = self
            .client
            .post(format!("{OKX_API_BASE}{SET_LEVERAGE_ENDPOINT}"))
            .header("OK-ACCESS-KEY", &self.config.api_key)
            .header("OK-ACCESS-PASSPHRASE", &self.config.passphrase)
            .header("OK-ACCESS-TIMESTAMP", &timestamp)
            .header("OK-ACCESS-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| "sending set leverage request to OKX")?
            .error_for_status()
            .with_context(|| "OKX returned non-success status for set leverage")?
            .json::<SetLeverageResponse>()
            .await
            .with_context(|| "decoding OKX set leverage response")?;
        if response.code != "0" {
            let message = if response.msg.is_empty() {
                "OKX 调整杠杆失败".to_string()
            } else {
                response.msg
            };
            return Err(anyhow!(message));
        }
        Ok(())
    }
}

impl OkxPrivateWsClient {
    pub fn new(
        config: TradingConfig,
        tx: broadcast::Sender<Command>,
    ) -> Result<Self, anyhow::Error> {
        Ok(OkxPrivateWsClient {
            client: build_http_client()?,
            tx,
            config,
        })
    }

    fn emit_error(&self, message: impl Into<String>) {
        let _ = self.tx.send(Command::Error(message.into()));
    }

    async fn connect(&self) -> Result<WebSocket, anyhow::Error> {
        let response = self
            .client
            .get(PRIVATE_WS_ENDPOINT)
            .upgrade()
            .send()
            .await
            .with_context(|| "connecting to OKX private websocket")?;
        Ok(response.into_websocket().await?)
    }

    pub async fn stream_account(&self) -> Result<(), anyhow::Error> {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(32);
        loop {
            match self.connect().await {
                Ok(websocket) => {
                    backoff = Duration::from_secs(1);
                    if let Err(err) = self.run_private_stream(websocket).await {
                        self.emit_error(format!("okx private ws error: {err}"));
                    } else {
                        backoff = Duration::from_secs(1);
                    }
                }
                Err(err) => {
                    self.emit_error(format!("failed to connect okx private ws: {err}"));
                }
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }

    async fn run_private_stream(&self, websocket: WebSocket) -> Result<(), anyhow::Error> {
        let (mut ws_tx, mut ws_rx) = websocket.split();
        self.send_login(&mut ws_tx).await?;
        self.wait_for_login(&mut ws_tx, &mut ws_rx).await?;
        self.subscribe_private(&mut ws_tx).await?;
        let state = SharedAccountState::global();
        let mut ping_interval = interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    ws_tx.send(Message::Ping(Vec::new().into())).await?;
                }
                result = ws_rx.next() => {
                    match result {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_private_text(&text, state).await?;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            ws_tx.send(Message::Pong(payload)).await?;
                        }
                        Some(Ok(Message::Close { code, reason })) => {
                            return Err(anyhow!(
                                "private websocket closed by server: code={code}, reason={reason:?}"
                            ));
                        }
                        Some(Ok(Message::Pong(_)) | Ok(Message::Binary(_))) => {}
                        Some(Err(err)) => return Err(err.into()),
                        None => return Err(anyhow!("private websocket closed")),
                    }
                }
            }
        }
    }

    async fn send_login(
        &self,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ) -> Result<(), anyhow::Error> {
        let timestamp = current_ws_timestamp();
        let sign = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "GET",
            "/users/self/verify",
            "",
        )?;
        let login = LoginRequest {
            op: "login".to_string(),
            args: vec![LoginArgs {
                api_key: self.config.api_key.clone(),
                passphrase: self.config.passphrase.clone(),
                timestamp,
                sign,
            }],
        };
        let payload = serde_json::to_string(&login)?;
        ws_tx
            .send(Message::Text(payload))
            .await
            .with_context(|| "sending login payload to OKX")?;
        Ok(())
    }

    async fn wait_for_login(
        &self,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
    ) -> Result<(), anyhow::Error> {
        loop {
            match ws_rx.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse_ws_event(&text) {
                        if event.event == "login" {
                            if event.code == "0" {
                                return Ok(());
                            } else {
                                return Err(anyhow!(
                                    "okx private ws login failed (code {}): {}",
                                    event.code,
                                    event.msg
                                ));
                            }
                        } else if event.event == "error" {
                            self.emit_error(format!(
                                "okx private ws event error (code {}): {}",
                                event.code, event.msg
                            ));
                        }
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    ws_tx.send(Message::Pong(payload)).await?;
                }
                Some(Ok(Message::Close { code, reason })) => {
                    return Err(anyhow!(
                        "private websocket closed during login: code={code}, reason={reason:?}"
                    ));
                }
                Some(Ok(Message::Binary(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Err(err)) => return Err(err.into()),
                None => return Err(anyhow!("private websocket closed before login ack")),
            }
        }
    }

    async fn subscribe_private(
        &self,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ) -> Result<(), anyhow::Error> {
        let args = vec![
            PrivateSubscribeArg {
                channel: "positions".to_string(),
                inst_type: Some("ANY".to_string()),
            },
            PrivateSubscribeArg {
                channel: "orders".to_string(),
                inst_type: Some("ANY".to_string()),
            },
            PrivateSubscribeArg {
                channel: "account".to_string(),
                inst_type: None,
            },
        ];
        let payload = serde_json::to_string(&PrivateSubscribeMessage {
            id: None,
            op: "subscribe".to_string(),
            args,
        })?;
        ws_tx
            .send(Message::Text(payload))
            .await
            .with_context(|| "subscribing to private channels")?;
        Ok(())
    }

    async fn handle_private_text(
        &self,
        text: &str,
        state: SharedAccountState,
    ) -> Result<(), anyhow::Error> {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(val) => val,
            Err(err) => {
                self.emit_error(format!("invalid private ws payload: {err}"));
                return Ok(());
            }
        };
        if let Some(event) = value.get("event") {
            if event.is_string() {
                if let Ok(msg) = serde_json::from_value::<WsEventMessage>(value) {
                    self.handle_event(msg)?;
                }
            }
            return Ok(());
        }
        let channel = value
            .get("arg")
            .and_then(|arg| arg.get("channel"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());
        let Some(channel) = channel else {
            return Ok(());
        };
        match channel.as_str() {
            "positions" => {
                let message: PrivateDataMessage<WsPositionEntry> = serde_json::from_value(value)?;
                if let Some(snapshot) = state.update_positions(&message.data).await {
                    let _ = self.tx.send(Command::AccountSnapshot(snapshot));
                }
            }
            "orders" => {
                let message: PrivateDataMessage<WsOrderEntry> = serde_json::from_value(value)?;
                self.publish_order_fills(&message.data);
                if let Some(snapshot) = state.update_orders(&message.data).await {
                    let _ = self.tx.send(Command::AccountSnapshot(snapshot));
                }
            }
            "account" => {
                let message: PrivateDataMessage<WsAccountEntry> = serde_json::from_value(value)?;
                if let Some(snapshot) = state.update_balances(&message.data).await {
                    let _ = self.tx.send(Command::AccountSnapshot(snapshot));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_event(&self, event: WsEventMessage) -> Result<(), anyhow::Error> {
        match event.event.as_str() {
            "login" => {
                if event.code != "0" {
                    return Err(anyhow!(
                        "okx private ws login failed (code {}): {}",
                        event.code,
                        event.msg
                    ));
                }
            }
            "error" => {
                self.emit_error(format!(
                    "okx private ws error (code {}): {}",
                    event.code, event.msg
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn publish_order_fills(&self, entries: &[WsOrderEntry]) {
        for entry in entries {
            if let Some(fill) = build_trade_fill(entry) {
                let _ = self.tx.send(Command::TradeResult(TradeEvent::Fill(fill)));
            }
        }
    }
}
impl OkxBusinessWsClient {
    pub fn new(
        config: TradingConfig,
        tx: broadcast::Sender<Command>,
    ) -> Result<Self, anyhow::Error> {
        Ok(OkxBusinessWsClient {
            client: build_http_client()?,
            tx,
            config,
        })
    }

    fn emit_error(&self, message: impl Into<String>) {
        let _ = self.tx.send(Command::Error(message.into()));
    }

    async fn connect(&self) -> Result<WebSocket, anyhow::Error> {
        let response = self
            .client
            .get(BUSINESS_WS_ENDPOINT)
            .upgrade()
            .send()
            .await
            .with_context(|| "connecting to OKX private websocket")?;
        Ok(response.into_websocket().await?)
    }

    pub async fn stream_business(&self) -> Result<(), anyhow::Error> {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(32);
        loop {
            match self.connect().await {
                Ok(websocket) => {
                    backoff = Duration::from_secs(1);
                    if let Err(err) = self.run_business_stream(websocket).await {
                        self.emit_error(format!("okx business ws error: {err}"));
                    } else {
                        backoff = Duration::from_secs(1);
                    }
                }
                Err(err) => {
                    self.emit_error(format!("failed to connect okx business ws: {err}"));
                }
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }

    async fn run_business_stream(&self, websocket: WebSocket) -> Result<(), anyhow::Error> {
        let (mut ws_tx, mut ws_rx) = websocket.split();
        self.send_login(&mut ws_tx).await?;
        self.wait_for_login(&mut ws_tx, &mut ws_rx).await?;
        self.subscribe_business(&mut ws_tx).await?;
        let state = SharedAccountState::global();
        let mut ping_interval = interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    ws_tx.send(Message::Ping(Vec::new().into())).await?;
                }
                maybe_message = ws_rx.next() => {
                    match maybe_message {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_private_text(&text, state).await?;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            ws_tx.send(Message::Pong(payload)).await?;
                        }
                        Some(Ok(Message::Close { code, reason })) => {
                            return Err(anyhow!(
                                "business websocket closed by server: code={code}, reason={reason:?}"
                            ));
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                        Some(Err(err)) => return Err(err.into()),
                        None => return Err(anyhow!("business websocket closed")),
                    }
                }
            }
        }
    }

    async fn send_login(
        &self,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ) -> Result<(), anyhow::Error> {
        let timestamp = current_ws_timestamp();
        let sign = sign_payload(
            &self.config.api_secret,
            &timestamp,
            "GET",
            "/users/self/verify",
            "",
        )?;
        let login = LoginRequest {
            op: "login".to_string(),
            args: vec![LoginArgs {
                api_key: self.config.api_key.clone(),
                passphrase: self.config.passphrase.clone(),
                timestamp,
                sign,
            }],
        };
        let payload = serde_json::to_string(&login)?;
        ws_tx
            .send(Message::Text(payload))
            .await
            .with_context(|| "sending login payload to OKX")?;
        Ok(())
    }

    async fn wait_for_login(
        &self,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
    ) -> Result<(), anyhow::Error> {
        loop {
            match ws_rx.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse_ws_event(&text) {
                        if event.event == "login" {
                            if event.code == "0" {
                                return Ok(());
                            } else {
                                return Err(anyhow!(
                                    "okx business ws login failed (code {}): {}",
                                    event.code,
                                    event.msg
                                ));
                            }
                        } else if event.event == "error" {
                            self.emit_error(format!(
                                "okx business ws event error (code {}): {}",
                                event.code, event.msg
                            ));
                        }
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    ws_tx.send(Message::Pong(payload)).await?;
                }
                Some(Ok(Message::Close { code, reason })) => {
                    return Err(anyhow!(
                        "business websocket closed during login: code={code}, reason={reason:?}"
                    ));
                }
                Some(Ok(Message::Binary(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Err(err)) => return Err(err.into()),
                None => return Err(anyhow!("business websocket closed before login ack")),
            }
        }
    }

    async fn subscribe_business(
        &self,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ) -> Result<(), anyhow::Error> {
        let args = vec![BusinessSubscribeArg {
            channel: "orders-algo".to_string(),
            inst_type: Some("ANY".to_string()),
        }];
        let payload = serde_json::to_string(&BusinessSubscribeMessage {
            id: None,
            op: "subscribe".to_string(),
            args,
        })?;
        ws_tx
            .send(Message::Text(payload))
            .await
            .with_context(|| "subscribing to business channels")?;
        Ok(())
    }

    async fn handle_private_text(
        &self,
        text: &str,
        state: SharedAccountState,
    ) -> Result<(), anyhow::Error> {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(val) => val,
            Err(err) => {
                self.emit_error(format!("invalid business ws payload: {err}"));
                return Ok(());
            }
        };
        if let Some(event) = value.get("event") {
            if event.is_string() {
                if let Ok(msg) = serde_json::from_value::<WsEventMessage>(value) {
                    self.handle_event(msg)?;
                }
            }
            return Ok(());
        }
        let channel = value
            .get("arg")
            .and_then(|arg| arg.get("channel"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());
        let Some(channel) = channel else {
            return Ok(());
        };
        match channel.as_str() {
            "orders-algo" => {
                let message: PrivateDataMessage<WsAlgoOrderEntry> = serde_json::from_value(value)?;
                if let Some(snapshot) = state.update_algo_orders(&message.data).await {
                    let _ = self.tx.send(Command::AccountSnapshot(snapshot));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_event(&self, event: WsEventMessage) -> Result<(), anyhow::Error> {
        match event.event.as_str() {
            "login" => {
                if event.code != "0" {
                    return Err(anyhow!(
                        "okx business ws login failed (code {}): {}",
                        event.code,
                        event.msg
                    ));
                }
            }
            "error" => {
                self.emit_error(format!(
                    "okx business ws error (code {}): {}",
                    event.code, event.msg
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

fn build_regular_trade_response(
    request: &TradeRequest,
    response: TradeOrderResponse,
) -> TradeResponse {
    let mut success = response.code == "0";
    let mut message = if response.msg.is_empty() {
        String::new()
    } else {
        response.msg
    };
    let mut order_id = None;

    for entry in &response.data {
        if order_id.is_none() {
            order_id = Some(entry.ord_id.clone());
        }
        if entry.s_code != "0" {
            success = false;
            if !entry.s_msg.is_empty() {
                message = entry.s_msg.clone();
            }
        }
    }

    if success {
        let side = request.side.as_okx_side().to_uppercase();
        message = match &order_id {
            Some(ord_id) => format!(
                "OKX 已提交订单 {ord_id} {side} {} {:.4} @ {:.4}",
                request.inst_id, request.size, request.price
            ),
            None => format!(
                "OKX 已提交 {side} {} {:.4} @ {:.4}",
                request.inst_id, request.size, request.price
            ),
        };
    } else if message.is_empty() {
        message = "OKX 下单失败".to_string();
    }

    TradeResponse {
        inst_id: request.inst_id.clone(),
        side: request.side,
        price: request.price,
        size: request.size,
        order_id,
        message,
        success,
        operator: request.operator.clone(),
        pos_side: request.pos_side.clone(),
        leverage: request.leverage,
        kind: request.kind,
    }
}

fn build_algo_trade_response(request: &TradeRequest, response: AlgoOrderResponse) -> TradeResponse {
    let mut success = response.code == "0";
    let mut message = if response.msg.is_empty() {
        String::new()
    } else {
        response.msg
    };
    let mut order_id = None;

    for entry in &response.data {
        if order_id.is_none() {
            order_id = Some(entry.algo_id.clone());
        }
        if entry.s_code != "0" {
            success = false;
            if !entry.s_msg.is_empty() {
                message = entry.s_msg.clone();
            }
        }
    }

    if success {
        let label = match request.kind {
            TradeOrderKind::TakeProfit => "止盈",
            TradeOrderKind::StopLoss => "止损",
            TradeOrderKind::Regular => "策略",
        };
        let side = request.side.as_okx_side().to_uppercase();
        message = match &order_id {
            Some(ord_id) => format!(
                "OKX 已提交{label}策略 {ord_id} {side} {} {:.4} @ {:.4}",
                request.inst_id, request.size, request.price
            ),
            None => format!(
                "OKX 已提交{label}策略 {side} {} {:.4} @ {:.4}",
                request.inst_id, request.size, request.price
            ),
        };
    } else if message.is_empty() {
        message = "OKX 策略下单失败".to_string();
    }

    TradeResponse {
        inst_id: request.inst_id.clone(),
        side: request.side,
        price: request.price,
        size: request.size,
        order_id,
        message,
        success,
        operator: request.operator.clone(),
        pos_side: request.pos_side.clone(),
        leverage: request.leverage,
        kind: request.kind,
    }
}

fn build_cancel_response(
    request: &CancelOrderRequest,
    response: CancelOrderResponse,
) -> CancelResponse {
    let mut success = response.code == "0";
    let mut message = if response.msg.is_empty() {
        String::new()
    } else {
        response.msg
    };
    let mut ord_id = request.ord_id.clone();
    let mut inst_id = request.inst_id.clone();

    for entry in response.data {
        if let Some(inst) = entry.inst_id {
            if !inst.is_empty() {
                inst_id = inst;
            }
        }
        if !entry.ord_id.is_empty() {
            ord_id = entry.ord_id.clone();
        }
        if entry.s_code != "0" {
            success = false;
            if !entry.s_msg.is_empty() {
                message = entry.s_msg.clone();
            }
        }
    }

    if success {
        message = format!("OKX 已取消订单 {ord_id}");
    } else if message.is_empty() {
        message = format!("OKX 撤单失败 {ord_id}");
    }

    CancelResponse {
        inst_id,
        ord_id,
        message,
        success,
        operator: request.operator.clone(),
        pos_side: request.pos_side.clone(),
    }
}

fn build_algo_cancel_response(
    request: &CancelOrderRequest,
    response: CancelAlgoResponse,
) -> CancelResponse {
    let mut success = response.code == "0";
    let mut message = if response.msg.is_empty() {
        String::new()
    } else {
        response.msg
    };
    let mut ord_id = request.ord_id.clone();
    let mut inst_id = request.inst_id.clone();

    for entry in response.data {
        if let Some(inst) = entry.inst_id {
            if !inst.is_empty() {
                inst_id = inst;
            }
        }
        if !entry.algo_id.is_empty() {
            ord_id = entry.algo_id.clone();
        }
        if entry.s_code != "0" {
            success = false;
            if !entry.s_msg.is_empty() {
                message = entry.s_msg.clone();
            }
        }
    }

    if success {
        message = format!("OKX 已取消策略订单 {ord_id}");
    } else if message.is_empty() {
        message = format!("OKX 撤销策略失败 {ord_id}");
    }

    CancelResponse {
        inst_id,
        ord_id,
        message,
        success,
        operator: request.operator.clone(),
        pos_side: request.pos_side.clone(),
    }
}

fn sign_payload(
    secret: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body: &str,
) -> Result<String, anyhow::Error> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|err| anyhow!("invalid OKX secret key: {err}"))?;
    let payload = format!("{timestamp}{method}{path}{body}");
    mac.update(payload.as_bytes());
    Ok(BASE64.encode(mac.finalize().into_bytes()))
}

fn format_float(value: f64) -> String {
    let mut repr = format!("{value:.8}");
    while repr.contains('.') && repr.ends_with('0') {
        repr.pop();
    }
    if repr.ends_with('.') {
        repr.push('0');
    }
    repr
}

fn format_leverage_display(value: f64) -> String {
    let mut repr = format!("{value:.4}");
    while repr.contains('.') && repr.ends_with('0') {
        repr.pop();
    }
    if repr.ends_with('.') {
        repr.pop();
    }
    if repr.is_empty() {
        repr.push('0');
    }
    repr
}

fn pos_side_for(inst_id: &str, side: TradeSide) -> Option<&'static str> {
    let upper = inst_id.to_ascii_uppercase();
    if upper.ends_with("-SWAP") || upper.ends_with("-FUTURES") {
        return Some(match side {
            TradeSide::Buy => "long",
            TradeSide::Sell => "short",
        });
    }
    None
}

fn sanitize_order_tag(tag: &Option<String>) -> Option<String> {
    let raw = tag.as_ref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let filtered: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(16)
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TradeOrderRequest {
    inst_id: String,
    td_mode: String,
    side: String,
    ord_type: String,
    sz: String,
    px: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pos_side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reduce_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

impl TradeOrderRequest {
    fn from_request(request: &TradeRequest, td_mode: &str) -> Self {
        TradeOrderRequest {
            inst_id: request.inst_id.clone(),
            td_mode: td_mode.to_string(),
            side: request.side.as_okx_side().to_string(),
            ord_type: "limit".to_string(),
            sz: format_float(request.size),
            px: format_float(request.price),
            pos_side: request
                .pos_side
                .clone()
                .or_else(|| pos_side_for(&request.inst_id, request.side).map(|s| s.to_string())),
            reduce_only: if request.reduce_only {
                Some(true)
            } else {
                None
            },
            tag: sanitize_order_tag(&request.tag),
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AlgoOrderRequest {
    inst_id: String,
    td_mode: String,
    side: String,
    ord_type: String,
    sz: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pos_side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reduce_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tp_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tp_ord_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sl_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sl_ord_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

impl AlgoOrderRequest {
    fn from_request(request: &TradeRequest, td_mode: &str) -> Self {
        let price = format_float(request.price);
        let (tp_trigger_px, tp_ord_px, sl_trigger_px, sl_ord_px) = match request.kind {
            TradeOrderKind::TakeProfit => (Some(price.clone()), Some(price.clone()), None, None),
            TradeOrderKind::StopLoss => (None, None, Some(price.clone()), Some(price.clone())),
            TradeOrderKind::Regular => (None, None, None, None),
        };
        let pos_side = request
            .pos_side
            .clone()
            .or_else(|| pos_side_for(&request.inst_id, request.side).map(|s| s.to_string()));
        AlgoOrderRequest {
            inst_id: request.inst_id.clone(),
            td_mode: td_mode.to_string(),
            side: request.side.as_okx_side().to_string(),
            ord_type: "conditional".to_string(),
            sz: format_float(request.size),
            pos_side,
            reduce_only: if request.reduce_only {
                Some(true)
            } else {
                None
            },
            tp_trigger_px,
            tp_ord_px,
            sl_trigger_px,
            sl_ord_px,
            tag: sanitize_order_tag(&request.tag),
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelOrderPayload {
    inst_id: String,
    ord_id: String,
}

impl CancelOrderPayload {
    fn from_request(request: &CancelOrderRequest) -> Self {
        CancelOrderPayload {
            inst_id: request.inst_id.clone(),
            ord_id: request.ord_id.clone(),
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct CancelAlgoPayload(Vec<CancelAlgoPayloadEntry>);

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelAlgoPayloadEntry {
    algo_id: String,
    inst_id: String,
}

impl CancelAlgoPayload {
    fn new(request: &CancelOrderRequest) -> Self {
        CancelAlgoPayload(vec![CancelAlgoPayloadEntry {
            algo_id: request.ord_id.clone(),
            inst_id: request.inst_id.clone(),
        }])
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetLeveragePayload {
    inst_id: String,
    lever: String,
    mgn_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pos_side: Option<String>,
}

impl SetLeveragePayload {
    fn from_request(request: &SetLeverageRequest, td_mode: &str) -> Self {
        SetLeveragePayload {
            inst_id: request.inst_id.clone(),
            lever: format_leverage_display(request.lever),
            mgn_mode: td_mode.to_string(),
            pos_side: request.pos_side.clone(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradeOrderResponse {
    code: String,
    msg: String,
    data: Vec<TradeOrderResponseData>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradeOrderResponseData {
    ord_id: String,
    #[serde(rename = "clOrdId", default)]
    _cl_ord_id: Option<String>,
    s_code: String,
    s_msg: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlgoOrderResponse {
    code: String,
    msg: String,
    data: Vec<AlgoOrderResponseData>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlgoOrderResponseData {
    algo_id: String,
    s_code: String,
    s_msg: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetLeverageResponse {
    code: String,
    msg: String,
    #[serde(default)]
    _data: Vec<SetLeverageResponseEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetLeverageResponseEntry {
    #[allow(dead_code)]
    inst_id: Option<String>,
    #[allow(dead_code)]
    lever: Option<String>,
    #[allow(dead_code)]
    mgn_mode: Option<String>,
    #[allow(dead_code)]
    pos_side: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelOrderResponse {
    code: String,
    msg: String,
    data: Vec<CancelOrderResponseData>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelOrderResponseData {
    ord_id: String,
    #[serde(default)]
    inst_id: Option<String>,
    s_code: String,
    s_msg: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelAlgoResponse {
    code: String,
    msg: String,
    data: Vec<CancelAlgoResponseData>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelAlgoResponseData {
    algo_id: String,
    #[serde(default)]
    inst_id: Option<String>,
    s_code: String,
    s_msg: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginRequest {
    op: String,
    args: Vec<LoginArgs>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginArgs {
    api_key: String,
    passphrase: String,
    timestamp: String,
    sign: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PrivateSubscribeMessage {
    id: Option<String>,
    op: String,
    args: Vec<PrivateSubscribeArg>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PrivateSubscribeArg {
    channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    inst_type: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BusinessSubscribeMessage {
    id: Option<String>,
    op: String,
    args: Vec<BusinessSubscribeArg>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BusinessSubscribeArg {
    channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    inst_type: Option<String>,
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

pub async fn fetch_mark_price(client: &Client, inst_id: &str) -> Result<f64, anyhow::Error> {
    let response = client
        .get(MARK_PRICE_ENDPOINT)
        .query(&[("instId", inst_id)])
        .send()
        .await
        .with_context(|| format!("requesting mark price for {inst_id}"))?
        .error_for_status()
        .with_context(|| format!("mark price response status for {inst_id}"))?
        .json::<MarkPriceResponse>()
        .await
        .with_context(|| format!("decoding mark price for {inst_id}"))?;
    if response.code != "0" {
        return Err(anyhow!(
            "okx mark price error for {} (code {}): {}",
            inst_id,
            response.code,
            response.msg
        ));
    }
    if let Some(entry) = response.data.first() {
        let mark_px = entry
            .mark_px
            .parse::<f64>()
            .with_context(|| format!("parsing mark price '{}' for {}", entry.mark_px, inst_id))?;
        Ok(mark_px)
    } else {
        Err(anyhow!("no mark price data for {}", inst_id))
    }
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

pub async fn fetch_market_info(
    mgn_mode: &str,
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<HashMap<String, MarketInfo>, anyhow::Error> {
    let mut markets = fetch_account_instruments(config, inst_ids).await?;
    let al = fetch_account_leverage(mgn_mode, config, inst_ids).await?;

    for (inst_id, market) in markets.iter_mut() {
        if let Some(lever) = al.get(inst_id) {
            market.lever = *lever;
        }
    }
    Ok(markets)
}

async fn fetch_account_leverage(
    mgn_mode: &str,
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<HashMap<String, f64>, anyhow::Error> {
    let client = build_http_client()?;
    let query = vec![
        ("mgnMode", mgn_mode.to_string()),
        ("instId", inst_ids.join(",")),
    ];
    let response: LeverageInfoResponse =
        signed_get(&client, config, ACCOUNT_LEVERAGE_ENDPOINT, &query).await?;
    if response.code != "0" {
        return Err(anyhow!(
            "okx positions error (code {}): {}",
            response.code,
            response.msg
        ));
    }
    let filter = inst_filter(inst_ids);
    let mut leverages = HashMap::new();
    for entry in response.data {
        if let Some(filter) = &filter {
            if !filter.contains(&entry.inst_id.to_ascii_uppercase()) {
                continue;
            }
        }
        if let Ok(lever) = entry.lever.parse::<f64>() {
            leverages.insert(entry.inst_id.clone(), lever);
        }
    }
    Ok(leverages)
}

async fn fetch_account_instruments(
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<HashMap<String, MarketInfo>, anyhow::Error> {
    let client = build_http_client()?;
    let mut instruments = HashMap::new();
    for inst_id in inst_ids {
        let query = vec![
            ("instId", inst_id.to_string()),
            ("instType", "SWAP".to_string()),
        ];
        let response: InstrumentsResponse =
            signed_get(&client, config, INSTRUMENTS_ENDPOINT, &query).await?;
        if response.code != "0" {
            return Err(anyhow!(
                "okx instruments error (code {}): {}",
                response.code,
                response.msg
            ));
        }
        for entry in response.data {
            let ct_val = entry.ct_val.parse::<f64>().unwrap_or(0.0);
            // let min_size = entry
            //     .min_sz
            //     .as_deref()
            //     .and_then(|value| value.parse::<f64>().ok())
            //     .unwrap_or(0.0);
            instruments.insert(
                entry.inst_id.clone(),
                MarketInfo {
                    ct_val,
                    // ct_val_ccy: entry.ct_val_ccy.clone(),
                    // min_size,
                    lever: 1.0,
                },
            );
        }
    }
    Ok(instruments)
}

pub async fn fetch_account_snapshot(
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<AccountSnapshot, anyhow::Error> {
    let client = build_http_client()?;
    let unique_inst_ids = unique_inst_ids(inst_ids);
    let mut positions = fetch_positions(&client, config, inst_ids).await?;
    let mut open_orders = fetch_open_orders(&client, config, &unique_inst_ids).await?;
    let mut algo_orders = fetch_open_algo_orders(&client, config, &unique_inst_ids).await?;
    open_orders.append(&mut algo_orders);
    open_orders.sort_by(|a, b| {
        b.create_time
            .cmp(&a.create_time)
            .then_with(|| a.inst_id.cmp(&b.inst_id))
            .then_with(|| a.ord_id.cmp(&b.ord_id))
    });
    positions.sort_by(|a, b| {
        b.create_time
            .cmp(&a.create_time)
            .then_with(|| a.inst_id.cmp(&b.inst_id))
            .then_with(|| a.pos_side.cmp(&b.pos_side))
    });
    let balance = fetch_account_balances(&client, config).await?;
    Ok(AccountSnapshot {
        positions,
        open_orders,
        balance,
    })
}

async fn fetch_positions(
    client: &Client,
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<Vec<PositionInfo>, anyhow::Error> {
    let response: PositionsResponse = signed_get(client, config, POSITIONS_ENDPOINT, &[]).await?;
    if response.code != "0" {
        return Err(anyhow!(
            "okx positions error (code {}): {}",
            response.code,
            response.msg
        ));
    }
    let filter = inst_filter(inst_ids);
    let mut positions = Vec::new();
    for entry in response.data {
        if let Some(filter) = &filter {
            if !filter.contains(&entry.inst_id.to_ascii_uppercase()) {
                continue;
            }
        }
        let size = entry.pos.parse::<f64>().unwrap_or(0.0);
        if size == 0.0 {
            continue;
        }
        let avg_px = entry.avg_px.parse::<f64>().ok();
        let lever = parse_optional_float(entry.lever.clone());
        let upl = parse_optional_float(entry.upl.clone());
        let upl_ratio = parse_optional_float(entry.upl_ratio.clone());
        let imr = parse_optional_float(entry.imr.clone()).unwrap_or(0.0);
        let create_time = parse_optional_i64(entry.c_time.clone());

        positions.push(PositionInfo {
            inst_id: entry.inst_id,
            pos_side: entry.pos_side,
            size,
            avg_px,
            lever,
            upl,
            upl_ratio,
            imr,
            create_time,
        });
    }
    Ok(positions)
}

async fn fetch_open_orders(
    client: &Client,
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<Vec<PendingOrderInfo>, anyhow::Error> {
    if inst_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut open_orders = Vec::new();
    for inst_id in inst_ids {
        let query = vec![("instId", inst_id.clone())];
        let response: PendingOrdersResponse =
            signed_get(client, config, ORDERS_PENDING_ENDPOINT, &query).await?;
        if response.code != "0" {
            return Err(anyhow!(
                "okx pending orders error for {} (code {}): {}",
                inst_id,
                response.code,
                response.msg
            ));
        }
        for entry in response.data {
            let size = entry.sz.parse::<f64>().unwrap_or(0.0);
            let price = parse_optional_float(entry.px);
            let lever = parse_optional_float(entry.lever.clone());
            let create_time = parse_optional_i64(entry.c_time.clone());
            open_orders.push(PendingOrderInfo {
                inst_id: entry.inst_id,
                ord_id: entry.ord_id,
                side: entry.side,
                pos_side: entry.pos_side,
                price,
                size,
                state: entry.state,
                reduce_only: parse_bool_flag(&entry.reduce_only),
                tag: entry.tag,
                lever,
                trigger_price: None,
                kind: TradeOrderKind::Regular,
                create_time,
            });
        }
    }
    Ok(open_orders)
}

async fn fetch_open_algo_orders(
    client: &Client,
    config: &TradingConfig,
    inst_ids: &[String],
) -> Result<Vec<PendingOrderInfo>, anyhow::Error> {
    if inst_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut open_orders = Vec::new();
    for inst_id in inst_ids {
        let mut query = vec![
            ("instId", inst_id.clone()),
            ("ordType", "conditional".to_string()),
        ];
        if let Some(inst_type) = inst_type_from_inst_id(inst_id) {
            query.push(("instType", inst_type.to_string()));
        }
        let response: PendingAlgoOrdersResponse =
            signed_get(client, config, ORDERS_ALGO_PENDING_ENDPOINT, &query).await?;
        if response.code != "0" {
            return Err(anyhow!(
                "okx pending algo orders error for {} (code {}): {}",
                inst_id,
                response.code,
                response.msg
            ));
        }
        for entry in response.data {
            if is_order_active(&entry.state) {
                open_orders.push(build_pending_order_from_algo(entry).await);
            }
        }
    }
    Ok(open_orders)
}

async fn build_pending_order_from_algo(entry: OkxPendingAlgoOrderEntry) -> PendingOrderInfo {
    let size = entry.sz.parse::<f64>().unwrap_or(0.0);
    let price = parse_optional_float(
        entry
            .tp_ord_px
            .filter(|s| !s.trim().is_empty())
            .or(entry.sl_ord_px.clone().filter(|s| !s.trim().is_empty()))
            .or(entry.order_px.clone()),
    );
    let trigger_price = parse_optional_float(
        entry
            .tp_trigger_px
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or(entry.sl_trigger_px.clone().filter(|s| !s.trim().is_empty()))
            .or(entry.trigger_px.clone()),
    );
    let reduce_only = parse_bool_flag(&entry.reduce_only);
    let lever = parse_optional_float(entry.lever.clone());
    let kind = determine_trade_order_kind(
        entry.tp_trigger_px.as_deref(),
        entry.sl_trigger_px.as_deref(),
    );
    let create_time = parse_optional_i64(entry.c_time.clone());
    PendingOrderInfo {
        inst_id: entry.inst_id,
        ord_id: entry.algo_id,
        side: entry.side,
        pos_side: entry.pos_side,
        price,
        size,
        state: entry.state,
        reduce_only,
        tag: entry.tag,
        lever,
        trigger_price,
        kind,
        create_time,
    }
}

async fn fetch_account_balances(
    client: &Client,
    config: &TradingConfig,
) -> Result<AccountBalance, anyhow::Error> {
    let mut balance = AccountBalance::default();
    let response: AccountBalanceResponse =
        signed_get(client, config, ACCOUNT_BALANCE_ENDPOINT, &[]).await?;
    if response.code != "0" {
        return Err(anyhow!(
            "okx balance error (code {}): {}",
            response.code,
            response.msg
        ));
    }
    balance.total_equity = parse_optional_float(
        response
            .data
            .get(0)
            .and_then(|entry| entry.total_eq.clone()),
    );
    balance.delta =
        aggregate_balance_details(response.data.iter().flat_map(|entry| entry.details.iter()));
    Ok(balance)
}

pub async fn fetch_long_short_account_ratio(
    client: &Client,
    inst_id: &str,
    period: &str,
    config: &TradingConfig,
) -> Result<Vec<LongShortRatio>, anyhow::Error> {
    let query = vec![("ccy", inst_id.to_string()), ("period", period.to_string())];
    let response: OkxResponse<Vec<LongShortRatioEntry>> =
        signed_get(client, config, LONG_SHORT_ACCOUNT_RATIO_ENDPOINT, &query).await?;
    if response.code != "0" {
        return Err(anyhow!(
            "okx long-short ratio error for {} (code {}): {}",
            inst_id,
            response.code,
            response.msg
        ));
    }
    let mut ratios = Vec::new();
    for entry in response.data {
        let ts = entry.ts.parse::<i64>().unwrap_or(0);
        let ratio = parse_float_str(&entry.ratio).unwrap_or(0.0);
        ratios.push(LongShortRatio { ts, ratio });
    }
    ratios.sort_by_key(|r| r.ts);
    Ok(ratios)
}

fn parse_optional_float(value: Option<String>) -> Option<f64> {
    value.as_deref().and_then(parse_float_str)
}

fn parse_float_str(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        trimmed.parse::<f64>().ok()
    }
}

fn parse_bool_flag(value: &Option<serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::String(text)) => {
            let lowered = text.trim().to_ascii_lowercase();
            matches!(lowered.as_str(), "true" | "1")
        }
        Some(serde_json::Value::Number(num)) => num.as_i64().map(|n| n != 0).unwrap_or(false),
        _ => false,
    }
}

fn parse_i64_str(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        trimmed.parse::<i64>().ok()
    }
}

const MIN_BALANCE_VALUE_USD: f64 = 1.0;

fn aggregate_balance_details<'a, I>(details: I) -> Vec<AccountBalanceDelta>
where
    I: Iterator<Item = &'a BalanceDetail>,
{
    struct BalanceAggregate {
        delta: AccountBalanceDelta,
        usd_value: Option<f64>,
    }

    let mut map: HashMap<String, BalanceAggregate> = HashMap::new();
    for detail in details {
        if let Some(avail_eq) = parse_optional_float(detail.avail_eq.clone()) {
            if avail_eq <= 0.0 {
                continue;
            }
        }
        let entry = map
            .entry(detail.ccy.clone())
            .or_insert_with(|| BalanceAggregate {
                delta: AccountBalanceDelta {
                    currency: detail.ccy.clone(),
                    cash_balance: None,
                    equity: None,
                    available: None,
                },
                usd_value: None,
            });
        accumulate_balance(
            &mut entry.delta.cash_balance,
            parse_optional_float(detail.cash_bal.clone()),
        );
        accumulate_balance(
            &mut entry.delta.equity,
            parse_optional_float(detail.eq.clone()),
        );
        let available = parse_optional_float(detail.avail_eq.clone())
            .or_else(|| parse_optional_float(detail.avail_bal.clone()));
        accumulate_balance(&mut entry.delta.available, available);
        accumulate_balance(
            &mut entry.usd_value,
            parse_optional_float(detail.eq_usd.clone()),
        );
    }
    let mut balances: Vec<_> = map.into_values().collect();
    balances.retain(|entry| {
        entry
            .usd_value
            .map(|value| value >= MIN_BALANCE_VALUE_USD)
            .unwrap_or(true)
    });
    balances.sort_by(|a, b| a.delta.currency.cmp(&b.delta.currency));
    balances.into_iter().map(|entry| entry.delta).collect()
}

fn accumulate_balance(target: &mut Option<f64>, value: Option<f64>) {
    if let Some(val) = value {
        match target {
            Some(existing) => {
                *existing += val;
            }
            None => {
                *target = Some(val);
            }
        }
    }
}

fn parse_optional_i64(value: Option<String>) -> Option<i64> {
    value.as_deref().and_then(parse_i64_str)
}

fn parse_okx_trade_side(value: &str) -> Option<TradeSide> {
    if value.eq_ignore_ascii_case("buy") {
        Some(TradeSide::Buy)
    } else if value.eq_ignore_ascii_case("sell") {
        Some(TradeSide::Sell)
    } else {
        None
    }
}

fn build_trade_fill(entry: &WsOrderEntry) -> Option<TradeFill> {
    let fill_size = parse_optional_float(entry.fill_sz.clone()).unwrap_or(0.0);
    if fill_size <= 0.0 {
        return None;
    }
    let side = parse_okx_trade_side(&entry.side)?;
    let price = parse_optional_float(entry.fill_px.clone())
        .or_else(|| parse_optional_float(entry.avg_px.clone()))
        .or_else(|| parse_optional_float(entry.px.clone()))
        .unwrap_or(0.0);
    Some(TradeFill {
        inst_id: entry.inst_id.clone(),
        side,
        price,
        size: fill_size,
        order_id: entry.ord_id.clone(),
        pos_side: entry.pos_side.clone(),
        trade_id: entry.trade_id.clone(),
        exec_type: entry.exec_type.clone(),
        fill_time: parse_optional_i64(entry.fill_time.clone()),
        fee: parse_optional_float(entry.fill_fee.clone()),
        fee_currency: entry.fill_fee_ccy.clone(),
        pnl: parse_optional_float(entry.pnl.clone()),
        acc_fill_size: parse_optional_float(entry.acc_fill_sz.clone()),
        avg_price: parse_optional_float(entry.avg_px.clone()),
        leverage: parse_optional_float(entry.lever.clone()),
        tag: entry.tag.clone(),
    })
}

fn determine_trade_order_kind(
    tp_trigger: Option<&str>,
    sl_trigger: Option<&str>,
) -> TradeOrderKind {
    if tp_trigger
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        TradeOrderKind::TakeProfit
    } else if sl_trigger
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        TradeOrderKind::StopLoss
    } else {
        TradeOrderKind::Regular
    }
}

fn inst_type_from_inst_id(inst_id: &str) -> Option<&'static str> {
    let upper = inst_id.to_ascii_uppercase();
    if upper.ends_with("-SWAP") {
        Some("SWAP")
    } else if upper.ends_with("-FUTURES") {
        Some("FUTURES")
    } else if upper.ends_with("-SPOT") {
        Some("SPOT")
    } else {
        None
    }
}

pub fn inst_filter(inst_ids: &[String]) -> Option<HashSet<String>> {
    if inst_ids.is_empty() {
        None
    } else {
        Some(
            inst_ids
                .iter()
                .map(|inst| inst.to_ascii_uppercase())
                .collect(),
        )
    }
}

fn unique_inst_ids(inst_ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut uniques = Vec::new();
    for inst in inst_ids {
        let key = inst.to_ascii_uppercase();
        if seen.insert(key) {
            uniques.push(inst.clone());
        }
    }
    uniques
}

async fn signed_get<T>(
    client: &Client,
    config: &TradingConfig,
    path: &str,
    query: &[(&str, String)],
) -> Result<T, anyhow::Error>
where
    T: DeserializeOwned,
{
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let query_refs: Vec<(&str, &str)> = query
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let mut request_path = path.to_string();
    if !query_refs.is_empty() {
        let query_string = serde_urlencoded::to_string(&query_refs)?;
        request_path.push('?');
        request_path.push_str(&query_string);
    }
    let signature = sign_payload(&config.api_secret, &timestamp, "GET", &request_path, "")?;
    let mut request = client
        .get(format!("{OKX_API_BASE}{path}"))
        .header("OK-ACCESS-KEY", &config.api_key)
        .header("OK-ACCESS-PASSPHRASE", &config.passphrase)
        .header("OK-ACCESS-TIMESTAMP", &timestamp)
        .header("OK-ACCESS-SIGN", signature);
    if !query_refs.is_empty() {
        request = request.query(&query_refs);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("requesting OKX {}", path))?
        .error_for_status()
        .with_context(|| format!("OKX status {}", path))?
        .json::<T>()
        .await
        .with_context(|| format!("decoding OKX response for {}", path))?;
    Ok(response)
}

fn parse_ws_event(text: &str) -> Option<WsEventMessage> {
    serde_json::from_str::<WsEventMessage>(text).ok()
}

fn current_ws_timestamp() -> String {
    let now = Utc::now();
    let seconds = now.timestamp() as f64;
    let nanos = now.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
    format!("{:.3}", seconds + nanos)
}

fn is_order_active(state: &str) -> bool {
    match state {
        "live" | "partially_filled" | "not_triggered" | "partially_filled_not_triggered" => true,
        other => {
            let lowered = other.to_ascii_lowercase();
            matches!(
                lowered.as_str(),
                "live" | "partially_filled" | "not_triggered" | "partially_filled_not_triggered"
            )
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionsResponse {
    code: String,
    msg: String,
    data: Vec<OkxPositionEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OkxPositionEntry {
    inst_id: String,
    #[serde(default)]
    pos_side: Option<String>,
    #[serde(default)]
    pos: String,
    #[serde(default)]
    avg_px: String,
    #[serde(default)]
    lever: Option<String>,
    #[serde(default)]
    upl: Option<String>,
    #[serde(rename = "uplRatio", default)]
    upl_ratio: Option<String>,
    #[serde(default)]
    imr: Option<String>,
    #[serde(rename = "cTime", default)]
    c_time: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingOrdersResponse {
    code: String,
    msg: String,
    data: Vec<OkxPendingOrderEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OkxPendingOrderEntry {
    inst_id: String,
    ord_id: String,
    side: String,
    #[serde(default)]
    pos_side: Option<String>,
    #[serde(default)]
    px: Option<String>,
    sz: String,
    state: String,
    #[serde(rename = "reduceOnly", default)]
    reduce_only: Option<serde_json::Value>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    lever: Option<String>,
    #[serde(rename = "cTime", default)]
    c_time: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingAlgoOrdersResponse {
    code: String,
    msg: String,
    data: Vec<OkxPendingAlgoOrderEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OkxPendingAlgoOrderEntry {
    inst_id: String,
    algo_id: String,
    side: String,
    #[serde(default)]
    pos_side: Option<String>,
    sz: String,
    state: String,
    #[serde(rename = "reduceOnly", default)]
    reduce_only: Option<serde_json::Value>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    lever: Option<String>,
    #[serde(default)]
    trigger_px: Option<String>,
    #[serde(default)]
    order_px: Option<String>,
    #[serde(default)]
    tp_trigger_px: Option<String>,
    #[serde(default)]
    tp_ord_px: Option<String>,
    #[serde(default)]
    sl_trigger_px: Option<String>,
    #[serde(default)]
    sl_ord_px: Option<String>,
    #[serde(rename = "cTime", default)]
    c_time: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountBalanceResponse {
    code: String,
    msg: String,
    data: Vec<AccountBalanceData>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountBalanceData {
    #[serde(default)]
    total_eq: Option<String>,
    #[serde(default)]
    details: Vec<BalanceDetail>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BalanceDetail {
    ccy: String,
    #[serde(default)]
    cash_bal: Option<String>,
    #[serde(default)]
    avail_bal: Option<String>,
    #[serde(default)]
    avail_eq: Option<String>,
    #[serde(default)]
    eq: Option<String>,
    #[serde(default)]
    eq_usd: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstrumentsResponse {
    code: String,
    msg: String,
    data: Vec<InstrumentsEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstrumentsEntry {
    inst_id: String,
    ct_val: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeverageInfoResponse {
    code: String,
    msg: String,
    data: Vec<LeverageInfoEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeverageInfoEntry {
    inst_id: String,
    lever: String,
}

#[derive(Debug, serde::Deserialize)]
struct WsEventMessage {
    event: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    msg: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrivateDataMessage<T> {
    #[serde(rename = "arg")]
    _arg: PrivateChannelArg,
    data: Vec<T>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrivateChannelArg {
    #[allow(dead_code)]
    channel: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsPositionEntry {
    inst_id: String,
    #[serde(default)]
    pos_side: Option<String>,
    #[serde(default)]
    pos: String,
    #[serde(default)]
    avg_px: String,
    #[serde(default)]
    lever: Option<String>,
    #[serde(default)]
    upl: Option<String>,
    #[serde(rename = "uplRatio", default)]
    upl_ratio: Option<String>,
    #[serde(default)]
    imr: Option<String>,
    #[serde(rename = "cTime", default)]
    c_time: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsOrderEntry {
    inst_id: String,
    ord_id: String,
    side: String,
    #[serde(default)]
    pos_side: Option<String>,
    #[serde(default)]
    px: Option<String>,
    sz: String,
    state: String,
    #[serde(rename = "reduceOnly", default)]
    reduce_only: Option<serde_json::Value>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    lever: Option<String>,
    #[serde(default)]
    avg_px: Option<String>,
    #[serde(default)]
    acc_fill_sz: Option<String>,
    #[serde(default)]
    fill_px: Option<String>,
    #[serde(default)]
    fill_sz: Option<String>,
    #[serde(default)]
    fill_time: Option<String>,
    #[serde(default)]
    fill_fee: Option<String>,
    #[serde(default)]
    fill_fee_ccy: Option<String>,
    #[serde(default)]
    pnl: Option<String>,
    #[serde(default)]
    trade_id: Option<String>,
    #[serde(default)]
    exec_type: Option<String>,
    #[serde(rename = "cTime", default)]
    c_time: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsAccountEntry {
    #[serde(default)]
    total_eq: Option<String>,
    #[serde(default)]
    details: Vec<BalanceDetail>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsAlgoOrderEntry {
    inst_id: String,
    algo_id: String,
    side: String,
    #[serde(default)]
    pos_side: Option<String>,
    sz: String,
    state: String,
    #[serde(rename = "reduceOnly", default)]
    reduce_only: Option<serde_json::Value>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    lever: Option<String>,
    #[serde(default)]
    trigger_px: Option<String>,
    #[serde(default)]
    order_px: Option<String>,
    #[serde(default)]
    tp_trigger_px: Option<String>,
    #[serde(default)]
    tp_ord_px: Option<String>,
    #[serde(default)]
    sl_trigger_px: Option<String>,
    #[serde(default)]
    sl_ord_px: Option<String>,
    #[serde(rename = "cTime", default)]
    c_time: Option<String>,
}

static GLOBAL_ACCOUNT_STATE: Lazy<Mutex<AccountState>> =
    Lazy::new(|| Mutex::new(AccountState::new(None)));

#[derive(Clone, Copy)]
pub struct SharedAccountState {
    inner: &'static Mutex<AccountState>,
}

impl SharedAccountState {
    pub fn global() -> Self {
        SharedAccountState {
            inner: &GLOBAL_ACCOUNT_STATE,
        }
    }

    pub async fn update_filter(&self, filter: Option<HashSet<String>>) {
        if filter.is_none() {
            return;
        }
        let mut state = self.inner.lock().await;
        state.update_filter(filter);
    }

    pub async fn seed(&self, snapshot: &AccountSnapshot) {
        let mut state = self.inner.lock().await;
        state.seed(snapshot);
    }

    pub async fn snapshot(&self) -> AccountSnapshot {
        let state = self.inner.lock().await;
        state.snapshot()
    }

    async fn update_positions(&self, entries: &[WsPositionEntry]) -> Option<AccountSnapshot> {
        let mut state = self.inner.lock().await;
        if state.update_positions(entries).await {
            Some(state.snapshot())
        } else {
            None
        }
    }

    async fn update_orders(&self, entries: &[WsOrderEntry]) -> Option<AccountSnapshot> {
        let mut state = self.inner.lock().await;
        if state.update_orders(entries).await {
            Some(state.snapshot())
        } else {
            None
        }
    }

    async fn update_algo_orders(&self, entries: &[WsAlgoOrderEntry]) -> Option<AccountSnapshot> {
        let mut state = self.inner.lock().await;
        if state.update_algo_orders(entries).await {
            Some(state.snapshot())
        } else {
            None
        }
    }

    async fn update_balances(&self, entries: &[WsAccountEntry]) -> Option<AccountSnapshot> {
        let mut state = self.inner.lock().await;
        if state.update_balances(entries) {
            Some(state.snapshot())
        } else {
            None
        }
    }
}
#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub ct_val: f64,
    pub lever: f64,
}

struct AccountState {
    filter: Option<HashSet<String>>,
    positions: HashMap<PositionKey, PositionInfo>,
    open_orders: HashMap<String, PendingOrderInfo>,
    balance: AccountBalance,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct PositionKey {
    inst_id: String,
    pos_side: Option<String>,
}

impl AccountState {
    fn new(filter: Option<HashSet<String>>) -> Self {
        AccountState {
            filter,
            positions: HashMap::new(),
            open_orders: HashMap::new(),
            balance: AccountBalance::default(),
        }
    }

    fn update_filter(&mut self, filter: Option<HashSet<String>>) {
        if let Some(mut incoming) = filter {
            match self.filter.as_mut() {
                Some(existing) => {
                    for inst in incoming.drain() {
                        existing.insert(inst);
                    }
                }
                None => {
                    self.filter = Some(incoming);
                }
            }
        }
    }

    fn seed(&mut self, snapshot: &AccountSnapshot) {
        self.positions.clear();
        self.open_orders.clear();
        self.balance = snapshot.balance.clone();
        for position in &snapshot.positions {
            if !self.accepts(&position.inst_id) {
                continue;
            }
            let key = PositionKey {
                inst_id: position.inst_id.clone(),
                pos_side: position.pos_side.clone(),
            };
            self.positions.insert(key, position.clone());
        }
        for order in &snapshot.open_orders {
            if !self.accepts(&order.inst_id) {
                continue;
            }
            self.open_orders.insert(order.ord_id.clone(), order.clone());
        }
    }

    fn accepts(&self, inst_id: &str) -> bool {
        if let Some(filter) = &self.filter {
            filter.contains(&inst_id.to_ascii_uppercase())
        } else {
            true
        }
    }

    async fn update_positions(&mut self, entries: &[WsPositionEntry]) -> bool {
        let mut changed = false;
        for entry in entries {
            if !self.accepts(&entry.inst_id) {
                continue;
            }
            let size = entry.pos.parse::<f64>().unwrap_or(0.0);
            let avg_px = parse_float_str(&entry.avg_px);
            let lever = parse_optional_float(entry.lever.clone());
            let upl = parse_optional_float(entry.upl.clone());
            let upl_ratio = parse_optional_float(entry.upl_ratio.clone());
            let imr = parse_optional_float(entry.imr.clone()).unwrap_or(0.0);
            let create_time = parse_optional_i64(entry.c_time.clone());
            let key = PositionKey {
                inst_id: entry.inst_id.clone(),
                pos_side: entry.pos_side.clone(),
            };
            if size == 0.0 {
                if self.positions.remove(&key).is_some() {
                    changed = true;
                }
                continue;
            }
            let entry_changed = match self.positions.get(&key) {
                Some(existing) => {
                    existing.size != size
                        || existing.avg_px != avg_px
                        || existing.lever != lever
                        || existing.upl != upl
                        || existing.upl_ratio != upl_ratio
                        || existing.create_time != create_time
                }
                None => true,
            };
            if entry_changed {
                self.positions.insert(
                    key,
                    PositionInfo {
                        inst_id: entry.inst_id.clone(),
                        pos_side: entry.pos_side.clone(),
                        size,
                        avg_px,
                        lever,
                        upl,
                        upl_ratio,
                        imr,
                        create_time,
                    },
                );
                changed = true;
            }
        }
        changed
    }

    async fn update_orders(&mut self, entries: &[WsOrderEntry]) -> bool {
        let mut changed = false;
        for entry in entries {
            if !self.accepts(&entry.inst_id) {
                continue;
            }
            if is_order_active(&entry.state) {
                let size = entry.sz.parse::<f64>().unwrap_or(0.0);
                let price = parse_optional_float(entry.px.clone());
                let reduce_only = parse_bool_flag(&entry.reduce_only);
                let lever = parse_optional_float(entry.lever.clone());
                let create_time = parse_optional_i64(entry.c_time.clone());
                let entry_changed = match self.open_orders.get(&entry.ord_id) {
                    Some(existing) => {
                        existing.size != size
                            || existing.price != price
                            || existing.state != entry.state
                            || existing.reduce_only != reduce_only
                            || existing.lever != lever
                            || existing.create_time != create_time
                    }
                    None => true,
                };
                if entry_changed {
                    self.open_orders.insert(
                        entry.ord_id.clone(),
                        PendingOrderInfo {
                            inst_id: entry.inst_id.clone(),
                            ord_id: entry.ord_id.clone(),
                            side: entry.side.clone(),
                            pos_side: entry.pos_side.clone(),
                            price,
                            size,
                            state: entry.state.clone(),
                            reduce_only,
                            tag: entry.tag.clone(),
                            lever,
                            trigger_price: None,
                            kind: TradeOrderKind::Regular,
                            create_time,
                        },
                    );
                    changed = true;
                }
            } else if self.open_orders.remove(&entry.ord_id).is_some() {
                changed = true;
            }
        }
        changed
    }

    async fn update_algo_orders(&mut self, entries: &[WsAlgoOrderEntry]) -> bool {
        let mut changed = false;
        for entry in entries {
            if !self.accepts(&entry.inst_id) {
                continue;
            }
            if is_order_active(&entry.state) {
                let size = entry.sz.parse::<f64>().unwrap_or(0.0);
                let price = parse_optional_float(
                    entry
                        .tp_ord_px
                        .clone()
                        .or(entry.sl_ord_px.clone())
                        .or(entry.order_px.clone()),
                );
                let trigger_price = parse_optional_float(
                    entry
                        .tp_trigger_px
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .or(entry.sl_trigger_px.clone().filter(|s| !s.trim().is_empty()))
                        .or(entry.trigger_px.clone()),
                );
                let reduce_only = parse_bool_flag(&entry.reduce_only);
                let lever = parse_optional_float(entry.lever.clone());
                let kind = determine_trade_order_kind(
                    entry.tp_trigger_px.as_deref(),
                    entry.sl_trigger_px.as_deref(),
                );
                let create_time = parse_optional_i64(entry.c_time.clone());
                let entry_changed = match self.open_orders.get(&entry.algo_id) {
                    Some(existing) => {
                        existing.size != size
                            || existing.price != price
                            || existing.trigger_price != trigger_price
                            || existing.state != entry.state
                            || existing.reduce_only != reduce_only
                            || existing.lever != lever
                            || existing.kind != kind
                            || existing.create_time != create_time
                    }
                    None => true,
                };
                if entry_changed {
                    self.open_orders.insert(
                        entry.algo_id.clone(),
                        PendingOrderInfo {
                            inst_id: entry.inst_id.clone(),
                            ord_id: entry.algo_id.clone(),
                            side: entry.side.clone(),
                            pos_side: entry.pos_side.clone(),
                            price,
                            size,
                            state: entry.state.clone(),
                            reduce_only,
                            tag: entry.tag.clone(),
                            lever,
                            trigger_price,
                            kind,
                            create_time,
                        },
                    );
                    changed = true;
                }
            } else if self.open_orders.remove(&entry.algo_id).is_some() {
                changed = true;
            }
        }
        changed
    }

    fn update_balances(&mut self, entries: &[WsAccountEntry]) -> bool {
        self.balance.total_equity = entries
            .get(0)
            .and_then(|entry| parse_optional_float(entry.total_eq.clone()));
        let balances =
            aggregate_balance_details(entries.iter().flat_map(|entry| entry.details.iter()));
        if balances != self.balance.delta {
            self.balance.delta = balances;
            true
        } else {
            false
        }
    }

    fn snapshot(&self) -> AccountSnapshot {
        let mut positions: Vec<_> = self.positions.values().cloned().collect();
        positions.sort_by(|a, b| {
            b.create_time
                .cmp(&a.create_time)
                .then_with(|| a.inst_id.cmp(&b.inst_id))
                .then_with(|| a.pos_side.cmp(&b.pos_side))
        });
        let mut open_orders: Vec<_> = self.open_orders.values().cloned().collect();
        open_orders.sort_by(|a, b| {
            b.create_time
                .cmp(&a.create_time)
                .then_with(|| a.inst_id.cmp(&b.inst_id))
                .then_with(|| a.ord_id.cmp(&b.ord_id))
        });
        AccountSnapshot {
            positions,
            open_orders,
            balance: self.balance.clone(),
        }
    }
}

fn build_http_client() -> Result<Client, anyhow::Error> {
    Ok(ClientBuilder::new()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_decimal_places() {
        assert_eq!(decimal_places("123.456"), 3);
        assert_eq!(decimal_places("0.00100"), 5);
        assert_eq!(decimal_places("100"), 0);
        assert_eq!(decimal_places("3.14e2"), 4);
        assert_eq!(decimal_places(""), 0);
    }

    #[test]
    fn test_parse_float_str() {
        assert_eq!(parse_float_str("123.456"), Some(123.456));
        assert_eq!(parse_float_str("  0.00100  "), Some(0.00100));
        assert_eq!(parse_float_str("100"), Some(100.0));
        assert_eq!(parse_float_str("abc"), None);
        assert_eq!(parse_float_str(""), None);
        assert_eq!(parse_float_str("1e-8"), Some(1e-8));
    }
}
