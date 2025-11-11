use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum Command {
    MarkPriceUpdate(String, f64, i64, usize),
    Notify(String, String),
    Error(String),
    TradeResult(TradeEvent),
    AccountSnapshot(AccountSnapshot),
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricePoint {
    pub inst_id: String,
    pub mark_px: f64,
    pub ts: i64,
    pub precision: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TradeSide {
    Buy,
    Sell,
}

impl TradeSide {
    pub fn as_okx_side(&self) -> &'static str {
        match self {
            TradeSide::Buy => "buy",
            TradeSide::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradeOperator {
    Manual,
    Ai { name: Option<String> },
    Custom(String),
}

impl TradeOperator {
    pub fn label(&self) -> String {
        match self {
            TradeOperator::Manual => "手动".to_string(),
            TradeOperator::Ai { name } => match name {
                Some(name) if !name.is_empty() => format!("AI:{name}"),
                _ => "AI".to_string(),
            },
            TradeOperator::Custom(value) => value.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRequest {
    pub inst_id: String,
    pub side: TradeSide,
    pub price: f64,
    pub size: f64,
    pub pos_side: Option<String>,
    pub reduce_only: bool,
    pub tag: Option<String>,
    pub operator: TradeOperator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeResponse {
    pub inst_id: String,
    pub side: TradeSide,
    pub price: f64,
    pub size: f64,
    pub order_id: Option<String>,
    pub message: String,
    pub success: bool,
    pub operator: TradeOperator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradeEvent {
    Order(TradeResponse),
    Cancel(CancelResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradingCommand {
    Place(TradeRequest),
    Cancel(CancelOrderRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelOrderRequest {
    pub inst_id: String,
    pub ord_id: String,
    pub operator: TradeOperator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    pub inst_id: String,
    pub ord_id: String,
    pub message: String,
    pub success: bool,
    pub operator: TradeOperator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub positions: Vec<PositionInfo>,
    pub open_orders: Vec<PendingOrderInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionInfo {
    pub inst_id: String,
    pub pos_side: Option<String>,
    pub size: f64,
    pub avg_px: Option<f64>,
    pub lever: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOrderInfo {
    pub inst_id: String,
    pub ord_id: String,
    pub side: String,
    pub pos_side: Option<String>,
    pub price: Option<f64>,
    pub size: f64,
    pub state: String,
    pub reduce_only: bool,
    pub tag: Option<String>,
}
