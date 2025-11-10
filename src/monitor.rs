use std::collections::HashMap;

use tokio::sync::broadcast;

pub struct Monitor {
    pub thresholds: HashMap<String, (f64, f64)>,
    pub tx: broadcast::Sender<crate::command::Command>,
    pub rx: broadcast::Receiver<crate::command::Command>,
}

impl Monitor {
    pub fn new(
        thresholds: HashMap<String, (f64, f64)>,
        tx: broadcast::Sender<crate::command::Command>,
        rx: broadcast::Receiver<crate::command::Command>,
    ) -> Monitor {
        Monitor { thresholds, tx, rx }
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        loop {
            match self.rx.recv().await {
                Ok(message) => match message {
                    crate::command::Command::MarkPriceUpdate(
                        inst_id,
                        mark_price,
                        _ts,
                        _precision,
                    ) => {
                        let (lower, upper) = self.threshold_for(&inst_id);
                        if mark_price < lower {
                            let notify_msg = format!(
                                "{} mark price {:.4} is below lower bound {:.4}",
                                inst_id, mark_price, lower
                            );
                            let _ = self
                                .tx
                                .send(crate::command::Command::Notify(inst_id.clone(), notify_msg));
                        } else if mark_price > upper {
                            let notify_msg = format!(
                                "{} mark price {:.4} is above upper bound {:.4}",
                                inst_id, mark_price, upper
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
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        Ok(())
    }

    fn threshold_for(&self, inst_id: &str) -> (f64, f64) {
        self.thresholds
            .get(inst_id)
            .copied()
            .unwrap_or((0.0, f64::MAX))
    }
}
