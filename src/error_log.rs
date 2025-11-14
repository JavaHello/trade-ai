use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct ErrorLogEntry {
    pub timestamp: DateTime<Local>,
    pub message: String,
}

impl ErrorLogEntry {
    pub fn new(message: impl Into<String>) -> Self {
        ErrorLogEntry {
            timestamp: Local::now(),
            message: message.into(),
        }
    }

    fn timestamp_ms(&self) -> i64 {
        self.timestamp.timestamp_millis()
    }
}

#[derive(Clone, Debug)]
pub struct ErrorLogStore {
    path: PathBuf,
}

impl ErrorLogStore {
    pub fn new(path: PathBuf) -> Self {
        ErrorLogStore { path }
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from("error_logs.jsonl")
    }

    pub fn append_message(&self, message: impl Into<String>) -> Result<()> {
        let entry = ErrorLogEntry::new(message);
        self.append(&entry)
    }

    fn append(&self, entry: &ErrorLogEntry) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &StoredErrorLogEntry::from(entry))?;
        file.write_all(b"\n")?;
        Ok(())
    }
}

#[derive(Serialize)]
struct StoredErrorLogEntry {
    timestamp_ms: i64,
    message: String,
}

impl From<&ErrorLogEntry> for StoredErrorLogEntry {
    fn from(entry: &ErrorLogEntry) -> Self {
        StoredErrorLogEntry {
            timestamp_ms: entry.timestamp_ms(),
            message: entry.message.clone(),
        }
    }
}
