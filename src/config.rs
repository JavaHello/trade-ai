use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result as AnyResult, anyhow};
use chrono::{FixedOffset, Local, TimeZone, Utc};
use chrono_tz::Tz;
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser, Clone, Debug)]
pub struct CliParams {
    /// Instrument IDs to monitor; comma separated or pass multiple times
    #[clap(
        short = 'i',
        long = "inst-id",
        value_delimiter = ',',
        num_args = 1..,
        default_values_t = vec!["BTC-USDT-SWAP".to_string()]
    )]
    pub inst_ids: Vec<String>,

    /// Per-instrument thresholds in format INST:LOWER:UPPER; repeat as needed
    #[clap(long = "threshold", value_name = "INST:LOWER:UPPER")]
    pub thresholds: Vec<ThresholdSpec>,

    /// Amount of history the TUI keeps in memory (e.g., 15m, 1h, 1d)
    #[clap(long = "window", value_name = "DURATION", default_value = "15m")]
    pub window: DurationSpec,

    /// OKX API key used for authenticated trading
    #[clap(long = "okx-api-key", env = "OKX_API_KEY")]
    pub okx_api_key: Option<String>,

    /// OKX API secret used for authenticated trading
    #[clap(long = "okx-api-secret", env = "OKX_API_SECRET")]
    pub okx_api_secret: Option<String>,

    /// OKX API passphrase used for authenticated trading
    #[clap(long = "okx-api-passphrase", env = "OKX_API_PASSPHRASE")]
    pub okx_api_passphrase: Option<String>,

    /// Trading mode for OKX orders (cash, cross, or isolated)
    #[clap(
        long = "okx-td-mode",
        default_value = "cross",
        value_parser = ["cash", "cross", "isolated"]
    )]
    pub okx_td_mode: String,

    /// Deepseek API key used for AI analysis of account states
    #[clap(long = "deepseek-api-key", env = "DEEPSEEK_API_KEY")]
    pub deepseek_api_key: Option<String>,

    /// AI provider used for analysis (deepseek or openrouter)
    #[clap(
        long = "ai-provider",
        env = "AI_PROVIDER",
        default_value = "deepseek",
        value_parser = ["deepseek", "openrouter"]
    )]
    pub ai_provider: String,

    /// Deepseek model name for chat completions
    #[clap(
        long = "deepseek-model",
        env = "DEEPSEEK_MODEL",
        default_value = "deepseek-chat"
    )]
    pub deepseek_model: String,

    /// Deepseek endpoint base URL (default https://api.deepseek.com)
    #[clap(
        long = "deepseek-endpoint",
        env = "DEEPSEEK_API_BASE",
        default_value = "https://api.deepseek.com"
    )]
    pub deepseek_endpoint: String,

    /// OpenRouter API key used for AI analysis of account states
    #[clap(long = "openrouter-api-key", env = "OPENROUTER_API_KEY")]
    pub openrouter_api_key: Option<String>,

    /// OpenRouter model name for chat completions
    #[clap(
        long = "openrouter-model",
        env = "OPENROUTER_MODEL",
        default_value = "deepseek/deepseek-chat-v3.1"
    )]
    pub openrouter_model: String,

    /// OpenRouter endpoint base URL (default https://openrouter.ai/api/v1)
    #[clap(
        long = "openrouter-endpoint",
        env = "OPENROUTER_API_BASE",
        default_value = "https://openrouter.ai/api/v1"
    )]
    pub openrouter_endpoint: String,

    /// Interval between AI decisions (e.g., 5m, 15m)
    #[clap(
        long = "decision_interval",
        value_name = "DURATION",
        default_value = "5m"
    )]
    decision_interval: DurationSpec,
}

#[derive(Clone, Debug)]
pub struct ThresholdSpec {
    pub inst_id: String,
    pub lower: f64,
    pub upper: f64,
}

impl FromStr for ThresholdSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(':');
        let inst_id = parts
            .next()
            .ok_or_else(|| "threshold spec must include inst_id".to_string())?
            .trim();
        let lower = parts
            .next()
            .ok_or_else(|| "threshold spec must include lower value".to_string())
            .and_then(|value| {
                value
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| format!("invalid lower threshold value: {value}").to_string())
            })?;
        let upper = parts
            .next()
            .ok_or_else(|| "threshold spec must include upper value".to_string())
            .and_then(|value| {
                value
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| format!("invalid upper threshold value: {value}").to_string())
            })?;
        if parts.next().is_some() {
            return Err("threshold spec should only have INST:LOWER:UPPER".to_string());
        }
        if inst_id.is_empty() {
            return Err("threshold spec inst_id cannot be empty".to_string());
        }
        Ok(ThresholdSpec {
            inst_id: inst_id.to_string(),
            lower,
            upper,
        })
    }
}

impl CliParams {
    pub fn threshold_map(&self) -> HashMap<String, (f64, f64)> {
        let mut map = HashMap::new();
        for spec in &self.thresholds {
            map.insert(spec.inst_id.clone(), (spec.lower, spec.upper));
        }
        map
    }

    pub fn history_window(&self) -> Duration {
        self.window.as_duration()
    }

    pub fn trading_config(&self) -> Option<TradingConfig> {
        let api_key = self.okx_api_key.as_ref()?.trim();
        let api_secret = self.okx_api_secret.as_ref()?.trim();
        let passphrase = self.okx_api_passphrase.as_ref()?.trim();
        if api_key.is_empty() || api_secret.is_empty() || passphrase.is_empty() {
            return None;
        }
        Some(TradingConfig {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            passphrase: passphrase.to_string(),
            td_mode: self.okx_td_mode.clone(),
        })
    }

    pub fn ai_config(&self) -> Option<DeepseekConfig> {
        let provider = parse_ai_provider(&self.ai_provider);
        match provider {
            AiProvider::Deepseek => {
                let api_key = self.deepseek_api_key.as_ref()?.trim();
                if api_key.is_empty() {
                    return None;
                }
                let endpoint =
                    normalize_endpoint(self.deepseek_endpoint.trim(), DEFAULT_DEEPSEEK_ENDPOINT);
                let model = self.deepseek_model.trim();
                if model.is_empty() {
                    return None;
                }
                Some(DeepseekConfig {
                    api_key: api_key.to_string(),
                    endpoint,
                    model: model.to_string(),
                    interval: self.decision_interval.as_duration(),
                    provider,
                })
            }
            AiProvider::OpenRouter => {
                let api_key = if let Some(key) = &self.openrouter_api_key {
                    key.trim()
                } else {
                    return None;
                };
                let endpoint = normalize_endpoint(
                    self.openrouter_endpoint.trim(),
                    DEFAULT_OPENROUTER_ENDPOINT,
                );
                let model = self.openrouter_model.trim();
                if model.is_empty() {
                    return None;
                }
                Some(DeepseekConfig {
                    api_key: api_key.to_string(),
                    endpoint,
                    model: model.to_string(),
                    interval: self.decision_interval.as_duration(),
                    provider,
                })
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct DurationSpec(Duration);

impl DurationSpec {
    pub fn as_duration(&self) -> Duration {
        self.0
    }
}

impl FromStr for DurationSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let duration = parse_duration_spec(s)?;
        Ok(DurationSpec(duration))
    }
}

fn parse_duration_spec(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("duration spec cannot be empty (examples: 15m, 1h, 1d)".to_string());
    }
    let split_idx = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| "duration spec must end with a unit like s, m, h, or d".to_string())?;
    if split_idx == 0 {
        return Err("duration spec must start with a number (examples: 15m, 1h)".to_string());
    }
    let (value_part, unit_part) = trimmed.split_at(split_idx);
    let value: f64 = value_part.parse().map_err(|_| {
        format!(
            "invalid numeric portion `{}` in duration spec `{}`",
            value_part, trimmed
        )
    })?;
    let unit = unit_part.trim().to_lowercase();
    if unit.is_empty() {
        return Err("duration spec missing unit (use s, m, h, or d)".to_string());
    }
    let seconds_multiplier = match unit.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60.0 * 60.0,
        "d" | "day" | "days" => 60.0 * 60.0 * 24.0,
        other => {
            return Err(format!(
                "unsupported duration unit `{}` (use s, m, h, or d)",
                other
            ));
        }
    };
    let seconds = value * seconds_multiplier;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("duration must be positive: `{}`", trimmed));
    }
    let max_seconds = Duration::MAX.as_secs_f64();
    if seconds > max_seconds {
        return Err(format!("duration `{}` is too large", trimmed));
    }
    Ok(Duration::from_secs_f64(seconds))
}

#[derive(Clone, Debug)]
pub struct TradingConfig {
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: String,
    pub td_mode: String,
}

#[derive(Clone, Debug)]
pub struct DeepseekConfig {
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
    pub interval: Duration,
    pub provider: AiProvider,
}

impl DeepseekConfig {
    pub fn provider_label(&self) -> String {
        match self.provider {
            AiProvider::Deepseek => "Deepseek",
            AiProvider::OpenRouter => "OpenRouter",
        }
        .to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiProvider {
    Deepseek,
    OpenRouter,
}

const DEFAULT_DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com";
const DEFAULT_OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1";

fn parse_ai_provider(value: &str) -> AiProvider {
    match value.trim().to_ascii_lowercase().as_str() {
        "openrouter" => AiProvider::OpenRouter,
        _ => AiProvider::Deepseek,
    }
}

fn normalize_endpoint(value: &str, default_base: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        default_base.to_string()
    } else {
        trimmed.to_string()
    }
}

const DEFAULT_TIMEZONE_LABEL: &str = "Asia/Shanghai";

#[derive(Clone, Copy, Debug)]
pub enum ConfiguredTimeZone {
    Local,
    Named(Tz),
    Fixed(FixedOffset),
}

impl ConfiguredTimeZone {
    pub fn format_timestamp(&self, timestamp_ms: i64, fmt: &str) -> Option<String> {
        let seconds = timestamp_ms.div_euclid(1000);
        let millis = timestamp_ms.rem_euclid(1000);
        let nanos = (millis as u32) * 1_000_000;
        let base = Utc.timestamp_opt(seconds, nanos).single()?;
        Some(self.apply_timezone(base).format(fmt).to_string())
    }

    fn apply_timezone<TzSrc: TimeZone>(
        &self,
        datetime: chrono::DateTime<TzSrc>,
    ) -> chrono::DateTime<FixedOffset> {
        match self {
            ConfiguredTimeZone::Local => {
                let localized = datetime.with_timezone(&Local);
                localized.fixed_offset()
            }
            ConfiguredTimeZone::Named(tz) => {
                let localized = datetime.with_timezone(tz);
                localized.fixed_offset()
            }
            ConfiguredTimeZone::Fixed(offset) => datetime.with_timezone(offset),
        }
    }
}

fn parse_timezone_label(label: Option<String>) -> AnyResult<ConfiguredTimeZone> {
    let provided = label.unwrap_or_else(|| "local".to_string());
    let trimmed = provided.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("local") {
        return Ok(ConfiguredTimeZone::Local);
    }
    if let Ok(tz) = trimmed.parse::<Tz>() {
        return Ok(ConfiguredTimeZone::Named(tz));
    }
    if let Some(offset) = try_parse_fixed_offset(trimmed) {
        return Ok(ConfiguredTimeZone::Fixed(offset));
    }
    Err(anyhow!(
        "无法解析时区 `{}`，请使用 IANA 时区名称（如 Asia/Shanghai）或 UTC±HH:MM 形式",
        trimmed
    ))
}

fn try_parse_fixed_offset(input: &str) -> Option<FixedOffset> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("z")
        || trimmed.eq_ignore_ascii_case("utc")
        || trimmed.eq_ignore_ascii_case("gmt")
    {
        return FixedOffset::east_opt(0);
    }
    let normalized = if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("utc") {
        trimmed[3..].trim()
    } else if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("gmt") {
        trimmed[3..].trim()
    } else {
        trimmed
    };
    if normalized.is_empty() {
        return FixedOffset::east_opt(0);
    }
    let bytes = normalized.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (sign, rest) = match bytes[0] {
        b'+' => (1, &normalized[1..]),
        b'-' => (-1, &normalized[1..]),
        value if (value as char).is_ascii_digit() => (1, normalized),
        _ => return None,
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let (hour_part, minute_part) = if let Some(idx) = rest.find(':') {
        let hour = rest[..idx].trim();
        let minute = rest[idx + 1..].trim();
        (hour, minute)
    } else if rest.len() == 4 {
        (&rest[..2], &rest[2..])
    } else {
        (rest, "0")
    };
    if hour_part.is_empty() {
        return None;
    }
    let hours: i32 = hour_part.parse().ok()?;
    let minutes: i32 = if minute_part.is_empty() {
        0
    } else {
        minute_part.parse().ok()?
    };
    if minutes >= 60 {
        return None;
    }
    let seconds = hours
        .checked_mul(3600)?
        .checked_add(minutes * 60)?
        .checked_mul(sign)?;
    FixedOffset::east_opt(seconds)
}

#[derive(Debug, Clone)]
pub struct AppRunConfig {
    start_timestamp_ms: i64,
    timezone: ConfiguredTimeZone,
}

impl AppRunConfig {
    pub fn load_or_init(path: impl AsRef<Path>) -> AnyResult<Self> {
        let path = path.as_ref();
        let stored = match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<StoredAppRunConfig>(&contents)
                .with_context(|| format!("解析 {} 失败", path.display()))?,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let now_ms = Local::now().timestamp_millis();
                let stored = StoredAppRunConfig {
                    start_timestamp_ms: now_ms,
                    timezone: Some(DEFAULT_TIMEZONE_LABEL.to_string()),
                };
                let payload = serde_json::to_string_pretty(&stored)?;
                fs::write(path, payload).with_context(|| format!("无法写入 {}", path.display()))?;
                stored
            }
            Err(err) => {
                return Err(anyhow!("读取 {} 失败: {}", path.display(), err));
            }
        };
        let timezone = parse_timezone_label(stored.timezone.clone())?;
        Ok(AppRunConfig {
            start_timestamp_ms: stored.start_timestamp_ms,
            timezone,
        })
    }

    pub fn start_timestamp_ms(&self) -> i64 {
        self.start_timestamp_ms
    }

    pub fn timezone(&self) -> ConfiguredTimeZone {
        self.timezone
    }
}

#[derive(Serialize, Deserialize)]
struct StoredAppRunConfig {
    start_timestamp_ms: i64,
    #[serde(default)]
    timezone: Option<String>,
}
