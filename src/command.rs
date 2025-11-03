#[derive(Debug, Clone)]
pub enum Command {
    MarkPriceUpdate(String, f64, i64),
    Notify(String, String),
    Exit,
}
