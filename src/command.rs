#[derive(Debug, Clone)]
pub enum Command {
    MarkPriceUpdate(String, f64, i64, usize),
    Notify(String, String),
    Error(String),
    Exit,
}
