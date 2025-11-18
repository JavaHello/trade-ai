use std::io::{self, Write};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::command::Command;

pub struct OsNotification {
    pub rx: broadcast::Receiver<Command>,
    exit_rx: broadcast::Receiver<()>,
    interval: Duration,
}

impl OsNotification {
    pub fn new(
        rx: broadcast::Receiver<Command>,
        exit_rx: broadcast::Receiver<()>,
    ) -> OsNotification {
        OsNotification {
            rx,
            exit_rx,
            interval: Duration::from_secs(10),
        }
    }
    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let mut start = tokio::time::Instant::now();
        let rx = &mut self.rx;
        let exit_rx = &mut self.exit_rx;
        loop {
            tokio::select! {
                result = rx.recv() => match result {
                    Ok(Command::Notify(inst_id, msg)) => {
                        if start.elapsed() <= self.interval {
                            continue;
                        }
                        terminal_notify(&inst_id, &msg)?;
                        start = tokio::time::Instant::now();
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                signal = exit_rx.recv() => match signal {
                    Ok(_) | Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
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
