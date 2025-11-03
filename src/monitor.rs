use tokio::sync::broadcast;

pub struct Monitor {
    pub lower_threshold: f64,
    pub upper_threshold: f64,
    pub tx: broadcast::Sender<crate::command::Command>,
    pub rx: broadcast::Receiver<crate::command::Command>,
}

impl Monitor {
    pub fn new(
        lower_threshold: f64,
        upper_threshold: f64,
        tx: broadcast::Sender<crate::command::Command>,
        rx: broadcast::Receiver<crate::command::Command>,
    ) -> Monitor {
        Monitor {
            lower_threshold,
            upper_threshold,
            tx,
            rx,
        }
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        while let Ok(message) = self.rx.recv().await {
            match message {
                crate::command::Command::MarkPriceUpdate(inst_id, mark_price, _ts) => {
                    if mark_price < self.lower_threshold {
                        let notify_msg = format!(
                            "{} 当前标记价格 {:.4} 低于下限 {:.4}",
                            inst_id, mark_price, self.lower_threshold
                        );
                        let _ = self
                            .tx
                            .send(crate::command::Command::Notify(inst_id.clone(), notify_msg));
                    } else if mark_price > self.upper_threshold {
                        let notify_msg = format!(
                            "{} 当前标记价格 {:.4} 高于上限 {:.4}",
                            inst_id, mark_price, self.upper_threshold
                        );
                        let _ = self
                            .tx
                            .send(crate::command::Command::Notify(inst_id.clone(), notify_msg));
                    }
                }
                crate::command::Command::Exit => {
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    }
}
