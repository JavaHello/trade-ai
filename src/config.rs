use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;

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
        let secret = self.okx_api_secret.as_ref()?.trim();
        let passphrase = self.okx_api_passphrase.as_ref()?.trim();
        if api_key.is_empty() || secret.is_empty() || passphrase.is_empty() {
            return None;
        }
        Some(TradingConfig {
            api_key: api_key.to_string(),
            api_secret: secret.to_string(),
            passphrase: passphrase.to_string(),
            td_mode: self.okx_td_mode.clone(),
        })
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
