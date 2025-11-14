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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TradeOrderKind {
    Regular,
    TakeProfit,
    StopLoss,
}

impl Default for TradeOrderKind {
    fn default() -> Self {
        TradeOrderKind::Regular
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(default)]
    pub leverage: Option<f64>,
    #[serde(default)]
    pub kind: TradeOrderKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TradeResponse {
    pub inst_id: String,
    pub side: TradeSide,
    pub price: f64,
    pub size: f64,
    pub order_id: Option<String>,
    pub message: String,
    pub success: bool,
    pub operator: TradeOperator,
    #[serde(default)]
    pub pos_side: Option<String>,
    #[serde(default)]
    pub leverage: Option<f64>,
    #[serde(default)]
    pub kind: TradeOrderKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TradeFill {
    pub inst_id: String,
    pub side: TradeSide,
    pub price: f64,
    pub size: f64,
    pub order_id: String,
    #[serde(default)]
    pub pos_side: Option<String>,
    #[serde(default)]
    pub trade_id: Option<String>,
    #[serde(default)]
    pub exec_type: Option<String>,
    #[serde(default)]
    pub fill_time: Option<i64>,
    #[serde(default)]
    pub fee: Option<f64>,
    #[serde(default)]
    pub fee_currency: Option<String>,
    #[serde(default)]
    pub pnl: Option<f64>,
    #[serde(default)]
    pub acc_fill_size: Option<f64>,
    #[serde(default)]
    pub avg_price: Option<f64>,
    #[serde(default)]
    pub leverage: Option<f64>,
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TradeEvent {
    Order(TradeResponse),
    Cancel(CancelResponse),
    Fill(TradeFill),
}

impl TradeEvent {
    pub fn leverage_hint(&self) -> Option<f64> {
        match self {
            TradeEvent::Order(response) => response.leverage,
            TradeEvent::Cancel(_) => None,
            TradeEvent::Fill(fill) => fill.leverage,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradingCommand {
    Place(TradeRequest),
    Cancel(CancelOrderRequest),
    SetLeverage(SetLeverageRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelOrderRequest {
    pub inst_id: String,
    pub ord_id: String,
    pub operator: TradeOperator,
    pub pos_side: Option<String>,
    #[serde(default)]
    pub kind: TradeOrderKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CancelResponse {
    pub inst_id: String,
    pub ord_id: String,
    pub message: String,
    pub success: bool,
    pub operator: TradeOperator,
    #[serde(default)]
    pub pos_side: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetLeverageRequest {
    pub inst_id: String,
    pub lever: f64,
    pub pos_side: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub positions: Vec<PositionInfo>,
    pub open_orders: Vec<PendingOrderInfo>,
    #[serde(default)]
    pub balance: AccountBalance,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountBalance {
    pub total_equity: Option<f64>,
    pub delta: Vec<AccountBalanceDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountBalanceDelta {
    pub currency: String,
    #[serde(default)]
    pub cash_balance: Option<f64>,
    #[serde(default)]
    pub equity: Option<f64>,
    #[serde(default)]
    pub available: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionInfo {
    pub inst_id: String,
    pub pos_side: Option<String>,
    pub size: f64,
    pub avg_px: Option<f64>,
    pub lever: Option<f64>,
    #[serde(default)]
    pub upl: Option<f64>,
    #[serde(default)]
    pub upl_ratio: Option<f64>,
    pub imr: f64,
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
    pub lever: Option<f64>,
    #[serde(default)]
    pub trigger_price: Option<f64>,
    #[serde(default)]
    pub kind: TradeOrderKind,
}
