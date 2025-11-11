use std::collections::HashMap;

use tokio::sync::broadcast;

pub struct Monitor {
    pub thresholds: HashMap<String, (f64, f64)>,
    pub tx: broadcast::Sender<crate::command::Command>,
    pub rx: broadcast::Receiver<crate::command::Command>,
    price_precision: HashMap<String, usize>,
}

impl Monitor {
    pub fn new(
        thresholds: HashMap<String, (f64, f64)>,
        tx: broadcast::Sender<crate::command::Command>,
        rx: broadcast::Receiver<crate::command::Command>,
    ) -> Monitor {
        Monitor {
            thresholds,
            tx,
            rx,
            price_precision: HashMap::new(),
        }
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        loop {
            match self.rx.recv().await {
                Ok(message) => match message {
                    crate::command::Command::MarkPriceUpdate(
                        inst_id,
                        mark_price,
                        _ts,
                        precision,
                    ) => {
                        self.update_precision(&inst_id, precision);
                        let (lower, upper) = self.threshold_for(&inst_id);
                        if mark_price < lower {
                            let notify_msg = format!(
                                "{} mark price {} is below lower bound {}",
                                inst_id,
                                self.format_price(&inst_id, mark_price),
                                self.format_price(&inst_id, lower)
                            );
                            let _ = self
                                .tx
                                .send(crate::command::Command::Notify(inst_id.clone(), notify_msg));
                        } else if mark_price > upper {
                            let notify_msg = format!(
                                "{} mark price {} is above upper bound {}",
                                inst_id,
                                self.format_price(&inst_id, mark_price),
                                self.format_price(&inst_id, upper)
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

    fn update_precision(&mut self, inst_id: &str, precision: usize) {
        if precision == 0 {
            return;
        }
        self.price_precision
            .entry(inst_id.to_string())
            .and_modify(|existing| {
                if precision > *existing {
                    *existing = precision;
                }
            })
            .or_insert(precision);
    }

    fn format_price(&self, inst_id: &str, value: f64) -> String {
        let precision = self.price_precision.get(inst_id).copied().unwrap_or(2);
        format!("{value:.prec$}", value = value, prec = precision)
    }
}
