use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};

use crate::ai_decision::{AI_TAG_CLOSE, AI_TAG_ENTRY, AI_TAG_STOP_LOSS, AI_TAG_TAKE_PROFIT};
use crate::command::{AccountBalanceDelta, AccountSnapshot, PendingOrderInfo, PositionInfo};
use crate::config::ConfiguredTimeZone;
use crate::okx::MarketInfo;
use crate::okx_analytics::{InstrumentAnalytics, KlineRecord};

pub const SYSTEM_PROMPT_PATH: &str = "prompt/system.md";
const MAX_POSITIONS: usize = 12;
const MAX_ORDERS: usize = 12;
const MAX_BALANCES: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct InstrumentLeverage {
    pub inst_id: String,
    pub net: Option<f64>,
    pub long: Option<f64>,
    pub short: Option<f64>,
}

impl InstrumentLeverage {
    pub fn new(inst_id: String) -> Self {
        InstrumentLeverage {
            inst_id,
            net: None,
            long: None,
            short: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PerformanceStats {
    pub label: String,
    pub start_timestamp_ms: i64,
    pub trade_count: usize,
    pub sharpe_ratio: Option<f64>,
    pub total_pnl: f64,
}

impl PerformanceStats {
    fn start_time(&self) -> DateTime<Local> {
        Local
            .timestamp_millis_opt(self.start_timestamp_ms)
            .single()
            .unwrap_or_else(Local::now)
    }

    pub fn start_time_label(&self) -> String {
        self.start_time().format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PerformanceSummary {
    pub overall: Option<PerformanceStats>,
    pub recent: Option<PerformanceStats>,
}

pub fn load_system_prompt() -> Result<String> {
    fs::read_to_string(SYSTEM_PROMPT_PATH)
        .with_context(|| format!("读取系统提示词模板失败: {}", SYSTEM_PROMPT_PATH))
}

pub fn build_snapshot_prompt(
    snapshot: &AccountSnapshot,
    analytics: &[InstrumentAnalytics],
    performance: Option<&PerformanceSummary>,
    inst_ids: &[String],
    markets: &HashMap<String, MarketInfo>,
    leverages: &[InstrumentLeverage],
    timezone: ConfiguredTimeZone,
) -> String {
    let mut buffer = String::new();
    buffer.push_str(&format!(
        "当前时间: {}\n",
        timezone
            .format_timestamp(Local::now().timestamp_millis(), "%Y-%m-%d %H:%M:%S")
            .unwrap_or_else(|| "未知".to_string())
    ));
    buffer.push_str("下方为您提供各种状态数据、价格数据和预测信号，助您发掘超额收益。再下方是您当前的账户信息，包括账户价值、业绩、持仓等。\n\n");
    buffer.push_str("⚠️ **重要提示：以下所有价格或信号数据均按时间顺序排列：最早 → 最新**\n\n");
    buffer.push_str("**时间周期说明：**除非章节标题另有说明，否则日内数据以**5分钟为间隔**提供。如果某个币种使用不同的时间间隔，则会在该币种的章节中明确说明。\n\n");

    buffer.push_str("\n---\n");
    append_trade_limits(&mut buffer, inst_ids, markets, analytics);
    buffer.push_str("\n---\n");
    append_leverage_settings(&mut buffer, leverages);
    buffer.push_str("\n---\n");
    append_market_analytics(&mut buffer, analytics, timezone);

    buffer.push_str("\n---\n");
    buffer.push_str("\n#【资金币种】\n");
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

    if let Some(summary) = performance {
        buffer.push_str("\n---\n");
        buffer.push_str("\n#【策略运行概览】\n");
        if let Some(eq) = snapshot.balance.total_equity {
            buffer.push_str(&format!("总权益: {}\n", format_float(eq)));
        }
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

    buffer.push_str("\n---\n");
    buffer.push_str("\n#【持仓情况】\n");
    if snapshot.positions.is_empty() {
        buffer.push_str("无持仓\n");
    } else {
        for position in snapshot.positions.iter().take(MAX_POSITIONS) {
            buffer.push_str("- ");
            buffer.push_str(&format_position(position, timezone));
            buffer.push('\n');
        }
        if snapshot.positions.len() > MAX_POSITIONS {
            buffer.push_str(&format!(
                "... 其余 {} 条持仓已省略\n",
                snapshot.positions.len() - MAX_POSITIONS
            ));
        }
    }

    buffer.push_str("\n---\n");
    buffer.push_str("\n#【挂单情况】\n");
    if snapshot.open_orders.is_empty() {
        buffer.push_str("无挂单\n");
    } else {
        for order in snapshot.open_orders.iter().take(MAX_ORDERS) {
            buffer.push_str("- ");
            buffer.push_str(&format_order(order, timezone));
            buffer.push('\n');
        }
        if snapshot.open_orders.len() > MAX_ORDERS {
            buffer.push_str(&format!(
                "... 其余 {} 条挂单已省略\n",
                snapshot.open_orders.len() - MAX_ORDERS
            ));
        }
    }

    buffer.push_str("\n\n根据以上数据，请以要求的 JSON 格式提供您的交易决策。");
    buffer
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
            buffer.push_str("\n#【最小交易金额】\n");
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
    buffer.push_str("\n#【杠杆设置】\n");
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

fn append_market_analytics(
    buffer: &mut String,
    analytics: &[InstrumentAnalytics],
    timezone: ConfiguredTimeZone,
) {
    if analytics.is_empty() {
        return;
    }
    buffer.push_str("\n#【市场技术指标】\n");
    for entry in analytics {
        buffer.push_str(&format!("\n## {} ({})\n", entry.symbol, entry.inst_id));
        buffer.push_str("\n### **当前价格**\n");
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
        buffer.push_str("\n### **永续合约指标：**\n");
        buffer.push_str(&format!(
            "- 未平仓合约：最新：{} | 平均值：{}\n",
            optional_float(entry.oi_latest),
            optional_float(entry.oi_average)
        ));
        buffer.push_str(&format!(
            "- 资金费率：{}\n",
            optional_float(entry.funding_rate)
        ));
        append_recent_kline_table(buffer, &entry.recent_candles_5m, "5m", timezone);
        buffer.push_str("\n### **日内走势（5分钟间隔，最早→最新）：**\n");
        buffer.push_str(&format!(
            "- 中间价：{}\n",
            format_series(&entry.intraday_prices)
        ));
        buffer.push_str(&format!(
            "- EMA指标（20周期）：{}\n",
            format_series(&entry.intraday_ema20)
        ));
        buffer.push_str(&format!(
            "- MACD指标：{}\n",
            format_series(&entry.intraday_macd)
        ));
        buffer.push_str(&format!(
            "- RSI指标（7周期）：{}\n",
            format_series(&entry.intraday_rsi7)
        ));
        buffer.push_str(&format!(
            "- RSI指标（14周期）：{}\n",
            format_series(&entry.intraday_rsi14)
        ));
        append_recent_kline_table(buffer, &entry.recent_candles_4h, "4h", timezone);
        buffer.push_str("\n### **长期走势（4小时图）：**\n");
        buffer.push_str(&format!(
            "- 20周期EMA：{} vs. 50周期EMA：{}\n",
            optional_float(entry.swing_ema20),
            optional_float(entry.swing_ema50)
        ));
        buffer.push_str(&format!(
            "- 3周期ATR： {} 与 14 周期 ATR 对比：{}\n",
            optional_float(entry.swing_atr3),
            optional_float(entry.swing_atr14)
        ));
        buffer.push_str(&format!(
            "- 当前成交量：{} 与平均成交量对比：{}\n",
            optional_float(entry.swing_volume_current),
            optional_float(entry.swing_volume_avg)
        ));
        buffer.push_str(&format!(
            "- MACD 指标（4 小时）：{}\n",
            format_series(&entry.swing_macd)
        ));
        buffer.push_str(&format!(
            "- RSI 指标（14 周期，4 小时）：{}\n",
            format_series(&entry.swing_rsi14)
        ));
    }
}

fn append_recent_kline_table(
    buffer: &mut String,
    recent_candles: &Vec<KlineRecord>,
    m: &str,
    timezone: ConfiguredTimeZone,
) {
    if !recent_candles.is_empty() {
        buffer.push_str(&format!("### **最近 {} K 线（旧→新）：**\n\n", m));
        buffer
            .push_str("| 日期                | 开盘价 | 高价   | 低价   | 收盘价 | 成交量     |\n");
        buffer
            .push_str("| ------------------- | ------ | ------ | ------ | ------ | ---------- |\n");
        for candle in recent_candles {
            let timestamp = format_kline_timestamp(candle.timestamp_ms, timezone);
            buffer.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                timestamp,
                format_float(candle.open),
                format_float(candle.high),
                format_float(candle.low),
                format_float(candle.close),
                format_float(candle.volume)
            ));
        }
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

fn format_kline_timestamp(timestamp_ms: i64, timezone: ConfiguredTimeZone) -> String {
    format_timestamp_label(Some(timestamp_ms), timezone).unwrap_or_else(|| timestamp_ms.to_string())
}

fn format_position(position: &PositionInfo, timezone: ConfiguredTimeZone) -> String {
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
    if let Some(timestamp) = format_timestamp_label(position.create_time, timezone) {
        segments.push(format!("建仓 {}", timestamp));
    }
    segments.push(format!("保证金 {}", format_float(position.imr)));
    segments.join(" · ")
}

fn format_order(order: &PendingOrderInfo, timezone: ConfiguredTimeZone) -> String {
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
    if let Some(tag) = &order.tag {
        if tag.eq_ignore_ascii_case(AI_TAG_TAKE_PROFIT) {
            segments.push("止盈单".to_string());
        } else if tag.eq_ignore_ascii_case(AI_TAG_STOP_LOSS) {
            segments.push("止损单".to_string());
        } else if tag.eq_ignore_ascii_case(AI_TAG_ENTRY) {
            segments.push("限价开仓".to_string());
        } else if tag.eq_ignore_ascii_case(AI_TAG_CLOSE) {
            segments.push("限价平仓".to_string());
        } else {
            segments.push(format!("标签 {}", tag));
        }
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
    if let Some(timestamp) = format_timestamp_label(order.create_time, timezone) {
        segments.push(format!("创建 {}", timestamp));
    }
    segments.push(format!("订单号 {}", order.ord_id));
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

fn format_timestamp_label(timestamp: Option<i64>, timezone: ConfiguredTimeZone) -> Option<String> {
    timestamp.and_then(|ts| timezone.format_timestamp(ts, "%Y-%m-%d %H:%M:%S"))
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
