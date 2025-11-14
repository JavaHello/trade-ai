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
    pub justification: Option<String>,
}

impl AiDecisionRecord {
    pub fn from_payload(payload: AiInsightRecord) -> Self {
        let timestamp = match Local.timestamp_millis_opt(payload.timestamp_ms) {
            LocalResult::Single(dt) => dt,
            _ => Local::now(),
        };
        let justification = Self::compute_justification(&payload.response, None);
        AiDecisionRecord {
            timestamp,
            system_prompt: payload.system_prompt,
            user_prompt: payload.user_prompt,
            response: payload.response,
            justification,
        }
    }

    pub fn summary(&self) -> String {
        if let Some(justification) = self.justification.as_deref() {
            let trimmed = justification.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let mut fragments = self
            .response
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .take(2)
            .collect::<Vec<_>>();
        if fragments.is_empty() {
            fragments.push(self.response.trim().to_string());
        }
        fragments.retain(|s| !s.is_empty());
        if fragments.is_empty() {
            "无内容".to_string()
        } else {
            fragments.join(" | ")
        }
    }

    fn from_parts(
        timestamp_ms: i64,
        system_prompt: String,
        user_prompt: String,
        response: String,
        justification: Option<String>,
    ) -> Self {
        let timestamp = match Local.timestamp_millis_opt(timestamp_ms) {
            LocalResult::Single(dt) => dt,
            _ => Local::now(),
        };
        let justification = Self::compute_justification(&response, justification);
        AiDecisionRecord {
            timestamp,
            system_prompt,
            user_prompt,
            response,
            justification,
        }
    }

    fn timestamp_ms(&self) -> i64 {
        self.timestamp.timestamp_millis()
    }

    fn compute_justification(response: &str, provided: Option<String>) -> Option<String> {
        let normalized = provided
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        normalized.or_else(|| Self::extract_justification(response))
    }

    fn extract_justification(response: &str) -> Option<String> {
        fn parse_block(block: &str) -> Option<String> {
            let value: serde_json::Value = serde_json::from_str(block).ok()?;
            let justification = value.get("justification")?;
            let text = justification.as_str()?.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        parse_block(response).or_else(|| {
            let start = response.find('{')?;
            let end = response.rfind('}')?;
            if end <= start {
                return None;
            }
            let slice = response.get(start..=end)?;
            parse_block(slice)
        })
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
    justification: Option<String>,
}

impl StoredAiDecision {
    fn into_record(self) -> AiDecisionRecord {
        AiDecisionRecord::from_parts(
            self.timestamp_ms,
            self.system_prompt,
            self.user_prompt,
            self.response,
            self.justification,
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
            justification: value.justification.clone(),
        }
    }
}
