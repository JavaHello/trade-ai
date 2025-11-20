use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde_json::{Value, json};

use crate::ai_decision::{AI_TAG_CLOSE, AI_TAG_ENTRY, AI_TAG_STOP_LOSS, AI_TAG_TAKE_PROFIT};
use crate::command::AccountSnapshot;
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
    let data = build_snapshot(
        snapshot,
        analytics,
        performance,
        inst_ids,
        markets,
        leverages,
        timezone,
    );
    data
}

fn build_snapshot(
    snapshot: &AccountSnapshot,
    analytics: &[InstrumentAnalytics],
    performance: Option<&PerformanceSummary>,
    inst_ids: &[String],
    markets: &HashMap<String, MarketInfo>,
    leverages: &[InstrumentLeverage],
    timezone: ConfiguredTimeZone,
) -> String {
    let current_time = timezone
        .format_timestamp(Local::now().timestamp_millis(), "%Y-%m-%d %H:%M:%S")
        .unwrap_or_else(|| "未知".to_string());

    let mut data = String::new();
    data.push_str("当前时间: ");
    data.push_str(&current_time);
    data.push_str("\n");
    data.push_str("下方为您提供各种状态数据、价格数据和预测信号，助您发掘超额收益。再下方是您当前的账户信息，包括账户价值、业绩、持仓等。\n\n");
    data.push_str("⚠️ 所有数组、K线均按时间从旧 → 新排列（与系统说明一致）。\n");
    data.push_str("除非另有说明，日内数据以5分钟为间隔提供。\n\n");

    // Trade limits
    if !inst_ids.is_empty() && !markets.is_empty() {
        data.push_str("## 交易限制:\n");
        let limits_json = build_trade_limits_json(inst_ids, markets, analytics);
        data.push_str("```json\n");
        data.push_str(&limits_json.to_string());
        data.push_str("\n```\n\n");
    }

    // Leverage settings
    if !leverages.is_empty() {
        data.push_str("## 杠杆设置:\n");
        let leverage_json = build_leverage_json(leverages);
        data.push_str("```json\n");
        data.push_str(&leverage_json.to_string());
        data.push_str("\n```\n\n");
    }

    // Market analytics
    if !analytics.is_empty() {
        data.push_str("## 市场分析:\n");
        let market_json = build_market_analytics_json(analytics, timezone);
        data.push_str("```json\n");
        data.push_str(&market_json.to_string());
        data.push_str("\n```\n\n");
    }

    // Balance
    data.push_str("## 账户情况:\n");
    let balance_json = build_balance_json(snapshot, performance);
    data.push_str("```json\n");
    data.push_str(&balance_json.to_string());
    data.push_str("\n```\n\n");

    // Positions
    data.push_str("## 持仓情况:\n");
    let positions_json = build_positions_json(snapshot, timezone);
    data.push_str("```json\n");
    data.push_str(&positions_json.to_string());
    data.push_str("\n```\n\n");

    // Open orders
    data.push_str("## 挂单情况:\n");
    data.push_str("```json\n");
    data.push_str(&build_orders_json(snapshot, timezone).to_string());
    data.push_str("\n```\n\n");

    data.push_str("\n根据以上数据，请以要求的 JSON 格式提供您的交易决策。");
    data
}

fn build_trade_limits_json(
    inst_ids: &[String],
    markets: &HashMap<String, MarketInfo>,
    analytics: &[InstrumentAnalytics],
) -> Value {
    let mut price_lookup = HashMap::new();
    for entry in analytics {
        if let Some(price) = entry.current_price {
            price_lookup.insert(entry.inst_id.to_ascii_uppercase(), price);
        }
    }

    let mut limits = Vec::new();
    for inst_id in inst_ids {
        let Some(market) = markets.get(inst_id) else {
            continue;
        };
        let price = price_lookup.get(&inst_id.to_ascii_uppercase()).copied();

        let min_size = if market.min_size > 0.0 {
            format_contract_count(market.min_size)
        } else {
            "未知".to_string()
        };

        let mut limit_obj = json!({
            "inst_id": inst_id,
            "min_size": min_size,
        });

        if market.ct_val > 0.0 {
            let obj = limit_obj.as_object_mut().unwrap();
            obj.insert(
                "contract_value".to_string(),
                json!(format_float(market.ct_val)),
            );
            if let Some(ccy) = market
                .ct_val_ccy
                .as_deref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                obj.insert("contract_currency".to_string(), json!(ccy));
            }

            if market.min_size > 0.0 {
                let notional = market.ct_val * market.min_size;
                obj.insert("min_notional".to_string(), json!(format_float(notional)));

                if is_usdt_quote(inst_id) {
                    if let Some(p) = price {
                        let approx = p * notional;
                        obj.insert(
                            "min_notional_usdt_approx".to_string(),
                            json!(format_float(approx)),
                        );
                    }
                }
            }
        }

        limits.push(limit_obj);
    }

    json!(limits)
}

fn build_leverage_json(leverages: &[InstrumentLeverage]) -> Value {
    let leverage_list: Vec<Value> = leverages
        .iter()
        .map(|entry| {
            let mut obj = json!({
                "inst_id": entry.inst_id,
            });
            let map = obj.as_object_mut().unwrap();

            if let Some(net) = entry.net {
                map.insert("net".to_string(), json!(format_float(net)));
            }
            if let Some(long) = entry.long {
                map.insert("long".to_string(), json!(format_float(long)));
            }
            if let Some(short) = entry.short {
                map.insert("short".to_string(), json!(format_float(short)));
            }

            obj
        })
        .collect();

    json!(leverage_list)
}

fn build_balance_json(
    snapshot: &AccountSnapshot,
    performance: Option<&PerformanceSummary>,
) -> Value {
    let mut balance_list = Vec::new();
    for balance in snapshot.balance.delta.iter().take(MAX_BALANCES) {
        let mut obj = json!({
            "currency": balance.currency,
        });
        let map = obj.as_object_mut().unwrap();

        if let Some(eq) = balance.equity {
            map.insert("equity".to_string(), json!(format_float(eq)));
        }
        if let Some(avail) = balance.available {
            map.insert("available".to_string(), json!(format_float(avail)));
        }
        if let Some(cash) = balance.cash_balance {
            map.insert("cash_balance".to_string(), json!(format_float(cash)));
        }

        balance_list.push(obj);
    }

    let mut result = json!({
        "currencies": balance_list,
    });

    if let Some(eq) = snapshot.balance.total_equity {
        result["total_equity"] = json!(format_float(eq));
    }

    if let Some(summary) = performance {
        let mut perf = json!({});
        let perf_obj = perf.as_object_mut().unwrap();

        if let Some(overall) = &summary.overall {
            perf_obj.insert("overall".to_string(), build_performance_stats_json(overall));
        }
        if let Some(recent) = &summary.recent {
            perf_obj.insert("recent".to_string(), build_performance_stats_json(recent));
        }

        result["performance"] = perf;
    }

    result
}

fn build_performance_stats_json(stats: &PerformanceStats) -> Value {
    let mut obj = json!({
        "label": stats.label,
        "start_time": stats.start_time_label(),
        "trade_count": stats.trade_count,
        "total_pnl": format_float(stats.total_pnl),
    });

    if let Some(sharpe) = stats.sharpe_ratio {
        obj["sharpe_ratio"] = json!(format_float(sharpe));
    } else {
        obj["sharpe_ratio"] = json!(null);
        obj["sharpe_note"] = json!("数据不足（少于 2 笔成交）");
    }

    obj
}

fn build_positions_json(snapshot: &AccountSnapshot, timezone: ConfiguredTimeZone) -> Value {
    let positions: Vec<Value> = snapshot
        .positions
        .iter()
        .take(MAX_POSITIONS)
        .map(|pos| {
            let side = pos
                .pos_side
                .as_deref()
                .unwrap_or(if pos.size >= 0.0 { "net" } else { "" });

            let mut obj = json!({
                "inst_id": pos.inst_id,
                "side": side,
                "size": format_float(pos.size),
                "margin": format_float(pos.imr),
            });
            let map = obj.as_object_mut().unwrap();

            if let Some(px) = pos.avg_px {
                map.insert("avg_price".to_string(), json!(format_float(px)));
            }
            if let Some(upl) = pos.upl {
                map.insert("unrealized_pnl".to_string(), json!(format_float(upl)));
            }
            if let Some(ratio) = pos.upl_ratio {
                map.insert(
                    "pnl_ratio_pct".to_string(),
                    json!(format!("{:.2}", ratio * 100.0)),
                );
            }
            if let Some(lev) = pos.lever {
                map.insert("leverage".to_string(), json!(format_float(lev)));
            }
            if let Some(ts) = format_timestamp_label(pos.create_time, timezone) {
                map.insert("create_time".to_string(), json!(ts));
            }

            obj
        })
        .collect();

    json!(positions)
}

fn build_orders_json(snapshot: &AccountSnapshot, timezone: ConfiguredTimeZone) -> Value {
    let orders: Vec<Value> = snapshot
        .open_orders
        .iter()
        .take(MAX_ORDERS)
        .map(|order| {
            let mut obj = json!({
                "ord_id": order.ord_id,
                "inst_id": order.inst_id,
                "side": order.side,
                "size": format_float(order.size),
                "state": order.state,
            });
            let map = obj.as_object_mut().unwrap();

            if let Some(pos_side) = &order.pos_side {
                map.insert("pos_side".to_string(), json!(pos_side));
            }
            if let Some(tag) = &order.tag {
                let tag_label = if tag.eq_ignore_ascii_case(AI_TAG_TAKE_PROFIT) {
                    "止盈单"
                } else if tag.eq_ignore_ascii_case(AI_TAG_STOP_LOSS) {
                    "止损单"
                } else if tag.eq_ignore_ascii_case(AI_TAG_ENTRY) {
                    "限价开仓"
                } else if tag.eq_ignore_ascii_case(AI_TAG_CLOSE) {
                    "限价平仓"
                } else {
                    tag.as_str()
                };
                map.insert("tag".to_string(), json!(tag_label));
            }
            if let Some(trigger_px) = order.trigger_price {
                map.insert("trigger_price".to_string(), json!(format_float(trigger_px)));
            }
            if let Some(limit_px) = order.price {
                map.insert("limit_price".to_string(), json!(format_float(limit_px)));
            }
            if let Some(lev) = order.lever {
                map.insert("leverage".to_string(), json!(format_float(lev)));
            }
            if order.reduce_only {
                map.insert("reduce_only".to_string(), json!(true));
            }
            if let Some(ts) = format_timestamp_label(order.create_time, timezone) {
                map.insert("create_time".to_string(), json!(ts));
            }

            obj
        })
        .collect();

    json!(orders)
}

fn build_market_analytics_json(
    analytics: &[InstrumentAnalytics],
    timezone: ConfiguredTimeZone,
) -> Value {
    let instruments: Vec<Value> = analytics
        .iter()
        .map(|entry| {
            json!({
                "symbol": entry.symbol,
                "inst_id": entry.inst_id,
                "current_price": optional_float(entry.current_price),
                "current_ema20": optional_float(entry.current_ema20),
                "current_macd": optional_float(entry.current_macd),
                "current_rsi7": optional_float(entry.current_rsi7),
                "perpetual_indicators": {
                    "open_interest_latest": optional_float(entry.oi_latest),
                    "open_interest_average": optional_float(entry.oi_average),
                    "funding_rate": optional_float(entry.funding_rate),
                },
                "recent_candles_5m": build_kline_table_json(&entry.recent_candles_5m, timezone),
                "intraday_1m": {
                    "ema20": format_series_json(&entry.intraday_1m_ema20),
                    "macd": format_series_json(&entry.intraday_1m_macd),
                    "rsi7": format_series_json(&entry.intraday_1m_rsi7),
                    "rsi14": format_series_json(&entry.intraday_1m_rsi14),
                },
                "intraday_3m": {
                    "ema20": format_series_json(&entry.intraday_3m_ema20),
                    "macd": format_series_json(&entry.intraday_3m_macd),
                    "rsi7": format_series_json(&entry.intraday_3m_rsi7),
                    "rsi14": format_series_json(&entry.intraday_3m_rsi14),
                },
                "intraday_5m": {
                    "prices": format_series_json(&entry.intraday_prices),
                    "ema20": format_series_json(&entry.intraday_ema20),
                    "macd": format_series_json(&entry.intraday_macd),
                    "rsi7": format_series_json(&entry.intraday_rsi7),
                    "rsi14": format_series_json(&entry.intraday_rsi14),
                },
                "intraday_15m": {
                    "ema20": format_series_json(&entry.intraday_15m_ema20),
                    "macd": format_series_json(&entry.intraday_15m_macd),
                    "rsi7": format_series_json(&entry.intraday_15m_rsi7),
                    "rsi14": format_series_json(&entry.intraday_15m_rsi14),
                },
                "recent_candles_4h": build_kline_table_json(&entry.recent_candles_4h, timezone),
                "swing_4h": {
                    "ema20": optional_float(entry.swing_ema20),
                    "ema50": optional_float(entry.swing_ema50),
                    "atr3": optional_float(entry.swing_atr3),
                    "atr14": optional_float(entry.swing_atr14),
                    "volume_current": optional_float(entry.swing_volume_current),
                    "volume_avg": optional_float(entry.swing_volume_avg),
                    "macd": format_series_json(&entry.swing_macd),
                    "rsi14": format_series_json(&entry.swing_rsi14),
                }
            })
        })
        .collect();

    json!(instruments)
}

fn build_kline_table_json(candles: &Vec<KlineRecord>, timezone: ConfiguredTimeZone) -> Value {
    let klines: Vec<Value> = candles
        .iter()
        .map(|candle| {
            json!({
                "timestamp": format_kline_timestamp(candle.timestamp_ms, timezone),
                "open": format_float(candle.open),
                "high": format_float(candle.high),
                "low": format_float(candle.low),
                "close": format_float(candle.close),
                "volume": format_float(candle.volume),
            })
        })
        .collect();

    json!(klines)
}

fn format_series_json(values: &[f64]) -> Value {
    let formatted: Vec<String> = values.iter().map(|v| format_float(*v)).collect();
    json!(formatted)
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

fn format_kline_timestamp(timestamp_ms: i64, timezone: ConfiguredTimeZone) -> String {
    format_timestamp_label(Some(timestamp_ms), timezone).unwrap_or_else(|| timestamp_ms.to_string())
}

fn format_timestamp_label(timestamp: Option<i64>, timezone: ConfiguredTimeZone) -> Option<String> {
    timestamp.and_then(|ts| timezone.format_timestamp(ts, "%Y-%m-%d %H:%M:%S"))
}

fn optional_float(value: Option<f64>) -> Value {
    match value {
        Some(v) => json!(format_float(v)),
        None => json!(null),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{AccountBalance, AccountBalanceDelta, PendingOrderInfo, PositionInfo};

    fn create_test_snapshot() -> AccountSnapshot {
        AccountSnapshot {
            positions: vec![PositionInfo {
                inst_id: "BTC-USDT-SWAP".to_string(),
                pos_side: Some("long".to_string()),
                size: 1.0,
                avg_px: Some(50000.0),
                lever: Some(10.0),
                upl: Some(500.0),
                upl_ratio: Some(0.01),
                imr: 5000.0,
                create_time: Some(1700000000000),
            }],
            open_orders: vec![PendingOrderInfo {
                inst_id: "BTC-USDT-SWAP".to_string(),
                ord_id: "12345".to_string(),
                side: "buy".to_string(),
                pos_side: Some("long".to_string()),
                price: Some(49000.0),
                size: 1.0,
                state: "live".to_string(),
                reduce_only: false,
                tag: Some(AI_TAG_ENTRY.to_string()),
                lever: Some(10.0),
                trigger_price: None,
                kind: crate::command::TradeOrderKind::Regular,
                create_time: Some(1700000000000),
            }],
            balance: AccountBalance {
                total_equity: Some(10000.0),
                delta: vec![AccountBalanceDelta {
                    currency: "USDT".to_string(),
                    cash_balance: Some(10000.0),
                    equity: Some(10500.0),
                    available: Some(9500.0),
                }],
            },
        }
    }

    fn create_test_analytics() -> Vec<InstrumentAnalytics> {
        vec![InstrumentAnalytics {
            inst_id: "BTC-USDT-SWAP".to_string(),
            symbol: "BTC/USDT".to_string(),
            current_price: Some(50500.0),
            current_ema20: Some(50000.0),
            current_macd: Some(10.5),
            current_rsi7: Some(65.0),
            oi_latest: Some(1000000.0),
            oi_average: Some(950000.0),
            funding_rate: Some(0.0001),
            recent_candles_5m: vec![],
            intraday_1m_ema20: vec![49900.0, 50000.0, 50100.0],
            intraday_1m_macd: vec![8.0, 9.0, 10.5],
            intraday_1m_rsi7: vec![60.0, 62.0, 65.0],
            intraday_1m_rsi14: vec![58.0, 60.0, 63.0],
            intraday_3m_ema20: vec![49900.0, 50000.0, 50100.0],
            intraday_3m_macd: vec![8.0, 9.0, 10.5],
            intraday_3m_rsi7: vec![60.0, 62.0, 65.0],
            intraday_3m_rsi14: vec![58.0, 60.0, 63.0],
            intraday_prices: vec![50000.0, 50200.0, 50500.0],
            intraday_ema20: vec![49900.0, 50000.0, 50100.0],
            intraday_macd: vec![8.0, 9.0, 10.5],
            intraday_rsi7: vec![60.0, 62.0, 65.0],
            intraday_rsi14: vec![58.0, 60.0, 63.0],
            intraday_15m_ema20: vec![49900.0, 50000.0, 50100.0],
            intraday_15m_macd: vec![8.0, 9.0, 10.5],
            intraday_15m_rsi7: vec![60.0, 62.0, 65.0],
            intraday_15m_rsi14: vec![58.0, 60.0, 63.0],
            recent_candles_4h: vec![],
            swing_ema20: Some(49500.0),
            swing_ema50: Some(49000.0),
            swing_atr3: Some(500.0),
            swing_atr14: Some(800.0),
            swing_volume_current: Some(1000.0),
            swing_volume_avg: Some(900.0),
            swing_macd: vec![5.0, 7.0, 9.0],
            swing_rsi14: vec![55.0, 58.0, 62.0],
        }]
    }

    #[test]
    fn test_build_snapshot_prompt_basic() {
        let snapshot = create_test_snapshot();
        let analytics = create_test_analytics();
        let inst_ids = vec!["BTC-USDT-SWAP".to_string()];
        let mut markets = HashMap::new();
        markets.insert(
            "BTC-USDT-SWAP".to_string(),
            MarketInfo {
                min_size: 1.0,
                ct_val: 0.01,
                ct_val_ccy: Some("BTC".to_string()),
                lever: 100.0,
            },
        );
        let leverages = vec![InstrumentLeverage {
            inst_id: "BTC-USDT-SWAP".to_string(),
            net: Some(10.0),
            long: Some(10.0),
            short: Some(10.0),
        }];

        let result = build_snapshot_prompt(
            &snapshot,
            &analytics,
            None,
            &inst_ids,
            &markets,
            &leverages,
            ConfiguredTimeZone::Local,
        );

        assert!(!result.is_empty());
        assert!(result.contains("BTC-USDT-SWAP"));
        assert!(result.contains("leverage"));
    }

    #[test]
    fn test_build_snapshot_prompt_with_performance() {
        let snapshot = create_test_snapshot();
        let analytics = create_test_analytics();
        let performance = PerformanceSummary {
            overall: Some(PerformanceStats {
                label: "Overall".to_string(),
                start_timestamp_ms: 1700000000000,
                trade_count: 10,
                sharpe_ratio: Some(1.5),
                total_pnl: 1000.0,
            }),
            recent: Some(PerformanceStats {
                label: "Recent".to_string(),
                start_timestamp_ms: 1700000000000,
                trade_count: 5,
                sharpe_ratio: Some(2.0),
                total_pnl: 500.0,
            }),
        };

        let result = build_snapshot_prompt(
            &snapshot,
            &analytics,
            Some(&performance),
            &[],
            &HashMap::new(),
            &[],
            ConfiguredTimeZone::Local,
        );

        assert!(!result.is_empty());
        assert!(result.contains("performance"));
        assert!(result.contains("sharpe_ratio"));
        assert!(result.contains("1.5"));
    }

    #[test]
    fn test_build_snapshot_prompt_empty_data() {
        let snapshot = AccountSnapshot {
            positions: vec![],
            open_orders: vec![],
            balance: AccountBalance::default(),
        };

        let result = build_snapshot_prompt(
            &snapshot,
            &[],
            None,
            &[],
            &HashMap::new(),
            &[],
            ConfiguredTimeZone::Local,
        );

        assert!(!result.is_empty());
        assert!(result.contains("账户情况"));
    }

    #[test]
    fn test_format_float() {
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(1.123456789), "1.12345679");
        assert_eq!(format_float(0.00000001), "0.00000001");
        assert_eq!(format_float(0.1), "0.1");
    }

    #[test]
    fn test_format_contract_count() {
        assert_eq!(format_contract_count(1.0), "1");
        assert_eq!(format_contract_count(1.5), "1.5");
        assert_eq!(format_contract_count(10.0), "10");
        assert_eq!(format_contract_count(0.0), "-");
        assert_eq!(format_contract_count(-1.0), "-");
    }

    #[test]
    fn test_is_usdt_quote() {
        assert!(is_usdt_quote("BTC-USDT-SWAP"));
        assert!(is_usdt_quote("BTC-USDT"));
        assert!(is_usdt_quote("btc-usdt-swap"));
        assert!(!is_usdt_quote("BTC-USD-SWAP"));
        assert!(!is_usdt_quote("BTCUSDT"));
    }
}
