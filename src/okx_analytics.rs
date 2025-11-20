use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::Deserialize;

use crate::okx;

const MARKET_CANDLES_ENDPOINT: &str = "https://www.okx.com/api/v5/market/candles";
const FUNDING_RATE_ENDPOINT: &str = "https://www.okx.com/api/v5/public/funding-rate";
const OPEN_INTEREST_ENDPOINT: &str = "https://www.okx.com/api/v5/public/open-interest";
const OPEN_INTEREST_HISTORY_ENDPOINT: &str =
    "https://www.okx.com/api/v5/rubik/stat/contracts/open-interest-history";
const ANALYTICS_INTRADAY_LIMIT: usize = 160;
const ANALYTICS_SWING_LIMIT: usize = 120;
const ANALYTICS_SERIES_TAIL: usize = 10;
const RECENT_KLINE_TAIL: usize = 10;
const EMA_SHORT_PERIOD: usize = 20;
const EMA_LONG_PERIOD: usize = 50;
const RSI_SHORT_PERIOD: usize = 7;
const RSI_LONG_PERIOD: usize = 14;
const MACD_FAST_PERIOD: usize = 12;
const MACD_SLOW_PERIOD: usize = 26;
const ATR_FAST_PERIOD: usize = 3;
const ATR_SLOW_PERIOD: usize = 14;
const VOLUME_AVG_PERIOD: usize = 20;

#[derive(Debug, Clone, Default)]
pub struct InstrumentAnalytics {
    pub inst_id: String,
    pub symbol: String,
    pub current_price: Option<f64>,
    pub current_ema20: Option<f64>,
    pub current_macd: Option<f64>,
    pub current_rsi7: Option<f64>,
    pub oi_latest: Option<f64>,
    pub oi_average: Option<f64>,
    pub funding_rate: Option<f64>,

    pub intraday_1m_ema20: Vec<f64>,
    pub intraday_1m_macd: Vec<f64>,
    pub intraday_1m_rsi7: Vec<f64>,
    pub intraday_1m_rsi14: Vec<f64>,

    pub intraday_3m_ema20: Vec<f64>,
    pub intraday_3m_macd: Vec<f64>,
    pub intraday_3m_rsi7: Vec<f64>,
    pub intraday_3m_rsi14: Vec<f64>,

    pub intraday_5m_prices: Vec<f64>,
    pub intraday_5m_ema20: Vec<f64>,
    pub intraday_5m_macd: Vec<f64>,
    pub intraday_5m_rsi7: Vec<f64>,
    pub intraday_5m_rsi14: Vec<f64>,

    pub intraday_15m_ema20: Vec<f64>,
    pub intraday_15m_macd: Vec<f64>,
    pub intraday_15m_rsi7: Vec<f64>,
    pub intraday_15m_rsi14: Vec<f64>,

    pub swing_ema20: Option<f64>,
    pub swing_ema50: Option<f64>,
    pub swing_atr3: Option<f64>,
    pub swing_atr14: Option<f64>,
    pub swing_volume_current: Option<f64>,
    pub swing_volume_avg: Option<f64>,
    pub swing_macd: Vec<f64>,
    pub swing_rsi14: Vec<f64>,
    pub recent_candles_5m: Vec<KlineRecord>,
    pub recent_candles_4h: Vec<KlineRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct KlineRecord {
    pub timestamp_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, Default)]
struct OpenInterestStats {
    latest: Option<f64>,
    average: Option<f64>,
}

pub struct MarketDataFetcher {
    http: Client,
}

impl MarketDataFetcher {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(MarketDataFetcher { http })
    }

    pub async fn price_for_inst(&self, inst_id: &str) -> Result<f64> {
        okx::fetch_mark_price(&self.http, inst_id).await
    }

    pub async fn fetch_inst(&self, inst_id: &str) -> Result<InstrumentAnalytics> {
        let intraday_1m = self
            .fetch_candles(inst_id, "1m", ANALYTICS_INTRADAY_LIMIT)
            .await?;
        let intraday_3m = self
            .fetch_candles(inst_id, "3m", ANALYTICS_INTRADAY_LIMIT)
            .await?;
        let intraday_5m = self
            .fetch_candles(inst_id, "5m", ANALYTICS_INTRADAY_LIMIT)
            .await?;
        if intraday_5m.is_empty() {
            return Err(anyhow!("{} 缺少 5 分钟 K 线数据", inst_id));
        }
        let intraday_15m = self
            .fetch_candles(inst_id, "15m", ANALYTICS_INTRADAY_LIMIT)
            .await?;
        let swing = self
            .fetch_candles(inst_id, "4H", ANALYTICS_SWING_LIMIT)
            .await?;

        let closes_1m: Vec<f64> = intraday_1m.iter().map(|c| c.close).collect();
        let ema20_1m = compute_ema(&closes_1m, EMA_SHORT_PERIOD);
        let macd_1m = compute_macd(&closes_1m);
        let rsi7_1m = compute_rsi(&closes_1m, RSI_SHORT_PERIOD);
        let rsi14_1m = compute_rsi(&closes_1m, RSI_LONG_PERIOD);

        let closes_3m: Vec<f64> = intraday_3m.iter().map(|c| c.close).collect();
        let ema20_3m = compute_ema(&closes_3m, EMA_SHORT_PERIOD);
        let macd_3m = compute_macd(&closes_3m);
        let rsi7_3m = compute_rsi(&closes_3m, RSI_SHORT_PERIOD);
        let rsi14_3m = compute_rsi(&closes_3m, RSI_LONG_PERIOD);

        let closes_intraday: Vec<f64> = intraday_5m.iter().map(|c| c.close).collect();
        let closes_swing: Vec<f64> = swing.iter().map(|c| c.close).collect();
        let swing_volumes: Vec<f64> = swing.iter().map(|c| c.volume).collect();
        let ema20_intraday = compute_ema(&closes_intraday, EMA_SHORT_PERIOD);
        let macd_intraday = compute_macd(&closes_intraday);
        let rsi7_intraday = compute_rsi(&closes_intraday, RSI_SHORT_PERIOD);
        let rsi14_intraday = compute_rsi(&closes_intraday, RSI_LONG_PERIOD);

        let closes_15m: Vec<f64> = intraday_15m.iter().map(|c| c.close).collect();
        let ema20_15m = compute_ema(&closes_15m, EMA_SHORT_PERIOD);
        let macd_15m = compute_macd(&closes_15m);
        let rsi7_15m = compute_rsi(&closes_15m, RSI_SHORT_PERIOD);
        let rsi14_15m = compute_rsi(&closes_15m, RSI_LONG_PERIOD);

        let ema20_swing = compute_ema(&closes_swing, EMA_SHORT_PERIOD);
        let ema50_swing = compute_ema(&closes_swing, EMA_LONG_PERIOD);
        let macd_swing = compute_macd(&closes_swing);
        let rsi14_swing = compute_rsi(&closes_swing, RSI_LONG_PERIOD);
        let atr3_swing = compute_atr(&swing, ATR_FAST_PERIOD);
        let atr14_swing = compute_atr(&swing, ATR_SLOW_PERIOD);
        let oi_stats = self.fetch_open_interest(inst_id).await?;
        let funding_rate = self.fetch_funding_rate(inst_id).await?;
        let current_price = self.price_for_inst(inst_id).await?;
        let swing_volume_current = swing_volumes.last().copied();
        let swing_volume_avg = average_tail(&swing_volumes, VOLUME_AVG_PERIOD);
        Ok(InstrumentAnalytics {
            inst_id: inst_id.to_string(),
            symbol: inst_symbol(inst_id),
            current_price: Some(current_price),
            current_ema20: ema20_intraday.last().copied(),
            current_macd: macd_intraday.last().copied(),
            current_rsi7: rsi7_intraday.last().copied(),
            oi_latest: oi_stats.latest,
            oi_average: oi_stats.average,
            funding_rate,
            intraday_1m_ema20: take_tail(&ema20_1m, ANALYTICS_SERIES_TAIL),
            intraday_1m_macd: take_tail(&macd_1m, ANALYTICS_SERIES_TAIL),
            intraday_1m_rsi7: take_tail(&rsi7_1m, ANALYTICS_SERIES_TAIL),
            intraday_1m_rsi14: take_tail(&rsi14_1m, ANALYTICS_SERIES_TAIL),
            intraday_3m_ema20: take_tail(&ema20_3m, ANALYTICS_SERIES_TAIL),
            intraday_3m_macd: take_tail(&macd_3m, ANALYTICS_SERIES_TAIL),
            intraday_3m_rsi7: take_tail(&rsi7_3m, ANALYTICS_SERIES_TAIL),
            intraday_3m_rsi14: take_tail(&rsi14_3m, ANALYTICS_SERIES_TAIL),
            intraday_5m_prices: take_tail(&closes_intraday, ANALYTICS_SERIES_TAIL),
            intraday_5m_ema20: take_tail(&ema20_intraday, ANALYTICS_SERIES_TAIL),
            intraday_5m_macd: take_tail(&macd_intraday, ANALYTICS_SERIES_TAIL),
            intraday_5m_rsi7: take_tail(&rsi7_intraday, ANALYTICS_SERIES_TAIL),
            intraday_5m_rsi14: take_tail(&rsi14_intraday, ANALYTICS_SERIES_TAIL),
            intraday_15m_ema20: take_tail(&ema20_15m, ANALYTICS_SERIES_TAIL),
            intraday_15m_macd: take_tail(&macd_15m, ANALYTICS_SERIES_TAIL),
            intraday_15m_rsi7: take_tail(&rsi7_15m, ANALYTICS_SERIES_TAIL),
            intraday_15m_rsi14: take_tail(&rsi14_15m, ANALYTICS_SERIES_TAIL),
            swing_ema20: ema20_swing.last().copied(),
            swing_ema50: ema50_swing.last().copied(),
            swing_atr3: atr3_swing.last().copied(),
            swing_atr14: atr14_swing.last().copied(),
            swing_volume_current,
            swing_volume_avg,
            swing_macd: take_tail(&macd_swing, ANALYTICS_SERIES_TAIL),
            swing_rsi14: take_tail(&rsi14_swing, ANALYTICS_SERIES_TAIL),
            recent_candles_5m: take_tail_candles(&intraday_5m, RECENT_KLINE_TAIL),
            recent_candles_4h: take_tail_candles(&swing, RECENT_KLINE_TAIL),
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

    pub async fn fetch_funding_rate(&self, inst_id: &str) -> Result<Option<f64>> {
        let response: FundingRateResponse = self
            .http
            .get(FUNDING_RATE_ENDPOINT)
            .query(&[("instId", inst_id)])
            .send()
            .await
            .with_context(|| format!("请求 {} 融资利率失败", inst_id))?
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
            .query(&[("instType", "SWAP"), ("instId", inst_id), ("period", "6H")])
            .send()
            .await
            .with_context(|| format!("请求 {} 未平仓合约历史失败", inst_id))?
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
            if entry.len() < 2 {
                continue;
            }
            if let Some(oi) = parse_f64(&entry[1]) {
                values.push(oi);
            }
        }
        Ok(values)
    }
}

#[derive(Debug, Clone)]
pub struct Candle {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl Candle {
    fn from_entry(entry: &[String]) -> Option<Self> {
        if entry.len() < 6 {
            return None;
        }
        Some(Candle {
            ts: entry.get(0)?.parse().ok()?,
            open: parse_f64(entry.get(1)?)?,
            high: parse_f64(entry.get(2)?)?,
            low: parse_f64(entry.get(3)?)?,
            close: parse_f64(entry.get(4)?)?,
            volume: parse_f64(entry.get(5)?)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct MarketCandleResponse {
    code: String,
    msg: String,
    data: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingRateResponse {
    code: String,
    msg: String,
    data: Vec<FundingRateEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingRateEntry {
    funding_rate: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestResponse {
    code: String,
    msg: String,
    data: Vec<OpenInterestEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestEntry {
    oi: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestHistoryResponse {
    code: String,
    msg: String,
    data: Vec<Vec<String>>,
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

fn take_tail_candles(candles: &[Candle], count: usize) -> Vec<KlineRecord> {
    if count == 0 || candles.is_empty() {
        return Vec::new();
    }
    let start = candles.len().saturating_sub(count);
    candles[start..]
        .iter()
        .map(|candle| KlineRecord {
            timestamp_ms: candle.ts,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
            volume: candle.volume,
        })
        .collect()
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

pub fn compute_ema(series: &[f64], period: usize) -> Vec<f64> {
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

pub fn compute_macd(series: &[f64]) -> Vec<f64> {
    if series.is_empty() {
        return Vec::new();
    }
    let fast = compute_ema(series, MACD_FAST_PERIOD);
    let slow = compute_ema(series, MACD_SLOW_PERIOD);
    fast.into_iter()
        .zip(slow.into_iter())
        .map(|(f, s)| f - s)
        .collect()
}

pub fn compute_rsi(series: &[f64], period: usize) -> Vec<f64> {
    if series.len() <= period || period == 0 {
        return Vec::new();
    }
    let mut rs_values = Vec::with_capacity(series.len());
    let mut gains = Vec::with_capacity(series.len());
    let mut losses = Vec::with_capacity(series.len());
    gains.push(0.0);
    losses.push(0.0);
    for idx in 1..series.len() {
        let change = series[idx] - series[idx - 1];
        if change >= 0.0 {
            gains.push(change);
            losses.push(0.0);
        } else {
            gains.push(0.0);
            losses.push(-change);
        }
    }
    let mut avg_gain = gains[1..=period].iter().sum::<f64>() / period as f64;
    let mut avg_loss = losses[1..=period].iter().sum::<f64>() / period as f64;
    for idx in (period + 1)..gains.len() {
        avg_gain = ((period - 1) as f64 * avg_gain + gains[idx]) / period as f64;
        avg_loss = ((period - 1) as f64 * avg_loss + losses[idx]) / period as f64;
        let rs = if avg_loss.abs() <= f64::EPSILON {
            100.0
        } else {
            100.0 - (100.0 / (1.0 + avg_gain / avg_loss))
        };
        rs_values.push(rs);
    }
    while rs_values.len() < series.len() {
        rs_values.insert(0, 50.0);
    }
    rs_values
}

pub fn compute_atr(candles: &[Candle], period: usize) -> Vec<f64> {
    if candles.len() <= period || period == 0 {
        return Vec::new();
    }
    let mut tr_values = Vec::with_capacity(candles.len());
    tr_values.push(0.0);
    for idx in 1..candles.len() {
        let current = &candles[idx];
        let previous = &candles[idx - 1];
        let high_low = current.high - current.low;
        let high_close = (current.high - previous.close).abs();
        let low_close = (current.low - previous.close).abs();
        let tr = high_low.max(high_close).max(low_close);
        tr_values.push(tr);
    }
    let mut atr_values = vec![0.0; tr_values.len()];
    let mut atr = tr_values[1..=period].iter().sum::<f64>() / period as f64;
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
