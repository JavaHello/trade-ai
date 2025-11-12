use std::io::{self, Write};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::command::Command;

pub struct OsNotification {
    pub rx: broadcast::Receiver<Command>,
    interval: Duration,
}

impl OsNotification {
    pub fn new(rx: broadcast::Receiver<Command>) -> OsNotification {
        OsNotification {
            rx,
            interval: Duration::from_secs(10),
        }
    }
    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let mut start = tokio::time::Instant::now();
        loop {
            match self.rx.recv().await {
                Ok(message) => match message {
                    Command::Notify(inst_id, msg) => {
                        if start.elapsed() <= self.interval {
                            continue;
                        }
                        terminal_notify(&inst_id, &msg)?;
                        start = tokio::time::Instant::now();
                    }
                    Command::Exit => {
                        return Ok(());
                    }
                    _ => {}
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        Ok(())
    }
}

fn terminal_notify(inst_id: &str, msg: &str) -> Result<(), anyhow::Error> {
    let title = format!("Price Monitor - {inst_id}");
    let sanitized_title = sanitize_osc_field(&title);
    let sanitized_body = sanitize_osc_field(msg);
    emit_osc777(&sanitized_title, &sanitized_body)?;
    Ok(())
}

fn emit_osc777(title: &str, body: &str) -> Result<(), anyhow::Error> {
    let mut stdout = io::stdout();
    write!(stdout, "\u{1b}]777;notify;{};{}\u{7}", title, body)?;
    stdout.flush()?;
    Ok(())
}

fn sanitize_osc_field(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '\u{7}' | '\u{1b}'))
        .map(|c| match c {
            '\n' | '\r' => ' ',
            _ => c,
        })
        .collect()
}
