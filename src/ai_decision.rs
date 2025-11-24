use std::collections::HashMap;
use std::fmt;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use tokio::sync::{RwLock, broadcast, mpsc};

use crate::command::{
    AccountSnapshot, CancelOrderRequest, Command, PositionInfo, SetLeverageRequest, TradeOperator,
    TradeOrderKind, TradeOrderType, TradeRequest, TradeSide, TradingCommand,
};
use crate::error_log::ErrorLogStore;
use crate::okx::{MarketInfo, SharedAccountState};
use crate::okx_analytics::MarketDataFetcher;

pub const AI_TAG_ENTRY: &str = "dsentry";
pub const AI_TAG_STOP_LOSS: &str = "dssl";
pub const AI_TAG_TAKE_PROFIT: &str = "dstp";
pub const AI_TAG_CLOSE: &str = "dsclose";
pub const LEVERAGE_EPSILON: f64 = 1e-6;

pub struct DecisionExecutor<'a> {
    state: SharedAccountState,
    tx: broadcast::Sender<Command>,
    inst_ids: &'a [String],
    market: &'a MarketDataFetcher,
    order_tx: Option<mpsc::Sender<TradingCommand>>,
    leverage_cache: &'a RwLock<HashMap<LeverageKey, f64>>,
    error_log: ErrorLogStore,
    operator_name: String,
}

impl<'a> DecisionExecutor<'a> {
    pub fn new(
        state: SharedAccountState,
        tx: broadcast::Sender<Command>,
        inst_ids: &'a [String],
        market: &'a MarketDataFetcher,
        order_tx: Option<mpsc::Sender<TradingCommand>>,
        leverage_cache: &'a RwLock<HashMap<LeverageKey, f64>>,
        error_log: ErrorLogStore,
        operator_name: String,
    ) -> Self {
        DecisionExecutor {
            state,
            tx,
            inst_ids,
            market,
            order_tx,
            leverage_cache,
            error_log,
            operator_name,
        }
    }

    fn ai_operator(&self) -> TradeOperator {
        ai_operator_name(&self.operator_name)
    }

    pub async fn execute(&self, response: &str) -> Result<()> {
        let Some(_) = &self.order_tx else {
            return Ok(());
        };
        let decisions = match parse_ai_decisions(response) {
            Ok(payloads) => payloads,
            Err(err) => {
                self.log_decision_parse_failure(&err, response);
                return Err(anyhow!("解析 AI 决策失败: {err}"));
            }
        };
        for decision in decisions {
            match decision.signal {
                DecisionSignal::Hold => continue,
                DecisionSignal::Wait => continue,
                DecisionSignal::BuyToEnter | DecisionSignal::SellToEnter => {
                    self.place_entry_order(&decision).await?
                }
                DecisionSignal::Close => self.execute_close_signal(&decision).await?,
                DecisionSignal::CancelOrder => self.cancel_orders(&decision).await?,
            };
        }
        Ok(())
    }

    pub async fn capture_leverage_from_snapshot(&self, snapshot: &AccountSnapshot) {
        if snapshot.positions.is_empty() && snapshot.open_orders.is_empty() {
            return;
        }
        let mut cache = self.leverage_cache.write().await;
        for position in &snapshot.positions {
            if let Some(value) = position.lever {
                apply_leverage_entry(
                    &mut cache,
                    &position.inst_id,
                    position.pos_side.as_deref(),
                    value,
                );
            }
        }
        for order in &snapshot.open_orders {
            if let Some(value) = order.lever {
                apply_leverage_entry(&mut cache, &order.inst_id, order.pos_side.as_deref(), value);
            }
        }
    }

    async fn place_entry_order(&self, decision: &AiDecisionPayload) -> Result<()> {
        let inst_id = self
            .resolve_inst_id(&decision.coin)
            .ok_or_else(|| anyhow!("无法匹配交易币种 {}", decision.coin))?;
        if decision.quantity <= 0.0 {
            return Err(anyhow!(
                "{} 决策数量必须大于 0 (当前 {})",
                inst_id,
                decision.quantity
            ));
        }
        if decision.entry_price <= 0.0 {
            return Err(anyhow!(
                "{} 决策入场价必须大于 0 (当前 {})",
                inst_id,
                decision.entry_price
            ));
        }
        let current_price = self
            .market
            .price_for_inst(&inst_id)
            .await
            .with_context(|| format!("获取 {} 最新价格失败", inst_id))?;
        let side = match decision.signal {
            DecisionSignal::BuyToEnter => TradeSide::Buy,
            DecisionSignal::SellToEnter => TradeSide::Sell,
            _ => {
                return Err(anyhow!("信号 {:?} 不支持创建新仓位", decision.signal));
            }
        };
        let price = if side == TradeSide::Buy {
            if current_price < decision.entry_price {
                current_price
            } else {
                decision.entry_price
            }
        } else if side == TradeSide::Sell {
            if current_price > decision.entry_price {
                current_price
            } else {
                decision.entry_price
            }
        } else {
            return Err(anyhow!("无法确定 {} 的下单价格", inst_id));
        };
        let request = TradeRequest {
            inst_id,
            side,
            price,
            size: decision.quantity,
            pos_side: None,
            ord_type: Some(TradeOrderType::Market),
            reduce_only: false,
            tag: Some(AI_TAG_ENTRY.to_string()),
            operator: self.ai_operator(),
            leverage: if decision.leverage > 0.0 {
                Some(decision.leverage)
            } else {
                None
            },
            kind: TradeOrderKind::Regular,
        };
        if let Some(target_leverage) = request.leverage {
            let pos_side = determine_entry_pos_side(&request.inst_id, request.side);
            self.ensure_leverage_alignment(&request.inst_id, pos_side, target_leverage)
                .await?;
        }
        self.submit_trade_request(request.clone()).await?;
        self.place_protective_orders(&request, decision).await
    }

    async fn place_protective_orders(
        &self,
        entry: &TradeRequest,
        decision: &AiDecisionPayload,
    ) -> Result<()> {
        if decision.stop_loss <= 0.0 && decision.profit_target <= 0.0 {
            return Ok(());
        }
        let closing_side = entry.side.opposite();
        let pos_side = determine_entry_pos_side(&entry.inst_id, entry.side);
        let leverage = entry.leverage;
        if decision.stop_loss > 0.0 {
            if is_valid_stop_loss(entry.side, decision.entry_price, decision.stop_loss) {
                let request = TradeRequest {
                    inst_id: entry.inst_id.clone(),
                    side: closing_side,
                    price: decision.stop_loss,
                    size: entry.size,
                    pos_side: pos_side.clone(),
                    ord_type: None,
                    reduce_only: true,
                    tag: Some(AI_TAG_STOP_LOSS.to_string()),
                    operator: self.ai_operator(),
                    leverage,
                    kind: TradeOrderKind::StopLoss,
                };
                self.submit_trade_request(request).await?;
            } else {
                self.warn_invalid_protective_price(
                    &entry.inst_id,
                    "止损",
                    decision.stop_loss,
                    entry.side,
                );
            }
        }
        if decision.profit_target > 0.0 {
            if is_valid_take_profit(entry.side, decision.entry_price, decision.profit_target) {
                let request = TradeRequest {
                    inst_id: entry.inst_id.clone(),
                    side: closing_side,
                    price: decision.profit_target,
                    size: entry.size,
                    pos_side: pos_side.clone(),
                    ord_type: None,
                    reduce_only: true,
                    tag: Some(AI_TAG_TAKE_PROFIT.to_string()),
                    operator: self.ai_operator(),
                    leverage,
                    kind: TradeOrderKind::TakeProfit,
                };
                self.submit_trade_request(request).await?;
            } else {
                self.warn_invalid_protective_price(
                    &entry.inst_id,
                    "止盈",
                    decision.profit_target,
                    entry.side,
                );
            }
        }
        Ok(())
    }

    async fn execute_close_signal(&self, decision: &AiDecisionPayload) -> Result<()> {
        let inst_id = self
            .resolve_inst_id(&decision.coin)
            .ok_or_else(|| anyhow!("无法匹配交易币种 {}", decision.coin))?;
        let snapshot = self.state.snapshot().await;
        let position = snapshot
            .positions
            .into_iter()
            .find(|pos| pos.inst_id.eq_ignore_ascii_case(&inst_id))
            .ok_or_else(|| anyhow!("{} 无持仓可平", inst_id))?;
        let available = position.size.abs();
        if available <= 0.0 {
            return Err(anyhow!("{} 当前持仓数量无效", inst_id));
        }
        let mut size = if decision.quantity > 0.0 {
            decision.quantity.min(available)
        } else {
            available
        };
        if size <= 0.0 {
            size = available;
        }
        let price = self
            .market
            .price_for_inst(&inst_id)
            .await
            .with_context(|| format!("获取 {} 最新价格失败", inst_id))?;
        let (side, pos_side) = determine_close_side(&position);
        let request = TradeRequest {
            inst_id,
            side,
            price,
            size,
            pos_side,
            ord_type: Some(TradeOrderType::Market),
            reduce_only: true,
            tag: Some(AI_TAG_CLOSE.to_string()),
            operator: self.ai_operator(),
            leverage: position.lever,
            kind: TradeOrderKind::Regular,
        };
        self.submit_trade_request(request).await
    }

    async fn cancel_orders(&self, decision: &AiDecisionPayload) -> Result<()> {
        let Some(order_tx) = &self.order_tx else {
            return Ok(());
        };
        let inst_id = self
            .resolve_inst_id(&decision.coin)
            .ok_or_else(|| anyhow!("无法匹配交易币种 {}", decision.coin))?;
        let snapshot = self.state.snapshot().await;
        let cancel_orders = decision.cancel_orders.as_ref().context("缺少撤单列表")?;
        let orders: Vec<_> = snapshot
            .open_orders
            .into_iter()
            .filter(|order| {
                order.inst_id.eq_ignore_ascii_case(&inst_id)
                    && cancel_orders.contains(&order.ord_id)
            })
            .collect();
        if orders.is_empty() {
            let _ = self.tx.send(Command::Error(format!(
                "{} 未找到 {inst_id} 的 AI 挂单可撤，已忽略撤单信号",
                self.operator_name
            )));
            return Ok(());
        }
        for order in orders {
            let request = CancelOrderRequest {
                inst_id: order.inst_id.clone(),
                ord_id: order.ord_id.clone(),
                operator: self.ai_operator(),
                pos_side: order.pos_side.clone(),
                kind: order.kind,
            };
            order_tx
                .send(TradingCommand::Cancel(request))
                .await
                .map_err(|err| anyhow!("发送撤单命令失败: {err}"))?;
        }
        Ok(())
    }

    fn log_decision_parse_failure(&self, err: &anyhow::Error, response: &str) {
        let message = format!(
            "AI 决策解析失败: {err}\n响应原文:\n{response}",
            err = err,
            response = response
        );
        let _ = self.error_log.append_message(message);
    }

    async fn ensure_leverage_alignment(
        &self,
        inst_id: &str,
        pos_side: Option<String>,
        desired: f64,
    ) -> Result<()> {
        if desired <= 0.0 {
            return Ok(());
        }
        let Some(order_tx) = &self.order_tx else {
            return Ok(());
        };
        let current = self.lookup_leverage(inst_id, pos_side.as_deref()).await;
        let needs_update = match current {
            Some(value) => (value - desired).abs() > LEVERAGE_EPSILON,
            None => true,
        };
        if !needs_update {
            return Ok(());
        }
        let request = SetLeverageRequest {
            inst_id: inst_id.to_string(),
            lever: desired,
            pos_side: pos_side.clone(),
        };
        order_tx
            .send(TradingCommand::SetLeverage(request))
            .await
            .map_err(|err| anyhow!("发送杠杆调整命令失败: {err}"))?;
        self.record_leverage(inst_id, pos_side.as_deref(), desired)
            .await;
        Ok(())
    }

    async fn lookup_leverage(&self, inst_id: &str, pos_side: Option<&str>) -> Option<f64> {
        let cache = self.leverage_cache.read().await;
        let key = LeverageKey::new(inst_id, pos_side);
        if let Some(value) = cache.get(&key) {
            return Some(*value);
        }
        if pos_side.is_some() {
            let fallback = LeverageKey::new(inst_id, None);
            cache.get(&fallback).copied()
        } else {
            None
        }
    }

    async fn record_leverage(&self, inst_id: &str, pos_side: Option<&str>, value: f64) {
        if !value.is_finite() || value <= 0.0 {
            return;
        }
        let mut cache = self.leverage_cache.write().await;
        apply_leverage_entry(&mut cache, inst_id, pos_side, value);
    }

    async fn submit_trade_request(&self, request: TradeRequest) -> Result<()> {
        let Some(order_tx) = &self.order_tx else {
            return Ok(());
        };
        order_tx
            .send(TradingCommand::Place(request))
            .await
            .map_err(|err| anyhow!("发送下单命令失败: {err}"))?;
        Ok(())
    }

    fn warn_invalid_protective_price(
        &self,
        inst_id: &str,
        label: &str,
        price: f64,
        side: TradeSide,
    ) {
        let expectation = match (label, side) {
            ("止损", TradeSide::Buy) => "应低于入场价",
            ("止损", TradeSide::Sell) => "应高于入场价",
            ("止盈", TradeSide::Buy) => "应高于入场价",
            ("止盈", TradeSide::Sell) => "应低于入场价",
            _ => "价格方向不符",
        };
        let direction = match side {
            TradeSide::Buy => "做多",
            TradeSide::Sell => "做空",
        };
        let _ = self.tx.send(Command::Error(format!(
            "{} {inst_id} {label} 价格 {:.4} 与 {direction} 方向不符（{expectation}），已忽略",
            self.operator_name, price
        )));
    }

    fn resolve_inst_id(&self, coin: &str) -> Option<String> {
        let coin = coin.trim().to_ascii_uppercase();
        self.inst_ids
            .iter()
            .find(|inst| {
                if coin.contains('-') {
                    inst.to_ascii_uppercase() == coin
                } else {
                    inst.to_ascii_uppercase().starts_with(&format!("{coin}-"))
                }
            })
            .cloned()
    }
}

pub fn initial_leverage_cache(markets: &HashMap<String, MarketInfo>) -> HashMap<LeverageKey, f64> {
    let mut cache = HashMap::new();
    for (inst_id, market) in markets {
        apply_leverage_entry(&mut cache, inst_id, None, market.lever);
    }
    cache
}

pub fn apply_leverage_entry(
    cache: &mut HashMap<LeverageKey, f64>,
    inst_id: &str,
    pos_side: Option<&str>,
    value: f64,
) {
    if !value.is_finite() || value <= 0.0 {
        return;
    }
    cache.insert(LeverageKey::new(inst_id, pos_side), value);
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct LeverageKey {
    inst_id: String,
    pos_side: Option<String>,
}

impl LeverageKey {
    pub fn new(inst_id: &str, pos_side: Option<&str>) -> Self {
        let normalized_side = pos_side
            .map(|side| side.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        LeverageKey {
            inst_id: inst_id.to_ascii_uppercase(),
            pos_side: normalized_side,
        }
    }

    pub fn inst_id(&self) -> &str {
        &self.inst_id
    }

    pub fn pos_side(&self) -> Option<&str> {
        self.pos_side.as_deref()
    }
}

fn determine_close_side(position: &PositionInfo) -> (TradeSide, Option<String>) {
    if let Some(pos_side) = position.pos_side.as_deref() {
        if pos_side.eq_ignore_ascii_case("long") {
            return (TradeSide::Sell, Some(pos_side.to_string()));
        } else if pos_side.eq_ignore_ascii_case("short") {
            return (TradeSide::Buy, Some(pos_side.to_string()));
        }
        let side = if position.size >= 0.0 {
            TradeSide::Sell
        } else {
            TradeSide::Buy
        };
        return (side, Some(pos_side.to_string()));
    }
    let side = if position.size >= 0.0 {
        TradeSide::Sell
    } else {
        TradeSide::Buy
    };
    (side, None)
}

fn determine_entry_pos_side(inst_id: &str, side: TradeSide) -> Option<String> {
    let upper = inst_id.to_ascii_uppercase();
    if upper.ends_with("-SWAP") || upper.ends_with("-FUTURES") {
        return Some(
            match side {
                TradeSide::Buy => "long",
                TradeSide::Sell => "short",
            }
            .to_string(),
        );
    }
    None
}

fn is_valid_take_profit(side: TradeSide, entry_price: f64, target: f64) -> bool {
    if entry_price <= 0.0 || target <= 0.0 {
        return false;
    }
    match side {
        TradeSide::Buy => target > entry_price,
        TradeSide::Sell => target < entry_price,
    }
}

fn is_valid_stop_loss(side: TradeSide, entry_price: f64, stop: f64) -> bool {
    if entry_price <= 0.0 || stop <= 0.0 {
        return false;
    }
    match side {
        TradeSide::Buy => stop < entry_price,
        TradeSide::Sell => stop > entry_price,
    }
}

fn ai_operator_name(name: &str) -> TradeOperator {
    TradeOperator::Ai {
        name: Some(name.to_string()),
    }
}

fn parse_ai_decisions(raw: &str) -> Result<Vec<AiDecisionPayload>> {
    let value = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value,
        Err(_) => {
            if let (Some(start), Some(end)) = (raw.find('['), raw.rfind(']')) {
                if end <= start {
                    return Err(anyhow!("无法截取 AI JSON"));
                }
                let slice = raw
                    .get(start..=end)
                    .ok_or_else(|| anyhow!("无法截取 AI JSON"))?;
                serde_json::from_str::<serde_json::Value>(slice)
                    .map_err(|err| anyhow!("解析 JSON 失败: {err}"))?
            } else {
                let start = raw.find('{').ok_or_else(|| anyhow!("缺少 JSON 起始"))?;
                let end = raw.rfind('}').ok_or_else(|| anyhow!("缺少 JSON 结束"))?;
                if end <= start {
                    return Err(anyhow!("无法截取 AI JSON"));
                }
                let slice = raw
                    .get(start..=end)
                    .ok_or_else(|| anyhow!("无法截取 AI JSON"))?;
                serde_json::from_str::<serde_json::Value>(slice)
                    .map_err(|err| anyhow!("解析 JSON 失败: {err}"))?
            }
        }
    };
    match value {
        serde_json::Value::Array(items) => {
            let mut decisions = Vec::new();
            for (idx, item) in items.into_iter().enumerate() {
                if item.is_null() {
                    continue;
                }
                let payload = serde_json::from_value::<AiDecisionPayload>(item)
                    .map_err(|err| anyhow!("解析第 {} 个 AI 决策失败: {err}", idx + 1))?;
                decisions.push(payload);
            }
            if decisions.is_empty() {
                Err(anyhow!("AI 决策数组为空"))
            } else {
                Ok(decisions)
            }
        }
        serde_json::Value::Object(_) => {
            let payload = serde_json::from_value::<AiDecisionPayload>(value)
                .map_err(|err| anyhow!("解析 AI 决策失败: {err}"))?;
            Ok(vec![payload])
        }
        _ => Err(anyhow!("AI 决策必须是 JSON 对象或数组")),
    }
}

#[derive(Debug, Deserialize)]
struct AiDecisionPayload {
    signal: DecisionSignal,
    coin: String,
    #[serde(default, deserialize_with = "deserialize_f64")]
    quantity: f64,
    #[serde(default, deserialize_with = "deserialize_f64")]
    leverage: f64,
    #[serde(default, deserialize_with = "deserialize_f64")]
    entry_price: f64,
    #[serde(default, deserialize_with = "deserialize_f64")]
    profit_target: f64,
    #[serde(default, deserialize_with = "deserialize_f64")]
    stop_loss: f64,
    #[serde(default)]
    #[allow(dead_code)]
    invalidation_condition: Option<String>,
    #[serde(default, deserialize_with = "deserialize_f64")]
    #[allow(dead_code)]
    confidence: f64,
    #[serde(default)]
    cancel_orders: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_f64")]
    #[allow(dead_code)]
    risk_usd: f64,
    #[serde(default)]
    #[allow(dead_code)]
    justification: String,
}

fn deserialize_f64<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    struct F64Visitor;
    impl<'de> Visitor<'de> for F64Visitor {
        type Value = f64;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("数字或可解析的字符串")
        }
        fn visit_f64<E>(self, value: f64) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(value)
        }
        fn visit_i64<E>(self, value: i64) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(value as f64)
        }
        fn visit_u64<E>(self, value: u64) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(value as f64)
        }
        fn visit_str<E>(self, value: &str) -> Result<f64, E>
        where
            E: de::Error,
        {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(0.0);
            }
            trimmed
                .parse::<f64>()
                .map_err(|_| E::custom(format!("无法解析数字 {value}")))
        }
        fn visit_string<E>(self, value: String) -> Result<f64, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
        fn visit_none<E>(self) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(0.0)
        }
        fn visit_unit<E>(self) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(0.0)
        }
    }
    deserializer.deserialize_any(F64Visitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrapped_operations_with_string_numbers() {
        let raw = r#"{
            "operations": [{
                "signal": "buy_to_enter",
                "coin": "BTC-USDT-SWAP",
                "quantity": "0.01",
                "leverage": "3",
                "entry_price": "90000.5",
                "profit_target": "90500",
                "stop_loss": "89000"
            }]
        }"#;
        let decisions = parse_ai_decisions(raw).expect("should parse decisions");
        assert_eq!(decisions.len(), 1);
        let decision = &decisions[0];
        assert_eq!(decision.coin, "BTC-USDT-SWAP");
        assert!((decision.quantity - 0.01).abs() < 1e-9);
        assert!((decision.leverage - 3.0).abs() < 1e-9);
        assert!((decision.entry_price - 90000.5).abs() < 1e-9);
    }

    #[test]
    fn parses_string_wrapped_response_array() {
        let raw = r#"{"response": "[{\"signal\":\"hold\",\"coin\":\"ETH-USDT-SWAP\"}]" }"#;
        let decisions = parse_ai_decisions(raw).expect("should parse wrapped response");
        assert_eq!(decisions.len(), 1);
        assert!(matches!(decisions[0].signal, DecisionSignal::Hold));
        assert_eq!(decisions[0].coin, "ETH-USDT-SWAP");
    }
}

#[derive(Debug, Deserialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DecisionSignal {
    BuyToEnter,
    SellToEnter,
    Hold,
    Close,
    CancelOrder,
    Wait,
}
