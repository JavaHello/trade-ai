use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Local;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::time::{self, MissedTickBehavior};

use crate::command::{
    AccountBalanceDelta, AccountSnapshot, AiInsightRecord, Command, PendingOrderInfo, PositionInfo,
};
use crate::config::DeepseekConfig;
use crate::okx::SharedAccountState;

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

1. **买入入场**：建立新的多头头寸（押注价格上涨）

- 适用情况：看涨技术形态、积极动能、风险回报比有利于上涨

2. **卖出入场**：建立新的空头头寸（押注价格下跌）

- 适用情况：看跌技术形态、消极动能、风险回报比有利于下跌

3. **持有**：维持现有头寸不变

- 适用情况：现有头寸表现符合预期，或不存在明显的优势

4. **平仓**：完全退出现有头寸

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

1. **可用资金**：仅使用可用资金（而非账户余额）
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
"coin": "BTC" | "ETH" | "SOL" | "BNB" | "DOGE" | "XRP",
"quantity": <float>,
"leverage": <integer 1-20>,
"profit_target": <float>,
"stop_loss": <float>,
"invalidation_condition": "<string>",
"confidence": <float 0-1>,
"risk_usd": <float>,
"justification": "<string>"
}
```

## 输出验证规则
- 所有数值字段必须为正数（信号为“hold”时除外）
- 做多时，profit_target 必须高于入场价；做空时，profit_target 必须低于入场价
- 做多时，stop_loss 必须低于入场价；做空时，stop_loss 必须高于入场价
- 理由必须简洁明了（最多 500 个字符）
- 当信号为“hold”时：将 quantity 设置为 0，leverage 设置为 1，并将 risk 字段设置为占位符
---
这是法律管辖范围内的一项研究实验。
注重技术分析和风险管理原则。
"#;
const MARKET_CANDLES_ENDPOINT: &str = "https://www.okx.com/api/v5/market/candles";
const FUNDING_RATE_ENDPOINT: &str = "https://www.okx.com/api/v5/public/funding-rate";
const OPEN_INTEREST_ENDPOINT: &str = "https://www.okx.com/api/v5/public/open-interest";
const OPEN_INTEREST_HISTORY_ENDPOINT: &str =
    "https://www.okx.com/api/v5/public/open-interest-history";
const MAX_ANALYTICS_INSTRUMENTS: usize = 3;
const ANALYTICS_INTRADAY_LIMIT: usize = 160;
const ANALYTICS_SWING_LIMIT: usize = 120;
const ANALYTICS_SERIES_TAIL: usize = 8;
const EMA_SHORT_PERIOD: usize = 20;
const EMA_LONG_PERIOD: usize = 50;
const RSI_SHORT_PERIOD: usize = 7;
const RSI_LONG_PERIOD: usize = 14;
const MACD_FAST_PERIOD: usize = 12;
const MACD_SLOW_PERIOD: usize = 26;
const ATR_FAST_PERIOD: usize = 3;
const ATR_SLOW_PERIOD: usize = 14;
const VOLUME_AVG_PERIOD: usize = 20;
const MAX_POSITIONS: usize = 12;
const MAX_ORDERS: usize = 12;
const MAX_BALANCES: usize = 12;

pub struct DeepseekReporter {
    client: DeepseekClient,
    state: SharedAccountState,
    tx: broadcast::Sender<Command>,
    interval: Duration,
    inst_ids: Vec<String>,
    market: MarketDataFetcher,
}

impl DeepseekReporter {
    pub fn new(
        config: DeepseekConfig,
        state: SharedAccountState,
        tx: broadcast::Sender<Command>,
        inst_ids: Vec<String>,
    ) -> Result<Self> {
        let client = DeepseekClient::new(&config)?;
        let market = MarketDataFetcher::new()?;
        let inst_ids = normalize_inst_ids(inst_ids);
        Ok(DeepseekReporter {
            client,
            state,
            tx,
            interval: config.interval,
            inst_ids,
            market,
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
        let analytics = self.collect_market_analytics().await;
        let prompt = build_snapshot_prompt(&snapshot, &analytics);
        let insight = self.client.chat_completion(&prompt).await?;
        let trimmed = insight.trim();
        if trimmed.is_empty() {
            return Ok(());
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

    async fn collect_market_analytics(&self) -> Vec<InstrumentAnalytics> {
        if self.inst_ids.is_empty() {
            return Vec::new();
        }
        let mut analytics = Vec::new();
        for inst_id in self.inst_ids.iter().take(MAX_ANALYTICS_INSTRUMENTS) {
            match self.market.fetch_inst(inst_id).await {
                Ok(entry) => analytics.push(entry),
                Err(err) => {
                    let _ = self.tx.send(Command::Error(format!(
                        "加载 {} 市场指标失败: {err}",
                        inst_id
                    )));
                }
            }
        }
        analytics
    }
}

fn has_material_data(snapshot: &AccountSnapshot) -> bool {
    !snapshot.positions.is_empty()
        || !snapshot.open_orders.is_empty()
        || snapshot.balance.total_equity.is_some()
        || !snapshot.balance.delta.is_empty()
}

#[derive(Debug, Clone, Default)]
struct InstrumentAnalytics {
    inst_id: String,
    symbol: String,
    current_price: Option<f64>,
    current_ema20: Option<f64>,
    current_macd: Option<f64>,
    current_rsi7: Option<f64>,
    oi_latest: Option<f64>,
    oi_average: Option<f64>,
    funding_rate: Option<f64>,
    intraday_prices: Vec<f64>,
    intraday_ema20: Vec<f64>,
    intraday_macd: Vec<f64>,
    intraday_rsi7: Vec<f64>,
    intraday_rsi14: Vec<f64>,
    swing_ema20: Option<f64>,
    swing_ema50: Option<f64>,
    swing_atr3: Option<f64>,
    swing_atr14: Option<f64>,
    swing_volume_current: Option<f64>,
    swing_volume_avg: Option<f64>,
    swing_macd: Vec<f64>,
    swing_rsi14: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
struct OpenInterestStats {
    latest: Option<f64>,
    average: Option<f64>,
}

struct MarketDataFetcher {
    http: Client,
}

impl MarketDataFetcher {
    fn new() -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(MarketDataFetcher { http })
    }

    async fn fetch_inst(&self, inst_id: &str) -> Result<InstrumentAnalytics> {
        let intraday = self
            .fetch_candles(inst_id, "3m", ANALYTICS_INTRADAY_LIMIT)
            .await?;
        if intraday.is_empty() {
            return Err(anyhow!("{} 缺少 3 分钟 K 线数据", inst_id));
        }
        let swing = self
            .fetch_candles(inst_id, "4H", ANALYTICS_SWING_LIMIT)
            .await
            .unwrap_or_default();
        let closes_intraday: Vec<f64> = intraday.iter().map(|c| c.close).collect();
        let closes_swing: Vec<f64> = swing.iter().map(|c| c.close).collect();
        let swing_volumes: Vec<f64> = swing.iter().map(|c| c.volume).collect();
        let ema20_intraday = compute_ema(&closes_intraday, EMA_SHORT_PERIOD);
        let macd_intraday = compute_macd(&closes_intraday);
        let rsi7_intraday = compute_rsi(&closes_intraday, RSI_SHORT_PERIOD);
        let rsi14_intraday = compute_rsi(&closes_intraday, RSI_LONG_PERIOD);
        let ema20_swing = compute_ema(&closes_swing, EMA_SHORT_PERIOD);
        let ema50_swing = compute_ema(&closes_swing, EMA_LONG_PERIOD);
        let macd_swing = compute_macd(&closes_swing);
        let rsi14_swing = compute_rsi(&closes_swing, RSI_LONG_PERIOD);
        let atr3_swing = compute_atr(&swing, ATR_FAST_PERIOD);
        let atr14_swing = compute_atr(&swing, ATR_SLOW_PERIOD);
        let oi_stats = self.fetch_open_interest(inst_id).await.unwrap_or_default();
        let funding_rate = self.fetch_funding_rate(inst_id).await.unwrap_or(None);
        let current_price = closes_intraday.last().copied();
        let swing_volume_current = swing_volumes.last().copied();
        let swing_volume_avg = average_tail(&swing_volumes, VOLUME_AVG_PERIOD);
        Ok(InstrumentAnalytics {
            inst_id: inst_id.to_string(),
            symbol: inst_symbol(inst_id),
            current_price,
            current_ema20: ema20_intraday.last().copied(),
            current_macd: macd_intraday.last().copied(),
            current_rsi7: rsi7_intraday.last().copied(),
            oi_latest: oi_stats.latest,
            oi_average: oi_stats.average,
            funding_rate,
            intraday_prices: take_tail(&closes_intraday, ANALYTICS_SERIES_TAIL),
            intraday_ema20: take_tail(&ema20_intraday, ANALYTICS_SERIES_TAIL),
            intraday_macd: take_tail(&macd_intraday, ANALYTICS_SERIES_TAIL),
            intraday_rsi7: take_tail(&rsi7_intraday, ANALYTICS_SERIES_TAIL),
            intraday_rsi14: take_tail(&rsi14_intraday, ANALYTICS_SERIES_TAIL),
            swing_ema20: ema20_swing.last().copied(),
            swing_ema50: ema50_swing.last().copied(),
            swing_atr3: atr3_swing.last().copied(),
            swing_atr14: atr14_swing.last().copied(),
            swing_volume_current,
            swing_volume_avg,
            swing_macd: take_tail(&macd_swing, ANALYTICS_SERIES_TAIL),
            swing_rsi14: take_tail(&rsi14_swing, ANALYTICS_SERIES_TAIL),
        })
    }

    async fn fetch_candles(&self, inst_id: &str, bar: &str, limit: usize) -> Result<Vec<Candle>> {
        let limit_str = limit.to_string();
        let response: MarketCandleResponse = self
            .http
            .get(MARKET_CANDLES_ENDPOINT)
            .query(&[
                ("instId", inst_id),
                ("bar", bar),
                ("limit", limit_str.as_str()),
            ])
            .send()
            .await
            .with_context(|| format!("请求 {} {} K 线失败", inst_id, bar))?
            .error_for_status()
            .with_context(|| format!("{} {} K 线响应异常", inst_id, bar))?
            .json()
            .await
            .with_context(|| format!("解析 {} {} K 线数据失败", inst_id, bar))?;
        if response.code != "0" {
            return Err(anyhow!(
                "{} {} K 线接口返回错误 (code {}): {}",
                inst_id,
                bar,
                response.code,
                response.msg
            ));
        }
        let mut candles = Vec::new();
        for entry in response.data {
            if let Some(candle) = Candle::from_entry(&entry) {
                candles.push(candle);
            }
        }
        candles.sort_by_key(|c| c.ts);
        Ok(candles)
    }

    async fn fetch_funding_rate(&self, inst_id: &str) -> Result<Option<f64>> {
        let response: FundingRateResponse = self
            .http
            .get(FUNDING_RATE_ENDPOINT)
            .query(&[("instId", inst_id)])
            .send()
            .await
            .with_context(|| format!("请求 {} 融资利率失败", inst_id))?
            .error_for_status()
            .with_context(|| format!("{} 融资利率响应异常", inst_id))?
            .json()
            .await
            .with_context(|| format!("解析 {} 融资利率失败", inst_id))?;
        if response.code != "0" {
            return Err(anyhow!(
                "{} funding rate failed (code {}): {}",
                inst_id,
                response.code,
                response.msg
            ));
        }
        Ok(response
            .data
            .first()
            .and_then(|entry| parse_f64(&entry.funding_rate)))
    }

    async fn fetch_open_interest(&self, inst_id: &str) -> Result<OpenInterestStats> {
        let latest = self.fetch_open_interest_latest(inst_id).await?;
        let history = self.fetch_open_interest_history(inst_id).await?;
        let avg = if history.is_empty() {
            latest
        } else {
            Some(history.iter().sum::<f64>() / history.len() as f64)
        };
        Ok(OpenInterestStats {
            latest,
            average: avg,
        })
    }

    async fn fetch_open_interest_latest(&self, inst_id: &str) -> Result<Option<f64>> {
        let response: OpenInterestResponse = self
            .http
            .get(OPEN_INTEREST_ENDPOINT)
            .query(&[("instType", "SWAP"), ("instId", inst_id)])
            .send()
            .await
            .with_context(|| format!("请求 {} 未平仓合约失败", inst_id))?
            .error_for_status()
            .with_context(|| format!("{} 未平仓合约响应异常", inst_id))?
            .json()
            .await
            .with_context(|| format!("解析 {} 未平仓合约失败", inst_id))?;
        if response.code != "0" {
            return Err(anyhow!(
                "{} open interest failed (code {}): {}",
                inst_id,
                response.code,
                response.msg
            ));
        }
        Ok(response.data.first().and_then(|entry| parse_f64(&entry.oi)))
    }

    async fn fetch_open_interest_history(&self, inst_id: &str) -> Result<Vec<f64>> {
        let response: OpenInterestHistoryResponse = self
            .http
            .get(OPEN_INTEREST_HISTORY_ENDPOINT)
            .query(&[("instType", "SWAP"), ("instId", inst_id), ("period", "8H")])
            .send()
            .await
            .with_context(|| format!("请求 {} 未平仓合约历史失败", inst_id))?
            .error_for_status()
            .with_context(|| format!("{} 未平仓合约历史响应异常", inst_id))?
            .json()
            .await
            .with_context(|| format!("解析 {} 未平仓合约历史失败", inst_id))?;
        if response.code != "0" {
            return Err(anyhow!(
                "{} open interest history failed (code {}): {}",
                inst_id,
                response.code,
                response.msg
            ));
        }
        let mut values = Vec::new();
        for entry in response.data {
            if let Some(value) = parse_f64(&entry.oi) {
                values.push(value);
            }
        }
        Ok(values)
    }
}

#[derive(Debug, Clone)]
struct Candle {
    ts: i64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl Candle {
    fn from_entry(entry: &[String]) -> Option<Self> {
        if entry.len() < 6 {
            return None;
        }
        Some(Candle {
            ts: entry.get(0)?.parse().ok()?,
            high: parse_f64(entry.get(2)?)?,
            low: parse_f64(entry.get(3)?)?,
            close: parse_f64(entry.get(4)?)?,
            volume: parse_f64(entry.get(5)?)?,
        })
    }
}

#[derive(Debug, serde::Deserialize)]
struct MarketCandleResponse {
    code: String,
    msg: String,
    data: Vec<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingRateResponse {
    code: String,
    msg: String,
    data: Vec<FundingRateEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingRateEntry {
    funding_rate: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestResponse {
    code: String,
    msg: String,
    data: Vec<OpenInterestEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestEntry {
    oi: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestHistoryResponse {
    code: String,
    msg: String,
    data: Vec<OpenInterestHistoryEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestHistoryEntry {
    oi: String,
}

fn build_snapshot_prompt(snapshot: &AccountSnapshot, analytics: &[InstrumentAnalytics]) -> String {
    let mut buffer = String::new();
    buffer.push_str("以下为 OKX 账户的实时快照，请据此输出风险与操作建议：\n\n");

    if let Some(eq) = snapshot.balance.total_equity {
        buffer.push_str(&format!("总权益: {}\n", format_float(eq)));
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

    append_market_analytics(&mut buffer, analytics);

    buffer
}

fn append_market_analytics(buffer: &mut String, analytics: &[InstrumentAnalytics]) {
    if analytics.is_empty() {
        return;
    }
    buffer.push_str("\n【市场技术指标】\n");
    for entry in analytics {
        buffer.push_str(&format!("{} ({})\n", entry.symbol, entry.inst_id));
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
            "- 未平仓合约：最新：{} |平均值：{}\n",
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

fn take_tail(values: &[f64], count: usize) -> Vec<f64> {
    if count == 0 || values.is_empty() {
        Vec::new()
    } else if values.len() <= count {
        values.to_vec()
    } else {
        values[values.len() - count..].to_vec()
    }
}

fn average_tail(values: &[f64], period: usize) -> Option<f64> {
    if values.is_empty() || period == 0 {
        return None;
    }
    let start = values.len().saturating_sub(period);
    let slice = &values[start..];
    if slice.is_empty() {
        None
    } else {
        Some(slice.iter().sum::<f64>() / slice.len() as f64)
    }
}

fn compute_ema(series: &[f64], period: usize) -> Vec<f64> {
    if series.is_empty() || period == 0 {
        return Vec::new();
    }
    let mut ema_values = Vec::with_capacity(series.len());
    let k = 2.0 / (period as f64 + 1.0);
    let mut ema = series[0];
    for &value in series {
        ema = value * k + ema * (1.0 - k);
        ema_values.push(ema);
    }
    ema_values
}

fn compute_macd(series: &[f64]) -> Vec<f64> {
    if series.is_empty() {
        return Vec::new();
    }
    let fast = compute_ema(series, MACD_FAST_PERIOD);
    let slow = compute_ema(series, MACD_SLOW_PERIOD);
    fast.iter()
        .zip(slow.iter())
        .map(|(f, s)| f - s)
        .collect::<Vec<f64>>()
}

fn compute_rsi(series: &[f64], period: usize) -> Vec<f64> {
    if series.len() < 2 || period == 0 {
        return Vec::new();
    }
    let mut rsis = vec![50.0; series.len()];
    if series.len() <= period {
        return rsis;
    }
    let mut gain_sum = 0.0;
    let mut loss_sum = 0.0;
    for i in 1..=period {
        let delta = series[i] - series[i - 1];
        if delta >= 0.0 {
            gain_sum += delta;
        } else {
            loss_sum -= delta;
        }
    }
    let mut avg_gain = gain_sum / period as f64;
    let mut avg_loss = loss_sum / period as f64;
    rsis[period] = rsi_from_avg(avg_gain, avg_loss);
    for i in (period + 1)..series.len() {
        let delta = series[i] - series[i - 1];
        let gain = delta.max(0.0);
        let loss = (-delta).max(0.0);
        avg_gain = (avg_gain * (period as f64 - 1.0) + gain) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + loss) / period as f64;
        rsis[i] = rsi_from_avg(avg_gain, avg_loss);
    }
    rsis
}

fn rsi_from_avg(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss.abs() < f64::EPSILON {
        100.0
    } else {
        let rs = avg_gain / avg_loss;
        100.0 - (100.0 / (1.0 + rs))
    }
}

fn compute_atr(candles: &[Candle], period: usize) -> Vec<f64> {
    if candles.is_empty() || period == 0 {
        return Vec::new();
    }
    let mut atr_values = vec![0.0; candles.len()];
    let mut tr_values = Vec::with_capacity(candles.len());
    for (idx, candle) in candles.iter().enumerate() {
        let prev_close = if idx == 0 {
            candle.close
        } else {
            candles[idx - 1].close
        };
        let tr = (candle.high - candle.low)
            .max((candle.high - prev_close).abs())
            .max((candle.low - prev_close).abs());
        tr_values.push(tr);
    }
    if tr_values.len() <= period {
        return atr_values;
    }
    let mut atr = tr_values[..period].iter().sum::<f64>() / period as f64;
    atr_values[period] = atr;
    for idx in (period + 1)..tr_values.len() {
        atr = ((period as f64 - 1.0) * atr + tr_values[idx]) / period as f64;
        atr_values[idx] = atr;
    }
    atr_values
}

fn inst_symbol(inst_id: &str) -> String {
    inst_id.split('-').next().unwrap_or(inst_id).to_string()
}

fn parse_f64<S: AsRef<str>>(value: S) -> Option<f64> {
    value.as_ref().trim().parse().ok()
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
