use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, Local, TimeZone};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time::{self, MissedTickBehavior};

use crate::command::{
    AccountBalanceDelta, AccountSnapshot, AiInsightRecord, Command, PendingOrderInfo, PositionInfo,
    SetLeverageRequest, TradeEvent, TradeOperator, TradeOrderKind, TradeRequest, TradeSide,
    TradingCommand,
};
use crate::config::DeepseekConfig;
use crate::okx::{MarketInfo, SharedAccountState};
use crate::okx_analytics::{InstrumentAnalytics, MarketDataFetcher};
use crate::trade_log::{TradeLogEntry, TradeLogStore};

const SYSTEM_PROMPT: &str = r#"
# 角色与身份
您是一位自主运行的加密货币交易代理，在 Okx 交易所的实时市场中执行交易。
您的代号：AI 交易模型 [Deepseek]。
您的使命：通过系统化、纪律严明的交易，最大化风险调整后的收益（PnL）。
---

# 交易环境规范

## 市场参数

- **交易所**：Okx
- **资产范围**：用户提供（永续合约）
- **交易时间**：7x24 小时不间断交易
- **决策频率**：每 2-3 分钟一次（中低频交易）
- **杠杆范围**：1 倍至 20 倍（根据交易信念谨慎使用）

## 交易机制
- **合约类型**：永续期货（无到期日）
- **资金机制**：
- 正资金费率 = 多头支付空头（看涨市场情绪）
- 负资金费率 = 空头支付多头（看跌市场情绪）
- **交易**手续费：每笔交易约 0.02-0.05%（挂单/吃单手续费适用）
- 滑点：市价单预计滑点为 0.01-0.1%，具体取决于交易规模

---

# 操作空间定义

每个决策周期内，您有四种可能的操作：
1. **买入入场(buy_to_enter)**：建立新的多头头寸（押注价格上涨）
- 适用情况：看涨技术形态、积极动能、风险回报比有利于上涨
2. **卖出入场(sell_to_enter)**：建立新的空头头寸（押注价格下跌）
- 适用情况：看跌技术形态、消极动能、风险回报比有利于下跌
3. **持有(hold)**：维持现有头寸不变
- 适用情况：现有头寸表现符合预期，或不存在明显的优势
4. **平仓(close)**：完全退出现有头寸
- 适用情况：达到盈利目标、触发止损或符合交易逻辑已失效

## 仓位管理限制
- **禁止金字塔式加仓**：不能在现有仓位上加仓（每种币种最多只能持有一个仓位）
- **禁止对冲**：不能同时持有同一资产的多头和空头仓位
- **禁止部分平仓**：必须一次性平掉所有仓位
---

# 仓位规模框架
使用以下公式计算仓位规模：
仓位规模（美元）= 可用资金 × 杠杆 × 分配百分比
仓位规模（币种）= 仓位规模（美元）/ 当前价格
## 仓位规模注意事项

1. **可用资金**：仅使用可用资金 USDT（而非账户余额）
2. **杠杆选择**：
- 低信心（0.3-0.5）：使用 1-3 倍杠杆
- 中等信心（0.5-0.7）：使用 3-8 倍杠杆
- 高信心（0.7-1.0）：使用 8-20 倍杠杆
3. **分散投资**：避免将超过 40% 的资金集中于单一仓位
4. **费用影响**：对于低于 500 美元的仓位，费用会显著侵蚀利润
5. **清算风险**：确保清算价格与入场价格相差超过 15%
---

# 风险管理协议（强制性）

对于每一笔交易决策，您必须明确以下信息：
1. **止盈目标**（浮点数）：设定止盈的确切价格水平
- 应至少提供 2:1 的风险回报比
- 基于技术阻力位、斐波那契扩展位或波动率区间
2. **止损位**（浮点数）：设定止损的确切价格水平
- 应将每笔交易的损失限制在账户价值的 1-3% 以内
- 设置在近期支撑位/阻力位之外，以避免过早止损
3. **失效条件**（字符串）：使您的交易策略失效的特定市场信号
-示例：“BTC 跌破 10 万美元”、“RSI 跌破 30”、“资金费率转为负值”

- 必须客观且可观察
4. **信心指数**（浮动值，0-1）：您对这笔交易的信心程度
- 0.0-0.3：信心较低（避免交易或使用最小仓位）
- 0.3-0.6：信心中等（标准仓位）
- 0.6-0.8：信心较高（可接受较大仓位）
- 0.8-1.0：信心极高（谨慎使用，谨防过度自信）
5. **风险金额（美元）（浮动值）：风险金额（从入场价到止损价的距离）

- 计算公式：|入场价 - 止损价| × 持仓规模

---

# 输出格式规范

请以**有效的 JSON 对象**的形式返回您的决策，该对象必须包含以下字段：

```json
{
"signal": "buy_to_enter" | "sell_to_enter" | "hold" | "close",
"coin": "<string>",
"quantity": <float>,
"leverage": <integer 1-20>,
"profit_target": <float>,
"stop_loss": <float>,
"invalidation_condition": "<string>",
"confidence": <float 0-1>,
"risk_usd": <float>,
"justification": "<string>" // 使用中文输出
}
```

## 输出验证规则
- 所有数值字段必须为正数（信号为“hold”时除外）
- **数量粒度**：除信号为 “hold” 外，`quantity` 必须是 0.01 的整数倍（0.01 × n）
- 做多时，profit_target 必须高于入场价；做空时，profit_target 必须低于入场价
- 做多时，stop_loss 必须低于入场价；做空时，stop_loss 必须高于入场价
- 理由必须简洁明了（最多 500 个字符）
- 当信号为“hold”时：将 quantity 设置为 0，leverage 设置为 1，并将 risk 字段设置为占位符
---
这是法律管辖范围内的一项研究实验。
注重技术分析和风险管理原则。
"#;
const MAX_ANALYTICS_INSTRUMENTS: usize = 3;
const MAX_POSITIONS: usize = 12;
const MAX_ORDERS: usize = 12;
const MAX_BALANCES: usize = 12;
const AI_OPERATOR_NAME: &str = "Deepseek";
const AI_TAG_ENTRY: &str = "dsentry";
const AI_TAG_STOP_LOSS: &str = "dssl";
const AI_TAG_TAKE_PROFIT: &str = "dstp";
const AI_TAG_CLOSE: &str = "dsclose";
const LEVERAGE_EPSILON: f64 = 1e-6;

pub struct DeepseekReporter {
    client: DeepseekClient,
    state: SharedAccountState,
    tx: broadcast::Sender<Command>,
    interval: Duration,
    inst_ids: Vec<String>,
    markets: HashMap<String, MarketInfo>,
    leverage_cache: RwLock<HashMap<LeverageKey, f64>>,
    market: MarketDataFetcher,
    performance: PerformanceTracker,
    order_tx: Option<mpsc::Sender<TradingCommand>>,
}

impl DeepseekReporter {
    pub fn new(
        config: DeepseekConfig,
        state: SharedAccountState,
        tx: broadcast::Sender<Command>,
        inst_ids: Vec<String>,
        markets: HashMap<String, MarketInfo>,
        start_timestamp_ms: i64,
        order_tx: Option<mpsc::Sender<TradingCommand>>,
    ) -> Result<Self> {
        let client = DeepseekClient::new(&config)?;
        let market = MarketDataFetcher::new()?;
        let inst_ids = normalize_inst_ids(inst_ids);
        let performance = PerformanceTracker::new(start_timestamp_ms);
        let leverage_cache = RwLock::new(initial_leverage_cache(&markets));
        Ok(DeepseekReporter {
            client,
            state,
            tx,
            interval: config.interval,
            inst_ids,
            markets,
            leverage_cache,
            market,
            performance,
            order_tx,
        })
    }

    pub async fn run(self, mut exit_rx: broadcast::Receiver<Command>) -> Result<()> {
        let mut ticker = time::interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(err) = self.report_once().await {
                        let _ = self.tx.send(Command::Error(format!("Deepseek 分析失败: {err}")));
                    }
                }
                message = exit_rx.recv() => match message {
                    Ok(Command::Exit) | Err(broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn report_once(&self) -> Result<()> {
        let snapshot = self.state.snapshot().await;
        if !has_material_data(&snapshot) {
            return Ok(());
        }
        self.capture_leverage_from_snapshot(&snapshot).await;
        let leverage_overview = self.leverage_overview().await;
        let analytics = match self.collect_market_analytics().await {
            Ok(entries) => entries,
            Err(err) => {
                let _ = self.tx.send(Command::Error(format!(
                    "市场指标不完整，跳过本轮 AI 决策: {err}"
                )));
                return Ok(());
            }
        };
        let performance = match self.performance.summary(self.interval) {
            Ok(summary) => {
                if summary.overall.is_none() && summary.recent.is_none() {
                    None
                } else {
                    Some(summary)
                }
            }
            Err(err) => {
                let _ = self
                    .tx
                    .send(Command::Error(format!("统计交易表现失败: {err}")));
                None
            }
        };
        let prompt = build_snapshot_prompt(
            &snapshot,
            &analytics,
            performance.as_ref(),
            &self.inst_ids,
            &self.markets,
            &leverage_overview,
        );
        let insight = self.client.chat_completion(&prompt).await?;
        let trimmed = insight.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if let Err(err) = self.execute_ai_decision(trimmed, &analytics).await {
            let _ = self
                .tx
                .send(Command::Error(format!("执行 AI 决策失败: {err}")));
        }
        let record = AiInsightRecord {
            timestamp_ms: Local::now().timestamp_millis(),
            system_prompt: SYSTEM_PROMPT.to_string(),
            user_prompt: prompt,
            response: trimmed.to_string(),
        };
        let _ = self.tx.send(Command::AiInsight(record));
        Ok(())
    }

    async fn collect_market_analytics(&self) -> Result<Vec<InstrumentAnalytics>> {
        if self.inst_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut analytics = Vec::new();
        let mut has_error = false;
        for inst_id in self.inst_ids.iter().take(MAX_ANALYTICS_INSTRUMENTS) {
            match self.market.fetch_inst(inst_id).await {
                Ok(entry) => analytics.push(entry),
                Err(err) => {
                    let _ = self.tx.send(Command::Error(format!(
                        "加载 {} 市场指标失败: {err}",
                        inst_id
                    )));
                    has_error = true;
                }
            }
        }
        if has_error {
            Err(anyhow!("部分市场指标加载失败"))
        } else {
            Ok(analytics)
        }
    }

    async fn capture_leverage_from_snapshot(&self, snapshot: &AccountSnapshot) {
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

    async fn leverage_overview(&self) -> Vec<InstrumentLeverage> {
        let mut overview: HashMap<String, InstrumentLeverage> = HashMap::new();
        for inst in &self.inst_ids {
            overview
                .entry(inst.clone())
                .or_insert_with(|| InstrumentLeverage::new(inst.clone()));
        }
        {
            let cache = self.leverage_cache.read().await;
            for (key, value) in cache.iter() {
                let entry = overview
                    .entry(key.inst_id.clone())
                    .or_insert_with(|| InstrumentLeverage::new(key.inst_id.clone()));
                match key.pos_side.as_deref() {
                    Some(side) if side.eq_ignore_ascii_case("long") => entry.long = Some(*value),
                    Some(side) if side.eq_ignore_ascii_case("short") => entry.short = Some(*value),
                    Some(side) if side.eq_ignore_ascii_case("net") => entry.net = Some(*value),
                    _ => entry.net = Some(*value),
                }
            }
        }
        let mut entries: Vec<_> = overview.into_values().collect();
        entries.sort_by(|a, b| a.inst_id.cmp(&b.inst_id));
        entries
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

    async fn execute_ai_decision(
        &self,
        response: &str,
        analytics: &[InstrumentAnalytics],
    ) -> Result<()> {
        let Some(_) = &self.order_tx else {
            return Ok(());
        };
        let decision = match parse_ai_decision(response) {
            Ok(payload) => payload,
            Err(err) => {
                return Err(anyhow!("解析 AI 决策失败: {err}"));
            }
        };
        match decision.signal {
            DecisionSignal::Hold => Ok(()),
            DecisionSignal::BuyToEnter | DecisionSignal::SellToEnter => {
                self.place_entry_order(&decision, analytics).await
            }
            DecisionSignal::Close => self.execute_close_signal(&decision, analytics).await,
        }
    }

    async fn place_entry_order(
        &self,
        decision: &AiDecisionPayload,
        analytics: &[InstrumentAnalytics],
    ) -> Result<()> {
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
        let price = self
            .price_for_inst(&inst_id, analytics)
            .await
            .with_context(|| format!("获取 {} 最新价格失败", inst_id))?;
        let side = match decision.signal {
            DecisionSignal::BuyToEnter => TradeSide::Buy,
            DecisionSignal::SellToEnter => TradeSide::Sell,
            _ => {
                return Err(anyhow!("信号 {:?} 不支持创建新仓位", decision.signal));
            }
        };
        let request = TradeRequest {
            inst_id,
            side,
            price,
            size: decision.quantity,
            pos_side: None,
            reduce_only: false,
            tag: Some(AI_TAG_ENTRY.to_string()),
            operator: ai_operator(),
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
        self.place_protective_orders(&request, price, decision)
            .await
    }

    async fn place_protective_orders(
        &self,
        entry: &TradeRequest,
        entry_price: f64,
        decision: &AiDecisionPayload,
    ) -> Result<()> {
        if decision.stop_loss <= 0.0 && decision.profit_target <= 0.0 {
            return Ok(());
        }
        let closing_side = entry.side.opposite();
        let pos_side = determine_entry_pos_side(&entry.inst_id, entry.side);
        let leverage = entry.leverage;
        if decision.stop_loss > 0.0 {
            if is_valid_stop_loss(entry.side, entry_price, decision.stop_loss) {
                let request = TradeRequest {
                    inst_id: entry.inst_id.clone(),
                    side: closing_side,
                    price: decision.stop_loss,
                    size: entry.size,
                    pos_side: pos_side.clone(),
                    reduce_only: true,
                    tag: Some(AI_TAG_STOP_LOSS.to_string()),
                    operator: ai_operator(),
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
            if is_valid_take_profit(entry.side, entry_price, decision.profit_target) {
                let request = TradeRequest {
                    inst_id: entry.inst_id.clone(),
                    side: closing_side,
                    price: decision.profit_target,
                    size: entry.size,
                    pos_side: pos_side.clone(),
                    reduce_only: true,
                    tag: Some(AI_TAG_TAKE_PROFIT.to_string()),
                    operator: ai_operator(),
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

    async fn execute_close_signal(
        &self,
        decision: &AiDecisionPayload,
        analytics: &[InstrumentAnalytics],
    ) -> Result<()> {
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
            .price_for_inst(&inst_id, analytics)
            .await
            .with_context(|| format!("获取 {} 最新价格失败", inst_id))?;
        let (side, pos_side) = determine_close_side(&position);
        let request = TradeRequest {
            inst_id,
            side,
            price,
            size,
            pos_side,
            reduce_only: true,
            tag: Some(AI_TAG_CLOSE.to_string()),
            operator: ai_operator(),
            leverage: position.lever,
            kind: TradeOrderKind::Regular,
        };
        self.submit_trade_request(request).await
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
            "Deepseek {inst_id} {label} 价格 {:.4} 与 {direction} 方向不符（{expectation}），已忽略",
            price
        )));
    }

    async fn price_for_inst(
        &self,
        inst_id: &str,
        analytics: &[InstrumentAnalytics],
    ) -> Result<f64> {
        if let Some(price) = analytics
            .iter()
            .find(|entry| entry.inst_id.eq_ignore_ascii_case(inst_id))
            .and_then(|entry| entry.current_price)
        {
            return Ok(price);
        }
        let entry = self.market.fetch_inst(inst_id).await?;
        entry
            .current_price
            .ok_or_else(|| anyhow!("{} 缺少最新价格", inst_id))
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

fn has_material_data(snapshot: &AccountSnapshot) -> bool {
    !snapshot.positions.is_empty()
        || !snapshot.open_orders.is_empty()
        || snapshot.balance.total_equity.is_some()
        || !snapshot.balance.delta.is_empty()
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

fn ai_operator() -> TradeOperator {
    TradeOperator::Ai {
        name: Some(AI_OPERATOR_NAME.to_string()),
    }
}

fn parse_ai_decision(raw: &str) -> Result<AiDecisionPayload> {
    match serde_json::from_str::<AiDecisionPayload>(raw) {
        Ok(payload) => Ok(payload),
        Err(_) => {
            let start = raw.find('{').ok_or_else(|| anyhow!("缺少 JSON 起始"))?;
            let end = raw.rfind('}').ok_or_else(|| anyhow!("缺少 JSON 结束"))?;
            let slice = raw
                .get(start..=end)
                .ok_or_else(|| anyhow!("无法截取 AI JSON"))?;
            serde_json::from_str::<AiDecisionPayload>(slice)
                .map_err(|err| anyhow!("解析 JSON 失败: {err}"))
        }
    }
}

#[derive(Debug, Deserialize)]
struct AiDecisionPayload {
    signal: DecisionSignal,
    coin: String,
    #[serde(default)]
    quantity: f64,
    #[serde(default)]
    leverage: f64,
    #[serde(default)]
    #[allow(dead_code)]
    profit_target: f64,
    #[serde(default)]
    #[allow(dead_code)]
    stop_loss: f64,
    #[serde(default)]
    #[allow(dead_code)]
    invalidation_condition: String,
    #[serde(default)]
    #[allow(dead_code)]
    confidence: f64,
    #[serde(default)]
    #[allow(dead_code)]
    risk_usd: f64,
    #[serde(default)]
    #[allow(dead_code)]
    justification: String,
}

#[derive(Debug, Deserialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DecisionSignal {
    BuyToEnter,
    SellToEnter,
    Hold,
    Close,
}

#[derive(Debug, Clone, Default)]
struct InstrumentLeverage {
    inst_id: String,
    net: Option<f64>,
    long: Option<f64>,
    short: Option<f64>,
}

impl InstrumentLeverage {
    fn new(inst_id: String) -> Self {
        InstrumentLeverage {
            inst_id,
            net: None,
            long: None,
            short: None,
        }
    }
}

#[derive(Debug, Clone)]
struct PerformanceStats {
    label: String,
    start_timestamp_ms: i64,
    trade_count: usize,
    sharpe_ratio: Option<f64>,
    total_pnl: f64,
}

impl PerformanceStats {
    fn start_time(&self) -> DateTime<Local> {
        Local
            .timestamp_millis_opt(self.start_timestamp_ms)
            .single()
            .unwrap_or_else(Local::now)
    }

    fn start_time_label(&self) -> String {
        self.start_time().format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

#[derive(Debug, Clone, Default)]
struct PerformanceSummary {
    overall: Option<PerformanceStats>,
    recent: Option<PerformanceStats>,
}

struct PerformanceTracker {
    start_timestamp_ms: i64,
    log_store: TradeLogStore,
}

impl PerformanceTracker {
    fn new(start_timestamp_ms: i64) -> Self {
        PerformanceTracker {
            start_timestamp_ms,
            log_store: TradeLogStore::new(TradeLogStore::default_path()),
        }
    }

    fn summary(&self, recent_window: Duration) -> Result<PerformanceSummary> {
        let entries = self.log_store.load()?;
        let overall = self.build_stats(&entries, self.start_timestamp_ms, "运行以来".to_string());
        let recent = if recent_window.is_zero() {
            None
        } else {
            let chrono_window = ChronoDuration::from_std(recent_window)
                .unwrap_or_else(|_| ChronoDuration::seconds(0));
            let recent_start = (Local::now() - chrono_window).timestamp_millis();
            let label = format!("最近 {}（决策周期）", format_duration_brief(recent_window));
            self.build_stats(&entries, recent_start, label)
        };
        Ok(PerformanceSummary { overall, recent })
    }

    fn build_stats(
        &self,
        entries: &[TradeLogEntry],
        start_timestamp_ms: i64,
        label: String,
    ) -> Option<PerformanceStats> {
        let start_time = Local
            .timestamp_millis_opt(start_timestamp_ms)
            .single()
            .unwrap_or_else(Local::now);
        let mut fill_count = 0usize;
        let mut pnls = Vec::new();
        for entry in entries {
            if entry.timestamp < start_time {
                continue;
            }
            if let TradeEvent::Fill(fill) = &entry.event {
                fill_count += 1;
                if let Some(pnl) = fill.pnl {
                    if pnl.is_finite() {
                        pnls.push(pnl);
                    }
                }
            }
        }
        if fill_count == 0 && pnls.is_empty() {
            return None;
        }
        let total_pnl: f64 = pnls.iter().copied().sum();
        let sharpe_ratio = compute_sharpe(&pnls);
        Some(PerformanceStats {
            label,
            start_timestamp_ms,
            trade_count: fill_count,
            sharpe_ratio,
            total_pnl,
        })
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct LeverageKey {
    inst_id: String,
    pos_side: Option<String>,
}

impl LeverageKey {
    fn new(inst_id: &str, pos_side: Option<&str>) -> Self {
        let normalized_side = pos_side
            .map(|side| side.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        LeverageKey {
            inst_id: inst_id.to_ascii_uppercase(),
            pos_side: normalized_side,
        }
    }
}

fn build_snapshot_prompt(
    snapshot: &AccountSnapshot,
    analytics: &[InstrumentAnalytics],
    performance: Option<&PerformanceSummary>,
    inst_ids: &[String],
    markets: &HashMap<String, MarketInfo>,
    leverages: &[InstrumentLeverage],
) -> String {
    let mut buffer = String::new();
    buffer.push_str("以下为 OKX 账户的实时快照，请据此输出风险与操作建议：\n\n");

    if let Some(eq) = snapshot.balance.total_equity {
        buffer.push_str(&format!("总权益: {}\n", format_float(eq)));
    }

    if let Some(summary) = performance {
        buffer.push_str("\n【策略运行概览】\n");
        if let Some(overall) = &summary.overall {
            push_performance_stats(&mut buffer, overall);
        } else {
            buffer.push_str("运行以来暂无成交记录\n");
        }
        if let Some(recent) = &summary.recent {
            push_performance_stats(&mut buffer, recent);
        } else {
            buffer.push_str("最近决策周期暂无成交记录\n");
        }
    }

    buffer.push_str("\n【持仓情况】\n");
    if snapshot.positions.is_empty() {
        buffer.push_str("无持仓\n");
    } else {
        for position in snapshot.positions.iter().take(MAX_POSITIONS) {
            buffer.push_str("- ");
            buffer.push_str(&format_position(position));
            buffer.push('\n');
        }
        if snapshot.positions.len() > MAX_POSITIONS {
            buffer.push_str(&format!(
                "... 其余 {} 条持仓已省略\n",
                snapshot.positions.len() - MAX_POSITIONS
            ));
        }
    }

    buffer.push_str("\n【挂单情况】\n");
    if snapshot.open_orders.is_empty() {
        buffer.push_str("无挂单\n");
    } else {
        for order in snapshot.open_orders.iter().take(MAX_ORDERS) {
            buffer.push_str("- ");
            buffer.push_str(&format_order(order));
            buffer.push('\n');
        }
        if snapshot.open_orders.len() > MAX_ORDERS {
            buffer.push_str(&format!(
                "... 其余 {} 条挂单已省略\n",
                snapshot.open_orders.len() - MAX_ORDERS
            ));
        }
    }

    buffer.push_str("\n【资金币种】\n");
    if snapshot.balance.delta.is_empty() {
        buffer.push_str("无资金明细\n");
    } else {
        for balance in snapshot.balance.delta.iter().take(MAX_BALANCES) {
            buffer.push_str("- ");
            buffer.push_str(&format_balance(balance));
            buffer.push('\n');
        }
        if snapshot.balance.delta.len() > MAX_BALANCES {
            buffer.push_str(&format!(
                "... 其余 {} 个币种已省略\n",
                snapshot.balance.delta.len() - MAX_BALANCES
            ));
        }
    }

    append_trade_limits(&mut buffer, inst_ids, markets, analytics);
    append_leverage_settings(&mut buffer, leverages);
    append_market_analytics(&mut buffer, analytics);

    buffer
}

fn initial_leverage_cache(markets: &HashMap<String, MarketInfo>) -> HashMap<LeverageKey, f64> {
    let mut cache = HashMap::new();
    for (inst_id, market) in markets {
        apply_leverage_entry(&mut cache, inst_id, None, market.lever);
    }
    cache
}

fn apply_leverage_entry(
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

fn append_trade_limits(
    buffer: &mut String,
    inst_ids: &[String],
    markets: &HashMap<String, MarketInfo>,
    analytics: &[InstrumentAnalytics],
) {
    if inst_ids.is_empty() || markets.is_empty() {
        return;
    }
    let mut price_lookup = HashMap::new();
    for entry in analytics {
        if let Some(price) = entry.current_price {
            price_lookup.insert(entry.inst_id.to_ascii_uppercase(), price);
        }
    }
    let mut appended = false;
    for inst_id in inst_ids {
        let Some(market) = markets.get(inst_id) else {
            continue;
        };
        if !appended {
            buffer.push_str("\n【最小交易金额】\n");
            appended = true;
        }
        let price = price_lookup.get(&inst_id.to_ascii_uppercase()).copied();
        buffer.push_str("- ");
        buffer.push_str(&format_trade_limit(inst_id, market, price));
        buffer.push('\n');
    }
}

fn append_leverage_settings(buffer: &mut String, leverages: &[InstrumentLeverage]) {
    if leverages.is_empty() {
        return;
    }
    buffer.push_str("\n【杠杆设置】\n");
    for entry in leverages {
        buffer.push_str("- ");
        buffer.push_str(&format_leverage_entry(entry));
        buffer.push('\n');
    }
}

fn format_leverage_entry(entry: &InstrumentLeverage) -> String {
    let mut parts = Vec::new();
    if let Some(default) = entry.net {
        parts.push(format!("默认 {}x", format_float(default)));
    }
    if let Some(long) = entry.long {
        parts.push(format!("多头 {}x", format_float(long)));
    }
    if let Some(short) = entry.short {
        parts.push(format!("空头 {}x", format_float(short)));
    }
    if parts.is_empty() {
        format!("{}: 杠杆未知", entry.inst_id)
    } else {
        format!("{}: {}", entry.inst_id, parts.join(" / "))
    }
}

fn push_performance_stats(buffer: &mut String, stats: &PerformanceStats) {
    buffer.push_str(&format!(
        "{}（统计起点: {}）\n",
        stats.label,
        stats.start_time_label()
    ));
    buffer.push_str(&format!("- 成交笔数: {}\n", stats.trade_count));
    buffer.push_str(&format!("- 累计盈亏: {}\n", format_float(stats.total_pnl)));
    match stats.sharpe_ratio {
        Some(value) => buffer.push_str(&format!("- 夏普率: {}\n", format_float(value))),
        None => buffer.push_str("- 夏普率: 数据不足（少于 2 笔成交）\n"),
    }
}

fn format_trade_limit(inst_id: &str, market: &MarketInfo, latest_price: Option<f64>) -> String {
    let mut segments = Vec::new();
    if market.min_size > 0.0 {
        segments.push(format!(
            "最小 {} 张",
            format_contract_count(market.min_size)
        ));
    } else {
        segments.push("最小张数未知".to_string());
    }
    if market.ct_val > 0.0 {
        let face = format_float(market.ct_val);
        match market
            .ct_val_ccy
            .as_deref()
            .map(|ccy| ccy.trim())
            .filter(|value| !value.is_empty())
        {
            Some(ccy) => segments.push(format!("单张面值 {} {}", face, ccy)),
            None => segments.push(format!("单张面值 {}", face)),
        }
    }
    if market.ct_val > 0.0 && market.min_size > 0.0 {
        let notional = market.ct_val * market.min_size;
        let label = format_float(notional);
        match market
            .ct_val_ccy
            .as_deref()
            .map(|ccy| ccy.trim())
            .filter(|value| !value.is_empty())
        {
            Some(ccy) => segments.push(format!("最小名义 {} {}", label, ccy)),
            None => segments.push(format!("最小名义 {}", label)),
        }
        if is_usdt_quote(inst_id) {
            if let Some(price) = latest_price {
                let approx = price * notional;
                segments.push(format!("按现价约 {} USDT", format_float(approx)));
            }
        }
    }
    format!("{}: {}", inst_id, segments.join(" · "))
}

fn format_contract_count(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "-".to_string();
    }
    let rounded = value.round();
    if (value - rounded).abs() < 1e-6 {
        format!("{}", rounded as i64)
    } else {
        format_float(value)
    }
}

fn is_usdt_quote(inst_id: &str) -> bool {
    let upper = inst_id.to_ascii_uppercase();
    upper.ends_with("-USDT") || upper.contains("-USDT-")
}

fn append_market_analytics(buffer: &mut String, analytics: &[InstrumentAnalytics]) {
    if analytics.is_empty() {
        return;
    }
    buffer.push_str("\n【市场技术指标】\n");
    for entry in analytics {
        buffer.push_str(&format!("## {} ({})\n", entry.symbol, entry.inst_id));
        buffer.push_str("**当前价格**\n");
        buffer.push_str(&format!(
            "- current_price = {}\n",
            optional_float(entry.current_price)
        ));
        buffer.push_str(&format!(
            "- current_ema20 = {}\n",
            optional_float(entry.current_ema20)
        ));
        buffer.push_str(&format!(
            "- current_macd = {}\n",
            optional_float(entry.current_macd)
        ));
        buffer.push_str(&format!(
            "- current_rsi (7周期) = {}\n",
            optional_float(entry.current_rsi7)
        ));
        buffer.push_str("**永续合约指标：**\n");
        buffer.push_str(&format!(
            "- 未平仓合约：最新：{} | 平均值：{}\n",
            optional_float(entry.oi_latest),
            optional_float(entry.oi_average)
        ));
        buffer.push_str(&format!(
            "- 融资利率：{}\n",
            optional_float(entry.funding_rate)
        ));
        buffer.push_str("**日内走势（3分钟间隔，最早→最新）：**\n");
        buffer.push_str(&format!(
            "中间价：{}\n",
            format_series(&entry.intraday_prices)
        ));
        buffer.push_str(&format!(
            "EMA指标（20周期）：{}\n",
            format_series(&entry.intraday_ema20)
        ));
        buffer.push_str(&format!(
            "MACD指标：{}\n",
            format_series(&entry.intraday_macd)
        ));
        buffer.push_str(&format!(
            "RSI指标（7周期）：{}\n",
            format_series(&entry.intraday_rsi7)
        ));
        buffer.push_str(&format!(
            "RSI指标（14周期）：{}\n",
            format_series(&entry.intraday_rsi14)
        ));
        buffer.push_str("**长期走势（4小时图）：**\n");
        buffer.push_str(&format!(
            "20周期EMA：{} vs. 50周期EMA：{}\n",
            optional_float(entry.swing_ema20),
            optional_float(entry.swing_ema50)
        ));
        buffer.push_str(&format!(
            "3周期ATR： {} 与 14 周期 ATR 对比：{}\n",
            optional_float(entry.swing_atr3),
            optional_float(entry.swing_atr14)
        ));
        buffer.push_str(&format!(
            "当前成交量：{} 与平均成交量对比：{}\n",
            optional_float(entry.swing_volume_current),
            optional_float(entry.swing_volume_avg)
        ));
        buffer.push_str(&format!(
            "MACD 指标（4 小时）：{}\n",
            format_series(&entry.swing_macd)
        ));
        buffer.push_str(&format!(
            "RSI 指标（14 周期，4 小时）：{}\n",
            format_series(&entry.swing_rsi14)
        ));
    }
}

fn format_series(values: &[f64]) -> String {
    if values.is_empty() {
        "[]".to_string()
    } else {
        let joined = values
            .iter()
            .map(|value| format_float(*value))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{}]", joined)
    }
}

fn format_position(position: &PositionInfo) -> String {
    let side = position
        .pos_side
        .as_deref()
        .unwrap_or(if position.size >= 0.0 { "net" } else { "" });
    let upl = position.upl.map(format_float);
    let upl_ratio = position
        .upl_ratio
        .map(|ratio| format!("{:.2}%", ratio * 100.0));
    let gear = position.lever.map(format_float);
    let mut segments = Vec::new();
    segments.push(format!(
        "{} {} {} 张 @ {}",
        position.inst_id,
        side,
        format_float(position.size),
        optional_float(position.avg_px)
    ));
    if let Some(value) = upl {
        segments.push(format!("浮盈亏 {}", value));
    }
    if let Some(value) = upl_ratio {
        segments.push(format!("盈亏比 {}", value));
    }
    if let Some(value) = gear {
        segments.push(format!("杠杆 {}", value));
    }
    if let Some(timestamp) = format_timestamp_label(position.create_time) {
        segments.push(format!("建仓 {}", timestamp));
    }
    segments.push(format!("保证金 {}", format_float(position.imr)));
    segments.join(" · ")
}

fn format_order(order: &PendingOrderInfo) -> String {
    let trigger = order
        .trigger_price
        .map(|price| format!("触发 {}", format_float(price)));
    let limit_price = order
        .price
        .map(|price| format!("委托 {}", format_float(price)));
    let mut segments = Vec::new();
    segments.push(format!(
        "{} {} {} 张 ({})",
        order.inst_id,
        order.side,
        format_float(order.size),
        order.state
    ));
    if let Some(pos_side) = &order.pos_side {
        segments.push(format!("方向 {}", pos_side));
    }
    if let Some(value) = trigger {
        segments.push(value);
    }
    if let Some(value) = limit_price {
        segments.push(value);
    }
    if let Some(lever) = order.lever {
        segments.push(format!("杠杆 {}", format_float(lever)));
    }
    if order.reduce_only {
        segments.push("只减仓".to_string());
    }
    if let Some(timestamp) = format_timestamp_label(order.create_time) {
        segments.push(format!("创建 {}", timestamp));
    }
    segments.join(" · ")
}

fn format_balance(balance: &AccountBalanceDelta) -> String {
    let mut segments = Vec::new();
    segments.push(balance.currency.clone());
    if let Some(eq) = balance.equity {
        segments.push(format!("权益 {}", format_float(eq)));
    }
    if let Some(avail) = balance.available {
        segments.push(format!("可用 {}", format_float(avail)));
    }
    if let Some(cash) = balance.cash_balance {
        segments.push(format!("现金 {}", format_float(cash)));
    }
    segments.join(" · ")
}

fn format_timestamp_label(timestamp: Option<i64>) -> Option<String> {
    timestamp
        .and_then(|ts| Local.timestamp_millis_opt(ts).single())
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn format_float(value: f64) -> String {
    if value.abs() >= 100.0 {
        format!("{value:.2}")
    } else if value.abs() >= 1.0 {
        format!("{value:.4}")
    } else {
        format!("{value:.6}")
    }
}

fn optional_float(value: Option<f64>) -> String {
    value.map(format_float).unwrap_or_else(|| "-".to_string())
}

fn format_duration_brief(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs == 0 {
        return "0s".to_string();
    }
    if secs % 86_400 == 0 {
        return format!("{}d", secs / 86_400);
    }
    if secs % 3_600 == 0 {
        return format!("{}h", secs / 3_600);
    }
    if secs % 60 == 0 {
        return format!("{}m", secs / 60);
    }
    format!("{}s", secs)
}

fn compute_sharpe(returns: &[f64]) -> Option<f64> {
    if returns.len() < 2 {
        return None;
    }
    let mean = returns.iter().copied().sum::<f64>() / returns.len() as f64;
    let variance = returns
        .iter()
        .map(|value| {
            let diff = *value - mean;
            diff * diff
        })
        .sum::<f64>()
        / (returns.len() as f64 - 1.0);
    if !variance.is_finite() || variance <= 0.0 {
        return None;
    }
    let std_dev = variance.sqrt();
    if !std_dev.is_finite() || std_dev <= f64::EPSILON {
        return None;
    }
    let sharpe = (mean / std_dev) * (returns.len() as f64).sqrt();
    if sharpe.is_finite() {
        Some(sharpe)
    } else {
        None
    }
}

fn normalize_inst_ids(inst_ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for raw in inst_ids {
        let upper = raw.trim().to_ascii_uppercase();
        if upper.is_empty() {
            continue;
        }
        if seen.insert(upper.clone()) {
            normalized.push(upper);
        }
    }
    normalized
}

struct DeepseekClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl DeepseekClient {
    fn new(config: &DeepseekConfig) -> Result<Self> {
        Ok(DeepseekClient {
            http: Client::builder().timeout(Duration::from_secs(20)).build()?,
            base_url: config.endpoint.clone(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
        })
    }

    async fn chat_completion(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let request = ChatCompletionRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: SYSTEM_PROMPT.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: prompt.to_string(),
                },
            ],
            temperature: 0.3,
            response_format: Some(ResponseFormat {
                r#type: "json_object".to_string(),
            }),
        };
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("请求 Deepseek API 失败")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Deepseek 返回错误: {} - {}", status, body));
        }
        let completion: ChatCompletionResponse = response.json().await?;
        let choice = completion
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Deepseek 响应中缺少内容"))?;
        let content = choice.message.content.trim().to_string();
        if content.is_empty() {
            Err(anyhow!("Deepseek 响应为空"))
        } else {
            Ok(content)
        }
    }
}

#[derive(Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    temperature: f32,
    response_format: Option<ResponseFormat>,
}
#[derive(Serialize)]
struct ResponseFormat {
    r#type: String,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionMessage,
}

#[derive(Deserialize)]
struct ChatCompletionMessage {
    content: String,
}
