#[derive(Debug, Clone)]
pub enum Command {
    MarkPriceUpdate(String, f64, i64, usize),
    Notify(String, String),
    Error(String),
    Exit,
}

#[derive(Debug, Clone)]
pub struct PricePoint {
    pub inst_id: String,
    pub mark_px: f64,
    pub ts: i64,
    pub precision: usize,
}
