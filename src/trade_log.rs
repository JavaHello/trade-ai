use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local, LocalResult, TimeZone};
use serde::{Deserialize, Serialize};

use crate::command::TradeEvent;

#[derive(Clone, Debug)]
pub struct TradeLogEntry {
    pub timestamp: DateTime<Local>,
    pub event: TradeEvent,
}

impl TradeLogEntry {
    pub fn from_event(event: TradeEvent) -> Self {
        TradeLogEntry {
            timestamp: Local::now(),
            event,
        }
    }

    fn from_parts(timestamp_ms: i64, event: TradeEvent) -> Self {
        let timestamp = match Local.timestamp_millis_opt(timestamp_ms) {
            LocalResult::Single(dt) => dt,
            _ => Local::now(),
        };
        TradeLogEntry { timestamp, event }
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
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(stored) = serde_json::from_str::<StoredTradeLogEntry>(&line) {
                entries.push(stored.into_entry());
            }
        }
        if entries.len() > self.max_entries {
            entries = entries.split_off(entries.len() - self.max_entries);
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
        drop(file);
        self.compact_if_needed()?;
        Ok(())
    }

    fn compact_if_needed(&self) -> Result<()> {
        let metadata = match fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let estimated_limit = (self.max_entries as u64).saturating_mul(256);
        if metadata.len() <= estimated_limit {
            return Ok(());
        }
        let entries = self.load()?;
        self.overwrite(&entries)
    }

    fn overwrite(&self, entries: &[TradeLogEntry]) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        for entry in entries {
            serde_json::to_writer(&mut file, &StoredTradeLogEntry::from(entry))?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct StoredTradeLogEntry {
    timestamp_ms: i64,
    event: TradeEvent,
}

impl StoredTradeLogEntry {
    fn into_entry(self) -> TradeLogEntry {
        TradeLogEntry::from_parts(self.timestamp_ms, self.event)
    }
}

impl From<&TradeLogEntry> for StoredTradeLogEntry {
    fn from(value: &TradeLogEntry) -> Self {
        StoredTradeLogEntry {
            timestamp_ms: value.timestamp_ms(),
            event: value.event.clone(),
        }
    }
}
