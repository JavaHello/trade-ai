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
                        #[cfg(target_os = "linux")]
                        {
                            linux_notify(&msg, &inst_id).await?;
                        }
                        #[cfg(target_os = "macos")]
                        {
                            macos_notify(&msg, &inst_id).await?;
                        }
                        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                        {
                            // No desktop notification support on this platform.
                        }
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
#[cfg(target_os = "linux")]
async fn linux_notify(msg: &str, inst_id: &str) -> Result<(), anyhow::Error> {
    use notify_rust::{CloseReason, Hint, Notification};
    use tokio::process::Command;
    Notification::new()
        .summary("价格监控")
        .body(msg)
        .hint(Hint::Urgency(notify_rust::Urgency::Critical))
        .action("open", "okx")
        .show()
        .unwrap()
        .on_close(|_: CloseReason| {
            let _ = Command::new("xdg-open")
                .arg(format!(
                    "https://www.okx.com/zh-hans/trade-swap/{}",
                    inst_id.to_lowercase()
                ))
                .spawn();
        });
    Ok(())
}

#[cfg(target_os = "macos")]
async fn macos_notify(msg: &str, inst_id: &str) -> Result<(), anyhow::Error> {
    use tokio::process::Command;

    // Basic escaping for quotes so AppleScript parses the message correctly.
    let escaped_msg = msg.replace('"', "\\\"");
    let escaped_inst = inst_id.replace('"', "\\\"");
    let script = format!(
        r#"display notification "{}" with title "{}" subtitle "{}""#,
        escaped_msg, "价格监控", escaped_inst
    );

    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .await?;
    Ok(())
}
