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
        while let Ok(message) = self.rx.recv().await {
            match message {
                Command::Notify(inst_id, msg) => {
                    if start.elapsed() <= self.interval {
                        continue;
                    }
                    #[cfg(target_os = "linux")]
                    {
                        linux_notify(&msg, &inst_id).await?;
                        start = tokio::time::Instant::now();
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        Ok(())
                    }
                }
                _ => {}
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
