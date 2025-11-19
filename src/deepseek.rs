use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{Duration as ChronoDuration, Local, TimeZone};
use rand::Rng;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time;

use crate::command::{AccountSnapshot, AiInsightRecord, Command, TradeEvent, TradingCommand};
use crate::config::{ConfiguredTimeZone, DeepseekConfig};
use crate::ai_decision::{DecisionExecutor, LeverageKey, initial_leverage_cache};
use crate::ai_prompt::{
    InstrumentLeverage, PerformanceStats, PerformanceSummary, build_snapshot_prompt,
    load_system_prompt,
};
use crate::error_log::ErrorLogStore;
use crate::okx::{MarketInfo, SharedAccountState};
use crate::okx_analytics::{InstrumentAnalytics, MarketDataFetcher};
use crate::trade_log::{TradeLogEntry, TradeLogStore};

const MAX_ANALYTICS_INSTRUMENTS: usize = 3;

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
    error_log: ErrorLogStore,
    system_prompt: String,
    timezone: ConfiguredTimeZone,
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
        timezone: ConfiguredTimeZone,
    ) -> Result<Self> {
        let system_prompt = load_system_prompt()?;
        let client = DeepseekClient::new(&config, system_prompt.clone())?;
        let market = MarketDataFetcher::new()?;
        let inst_ids = normalize_inst_ids(inst_ids);
        let performance = PerformanceTracker::new(start_timestamp_ms);
        let leverage_cache = RwLock::new(initial_leverage_cache(&markets));
        let error_log = ErrorLogStore::new(ErrorLogStore::default_path());
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
            error_log,
            system_prompt,
            timezone,
        })
    }

    pub async fn run(self, mut exit_rx: broadcast::Receiver<()>) -> Result<()> {
        loop {
            let delay = self.random_dispatch_delay();
            tokio::select! {
                _ = time::sleep(delay) => {
                    if let Err(err) = self.report_once().await {
                        let _ = self.tx.send(Command::Error(format!("Deepseek 分析失败: {err}")));
                    }
                }
                message = exit_rx.recv() => match message {
                    Ok(_) | Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
        Ok(())
    }

    fn random_dispatch_delay(&self) -> Duration {
        let min_secs = 60;
        let max_secs = self.interval.as_secs().max(min_secs);
        let mut rng = rand::rng();
        let seconds = rng.random_range(min_secs..=max_secs);
        Duration::from_secs(seconds)
    }

    async fn report_once(&self) -> Result<()> {
        let snapshot = self.state.snapshot().await;
        if !has_material_data(&snapshot) {
            return Ok(());
        }
        let decision_engine = self.decision_executor();
        decision_engine
            .capture_leverage_from_snapshot(&snapshot)
            .await;
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
            self.timezone,
        );
        let insight = self.client.chat_completion(&prompt).await?;
        let trimmed = insight.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if let Err(err) = decision_engine.execute(trimmed, &analytics).await {
            let _ = self
                .tx
                .send(Command::Error(format!("执行 AI 决策失败: {err}")));
        }
        let record = AiInsightRecord {
            timestamp_ms: Local::now().timestamp_millis(),
            system_prompt: self.system_prompt.clone(),
            user_prompt: prompt,
            response: trimmed.to_string(),
        };
        let _ = self.tx.send(Command::AiInsight(record));
        Ok(())
    }

    fn decision_executor(&self) -> DecisionExecutor<'_> {
        DecisionExecutor::new(
            self.state.clone(),
            self.tx.clone(),
            &self.inst_ids,
            &self.market,
            self.order_tx.clone(),
            &self.leverage_cache,
            self.error_log.clone(),
        )
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
                    .entry(key.inst_id().to_string())
                    .or_insert_with(|| InstrumentLeverage::new(key.inst_id().to_string()));
                match key.pos_side() {
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
}

fn has_material_data(snapshot: &AccountSnapshot) -> bool {
    !snapshot.positions.is_empty()
        || !snapshot.open_orders.is_empty()
        || snapshot.balance.total_equity.is_some()
        || !snapshot.balance.delta.is_empty()
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
                if let Some(pnl) = fill.pnl
                    && fill.exec_type.is_some()
                {
                    fill_count += 1;
                    if pnl.is_finite() {
                        pnls.push(pnl + fill.fee.unwrap_or(0.0));
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
    system_prompt: String,
}

impl DeepseekClient {
    fn new(config: &DeepseekConfig, system_prompt: String) -> Result<Self> {
        Ok(DeepseekClient {
            http: ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(120))
                .timeout(Duration::from_secs(140))
                .build()?,
            base_url: config.endpoint.clone(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            system_prompt,
        })
    }

    async fn chat_completion(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let request = ChatCompletionRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: self.system_prompt.clone(),
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
        let response_text = response.text().await.unwrap_or_default();
        let completion: ChatCompletionResponse =
            serde_json::from_str(&response_text).map_err(|err| {
                anyhow!(
                    "解析 Deepseek 响应失败: {}\n响应原文:\n{}",
                    err,
                    response_text
                )
            })?;
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
