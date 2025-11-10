use std::collections::HashMap;
use std::str::FromStr;

use clap::Parser;

#[derive(Parser, Clone, Debug)]
pub struct CliParams {
    /// 需要监控的交易品种，支持使用逗号分隔或多次传入
    #[clap(
        short = 'i',
        long = "inst-id",
        value_delimiter = ',',
        num_args = 1..,
        default_values_t = vec!["BTC-USDT-SWAP".to_string()]
    )]
    pub inst_ids: Vec<String>,

    /// 指定单独品种的上下限，格式 INST_ID:LOWER:UPPER，可重复
    #[clap(long = "threshold", value_name = "INST:LOWER:UPPER")]
    pub thresholds: Vec<ThresholdSpec>,
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
}
