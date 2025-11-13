use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local, LocalResult, TimeZone};
use serde::{Deserialize, Serialize};

use crate::command::TradeEvent;

#[derive(Clone, Debug, PartialEq)]
pub struct TradeLogEntry {
    pub timestamp: DateTime<Local>,
    pub event: TradeEvent,
    pub leverage: Option<f64>,
}

impl TradeLogEntry {
    pub fn from_event(event: TradeEvent, leverage: Option<f64>) -> Self {
        let timestamp = match &event {
            TradeEvent::Fill(fill) => fill
                .fill_time
                .and_then(|ms| match Local.timestamp_millis_opt(ms) {
                    LocalResult::Single(dt) => Some(dt),
                    _ => None,
                })
                .unwrap_or_else(Local::now),
            _ => Local::now(),
        };
        TradeLogEntry {
            timestamp,
            event,
            leverage,
        }
    }

    fn from_parts(timestamp_ms: i64, event: TradeEvent, leverage: Option<f64>) -> Self {
        let timestamp = match Local.timestamp_millis_opt(timestamp_ms) {
            LocalResult::Single(dt) => dt,
            _ => Local::now(),
        };
        TradeLogEntry {
            timestamp,
            event,
            leverage,
        }
    }

    fn timestamp_ms(&self) -> i64 {
        self.timestamp.timestamp_millis()
    }
}

#[derive(Clone, Debug)]
pub struct TradeLogStore {
    path: PathBuf,
    max_entries: usize,
}

impl TradeLogStore {
    const TAIL_CHUNK_SIZE: usize = 8 * 1024;

    pub fn new(path: PathBuf) -> Self {
        TradeLogStore {
            path,
            max_entries: 512,
        }
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from("trade_logs.jsonl")
    }

    pub fn load(&self) -> Result<Vec<TradeLogEntry>> {
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
            if let Ok(stored) = serde_json::from_str::<StoredTradeLogEntry>(&line) {
                entries.push(stored.into_entry());
            }
        }
        Ok(entries)
    }

    pub fn append(&self, entry: &TradeLogEntry) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &StoredTradeLogEntry::from(entry))?;
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
struct StoredTradeLogEntry {
    timestamp_ms: i64,
    event: TradeEvent,
    #[serde(default)]
    leverage: Option<f64>,
}

impl StoredTradeLogEntry {
    fn into_entry(self) -> TradeLogEntry {
        TradeLogEntry::from_parts(self.timestamp_ms, self.event, self.leverage)
    }
}

impl From<&TradeLogEntry> for StoredTradeLogEntry {
    fn from(value: &TradeLogEntry) -> Self {
        StoredTradeLogEntry {
            timestamp_ms: value.timestamp_ms(),
            event: value.event.clone(),
            leverage: value.leverage,
        }
    }
}
