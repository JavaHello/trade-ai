use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local, LocalResult, TimeZone};
use serde::{Deserialize, Serialize};

use crate::command::AiInsightRecord;

#[derive(Clone, Debug, PartialEq)]
pub struct AiDecisionRecord {
    pub timestamp: DateTime<Local>,
    pub system_prompt: String,
    pub user_prompt: String,
    pub response: String,
    pub operations: Vec<AiDecisionOperation>,
    pub analysis_error: Option<String>,
}

impl AiDecisionRecord {
    pub fn from_payload(payload: AiInsightRecord) -> Self {
        let timestamp = match Local.timestamp_millis_opt(payload.timestamp_ms) {
            LocalResult::Single(dt) => dt,
            _ => Local::now(),
        };
        let (operations, analysis_error) = Self::analyze_response(&payload.response);
        AiDecisionRecord {
            timestamp,
            system_prompt: payload.system_prompt,
            user_prompt: payload.user_prompt,
            response: payload.response,
            operations,
            analysis_error,
        }
    }

    pub fn summary(&self) -> String {
        if let Some(error) = self.analysis_error.as_deref() {
            let trimmed = error.trim();
            if !trimmed.is_empty() {
                return format!("解析失败: {trimmed}");
            }
        }
        if !self.operations.is_empty() {
            let labels = self
                .operations
                .iter()
                .map(|op| op.brief_label())
                .collect::<Vec<_>>();
            let joined = labels.join(" | ");
            if !joined.is_empty() {
                return joined;
            }
        }
        "无操作".to_string()
    }

    fn from_parts(
        timestamp_ms: i64,
        system_prompt: String,
        user_prompt: String,
        response: String,
        operations: Vec<AiDecisionOperation>,
        analysis_error: Option<String>,
    ) -> Self {
        let timestamp = match Local.timestamp_millis_opt(timestamp_ms) {
            LocalResult::Single(dt) => dt,
            _ => Local::now(),
        };
        let (derived_operations, derived_error) = Self::analyze_response(&response);
        let operations = if operations.is_empty() {
            derived_operations
        } else {
            operations
        };
        let analysis_error = analysis_error.or(derived_error);
        AiDecisionRecord {
            timestamp,
            system_prompt,
            user_prompt,
            response,
            operations,
            analysis_error,
        }
    }

    pub fn timestamp_ms(&self) -> i64 {
        self.timestamp.timestamp_millis()
    }

    fn analyze_response(response: &str) -> (Vec<AiDecisionOperation>, Option<String>) {
        let (parsed, analysis_error) = match Self::parse_json_block(response) {
            Ok(parsed) => (Some(parsed), None),
            Err(err) => (None, Some(err)),
        };
        let operations = Self::derive_operations(parsed.as_ref());
        (operations, analysis_error)
    }

    fn parse_json_block(response: &str) -> Result<serde_json::Value, String> {
        match serde_json::from_str::<serde_json::Value>(response) {
            Ok(value) => Ok(value),
            Err(primary_err) => {
                if let Some(slice) = Self::extract_json_slice(response, '[', ']') {
                    return serde_json::from_str::<serde_json::Value>(slice)
                        .map_err(|err| format!("解析 JSON 数组片段失败: {err}"));
                }
                if let Some(slice) = Self::extract_json_slice(response, '{', '}') {
                    return serde_json::from_str::<serde_json::Value>(slice)
                        .map_err(|err| format!("解析 JSON 对象片段失败: {err}"));
                }
                Err(format!("无法解析 AI JSON: {primary_err}"))
            }
        }
    }

    fn extract_json_slice<'a>(response: &'a str, start: char, end: char) -> Option<&'a str> {
        let start_idx = response.find(start)?;
        let end_idx = response.rfind(end)?;
        if end_idx <= start_idx {
            return None;
        }
        response.get(start_idx..=end_idx)
    }

    fn derive_operations(parsed: Option<&serde_json::Value>) -> Vec<AiDecisionOperation> {
        let mut operations = Vec::new();
        match parsed {
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    if let Some(op) = AiDecisionOperation::from_value(item) {
                        operations.push(op);
                    }
                }
            }
            Some(value) => {
                if let Some(op) = AiDecisionOperation::from_value(value) {
                    operations.push(op);
                }
            }
            None => {}
        }
        operations
    }
}

#[derive(Clone, Debug)]
pub struct AiDecisionStore {
    path: PathBuf,
    max_entries: usize,
}

impl AiDecisionStore {
    const TAIL_CHUNK_SIZE: usize = 8 * 1024;

    pub fn new(path: PathBuf) -> Self {
        AiDecisionStore {
            path,
            max_entries: 512,
        }
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from("ai_decisions.jsonl")
    }

    pub fn load(&self) -> Result<Vec<AiDecisionRecord>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let lines = Self::read_tail_lines(&mut file, self.max_entries)?;
        let mut entries = Vec::with_capacity(lines.len());
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(stored) = serde_json::from_str::<StoredAiDecision>(&line) {
                entries.push(stored.into_record());
            }
        }
        Ok(entries)
    }

    pub fn append(&self, entry: &AiDecisionRecord) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &StoredAiDecision::from(entry))?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn read_tail_lines(file: &mut File, max_lines: usize) -> Result<Vec<String>> {
        if max_lines == 0 {
            return Ok(Vec::new());
        }

        let mut pos = file.seek(SeekFrom::End(0))?;
        let mut chunk = vec![0_u8; Self::TAIL_CHUNK_SIZE];
        let mut pending = Vec::new();
        let mut lines = Vec::new();

        'outer: while pos > 0 && lines.len() < max_lines {
            let read_size = std::cmp::min(pos, chunk.len() as u64) as usize;
            pos -= read_size as u64;
            file.seek(SeekFrom::Start(pos))?;
            file.read_exact(&mut chunk[..read_size])?;

            for &byte in chunk[..read_size].iter().rev() {
                if byte == b'\n' {
                    if Self::push_pending_line(&mut pending, &mut lines) && lines.len() == max_lines
                    {
                        break 'outer;
                    }
                } else {
                    pending.push(byte);
                }
            }
        }

        if lines.len() < max_lines {
            Self::push_pending_line(&mut pending, &mut lines);
        }

        lines.reverse();
        Ok(lines)
    }

    fn push_pending_line(pending: &mut Vec<u8>, lines: &mut Vec<String>) -> bool {
        if pending.is_empty() {
            return false;
        }
        pending.reverse();
        let line = String::from_utf8_lossy(pending).to_string();
        pending.clear();
        lines.push(line);
        true
    }
}

#[derive(Serialize, Deserialize)]
struct StoredAiDecision {
    timestamp_ms: i64,
    system_prompt: String,
    user_prompt: String,
    response: String,
    #[serde(default)]
    operations: Vec<AiDecisionOperation>,
    #[serde(default, rename = "operation", skip_serializing)]
    legacy_operation: Option<AiDecisionOperation>,
    #[serde(default)]
    analysis_error: Option<String>,
}

impl StoredAiDecision {
    fn into_record(self) -> AiDecisionRecord {
        let operations = if !self.operations.is_empty() {
            self.operations
        } else if let Some(operation) = self.legacy_operation {
            vec![operation]
        } else {
            Vec::new()
        };
        AiDecisionRecord::from_parts(
            self.timestamp_ms,
            self.system_prompt,
            self.user_prompt,
            self.response,
            operations,
            self.analysis_error,
        )
    }
}

impl From<&AiDecisionRecord> for StoredAiDecision {
    fn from(value: &AiDecisionRecord) -> Self {
        StoredAiDecision {
            timestamp_ms: value.timestamp_ms(),
            system_prompt: value.system_prompt.clone(),
            user_prompt: value.user_prompt.clone(),
            response: value.response.clone(),
            operations: value.operations.clone(),
            legacy_operation: None,
            analysis_error: value.analysis_error.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiDecisionSignal {
    BuyToEnter,
    SellToEnter,
    Hold,
    Close,
    Cancel,
    Wait,
}

impl AiDecisionSignal {
    fn from_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "buy_to_enter" => Some(AiDecisionSignal::BuyToEnter),
            "sell_to_enter" => Some(AiDecisionSignal::SellToEnter),
            "hold" => Some(AiDecisionSignal::Hold),
            "close" => Some(AiDecisionSignal::Close),
            "cancel_orders" => Some(AiDecisionSignal::Cancel),
            "wait" => Some(AiDecisionSignal::Wait),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AiDecisionSignal::BuyToEnter => "买入入场",
            AiDecisionSignal::SellToEnter => "卖出入场",
            AiDecisionSignal::Hold => "保持",
            AiDecisionSignal::Close => "平仓",
            AiDecisionSignal::Cancel => "取消订单",
            AiDecisionSignal::Wait => "等待",
        }
    }

    pub fn short_label(&self) -> &'static str {
        match self {
            AiDecisionSignal::BuyToEnter => "买入",
            AiDecisionSignal::SellToEnter => "卖出",
            AiDecisionSignal::Hold => "持有",
            AiDecisionSignal::Close => "平仓",
            AiDecisionSignal::Cancel => "取消",
            AiDecisionSignal::Wait => "等待",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AiDecisionOperation {
    pub signal: AiDecisionSignal,
    pub coin: String,
    pub quantity: Option<f64>,
    pub leverage: Option<f64>,
    pub profit_target: Option<f64>,
    pub stop_loss: Option<f64>,
    pub invalidation: Option<String>,
    pub confidence: Option<f64>,
    pub risk_usd: Option<f64>,
    pub justification: Option<String>,
}

impl AiDecisionOperation {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        let signal = value.get("signal")?.as_str()?;
        let signal = AiDecisionSignal::from_str(signal)?;
        let coin = value
            .get("coin")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let quantity = value.get("quantity").and_then(Self::parse_number);
        let leverage = value.get("leverage").and_then(Self::parse_number);
        let profit_target = value.get("profit_target").and_then(Self::parse_number);
        let stop_loss = value.get("stop_loss").and_then(Self::parse_number);
        let invalidation = value
            .get("invalidation_condition")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let confidence = value.get("confidence").and_then(Self::parse_number);
        let risk_usd = value.get("risk_usd").and_then(Self::parse_number);
        let justification = value
            .get("justification")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Some(AiDecisionOperation {
            signal,
            coin,
            quantity,
            leverage,
            profit_target,
            stop_loss,
            invalidation,
            confidence,
            risk_usd,
            justification,
        })
    }

    fn parse_number(value: &serde_json::Value) -> Option<f64> {
        let number = value.as_f64()?;
        if number.is_finite() {
            Some(number)
        } else {
            None
        }
    }

    fn format_number(value: f64) -> String {
        let mut formatted = format!("{value:.4}");
        if formatted.contains('.') {
            while formatted.ends_with('0') {
                formatted.pop();
            }
            if formatted.ends_with('.') {
                formatted.pop();
            }
        }
        if formatted.is_empty() {
            "0".to_string()
        } else {
            formatted
        }
    }

    pub fn brief_label(&self) -> String {
        let mut parts = vec![format!("{} {}", self.signal.short_label(), self.coin)];
        if let Some(quantity) = self.quantity {
            if quantity.abs() > f64::EPSILON {
                parts.push(format!("数量 {}", Self::format_number(quantity)));
            }
        }
        if let Some(leverage) = self.leverage {
            if leverage.abs() > f64::EPSILON {
                parts.push(format!("杠杆 {}x", Self::format_number(leverage)));
            }
        }
        parts.join(" · ")
    }

    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!("信号 {} · 合约 {}", self.signal.label(), self.coin));
        let mut stats = Vec::new();
        if let Some(quantity) = self.quantity {
            if quantity.abs() > f64::EPSILON {
                stats.push(format!("数量 {}", Self::format_number(quantity)));
            }
        }
        if let Some(leverage) = self.leverage {
            if leverage.abs() > f64::EPSILON {
                stats.push(format!("杠杆 {}x", Self::format_number(leverage)));
            }
        }
        if let Some(risk) = self.risk_usd {
            if risk.abs() > f64::EPSILON {
                stats.push(format!("风险 ${}", Self::format_number(risk)));
            }
        }
        if !stats.is_empty() {
            lines.push(stats.join(" · "));
        }
        if let Some(target) = self.profit_target {
            if target.is_sign_positive() {
                lines.push(format!("止盈 {}", Self::format_number(target)));
            }
        }
        if let Some(stop) = self.stop_loss {
            if stop.is_sign_positive() {
                lines.push(format!("止损 {}", Self::format_number(stop)));
            }
        }
        if let Some(invalidation) = self.invalidation.as_deref() {
            if !invalidation.is_empty() {
                lines.push(format!("失效条件 {}", invalidation));
            }
        }
        if let Some(confidence) = self.confidence {
            if (0.0..=1.0).contains(&confidence) {
                lines.push(format!("信心 {}", Self::format_number(confidence)));
            }
        }
        if let Some(justification) = self.justification.as_deref() {
            if !justification.is_empty() {
                lines.push(format!("理由 {}", justification));
            }
        }
        lines
    }
}
