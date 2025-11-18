use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result as AnyResult;
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::GraphType;
use ratatui::widgets::{Axis, Block, Chart, Clear, Dataset, Paragraph, Wrap};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ai_log::{AiDecisionRecord, AiDecisionStore};
use crate::command::{
    AccountBalance, AccountSnapshot, AiInsightRecord, CancelOrderRequest, Command,
    PendingOrderInfo, PositionInfo, PricePoint, SetLeverageRequest, TradeEvent, TradeOperator,
    TradeOrderKind, TradeRequest, TradeSide, TradingCommand,
};
use crate::config::ConfiguredTimeZone;
use crate::okx::MarketInfo;
use crate::trade_log::{TradeLogEntry, TradeLogStore};

const COLOR_PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Green,
    Color::LightBlue,
    Color::Red,
    Color::LightMagenta,
    Color::LightCyan,
];
const EMPTY_SERIES: &[(f64, f64)] = &[];
const MAX_TRADE_LOGS: usize = 1000;
const MAX_POSITION_RECORDS: usize = 100;
const MAX_ORDER_RECORDS: usize = 100;
const MAX_AI_INSIGHTS: usize = 64;
const LEVERAGE_EPSILON: f64 = 1e-6;
const AI_INDEX_COLUMN_WIDTH: usize = 5;
const AI_TIME_COLUMN_WIDTH: usize = 8;
const LOADING_SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];

struct LoadingOverlay {
    message: String,
    spinner_index: usize,
    last_tick: Instant,
    block_input: bool,
}

impl LoadingOverlay {
    fn new(message: impl Into<String>, block_input: bool) -> Self {
        LoadingOverlay {
            message: message.into(),
            spinner_index: 0,
            last_tick: Instant::now(),
            block_input,
        }
    }

    fn tick(&mut self) -> bool {
        let interval = Duration::from_millis(120);
        if self.last_tick.elapsed() < interval {
            return false;
        }
        self.spinner_index = (self.spinner_index + 1) % LOADING_SPINNER_FRAMES.len();
        self.last_tick = Instant::now();
        true
    }

    fn spinner(&self) -> char {
        LOADING_SPINNER_FRAMES[self.spinner_index % LOADING_SPINNER_FRAMES.len()]
    }

    fn message(&self) -> &str {
        &self.message
    }

    fn blocks_input(&self) -> bool {
        self.block_input
    }
}

#[derive(Clone, Debug)]
struct AxisInfo {
    inst_id: String,
    color: Color,
    min: f64,
    mid: f64,
    max: f64,
}

#[derive(Clone, Debug)]
struct PricePanelEntry {
    inst_id: String,
    color: Color,
    price: f64,
    change_pct: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    Chart,
    Trade,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrderInputField {
    Price,
    Size,
    Leverage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TradeFocus {
    Instruments,
    Positions,
    Orders,
    Logs,
    AiInsights,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrderIntent {
    Manual,
    TakeProfit,
    StopLoss,
    Modify,
}

#[derive(Clone, Debug)]
struct OrderInputState {
    side: TradeSide,
    inst_id: String,
    price: String,
    size: String,
    leverage: String,
    initial_leverage: Option<f64>,
    active_field: OrderInputField,
    error: Option<String>,
    pos_side: Option<String>,
    intent: OrderIntent,
    order_kind: TradeOrderKind,
    reduce_only: bool,
    tag: Option<String>,
    replace_order_id: Option<String>,
}

#[derive(Clone, Debug)]
struct TradeState {
    selected_inst_idx: usize,
    selected_position_idx: usize,
    selected_order_idx: usize,
    selected_log_idx: usize,
    selected_ai_idx: usize,
    input: Option<OrderInputState>,
    order_tx: Option<mpsc::Sender<TradingCommand>>,
    logs: Vec<TradeLogEntry>,
    ai_insights: Vec<AiDecisionRecord>,
    log_view_height: u16,
    position_view_height: u16,
    order_view_height: u16,
    ai_view_height: u16,
    positions: Vec<PositionInfo>,
    open_orders: Vec<PendingOrderInfo>,
    focus: TradeFocus,
    log_store: Option<TradeLogStore>,
    ai_store: Option<AiDecisionStore>,
    log_detail: Option<TradeLogEntry>,
    ai_detail: Option<AiDecisionRecord>,
    ai_detail_scroll: u16,
    ai_detail_view_height: u16,
    ai_detail_total_rows: usize,
    markets: HashMap<String, MarketInfo>,
    balance: AccountBalance,
    ai_enabled: bool,
}

impl TradeState {
    fn new(
        order_tx: Option<mpsc::Sender<TradingCommand>>,
        log_store: Option<TradeLogStore>,
        ai_store: Option<AiDecisionStore>,
        markets: HashMap<String, MarketInfo>,
        ai_enabled: bool,
    ) -> Self {
        TradeState {
            selected_inst_idx: 0,
            selected_position_idx: 0,
            selected_order_idx: 0,
            selected_log_idx: 0,
            selected_ai_idx: 0,
            input: None,
            order_tx,
            logs: Vec::new(),
            ai_insights: Vec::new(),
            log_view_height: 0,
            position_view_height: 0,
            order_view_height: 0,
            ai_view_height: 0,
            positions: Vec::new(),
            open_orders: Vec::new(),
            focus: TradeFocus::Instruments,
            log_store,
            ai_store,
            log_detail: None,
            ai_detail: None,
            ai_detail_scroll: 0,
            ai_detail_view_height: 0,
            ai_detail_total_rows: 0,
            markets,
            balance: AccountBalance::default(),
            ai_enabled,
        }
    }

    fn ensure_selection(&mut self, inst_ids: &[String]) {
        if inst_ids.is_empty() {
            self.selected_inst_idx = 0;
        } else if self.selected_inst_idx >= inst_ids.len() {
            self.selected_inst_idx = inst_ids.len().saturating_sub(1);
        }
        self.ensure_position_selection();
        self.ensure_order_selection();
        self.ensure_ai_selection();
    }

    fn ensure_position_selection(&mut self) {
        let len = self.positions.len().min(MAX_POSITION_RECORDS);
        if len == 0 {
            self.selected_position_idx = 0;
        } else if self.selected_position_idx >= len {
            self.selected_position_idx = len.saturating_sub(1);
        }
    }

    fn ensure_order_selection(&mut self) {
        let len = self.open_orders.len().min(MAX_ORDER_RECORDS);
        if len == 0 {
            self.selected_order_idx = 0;
        } else if self.selected_order_idx >= len {
            self.selected_order_idx = len.saturating_sub(1);
        }
    }

    fn ensure_log_selection(&mut self) {
        if self.logs.is_empty() {
            self.selected_log_idx = 0;
        } else if self.selected_log_idx >= self.logs.len() {
            self.selected_log_idx = self.logs.len().saturating_sub(1);
        }
    }

    fn ensure_ai_selection(&mut self) {
        if self.ai_insights.is_empty() {
            self.selected_ai_idx = 0;
        } else if self.selected_ai_idx >= self.ai_insights.len() {
            self.selected_ai_idx = self.ai_insights.len().saturating_sub(1);
        }
    }

    fn move_focus(&mut self, inst_ids: &[String], delta: isize) {
        match self.focus {
            TradeFocus::Instruments => self.move_instruments(inst_ids, delta),
            TradeFocus::Positions => self.move_positions(delta),
            TradeFocus::Orders => self.move_orders(delta),
            TradeFocus::Logs => self.move_logs(delta),
            TradeFocus::AiInsights => self.move_ai(delta),
        }
    }

    fn move_instruments(&mut self, inst_ids: &[String], delta: isize) {
        if inst_ids.is_empty() {
            self.selected_inst_idx = 0;
            return;
        }
        let len = inst_ids.len() as isize;
        let current = self.selected_inst_idx as isize;
        let mut next = current + delta;
        if next < 0 {
            next = len - 1;
        } else if next >= len {
            next = 0;
        }
        self.selected_inst_idx = next as usize;
    }

    fn move_positions(&mut self, delta: isize) {
        let len = self.positions.len().min(MAX_POSITION_RECORDS);
        if len == 0 {
            self.selected_position_idx = 0;
            return;
        }
        let len = len as isize;
        let current = self.selected_position_idx.min((len - 1) as usize) as isize;
        let mut next = current + delta;
        if next < 0 {
            next = 0;
        } else if next >= len {
            next = len - 1;
        }
        self.selected_position_idx = next as usize;
    }

    fn move_orders(&mut self, delta: isize) {
        let len = self.open_orders.len().min(MAX_ORDER_RECORDS);
        if len == 0 {
            self.selected_order_idx = 0;
            return;
        }
        let len = len as isize;
        let current = self.selected_order_idx.min((len - 1) as usize) as isize;
        let mut next = current + delta;
        if next < 0 {
            next = 0;
        } else if next >= len {
            next = len - 1;
        }
        self.selected_order_idx = next as usize;
    }

    fn move_ai(&mut self, delta: isize) {
        if self.ai_insights.is_empty() {
            self.selected_ai_idx = 0;
            return;
        }
        let len = self.ai_insights.len() as isize;
        let current = self.selected_ai_idx.min((len - 1) as usize) as isize;
        let mut next = current + delta;
        if next < 0 {
            next = 0;
        } else if next >= len {
            next = len - 1;
        }
        self.selected_ai_idx = next as usize;
    }

    fn selected_inst<'a>(&self, inst_ids: &'a [String]) -> Option<&'a str> {
        inst_ids
            .get(self.selected_inst_idx)
            .map(|inst| inst.as_str())
    }

    fn trading_enabled(&self) -> bool {
        self.order_tx.is_some()
    }

    fn order_sender(&self) -> Option<&mpsc::Sender<TradingCommand>> {
        self.order_tx.as_ref()
    }

    fn load_persisted_logs(&mut self) -> AnyResult<usize> {
        let Some(store) = &self.log_store else {
            return Ok(0);
        };
        let entries = store.load()?;
        let count = entries.len();
        for entry in entries {
            self.push_log(entry);
        }
        self.selected_log_idx = self.logs.len().saturating_sub(1);
        Ok(count)
    }

    fn load_persisted_ai_insights(&mut self) -> AnyResult<usize> {
        if !self.ai_enabled {
            return Ok(0);
        }
        let Some(store) = &self.ai_store else {
            return Ok(0);
        };
        let entries = store.load()?;
        let count = entries.len();
        for entry in entries {
            self.push_ai_entry(entry);
        }
        Ok(count)
    }

    fn push_log(&mut self, entry: TradeLogEntry) {
        let was_empty = self.logs.is_empty();
        self.logs.push(entry);
        if self.logs.len() > MAX_TRADE_LOGS {
            let overflow = self.logs.len() - MAX_TRADE_LOGS;
            self.logs.drain(0..overflow);
            if self.logs.is_empty() {
                self.selected_log_idx = 0;
            } else if overflow > 0 {
                self.selected_log_idx = self.selected_log_idx.saturating_sub(overflow);
            }
        }
        if was_empty {
            self.selected_log_idx = self.logs.len().saturating_sub(1);
        } else {
            self.ensure_log_selection();
        }
    }

    fn push_ai_entry(&mut self, entry: AiDecisionRecord) {
        self.ai_insights.push(entry);
        if self.ai_insights.len() > MAX_AI_INSIGHTS {
            let overflow = self.ai_insights.len() - MAX_AI_INSIGHTS;
            self.ai_insights.drain(0..overflow);
            self.selected_ai_idx = self.selected_ai_idx.saturating_sub(overflow);
        }
        self.ensure_ai_selection();
        self.selected_ai_idx = self.ai_insights.len().saturating_sub(1);
    }

    fn push_ai_insight(&mut self, payload: AiInsightRecord) -> AnyResult<()> {
        if !self.ai_enabled {
            return Ok(());
        }
        let entry = AiDecisionRecord::from_payload(payload);
        self.push_ai_entry(entry.clone());
        if let Some(store) = &self.ai_store {
            store.append(&entry)?;
        }
        Ok(())
    }

    fn ai_panel_enabled(&self) -> bool {
        self.ai_enabled
    }

    fn ai_detail_active(&self) -> bool {
        self.ai_detail.is_some()
    }

    fn reset_ai_detail_scroll(&mut self) {
        self.ai_detail_scroll = 0;
        self.ai_detail_view_height = 0;
        self.ai_detail_total_rows = 0;
    }

    fn update_ai_detail_view(&mut self, view_height: u16, total_rows: usize) {
        let view_height = view_height.max(1);
        self.ai_detail_view_height = view_height;
        self.ai_detail_total_rows = total_rows;
        self.clamp_ai_detail_scroll();
    }

    fn clamp_ai_detail_scroll(&mut self) {
        let max_scroll = self.ai_detail_max_scroll();
        if (self.ai_detail_scroll as usize) > max_scroll {
            self.ai_detail_scroll = max_scroll as u16;
        }
    }

    fn ai_detail_max_scroll(&self) -> usize {
        if self.ai_detail_total_rows == 0 {
            0
        } else {
            let view = self.ai_detail_view_height.max(1) as usize;
            self.ai_detail_total_rows.saturating_sub(view)
        }
    }

    fn scroll_ai_detail(&mut self, delta: i16) {
        if self.ai_detail.is_none() || delta == 0 {
            return;
        }
        let max_scroll = self.ai_detail_max_scroll() as i32;
        let mut next = self.ai_detail_scroll as i32 + delta as i32;
        if next < 0 {
            next = 0;
        } else if next > max_scroll {
            next = max_scroll;
        }
        self.ai_detail_scroll = next as u16;
    }

    fn ai_insight_count(&self) -> usize {
        self.ai_insights.len()
    }

    fn selected_ai_idx(&self) -> usize {
        if self.ai_insights.is_empty() {
            0
        } else {
            self.selected_ai_idx
                .min(self.ai_insights.len().saturating_sub(1))
        }
    }

    fn selected_ai_entry(&self) -> Option<&AiDecisionRecord> {
        if self.ai_insights.is_empty() {
            None
        } else {
            let idx = self.selected_ai_idx();
            self.ai_insights.get(idx)
        }
    }

    fn latest_ai_entry(&self) -> Option<&AiDecisionRecord> {
        self.ai_insights.last()
    }

    fn latest_ai_summary(&self) -> Option<String> {
        self.latest_ai_entry().map(|entry| entry.summary())
    }

    fn set_ai_view_height(&mut self, view_height: u16) {
        self.ai_view_height = view_height.max(1);
    }

    fn page_scroll_ai(&mut self, pages: isize) {
        if pages == 0 || self.ai_insights.is_empty() {
            return;
        }
        let page = self.ai_view_height.max(1) as isize;
        let len = self.ai_insights.len() as isize;
        let mut next = self.selected_ai_idx() as isize + page * pages;
        if next < 0 {
            next = 0;
        } else if next >= len {
            next = len - 1;
        }
        self.selected_ai_idx = next as usize;
    }

    fn scroll_ai_to_start(&mut self) {
        if !self.ai_insights.is_empty() {
            self.selected_ai_idx = 0;
        }
    }

    fn scroll_ai_to_end(&mut self) {
        if !self.ai_insights.is_empty() {
            self.selected_ai_idx = self.ai_insights.len().saturating_sub(1);
        }
    }

    fn toggle_ai_detail(&mut self) {
        if let Some(active) = &self.ai_detail {
            if let Some(selected) = self.selected_ai_entry() {
                if active == selected {
                    self.ai_detail = None;
                    self.reset_ai_detail_scroll();
                    return;
                }
            } else {
                self.ai_detail = None;
                self.reset_ai_detail_scroll();
                return;
            }
        }
        if let Some(entry) = self.selected_ai_entry().cloned() {
            self.ai_detail = Some(entry);
            self.reset_ai_detail_scroll();
            self.log_detail = None;
        }
    }

    fn record_result(&mut self, event: TradeEvent) -> AnyResult<()> {
        let leverage = event
            .leverage_hint()
            .or_else(|| self.leverage_for_event(&event));
        let entry = TradeLogEntry::from_event(event, leverage);
        self.push_log(entry.clone());
        if let Some(store) = &self.log_store {
            store.append(&entry)?;
        }
        Ok(())
    }

    fn update_snapshot(&mut self, snapshot: AccountSnapshot, inst_ids: &[String]) {
        self.positions = snapshot.positions;
        self.open_orders = snapshot.open_orders;
        self.balance = snapshot.balance;
        self.ensure_selection(inst_ids);
    }

    fn snapshot_counts(&self) -> (usize, usize) {
        (self.positions.len(), self.open_orders.len())
    }

    fn selected_position(&self) -> Option<&PositionInfo> {
        let len = self.positions.len().min(MAX_POSITION_RECORDS);
        if len == 0 {
            None
        } else {
            let idx = self.selected_position_idx.min(len.saturating_sub(1));
            self.positions.get(idx)
        }
    }

    fn selected_order(&self) -> Option<&PendingOrderInfo> {
        let len = self.open_orders.len().min(MAX_ORDER_RECORDS);
        if len == 0 {
            None
        } else {
            let idx = self.selected_order_idx.min(len.saturating_sub(1));
            self.open_orders.get(idx)
        }
    }

    fn cycle_focus(&mut self, reverse: bool) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let current_idx = order
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let len = order.len();
        let next_idx = if reverse {
            (current_idx + len - 1) % len
        } else {
            (current_idx + 1) % len
        };
        self.focus = order[next_idx];
        match self.focus {
            TradeFocus::Instruments => {}
            TradeFocus::Positions => self.ensure_position_selection(),
            TradeFocus::Orders => self.ensure_order_selection(),
            TradeFocus::AiInsights => self.ensure_ai_selection(),
            TradeFocus::Logs => {}
        }
    }

    fn focus_order(&self) -> Vec<TradeFocus> {
        let order = vec![
            TradeFocus::Instruments,
            TradeFocus::Positions,
            TradeFocus::Orders,
            TradeFocus::AiInsights,
            TradeFocus::Logs,
        ];
        order
    }

    fn focus_label(&self) -> &'static str {
        match self.focus {
            TradeFocus::Instruments => "合约",
            TradeFocus::Positions => "持仓",
            TradeFocus::Orders => "挂单",
            TradeFocus::Logs => "委托记录",
            TradeFocus::AiInsights => "AI 决策",
        }
    }

    fn balance_lines(&self) -> Vec<String> {
        if let Some(line) = self.balance_summary_line() {
            vec![line]
        } else {
            Vec::new()
        }
    }

    fn balance_summary_line(&self) -> Option<String> {
        if self.balance.total_equity.is_none() && self.balance.delta.is_empty() {
            return None;
        }
        let mut segments = Vec::new();
        if let Some(total) = self.balance.total_equity {
            segments.push(format!(
                "总权益: {} USDT",
                Self::format_balance_amount(total)
            ));
        }
        Some(format!("账户余额：{}", segments.join(" · ")))
    }

    fn format_balance_amount(value: f64) -> String {
        let abs = value.abs();
        if abs >= 1000.0 {
            format!("{value:.2}")
        } else if abs >= 1.0 {
            format!("{value:.4}")
        } else {
            format!("{value:.6}")
        }
    }

    fn remove_open_order(&mut self, ord_id: &str) {
        let before = self.open_orders.len();
        self.open_orders.retain(|order| order.ord_id.ne(ord_id));
        if before != self.open_orders.len() {
            self.ensure_order_selection();
        }
    }

    fn move_logs(&mut self, delta: isize) {
        if delta == 0 || self.logs.is_empty() {
            return;
        }
        let len = self.logs.len() as isize;
        let current_display = len - 1 - self.selected_log_idx.min(self.logs.len() - 1) as isize;
        let mut next_display = current_display + delta;
        if next_display < 0 {
            next_display = 0;
        } else if next_display >= len {
            next_display = len - 1;
        }
        self.selected_log_idx = (len - 1 - next_display) as usize;
    }

    fn page_scroll_logs(&mut self, pages: isize) {
        if pages == 0 || self.logs.is_empty() {
            return;
        }
        let height = self.log_view_height.max(1) as isize;
        self.move_logs(height * pages);
    }

    fn scroll_logs_to_start(&mut self) {
        if self.logs.is_empty() {
            return;
        }
        self.selected_log_idx = self.logs.len().saturating_sub(1);
    }

    fn scroll_logs_to_end(&mut self) {
        if self.logs.is_empty() {
            return;
        }
        self.selected_log_idx = 0;
    }

    fn set_log_view_height(&mut self, view_height: u16) {
        self.log_view_height = view_height.max(1);
    }

    fn set_position_view_height(&mut self, view_height: u16) {
        self.position_view_height = view_height.max(1);
    }

    fn set_order_view_height(&mut self, view_height: u16) {
        self.order_view_height = view_height.max(1);
    }

    fn page_scroll_positions(&mut self, pages: isize) {
        if pages == 0 {
            return;
        }
        let len = self.positions.len().min(MAX_POSITION_RECORDS);
        if len == 0 {
            return;
        }
        let page = self.position_view_height.max(1) as isize;
        let len = len as isize;
        let mut next = self.selected_position_idx as isize + page * pages;
        if next < 0 {
            next = 0;
        } else if next >= len {
            next = len - 1;
        }
        self.selected_position_idx = next as usize;
    }

    fn page_scroll_orders(&mut self, pages: isize) {
        if pages == 0 {
            return;
        }
        let len = self.open_orders.len().min(MAX_ORDER_RECORDS);
        if len == 0 {
            return;
        }
        let page = self.order_view_height.max(1) as isize;
        let len = len as isize;
        let mut next = self.selected_order_idx as isize + page * pages;
        if next < 0 {
            next = 0;
        } else if next >= len {
            next = len - 1;
        }
        self.selected_order_idx = next as usize;
    }

    fn scroll_positions_to_start(&mut self) {
        if !self.positions.is_empty() {
            self.selected_position_idx = 0;
        }
    }

    fn scroll_positions_to_end(&mut self) {
        let len = self.positions.len().min(MAX_POSITION_RECORDS);
        if len > 0 {
            self.selected_position_idx = len - 1;
        }
    }

    fn scroll_orders_to_start(&mut self) {
        if !self.open_orders.is_empty() {
            self.selected_order_idx = 0;
        }
    }

    fn scroll_orders_to_end(&mut self) {
        let len = self.open_orders.len().min(MAX_ORDER_RECORDS);
        if len > 0 {
            self.selected_order_idx = len - 1;
        }
    }

    fn leverage_for_event(&self, event: &TradeEvent) -> Option<f64> {
        match event {
            TradeEvent::Order(response) => {
                self.leverage_for_inst(&response.inst_id, response.pos_side.as_deref())
            }
            TradeEvent::Cancel(cancel) => {
                self.leverage_for_inst(&cancel.inst_id, cancel.pos_side.as_deref())
            }
            TradeEvent::Fill(fill) => {
                self.leverage_for_inst(&fill.inst_id, fill.pos_side.as_deref())
            }
        }
    }

    fn leverage_for_inst(&self, inst_id: &str, pos_side: Option<&str>) -> Option<f64> {
        if let Some(side) = pos_side {
            let side_lower = side.to_ascii_lowercase();
            for position in &self.positions {
                if !position.inst_id.eq_ignore_ascii_case(inst_id) {
                    continue;
                }
                if position
                    .pos_side
                    .as_deref()
                    .map(|value| value.eq_ignore_ascii_case(&side_lower))
                    .unwrap_or(false)
                {
                    return position.lever;
                }
            }
            return None;
        }
        for position in &self.positions {
            if !position.inst_id.eq_ignore_ascii_case(inst_id) {
                continue;
            }
            return position.lever;
        }
        if let Some(market) = self.markets.get(inst_id) {
            return Some(market.lever);
        }
        None
    }

    fn update_markets(&mut self, markets: HashMap<String, MarketInfo>) {
        self.markets = markets;
    }

    fn selected_log_display_index(&self) -> usize {
        if self.logs.is_empty() {
            0
        } else {
            let idx = self.selected_log_idx.min(self.logs.len() - 1);
            self.logs.len().saturating_sub(1) - idx
        }
    }

    fn selected_log_entry(&self) -> Option<&TradeLogEntry> {
        if self.logs.is_empty() {
            None
        } else {
            let idx = self.selected_log_idx.min(self.logs.len().saturating_sub(1));
            self.logs.get(idx)
        }
    }

    fn toggle_log_detail(&mut self) {
        if let Some(active) = &self.log_detail {
            if let Some(selected) = self.selected_log_entry() {
                if active == selected {
                    self.log_detail = None;
                    return;
                }
            } else {
                self.log_detail = None;
                return;
            }
        }
        if let Some(entry) = self.selected_log_entry().cloned() {
            self.log_detail = Some(entry);
            self.ai_detail = None;
        }
    }
}

impl OrderInputState {
    fn active_value_mut(&mut self) -> &mut String {
        match self.active_field {
            OrderInputField::Price => &mut self.price,
            OrderInputField::Size => &mut self.size,
            OrderInputField::Leverage => &mut self.leverage,
        }
    }

    fn focus_next_field(&mut self) {
        self.active_field = match self.active_field {
            OrderInputField::Price => OrderInputField::Size,
            OrderInputField::Size => OrderInputField::Leverage,
            OrderInputField::Leverage => OrderInputField::Price,
        };
    }

    fn focus_prev_field(&mut self) {
        self.active_field = match self.active_field {
            OrderInputField::Price => OrderInputField::Leverage,
            OrderInputField::Size => OrderInputField::Price,
            OrderInputField::Leverage => OrderInputField::Size,
        };
    }
}

impl OrderIntent {
    fn title_prefix(&self) -> &'static str {
        match self {
            OrderIntent::Manual => "",
            OrderIntent::TakeProfit => "止盈 ",
            OrderIntent::StopLoss => "止损 ",
            OrderIntent::Modify => "修改 ",
        }
    }

    fn action_label(&self) -> &'static str {
        match self {
            OrderIntent::Manual => "下单",
            OrderIntent::TakeProfit => "止盈",
            OrderIntent::StopLoss => "止损",
            OrderIntent::Modify => "改单",
        }
    }
}

pub struct TuiApp {
    inst_ids: Vec<String>,
    colors: HashMap<String, Color>,
    data: HashMap<String, Vec<(f64, f64)>>,
    window: [f64; 2],
    last_draw: Instant,
    min_redraw_gap: Duration,
    retention: Duration,
    latest_prices: HashMap<String, f64>,
    price_precision: HashMap<String, usize>,
    last_update: Option<String>,
    status_message: Option<String>,
    status_visible_until: Option<Instant>,
    status_is_error: bool,
    normalize: bool,
    y_zoom: f64,
    multi_axis: bool,
    view_mode: ViewMode,
    trade: TradeState,
    exit_confirmation: bool,
    loading_overlay: Option<LoadingOverlay>,
    timezone: ConfiguredTimeZone,
}
impl TuiApp {
    fn is_exit_key(key: &KeyEvent) -> bool {
        matches!(
            key.code,
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
        ) || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    pub fn new(
        inst_ids: &[String],
        retention: Duration,
        markets: HashMap<String, MarketInfo>,
        order_tx: Option<mpsc::Sender<TradingCommand>>,
        ai_enabled: bool,
        wait_for_markets: bool,
        timezone: ConfiguredTimeZone,
    ) -> TuiApp {
        let min_redraw_gap = Duration::from_millis(100);
        let inst_ids = if inst_ids.is_empty() {
            vec!["BTC-USDT-SWAP".to_string()]
        } else {
            inst_ids.to_vec()
        };
        let mut data = HashMap::new();
        let mut colors = HashMap::new();
        for (idx, inst_id) in inst_ids.iter().enumerate() {
            data.insert(inst_id.clone(), Vec::new());
            colors.insert(inst_id.clone(), COLOR_PALETTE[idx % COLOR_PALETTE.len()]);
        }
        let log_store = TradeLogStore::new(TradeLogStore::default_path());
        let ai_store = if ai_enabled {
            Some(AiDecisionStore::new(AiDecisionStore::default_path()))
        } else {
            None
        };
        let loading_overlay = if wait_for_markets {
            Some(LoadingOverlay::new("正在加载交易品种数据...", true))
        } else {
            None
        };
        TuiApp {
            inst_ids,
            colors,
            data,
            window: [0.0, 100.0],
            last_draw: Instant::now() - min_redraw_gap,
            min_redraw_gap,
            retention,
            latest_prices: HashMap::new(),
            price_precision: HashMap::new(),
            last_update: None,
            status_message: None,
            status_visible_until: None,
            status_is_error: false,
            normalize: false,
            y_zoom: 1.0,
            multi_axis: false,
            view_mode: ViewMode::Chart,
            trade: TradeState::new(order_tx, Some(log_store), ai_store, markets, ai_enabled),
            exit_confirmation: false,
            loading_overlay,
            timezone,
        }
    }

    fn set_status_message(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
        self.status_visible_until = Some(Instant::now() + Duration::from_secs(3));
        self.status_is_error = false;
    }

    fn set_error_status_message(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
        self.status_visible_until = Some(Instant::now() + Duration::from_secs(5));
        self.status_is_error = true;
    }

    fn clear_status_message(&mut self) {
        self.status_message = None;
        self.status_visible_until = None;
        self.status_is_error = false;
    }

    fn clear_status_if_allowed(&mut self) {
        if let Some(visible_until) = self.status_visible_until {
            if Instant::now() < visible_until {
                return;
            }
        }
        self.status_message = None;
        self.status_visible_until = None;
        self.status_is_error = false;
    }

    fn loading_blocks_input(&self) -> bool {
        self.loading_overlay
            .as_ref()
            .map(|overlay| overlay.blocks_input())
            .unwrap_or(false)
    }

    fn update_loading_overlay_animation(&mut self) -> bool {
        if let Some(overlay) = &mut self.loading_overlay {
            return overlay.tick();
        }
        false
    }

    fn finish_market_loading(&mut self, loaded: bool) {
        self.loading_overlay = None;
        if loaded {
            self.set_status_message("币种数据加载完成，可开始操作");
        } else {
            self.set_error_status_message("未能加载币种数据，部分功能可能受限");
        }
    }

    pub fn dispose(&self) {
        ratatui::restore();
    }

    pub fn preload_history(&mut self, points: &[PricePoint]) {
        self.load_history(points);
    }

    pub fn preload_trade_logs(&mut self) {
        if let Err(err) = self.trade.load_persisted_logs() {
            self.set_error_status_message(format!("加载历史委托记录失败: {err}"));
        }
    }

    pub fn preload_ai_insights(&mut self) {
        if let Err(err) = self.trade.load_persisted_ai_insights() {
            self.set_error_status_message(format!("加载历史 AI 决策失败: {err}"));
        }
    }

    pub async fn run(
        &mut self,
        rx: &mut broadcast::Receiver<Command>,
        exit_rx: &mut broadcast::Receiver<()>,
    ) -> Result<()> {
        color_eyre::install()?;
        let mut terminal = ratatui::init();
        let mut input_tick = tokio::time::interval(self.min_redraw_gap);
        terminal.draw(|frame| self.render(frame))?;
        self.last_draw = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = input_tick.tick() => {
                    let mut should_redraw = false;
                    if self.update_loading_overlay_animation() {
                        should_redraw = true;
                    }
                    if self.poll_input()? {
                        return Ok(());
                    }
                    if should_redraw && self.last_draw.elapsed() >= self.min_redraw_gap {
                        terminal.draw(|frame| self.render(frame))?;
                        self.last_draw = Instant::now();
                    }
                }
                result = rx.recv() => {
                    match result {
                        Ok(Command::MarkPriceUpdate(inst_id, mark_px, ts, precision)) => {
                            self.clear_status_if_allowed();
                            self.on_tick(&inst_id, mark_px, ts, precision);
                            if self.last_draw.elapsed() >= self.min_redraw_gap {
                                terminal.draw(|frame| self.render(frame))?;
                                self.last_draw = Instant::now();
                            }
                        }
                        Ok(Command::Notify(inst_id, message)) => {
                            self.set_status_message(format!("{inst_id}: {message}"));
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Ok(Command::AiInsight(record)) => {
                            if let Err(err) = self.trade.push_ai_insight(record) {
                                self.set_error_status_message(format!(
                                    "记录 AI 决策失败: {err}"
                                ));
                            } else {
                                let summary = self
                                    .trade
                                    .latest_ai_summary()
                                    .unwrap_or_else(|| "收到新的 AI 决策".to_string());
                                self.status_message = Some(format!("Deepseek: {summary}"));
                                self.status_visible_until =
                                    Some(Instant::now() + Duration::from_secs(15));
                                self.status_is_error = false;
                            }
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Ok(Command::Error(message)) => {
                            self.set_error_status_message(message);
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Ok(Command::TradeResult(event)) => {
                            let (message, is_error) = match &event {
                                TradeEvent::Order(response) => {
                                    (response.message.to_string(), !response.success)
                                }
                                TradeEvent::Cancel(cancel) => {
                                    if cancel.success {
                                        self.trade.remove_open_order(&cancel.ord_id);
                                    }
                                    (cancel.message.to_string(), !cancel.success)
                                }
                                TradeEvent::Fill(fill) => {
                                    let size_label =
                                        self.format_contract_size(&fill.inst_id, fill.size);
                                    let price_label =
                                        self.format_price_for(&fill.inst_id, fill.price);
                                    let side_label = match fill.side {
                                        TradeSide::Buy => "买入",
                                        TradeSide::Sell => "卖出",
                                    };
                                    (
                                        format!(
                                            "{inst} {side} 成交 {size} @ {price}",
                                            inst = fill.inst_id,
                                            side = side_label,
                                            size = size_label,
                                            price = price_label,
                                        ),
                                        false,
                                    )
                                }
                            };
                            let event_for_log = event.clone();
                            if let Err(err) = self.trade.record_result(event_for_log) {
                                self.set_error_status_message(format!(
                                    "记录委托日志失败: {err}"
                                ));
                            }
                            if is_error {
                                self.set_error_status_message(message);
                            } else {
                                self.set_status_message(message);
                            }
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Ok(Command::AccountSnapshot(snapshot)) => {
                            self.trade.update_snapshot(snapshot, &self.inst_ids);
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Ok(Command::MarketsLoaded(markets)) => {
                            let has_data = !markets.is_empty();
                            self.trade.update_markets(markets);
                            self.finish_market_loading(has_data);
                            terminal.draw(|frame| self.render(frame))?;
                            self.last_draw = Instant::now();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
                signal = exit_rx.recv() => match signal {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Ok(())
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
        Ok(())
    }
    fn load_history(&mut self, points: &[PricePoint]) {
        if points.is_empty() {
            return;
        }
        let mut sorted = points.to_vec();
        sorted.sort_by_key(|point| point.ts);
        for point in sorted {
            self.on_tick(&point.inst_id, point.mark_px, point.ts, point.precision);
        }
        self.set_status_message(format!("Loaded {} historical points", points.len()));
    }
    fn on_tick(&mut self, inst_id: &str, mark_px: f64, ts: i64, precision: usize) {
        if !self.inst_ids.iter().any(|id| id == inst_id) {
            self.inst_ids.push(inst_id.to_string());
        }
        if !self.colors.contains_key(inst_id) {
            let idx = self.colors.len();
            self.colors.insert(
                inst_id.to_string(),
                COLOR_PALETTE[idx % COLOR_PALETTE.len()],
            );
        }
        let x = ts as f64;
        let retention_ms = self.retention.as_millis() as i64;
        let cutoff = (ts - retention_ms).max(0) as f64;
        let series = self
            .data
            .entry(inst_id.to_string())
            .or_insert_with(Vec::new);
        series.push((x, mark_px));
        series.retain(|(timestamp, _)| *timestamp >= cutoff);
        self.latest_prices.insert(inst_id.to_string(), mark_px);
        self.update_precision(inst_id, precision);
        self.last_update = Some(inst_id.to_string());
        self.trade.ensure_selection(&self.inst_ids);
        self.update_window();
    }
    fn render(&mut self, frame: &mut Frame) {
        match self.view_mode {
            ViewMode::Chart => self.render_chart_view(frame),
            ViewMode::Trade => self.render_trade_view(frame),
        }
        if self.exit_confirmation {
            self.render_exit_confirmation(frame);
        }
        if self.loading_overlay.is_some() {
            self.render_loading_overlay(frame);
        }
    }

    fn render_chart_view(&self, frame: &mut Frame) {
        let area = frame.area();
        if self.status_message.is_some() && area.height >= 4 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(3)])
                .split(area);
            self.render_chart(frame, chunks[0]);
            self.render_status(frame, chunks[1]);
        } else {
            self.render_chart(frame, area);
            if self.status_message.is_some() {
                self.render_status(frame, area);
            }
        }
    }

    fn render_trade_view(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let has_status = self.status_message.is_some() && area.height >= 6;
        let (main_area, status_area) = if has_status {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(3)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };
        self.render_trade_panel(frame, main_area);
        if let Some(status_area) = status_area {
            self.render_status(frame, status_area);
        } else if self.status_message.is_some() {
            self.render_status(frame, main_area);
        }
        if let Some(input) = &self.trade.input {
            self.render_order_dialog(frame, main_area, input);
        }
        if let Some(detail) = self.trade.ai_detail.clone() {
            self.render_ai_detail(frame, main_area, &detail);
        } else if let Some(detail) = &self.trade.log_detail {
            self.render_log_detail(frame, main_area, detail);
        }
    }

    fn render_exit_confirmation(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 24 || area.height < 5 {
            return;
        }
        let popup_width = area.width.saturating_sub(20).min(50).max(28);
        let popup_height = 6;
        let left = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let top = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let popup = Rect::new(left, top, popup_width, popup_height);
        let lines = vec![
            Line::from(Span::styled(
                "确定要退出交易终端？",
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Y/Enter 确认退出 · N/Esc 取消"),
            Line::from("再次按 q/Q 也可确认 · Ctrl+C 立即退出"),
        ];
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(Block::bordered().title("确认退出"));
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn render_loading_overlay(&self, frame: &mut Frame) {
        if let Some(overlay) = &self.loading_overlay {
            let area = frame.area();
            if area.width < 24 || area.height < 5 {
                return;
            }
            let popup_width = area.width.saturating_sub(10).min(60).max(30);
            let popup_height = 6;
            let left = area.x + (area.width.saturating_sub(popup_width)) / 2;
            let top = area.y + (area.height.saturating_sub(popup_height)) / 2;
            let popup = Rect::new(left, top, popup_width, popup_height);
            let message = format!("{} {}", overlay.spinner(), overlay.message());
            let lines = vec![
                Line::from(Span::styled(
                    message,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("正在同步 OKX 合约信息，加载完成前仅支持退出 (Q/Esc)"),
            ];
            let paragraph = Paragraph::new(lines)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: true })
                .block(Block::bordered().title("初始化中"));
            frame.render_widget(Clear, popup);
            frame.render_widget(paragraph, popup);
        }
    }

    fn render_trade_panel(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 4 || area.width < 20 {
            return;
        }
        let instruction_lines = self.trade_instruction_lines();
        let balance_lines = self.trade.balance_lines();
        let header_height =
            Self::trade_header_height(instruction_lines.len() + balance_lines.len());
        if area.height < header_height {
            return;
        }
        let show_snapshot = area.height >= header_height + 6;
        if show_snapshot {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(header_height),
                    Constraint::Length(14),
                    Constraint::Min(4),
                ])
                .split(area);
            self.render_trade_header(frame, chunks[0], &instruction_lines, &balance_lines);
            self.render_account_snapshot(frame, chunks[1]);
            self.render_trade_activity(frame, chunks[2]);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(header_height), Constraint::Min(4)])
                .split(area);
            self.render_trade_header(frame, chunks[0], &instruction_lines, &balance_lines);
            self.render_trade_activity(frame, chunks[1]);
        }
    }

    fn render_account_snapshot(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 3 || area.width < 10 {
            return;
        }
        let columns = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        self.render_positions_panel(frame, columns[0]);
        self.render_open_orders_panel(frame, columns[1]);
    }

    fn section_block(&self, title: &str, focus: TradeFocus) -> Block<'static> {
        let mut label = title.to_string();
        if self.trade.focus == focus {
            label.push_str(" *");
        }
        Block::bordered()
            .title(label)
            .border_style(self.focus_border_style(focus))
    }

    fn focus_border_style(&self, focus: TradeFocus) -> Style {
        if self.trade.focus == focus {
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    }

    fn render_positions_panel(&mut self, frame: &mut Frame, area: Rect) {
        let block = self.section_block("Positions", TradeFocus::Positions);
        if area.height < 3 {
            frame.render_widget(block, area);
            self.trade.set_position_view_height(1);
            return;
        }
        let mut lines = Vec::new();
        let inner_height = area.height.saturating_sub(2) as usize;
        let list_visible = inner_height.saturating_sub(1);
        let page_height = list_visible.max(1);
        self.trade
            .set_position_view_height(page_height.min(u16::MAX as usize) as u16);
        let total = self.trade.positions.len();
        let display_len = total.min(MAX_POSITION_RECORDS);
        if display_len == 0 {
            lines.push(Line::from("无持仓"));
        } else if list_visible == 0 {
            lines.push(Line::from("窗口高度不足，无法显示持仓"));
        } else {
            lines.push(Line::from(format_columns(&[
                ("序号", ColumnAlign::Right, 4),
                ("建仓", ColumnAlign::Left, 10),
                ("合约", ColumnAlign::Left, 14),
                ("方向", ColumnAlign::Left, 4),
                ("数量", ColumnAlign::Right, 12),
                ("均价", ColumnAlign::Right, 12),
                ("标记价", ColumnAlign::Right, 12),
                ("杠杆", ColumnAlign::Right, 8),
                ("保证金", ColumnAlign::Right, 12),
                ("盈亏", ColumnAlign::Right, 12),
                ("盈亏%", ColumnAlign::Right, 10),
            ])));
            let selected_idx = clamp_index(self.trade.selected_position_idx, display_len);
            let (start, end) = visible_range(display_len, list_visible, selected_idx);
            for (idx, position) in self
                .trade
                .positions
                .iter()
                .take(display_len)
                .enumerate()
                .skip(start)
                .take(end.saturating_sub(start))
            {
                let side_label = Self::pos_side_label(position.pos_side.as_deref());
                let avg_label = position
                    .avg_px
                    .map(|value| self.format_price_for(&position.inst_id, value))
                    .unwrap_or_else(|| "--".to_string());
                let mark_label = self
                    .latest_prices
                    .get(&position.inst_id)
                    .copied()
                    .map(|value| self.format_price_for(&position.inst_id, value))
                    .unwrap_or_else(|| "--".to_string());
                let size_label = self.format_contract_size(&position.inst_id, position.size);
                let lever_label = Self::leverage_label(position.lever);
                let imr_label = Self::format_imr(position.imr);
                let pnl_value = self.position_pnl(position);
                let pnl_label = pnl_value
                    .map(Self::format_pnl)
                    .unwrap_or_else(|| "--".to_string());
                let pnl_ratio_label = self
                    .position_pnl_ratio(position)
                    .map(Self::format_pnl_ratio)
                    .unwrap_or_else(|| "--".to_string());
                let ordinal_label = format!("{}", idx + 1);
                let time_label = self.snapshot_time_label(position.create_time);
                let row = format_columns(&[
                    (ordinal_label.as_str(), ColumnAlign::Right, 4),
                    (time_label.as_str(), ColumnAlign::Left, 10),
                    (position.inst_id.as_str(), ColumnAlign::Left, 14),
                    (side_label, ColumnAlign::Left, 4),
                    (size_label.as_str(), ColumnAlign::Right, 12),
                    (avg_label.as_str(), ColumnAlign::Right, 12),
                    (mark_label.as_str(), ColumnAlign::Right, 12),
                    (lever_label.as_str(), ColumnAlign::Right, 8),
                    (imr_label.as_str(), ColumnAlign::Right, 12),
                    (pnl_label.as_str(), ColumnAlign::Right, 12),
                    (pnl_ratio_label.as_str(), ColumnAlign::Right, 10),
                ]);
                let selected = idx == selected_idx && self.trade.focus == TradeFocus::Positions;
                lines.push(Line::styled(row, row_style(selected)));
            }
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn position_pnl(&self, position: &PositionInfo) -> Option<f64> {
        if let Some(upl) = position.upl {
            return Some(upl);
        }
        let avg = position.avg_px?;
        let current = self.latest_prices.get(&position.inst_id).copied()?;
        let signed_size = Self::signed_position_size(position);
        Some((current - avg) * signed_size)
    }

    fn position_pnl_ratio(&self, position: &PositionInfo) -> Option<f64> {
        if let Some(ratio) = position.upl_ratio {
            return Some(ratio);
        }
        let pnl = self.position_pnl(position)?;
        let avg = position.avg_px?;
        let notional = avg * position.size.abs();
        if notional.abs() < f64::EPSILON {
            None
        } else {
            Some(pnl / notional)
        }
    }

    fn signed_position_size(position: &PositionInfo) -> f64 {
        match position.pos_side.as_deref() {
            Some("long") => position.size.abs(),
            Some("short") => -position.size.abs(),
            _ => position.size,
        }
    }

    fn render_open_orders_panel(&mut self, frame: &mut Frame, area: Rect) {
        let block = self.section_block("Open Orders", TradeFocus::Orders);
        if area.height < 3 {
            frame.render_widget(block, area);
            self.trade.set_order_view_height(1);
            return;
        }
        let mut lines = Vec::new();
        let inner_height = area.height.saturating_sub(2) as usize;
        let list_visible = inner_height.saturating_sub(1);
        let page_height = list_visible.max(1);
        self.trade
            .set_order_view_height(page_height.min(u16::MAX as usize) as u16);
        let total = self.trade.open_orders.len();
        let display_len = total.min(MAX_ORDER_RECORDS);
        if display_len == 0 {
            lines.push(Line::from("无挂单"));
        } else if list_visible == 0 {
            lines.push(Line::from("窗口高度不足，无法显示挂单"));
        } else {
            lines.push(Line::from(format_columns(&[
                ("序号", ColumnAlign::Right, 4),
                ("创建", ColumnAlign::Left, 10),
                ("合约", ColumnAlign::Left, 14),
                ("方向", ColumnAlign::Left, 10),
                ("类型", ColumnAlign::Left, 10),
                ("数量", ColumnAlign::Right, 10),
                ("价格", ColumnAlign::Right, 10),
                ("杠杆", ColumnAlign::Right, 8),
                ("状态", ColumnAlign::Left, 8),
                ("订单", ColumnAlign::Left, 12),
            ])));
            let selected_idx = clamp_index(self.trade.selected_order_idx, display_len);
            let (start, end) = visible_range(display_len, list_visible, selected_idx);
            for (idx, order) in self
                .trade
                .open_orders
                .iter()
                .take(display_len)
                .enumerate()
                .skip(start)
                .take(end.saturating_sub(start))
            {
                let side_label = Self::order_side_label(&order.side, order.pos_side.as_deref());
                let intent_label = self.order_intent_label(order);
                let price_label = if let Some(trigger) = order.trigger_price {
                    let trigger_text = self.format_price_for(&order.inst_id, trigger);
                    match order.price {
                        Some(ord_px) if (ord_px - trigger).abs() > f64::EPSILON => {
                            let ord_text = self.format_price_for(&order.inst_id, ord_px);
                            format!("{}->{}", trigger_text, ord_text)
                        }
                        _ => trigger_text,
                    }
                } else {
                    order
                        .price
                        .map(|value| self.format_price_for(&order.inst_id, value))
                        .unwrap_or_else(|| "--".to_string())
                };
                let size_label = self.format_contract_size(&order.inst_id, order.size);
                let ord_label = Self::short_order_id(&order.ord_id);
                let lever_label = Self::leverage_label(order.lever);
                let ordinal_label = format!("{}", idx + 1);
                let time_label = self.snapshot_time_label(order.create_time);
                let row = format_columns(&[
                    (ordinal_label.as_str(), ColumnAlign::Right, 4),
                    (time_label.as_str(), ColumnAlign::Left, 10),
                    (order.inst_id.as_str(), ColumnAlign::Left, 14),
                    (side_label.as_str(), ColumnAlign::Left, 10),
                    (intent_label, ColumnAlign::Left, 10),
                    (size_label.as_str(), ColumnAlign::Right, 10),
                    (price_label.as_str(), ColumnAlign::Right, 10),
                    (lever_label.as_str(), ColumnAlign::Right, 8),
                    (order.state.as_str(), ColumnAlign::Left, 8),
                    (ord_label.as_str(), ColumnAlign::Left, 12),
                ]);
                let selected = idx == selected_idx && self.trade.focus == TradeFocus::Orders;
                lines.push(Line::styled(row, row_style(selected)));
            }
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_trade_header(
        &self,
        frame: &mut Frame,
        area: Rect,
        instruction_lines: &[String],
        balance_lines: &[String],
    ) {
        let inst = self
            .trade
            .selected_inst(&self.inst_ids)
            .map(|s| s.to_string())
            .or_else(|| self.inst_ids.first().cloned())
            .unwrap_or_else(|| "N/A".to_string());
        let price = self
            .latest_prices
            .get(&inst)
            .map(|value| self.format_price_for(&inst, *value))
            .unwrap_or_else(|| "--".to_string());
        let focus_label = self.trade.focus_label();
        let lines = vec![Line::from(vec![
            Span::styled(
                "交易页面",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · "),
            Span::styled(inst.as_str(), Style::default().fg(Color::LightGreen)),
            Span::raw(" · 最新价 "),
            Span::styled(price, Style::default().fg(Color::Yellow)),
            Span::raw(" · 焦点 "),
            Span::styled(
                focus_label,
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
        ])];
        let mut lines = lines;
        lines.extend(balance_lines.iter().map(|line| Line::from(line.as_str())));
        lines.extend(
            instruction_lines
                .iter()
                .map(|line| Line::from(line.as_str())),
        );

        let block = self.section_block("Trade", TradeFocus::Instruments);
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn trade_instruction_lines(&self) -> Vec<String> {
        let mut instruction_lines = Vec::new();
        if self.trade.trading_enabled() {
            let (pos_cnt, ord_cnt) = self.trade.snapshot_counts();
            let log_cnt = self.trade.logs.len();
            let mut summary_line = format!(
                "Tab 切换 · Shift+Tab 返回 · ↑↓/j k 浏览/滚动 · 持仓 {} · 挂单 {} · 委托 {}",
                pos_cnt, ord_cnt, log_cnt
            );
            if self.trade.ai_panel_enabled() {
                summary_line.push_str(&format!(" · AI {}", self.trade.ai_insight_count()));
            }
            summary_line.push_str(" · t 返回图表");
            instruction_lines.push(summary_line);
            self.push_focus_hints(&mut instruction_lines);
        } else {
            instruction_lines
                .push("未配置 OKX API，仅显示行情 · Tab 切换 · ↑↓ 浏览 · t 返回图表".to_string());
            self.push_focus_hints(&mut instruction_lines);
        }
        instruction_lines
    }

    fn trade_header_height(line_count: usize) -> u16 {
        let content_lines = 1 + line_count;
        let needed = content_lines as u16 + 2; // account for borders
        needed.max(4)
    }

    fn focus_shortcut_hint(&self) -> Option<String> {
        if !self.trade.trading_enabled() {
            return None;
        }
        let hint = match self.trade.focus {
            TradeFocus::Instruments => "焦点 合约：↑↓/j k 选择合约 · b 买入 · s 卖出",
            TradeFocus::Positions => "焦点 持仓：↑↓/j k 选择持仓 · p 止盈 · l 止损",
            TradeFocus::Orders => "焦点 挂单：↑↓/j k 选择挂单 · c 撤单 · r 改单",
            TradeFocus::AiInsights => {
                "焦点 AI 决策：↑↓/j k 浏览 · PageUp/PageDown 翻页 · Home/End 顶/底 · o 查看原始提示"
            }
            TradeFocus::Logs => {
                "焦点 委托记录：↑↓/j k 选择 · PageUp/PageDown 翻页 · Home/End 顶/底 · o 详情"
            }
        };
        Some(hint.to_string())
    }

    fn push_focus_hints(&self, lines: &mut Vec<String>) {
        if let Some(focus_hint) = self.focus_shortcut_hint() {
            lines.push(focus_hint);
        }
    }

    fn render_trade_activity(&mut self, frame: &mut Frame, area: Rect) {
        let show_ai_panel = self.trade.ai_panel_enabled();
        if !show_ai_panel {
            self.render_trade_logs(frame, area);
            return;
        }
        let min_logs_height: u16 = 4;
        if area.height <= min_logs_height + 1 {
            self.render_trade_logs(frame, area);
            return;
        }
        let mut ai_height = area.height / 3;
        if ai_height < 4 {
            ai_height = 4;
        }
        if ai_height > 10 {
            ai_height = 10;
        }
        let available_for_ai = area.height.saturating_sub(min_logs_height);
        if available_for_ai < 3 {
            self.render_trade_logs(frame, area);
            return;
        }
        ai_height = ai_height.min(available_for_ai);
        if ai_height < 3 {
            self.render_trade_logs(frame, area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(ai_height),
                Constraint::Min(min_logs_height),
            ])
            .split(area);
        self.render_ai_panel(frame, chunks[0]);
        self.render_trade_logs(frame, chunks[1]);
    }

    fn render_trade_logs(&mut self, frame: &mut Frame, area: Rect) {
        let log_count = self.trade.logs.len();
        let mut lines = Vec::new();
        let inner_height = area.height.saturating_sub(2) as usize;
        let list_visible = inner_height.saturating_sub(1);
        if log_count == 0 {
            if self.trade.trading_enabled() {
                lines.push(Line::from("暂无委托，按 b/s 提交订单"));
            } else {
                lines.push(Line::from("未配置 OKX API，无法下单"));
            }
        } else if list_visible == 0 {
            lines.push(Line::from("窗口高度不足，无法显示委托记录"));
        } else {
            lines.push(Line::from(format_columns(&[
                ("序号", ColumnAlign::Right, 5),
                ("时间", ColumnAlign::Left, 8),
                ("类型", ColumnAlign::Left, 4),
                ("合约", ColumnAlign::Left, 14),
                ("方向/单号", ColumnAlign::Left, 10),
                ("数量", ColumnAlign::Right, 10),
                ("价格", ColumnAlign::Right, 10),
                ("杠杆", ColumnAlign::Right, 6),
                ("状态", ColumnAlign::Left, 6),
                ("操作者", ColumnAlign::Left, 10),
            ])));
            let log_focus = self.trade.focus == TradeFocus::Logs;
            let selected_display_idx = self.trade.selected_log_display_index();
            let (start, end) = visible_range(log_count, list_visible, selected_display_idx);
            let mut display_idx = start;
            for (offset, entry) in self
                .trade
                .logs
                .iter()
                .rev()
                .enumerate()
                .skip(start)
                .take(end.saturating_sub(start))
            {
                let highlight = log_focus && display_idx == selected_display_idx;
                let ordinal = log_count.saturating_sub(offset);
                let ordinal = if ordinal == 0 { 1 } else { ordinal };
                lines.push(self.render_log_row(entry, highlight, ordinal));
                display_idx += 1;
            }
        }
        let title = format!("Logs {log_count}/{MAX_TRADE_LOGS}");
        let mut block = Block::bordered().title(title);
        if self.trade.focus == TradeFocus::Logs {
            block = block.border_style(Style::default().fg(Color::LightMagenta));
        }
        let page_height = list_visible.max(1);
        self.trade
            .set_log_view_height(page_height.min(u16::MAX as usize) as u16);
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_ai_panel(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!("AI 决策 {}", self.trade.ai_insight_count());
        let mut block = Block::bordered().title(title);
        if self.trade.focus == TradeFocus::AiInsights {
            block = block.border_style(Style::default().fg(Color::LightMagenta));
        }
        if area.height < 3 {
            frame.render_widget(block, area);
            self.trade.set_ai_view_height(1);
            return;
        }
        let mut lines = Vec::new();
        let inner_height = area.height.saturating_sub(2) as usize;
        let list_visible = inner_height.saturating_sub(1);
        let page_height = list_visible.max(1);
        self.trade
            .set_ai_view_height(page_height.min(u16::MAX as usize) as u16);
        if list_visible == 0 {
            lines.push(Line::from("窗口高度不足，无法显示 AI 决策"));
        } else if !self.trade.ai_panel_enabled() {
            lines.push(Line::from("未启用 Deepseek"));
        } else if self.trade.ai_insight_count() == 0 {
            lines.push(Line::from("等待 Deepseek 决策 · 按 o 查看原始提示"));
        } else {
            let fixed_columns = AI_INDEX_COLUMN_WIDTH + AI_TIME_COLUMN_WIDTH;
            let spacing = 3; // four columns -> three gaps
            let total_width = usize::from(
                area.width
                    .saturating_sub((fixed_columns + spacing) as u16)
                    .max(8),
            );
            let (operation_width, summary_width) = Self::ai_panel_column_widths(total_width);
            lines.push(Line::from(format_columns(&[
                ("序号", ColumnAlign::Right, AI_INDEX_COLUMN_WIDTH),
                ("时间", ColumnAlign::Left, AI_TIME_COLUMN_WIDTH),
                ("操作", ColumnAlign::Left, operation_width),
                ("摘要", ColumnAlign::Left, summary_width),
            ])));
            let len = self.trade.ai_insight_count();
            let selected = self.trade.selected_ai_idx();
            let (start, end) = visible_range(len, list_visible, selected);
            for idx in start..end {
                if let Some(entry) = self.trade.ai_insights.get(idx) {
                    let ordinal = idx + 1;
                    let ordinal_label = ordinal.to_string();
                    let time_label = self.format_timestamp_or_default(
                        entry.timestamp_ms(),
                        "%H:%M:%S",
                        "--:--:--",
                    );
                    let operation = Self::ai_operation_label(entry);
                    let summary = entry.summary();
                    let row = format_columns(&[
                        (
                            ordinal_label.as_str(),
                            ColumnAlign::Right,
                            AI_INDEX_COLUMN_WIDTH,
                        ),
                        (time_label.as_str(), ColumnAlign::Left, AI_TIME_COLUMN_WIDTH),
                        (operation.as_str(), ColumnAlign::Left, operation_width),
                        (summary.as_str(), ColumnAlign::Left, summary_width),
                    ]);
                    let highlight = self.trade.focus == TradeFocus::AiInsights && idx == selected;
                    lines.push(Line::styled(row, row_style(highlight)));
                }
            }
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn ai_operation_label(entry: &AiDecisionRecord) -> String {
        if entry.analysis_error.is_some() {
            return "解析失败".to_string();
        }
        if entry.operations.is_empty() {
            return "未识别".to_string();
        }
        let mut labels = Vec::new();
        for op in entry.operations.iter().take(2) {
            labels.push(op.brief_label());
        }
        if entry.operations.len() > 2 {
            labels.push(format!("+{} 更多", entry.operations.len() - 2));
        }
        labels.join(" | ")
    }

    fn ai_panel_column_widths(total_width: usize) -> (usize, usize) {
        if total_width <= 16 {
            let mut operation = (total_width / 2).max(4);
            if operation >= total_width {
                operation = total_width.saturating_sub(4).max(4);
            }
            let mut summary = total_width.saturating_sub(operation).max(4);
            if operation + summary > total_width {
                let overflow = operation + summary - total_width;
                if summary > overflow {
                    summary -= overflow;
                } else if operation > overflow {
                    operation -= overflow;
                }
            }
            (operation, summary)
        } else {
            let mut operation = (total_width / 3).max(10);
            if operation + 8 > total_width {
                operation = total_width.saturating_sub(8).max(6);
            }
            let summary = total_width.saturating_sub(operation).max(8);
            (operation, summary)
        }
    }

    fn render_log_row(
        &self,
        entry: &TradeLogEntry,
        highlight: bool,
        ordinal: usize,
    ) -> Line<'static> {
        let columns = self.log_row_columns(entry, ordinal);
        let column_count = columns.len();
        let mut spans = Vec::new();
        for (idx, (value, align, width, color)) in columns.into_iter().enumerate() {
            let text = format_column_value(&value, align, width);
            let mut style = Style::default();
            if let Some(color) = color {
                style = style.fg(color);
            }
            if highlight {
                style = style.bg(Color::LightCyan).add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(text, style));
            if idx + 1 != column_count {
                let mut spacer_style = Style::default();
                if highlight {
                    spacer_style = spacer_style
                        .bg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD);
                }
                spans.push(Span::styled(" ".to_string(), spacer_style));
            }
        }
        Line::from(spans)
    }

    fn log_row_columns(
        &self,
        entry: &TradeLogEntry,
        ordinal: usize,
    ) -> Vec<(String, ColumnAlign, usize, Option<Color>)> {
        let time = self.format_timestamp_or_default(entry.timestamp_ms(), "%H:%M:%S", "--:--:--");
        let leverage_label = Self::leverage_label(entry.leverage);
        let ordinal_label = ordinal.to_string();
        match &entry.event {
            TradeEvent::Order(response) => {
                let mut side_label = Self::side_short_label(response.side).to_string();
                let size_label = self.format_contract_size(&response.inst_id, response.size);
                let price_label = self.format_price_for(&response.inst_id, response.price);
                let status_color = Self::status_color(response.success);
                let order_type = Self::order_kind_label(response.kind);
                if let Some(kind_label) = order_type {
                    side_label = format!("{}({})", side_label, kind_label);
                }
                vec![
                    (ordinal_label, ColumnAlign::Right, 5, None),
                    (time, ColumnAlign::Left, 8, None),
                    ("委托".to_string(), ColumnAlign::Left, 4, None),
                    (response.inst_id.clone(), ColumnAlign::Left, 14, None),
                    (side_label, ColumnAlign::Left, 10, None),
                    (size_label, ColumnAlign::Right, 10, None),
                    (price_label, ColumnAlign::Right, 10, None),
                    (leverage_label, ColumnAlign::Right, 6, None),
                    (
                        Self::status_label(response.success).to_string(),
                        ColumnAlign::Left,
                        6,
                        Some(status_color),
                    ),
                    (
                        Self::operator_label(&response.operator),
                        ColumnAlign::Left,
                        10,
                        None,
                    ),
                ]
            }
            TradeEvent::Cancel(cancel) => {
                let ord_short = Self::short_order_id(&cancel.ord_id);
                let status_color = Self::status_color(cancel.success);
                vec![
                    (ordinal_label, ColumnAlign::Right, 5, None),
                    (time, ColumnAlign::Left, 8, None),
                    ("撤单".to_string(), ColumnAlign::Left, 4, None),
                    (cancel.inst_id.clone(), ColumnAlign::Left, 14, None),
                    (ord_short, ColumnAlign::Left, 10, None),
                    ("--".to_string(), ColumnAlign::Right, 10, None),
                    ("--".to_string(), ColumnAlign::Right, 10, None),
                    (leverage_label, ColumnAlign::Right, 6, None),
                    (
                        Self::status_label(cancel.success).to_string(),
                        ColumnAlign::Left,
                        6,
                        Some(status_color),
                    ),
                    (
                        Self::operator_label(&cancel.operator),
                        ColumnAlign::Left,
                        10,
                        None,
                    ),
                ]
            }
            TradeEvent::Fill(fill) => {
                let side_label =
                    Self::order_side_label(fill.side.as_okx_side(), fill.pos_side.as_deref());
                let size_label = self.format_contract_size(&fill.inst_id, fill.size);
                let price_label = self.format_price_for(&fill.inst_id, fill.price);
                let exec_label = Self::exec_type_label(fill.exec_type.as_deref());
                let order_short = Self::short_order_id(&fill.order_id);
                vec![
                    (ordinal_label, ColumnAlign::Right, 5, None),
                    (time, ColumnAlign::Left, 8, None),
                    ("成交".to_string(), ColumnAlign::Left, 4, None),
                    (fill.inst_id.clone(), ColumnAlign::Left, 14, None),
                    (side_label, ColumnAlign::Left, 10, None),
                    (size_label, ColumnAlign::Right, 10, None),
                    (price_label, ColumnAlign::Right, 10, None),
                    (leverage_label, ColumnAlign::Right, 6, None),
                    (exec_label.to_string(), ColumnAlign::Left, 6, None),
                    (order_short, ColumnAlign::Left, 10, None),
                ]
            }
        }
    }

    fn operator_label(operator: &TradeOperator) -> String {
        operator.label()
    }

    fn side_label(side: TradeSide) -> &'static str {
        match side {
            TradeSide::Buy => "买入",
            TradeSide::Sell => "卖出",
        }
    }

    fn parse_order_side(value: &str) -> Option<TradeSide> {
        if value.eq_ignore_ascii_case("buy") {
            Some(TradeSide::Buy)
        } else if value.eq_ignore_ascii_case("sell") {
            Some(TradeSide::Sell)
        } else {
            None
        }
    }

    fn side_short_label(side: TradeSide) -> &'static str {
        match side {
            TradeSide::Buy => "买",
            TradeSide::Sell => "卖",
        }
    }

    fn status_color(success: bool) -> Color {
        if success {
            Color::LightGreen
        } else {
            Color::LightRed
        }
    }

    fn status_label(success: bool) -> &'static str {
        if success { "成功" } else { "失败" }
    }

    fn order_kind_label(kind: TradeOrderKind) -> Option<&'static str> {
        match kind {
            TradeOrderKind::TakeProfit => Some("止盈"),
            TradeOrderKind::StopLoss => Some("止损"),
            TradeOrderKind::Regular => None,
        }
    }

    fn exec_type_label(exec_type: Option<&str>) -> &'static str {
        match exec_type {
            Some(value)
                if value.eq_ignore_ascii_case("m") || value.eq_ignore_ascii_case("maker") =>
            {
                "挂单"
            }
            Some(value)
                if value.eq_ignore_ascii_case("t") || value.eq_ignore_ascii_case("taker") =>
            {
                "吃单"
            }
            _ => "--",
        }
    }

    fn pos_side_label(side: Option<&str>) -> &'static str {
        match side {
            Some("long") => "多",
            Some("short") => "空",
            Some("net") => "净",
            _ => "--",
        }
    }

    fn order_side_label(side: &str, pos_side: Option<&str>) -> String {
        let base = match side {
            "buy" => "买".to_string(),
            "sell" => "卖".to_string(),
            other => other.to_string(),
        };
        match pos_side {
            Some("long") => format!("{}(多)", base),
            Some("short") => format!("{}(空)", base),
            Some("net") => format!("{}(净)", base),
            _ => base,
        }
    }

    fn order_intent_label(&self, order: &PendingOrderInfo) -> &'static str {
        match order.kind {
            TradeOrderKind::TakeProfit => return "止盈",
            TradeOrderKind::StopLoss => return "止损",
            TradeOrderKind::Regular => {}
        }
        if let Some(label) = Self::tagged_order_intent(order) {
            return label;
        }
        if order.reduce_only {
            self.heuristic_reduce_only_intent(order)
                .unwrap_or("止盈/止损")
        } else {
            "限价开仓"
        }
    }

    fn tagged_order_intent(order: &PendingOrderInfo) -> Option<&'static str> {
        let tag = order.tag.as_deref()?.trim();
        if tag.is_empty() {
            return None;
        }
        let lowered = tag.to_ascii_lowercase();
        match lowered.as_str() {
            "tp" | "takeprofit" | "take_profit" | "take-profit" | "dstp" => Some("止盈"),
            "sl" | "stoploss" | "stop_loss" | "stop-loss" | "dssl" => Some("止损"),
            _ => None,
        }
    }

    fn heuristic_reduce_only_intent(&self, order: &PendingOrderInfo) -> Option<&'static str> {
        let price = order.trigger_price.or(order.price)?;
        let position = self.position_for_order(order)?;
        let avg = position.avg_px?;
        let side = order.side.to_ascii_lowercase();
        if side == "sell" {
            if price >= avg {
                Some("止盈")
            } else {
                Some("止损")
            }
        } else if side == "buy" {
            if price <= avg {
                Some("止盈")
            } else {
                Some("止损")
            }
        } else {
            None
        }
    }

    fn position_for_order(&self, order: &PendingOrderInfo) -> Option<&PositionInfo> {
        self.trade
            .positions
            .iter()
            .find(|position| Self::position_matches_order(order, position))
    }

    fn position_matches_order(order: &PendingOrderInfo, position: &PositionInfo) -> bool {
        if position.inst_id != order.inst_id {
            return false;
        }
        match (&order.pos_side, &position.pos_side) {
            (Some(ord_side), Some(pos_side)) => ord_side.eq_ignore_ascii_case(pos_side.as_str()),
            (Some(ord_side), None) => Self::side_matches_size(ord_side.as_str(), position.size),
            (None, Some(pos_side)) => {
                Self::order_side_matches_position(order.side.as_str(), pos_side.as_str())
            }
            (None, None) => true,
        }
    }

    fn side_matches_size(ord_side: &str, size: f64) -> bool {
        let lowered = ord_side.to_ascii_lowercase();
        match lowered.as_str() {
            "long" => size > 0.0,
            "short" => size < 0.0,
            "net" => size != 0.0,
            _ => true,
        }
    }

    fn order_side_matches_position(order_side: &str, position_side: &str) -> bool {
        let ord = order_side.to_ascii_lowercase();
        let pos = position_side.to_ascii_lowercase();
        match pos.as_str() {
            "long" => ord == "sell",
            "short" => ord == "buy",
            "net" => true,
            _ => true,
        }
    }

    fn leverage_label(lever: Option<f64>) -> String {
        match lever {
            Some(value) => {
                let frac = value.fract().abs();
                if frac < 1e-6 {
                    format!("{value:.0}x")
                } else if value.abs() >= 10.0 {
                    format!("{value:.1}x")
                } else {
                    format!("{value:.2}x")
                }
            }
            None => "--".to_string(),
        }
    }

    fn closing_side_for_position(position: &PositionInfo) -> TradeSide {
        match position.pos_side.as_deref() {
            Some("long") => TradeSide::Sell,
            Some("short") => TradeSide::Buy,
            Some("net") => {
                if position.size < 0.0 {
                    TradeSide::Buy
                } else {
                    TradeSide::Sell
                }
            }
            _ => {
                if position.size < 0.0 {
                    TradeSide::Buy
                } else {
                    TradeSide::Sell
                }
            }
        }
    }

    fn pos_side_for_position(position: &PositionInfo) -> Option<String> {
        if let Some(pos_side) = &position.pos_side {
            if matches!(pos_side.as_str(), "long" | "short" | "net") {
                return Some(pos_side.clone());
            }
        }
        if position.size > 0.0 {
            Some("long".to_string())
        } else if position.size < 0.0 {
            Some("short".to_string())
        } else {
            None
        }
    }

    fn short_order_id(ord_id: &str) -> String {
        const MAX_CHARS: usize = 12;
        let total = ord_id.chars().count();
        if total <= MAX_CHARS {
            return ord_id.to_string();
        }
        let tail_len = MAX_CHARS.saturating_sub(1);
        let tail = ord_id
            .chars()
            .skip(total.saturating_sub(tail_len))
            .collect::<String>();
        format!("…{}", tail)
    }

    fn render_order_dialog(&self, frame: &mut Frame, area: Rect, input: &OrderInputState) {
        if area.width < 20 || area.height < 6 {
            return;
        }
        let popup_width = area.width.saturating_sub(10).min(60).max(30);
        let popup_height = area.height.min(8).max(6);
        let left = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let top = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let popup = Rect::new(left, top, popup_width, popup_height);
        let block = Block::bordered().title(format!(
            "{}{} {}",
            input.intent.title_prefix(),
            Self::side_label(input.side),
            input.inst_id.as_str()
        ));
        let price_span = self.order_field_span(
            "价格",
            &input.price,
            input.active_field == OrderInputField::Price,
        );
        let size_span = self.order_field_span(
            "数量",
            &input.size,
            input.active_field == OrderInputField::Size,
        );
        let leverage_span = self.order_field_span(
            "杠杆(x)",
            &input.leverage,
            input.active_field == OrderInputField::Leverage,
        );
        let mut lines = vec![
            Line::from(vec![
                Span::raw("合约 "),
                Span::styled(
                    input.inst_id.as_str(),
                    Style::default().fg(Color::LightCyan),
                ),
            ]),
            price_span,
            size_span,
            leverage_span,
        ];
        if let Some(ord_id) = &input.replace_order_id {
            lines.push(Line::from(vec![
                Span::raw("原单 "),
                Span::styled(
                    Self::short_order_id(ord_id),
                    Style::default().fg(Color::LightMagenta),
                ),
            ]));
        }
        lines.push(Line::from(format!(
            "Enter 提交{} · Esc 取消 · Tab/Shift+Tab 切换字段",
            input.intent.action_label()
        )));
        if let Some(err) = &input.error {
            lines.push(Line::from(Span::styled(
                err.as_str(),
                Style::default().fg(Color::LightRed),
            )));
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn render_ai_detail(&mut self, frame: &mut Frame, area: Rect, entry: &AiDecisionRecord) {
        if area.width < 36 || area.height < 10 {
            return;
        }
        let popup_width = area.width.saturating_sub(8).min(90).max(44);
        let popup_height = area.height.saturating_sub(6).min(24).max(12);
        let left = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let top = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let popup = Rect::new(left, top, popup_width, popup_height);
        let timestamp =
            self.format_timestamp_or_default(entry.timestamp_ms(), "%Y-%m-%d %H:%M:%S", "--");
        let mut lines = vec![Line::from(format!("时间 {timestamp}"))];
        lines.push(Line::from(Span::styled(
            "[User Snapshot]",
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        )));
        if entry.user_prompt.trim().is_empty() {
            lines.push(Line::from("--"));
        } else {
            for line in entry.user_prompt.lines() {
                lines.push(Line::from(line));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[Decision]",
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(error) = entry.analysis_error.as_deref() {
            lines.push(Line::from(Span::styled(
                format!("解析失败: {error}"),
                Style::default().fg(Color::LightRed),
            )));
        } else if entry.operations.is_empty() {
            lines.push(Line::from("未解析到决策操作"));
        } else {
            for (idx, operation) in entry.operations.iter().enumerate() {
                if entry.operations.len() > 1 {
                    lines.push(Line::from(format!(
                        "操作 {} · {}",
                        idx + 1,
                        operation.brief_label()
                    )));
                }
                for line in operation.detail_lines() {
                    lines.push(Line::from(line));
                }
                if idx + 1 < entry.operations.len() {
                    lines.push(Line::from(""));
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[AI Response]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        if entry.response.trim().is_empty() {
            lines.push(Line::from("--"));
        } else {
            for line in entry.response.lines() {
                lines.push(Line::from(line));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[Justification]",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(justification) = entry.justification.as_deref() {
            if justification.trim().is_empty() {
                lines.push(Line::from("--"));
            } else {
                for line in justification.lines() {
                    lines.push(Line::from(line));
                }
            }
        } else {
            lines.push(Line::from("未解析到 justification 字段"));
        }
        lines.push(Line::from(""));
        lines.push(Line::from("按 o 关闭 · 原文仅供参考"));
        let content_height = popup_height.saturating_sub(2);
        let content_width = popup_width.saturating_sub(2).max(1);
        let total_rows = Self::wrapped_line_count(&lines, content_width);
        self.trade.update_ai_detail_view(content_height, total_rows);
        let scroll = self
            .trade
            .ai_detail_scroll
            .min(self.trade.ai_detail_max_scroll().min(u16::MAX as usize) as u16);
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(
                Block::bordered()
                    .title("Deepseek 提示详情")
                    .border_style(Style::default().fg(Color::LightMagenta)),
            );
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn render_log_detail(&self, frame: &mut Frame, area: Rect, entry: &TradeLogEntry) {
        if area.width < 30 || area.height < 8 {
            return;
        }
        let popup_width = area.width.saturating_sub(10).min(72).max(40);
        let popup_height = area.height.min(14).max(8);
        let left = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let top = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let popup = Rect::new(left, top, popup_width, popup_height);
        let timestamp =
            self.format_timestamp_or_default(entry.timestamp_ms(), "%Y-%m-%d %H:%M:%S", "--");
        let mut lines = vec![Line::from(format!("时间 {timestamp}"))];
        let (title, status_success, message) = match &entry.event {
            TradeEvent::Order(response) => {
                lines.push(Line::from(format!("合约 {}", response.inst_id)));
                lines.push(Line::from(format!(
                    "方向 {} · 数量 {}",
                    Self::side_label(response.side),
                    self.format_contract_size(&response.inst_id, response.size),
                )));
                if let Some(kind_label) = Self::order_kind_label(response.kind) {
                    lines.push(Line::from(format!("类型 {kind_label}委托")));
                }
                lines.push(Line::from(format!(
                    "价格 {}",
                    self.format_price_for(&response.inst_id, response.price),
                )));
                if let Some(ord_id) = &response.order_id {
                    lines.push(Line::from(format!("订单ID {}", ord_id)));
                }
                lines.push(Line::from(format!(
                    "操作者 {}",
                    Self::operator_label(&response.operator)
                )));
                ("委托详情", response.success, response.message.clone())
            }
            TradeEvent::Cancel(cancel) => {
                lines.push(Line::from(format!("合约 {}", cancel.inst_id)));
                lines.push(Line::from(format!(
                    "订单 {}",
                    Self::short_order_id(&cancel.ord_id)
                )));
                lines.push(Line::from(format!(
                    "操作者 {}",
                    Self::operator_label(&cancel.operator)
                )));
                ("撤单详情", cancel.success, cancel.message.clone())
            }
            TradeEvent::Fill(fill) => {
                lines.push(Line::from(format!("合约 {}", fill.inst_id.clone())));
                lines.push(Line::from(format!(
                    "方向 {} · 数量 {}",
                    Self::side_label(fill.side),
                    self.format_contract_size(&fill.inst_id, fill.size),
                )));
                lines.push(Line::from(format!(
                    "价格 {}",
                    self.format_price_for(&fill.inst_id, fill.price),
                )));
                lines.push(Line::from(format!(
                    "成交类型 {}",
                    Self::exec_type_label(fill.exec_type.as_deref()),
                )));
                lines.push(Line::from(format!(
                    "订单 {}",
                    Self::short_order_id(&fill.order_id)
                )));
                if let Some(trade_id) = &fill.trade_id {
                    lines.push(Line::from(format!("成交ID {}", trade_id)));
                }
                if let Some(acc) = fill.acc_fill_size {
                    lines.push(Line::from(format!(
                        "累计成交 {}",
                        self.format_contract_size(&fill.inst_id, acc),
                    )));
                }
                if let Some(avg_price) = fill.avg_price {
                    lines.push(Line::from(format!(
                        "均价 {}",
                        self.format_price_for(&fill.inst_id, avg_price),
                    )));
                }
                if let Some(tag) = fill.tag.as_deref().filter(|tag| !tag.is_empty()) {
                    lines.push(Line::from(format!("标签 {}", tag)));
                }
                if let Some(fee) = fill.fee {
                    let ccy = fill.fee_currency.as_deref().unwrap_or("--");
                    lines.push(Line::from(format!("手续费 {} {}", fee, ccy)));
                }
                if let Some(pnl) = fill.pnl {
                    lines.push(Line::from(format!("PNL {}", pnl)));
                }
                (
                    "成交详情",
                    true,
                    format!(
                        "{inst} {side} 成交 {size} @ {price}",
                        inst = fill.inst_id,
                        side = Self::side_label(fill.side),
                        size = self.format_contract_size(&fill.inst_id, fill.size),
                        price = self.format_price_for(&fill.inst_id, fill.price),
                    ),
                )
            }
        };
        lines.push(Line::from(format!(
            "杠杆 {}",
            Self::leverage_label(entry.leverage)
        )));
        let status_color = Self::status_color(status_success);
        let status_label = Self::status_label(status_success);
        lines.push(Line::from(vec![
            Span::raw("状态 "),
            Span::styled(status_label, Style::default().fg(status_color)),
        ]));
        if !message.is_empty() {
            lines.push(Line::from(Span::styled(
                message.as_str(),
                Style::default().fg(status_color),
            )));
        }
        lines.push(Line::from("o 关闭详情 · ↑↓/PageUp/PageDown 浏览记录"));
        let block = Block::bordered().title(title);
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn wrapped_line_count(lines: &[Line], max_width: u16) -> usize {
        if max_width == 0 {
            return lines.len();
        }
        let width = max_width as usize;
        lines
            .iter()
            .map(|line| {
                let mut line_width = 0;
                for span in line.spans.iter() {
                    line_width += span.content.width();
                }
                if line_width == 0 {
                    1
                } else {
                    (line_width + width - 1) / width
                }
            })
            .sum()
    }

    fn order_field_span(&self, label: &str, value: &str, active: bool) -> Line<'static> {
        let mut spans = vec![Span::raw(format!("{label} "))];
        let mut style = Style::default().fg(Color::White);
        if active {
            style = style
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        spans.push(Span::styled(
            if value.is_empty() {
                "<空>".to_string()
            } else {
                value.to_string()
            },
            style,
        ));
        Line::from(spans)
    }

    fn leverage_input_value(value: f64) -> String {
        let mut formatted = format!("{value:.4}");
        if let Some(dot_pos) = formatted.find('.') {
            let mut trim_idx = formatted.len();
            while trim_idx > dot_pos + 1 && formatted.as_bytes()[trim_idx - 1] == b'0' {
                trim_idx -= 1;
            }
            if trim_idx == dot_pos + 1 {
                trim_idx -= 1;
            }
            formatted.truncate(trim_idx);
        }
        if formatted == "-0" {
            formatted = "0".to_string();
        }
        formatted
    }
    fn render_chart(&self, frame: &mut Frame, area: Rect) {
        let multi_axis_active = self.multi_axis_active();
        let x_mid = f64::midpoint(self.window[0], self.window[1]);
        let x_labels = vec![
            Span::styled(
                self.format_timestamp_label(self.window[0]),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.format_timestamp_label(x_mid)),
            Span::styled(
                self.format_timestamp_label(self.window[1]),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let mut views: Vec<(&str, Cow<'_, [(f64, f64)]>, Color)> = Vec::new();
        let mut axis_infos: Vec<AxisInfo> = Vec::new();
        let mut price_entries: Vec<PricePanelEntry> = Vec::new();
        let mut raw_min_y = f64::INFINITY;
        let mut raw_max_y = f64::NEG_INFINITY;
        for inst_id in &self.inst_ids {
            let color = self.color_for(inst_id);
            let source = self
                .data
                .get(inst_id)
                .map(|series| series.as_slice())
                .unwrap_or(EMPTY_SERIES);
            let (points, axis_info) = self.series_view(inst_id, source, multi_axis_active, color);
            if let Some(info) = axis_info {
                axis_infos.push(info);
            }
            if self.normalize {
                if let Some(price) = self.latest_prices.get(inst_id).copied() {
                    price_entries.push(PricePanelEntry {
                        inst_id: inst_id.clone(),
                        color,
                        price,
                        change_pct: self.normalized_latest_value(inst_id),
                    });
                }
            }
            for (_, y) in points.iter() {
                if y.is_finite() {
                    raw_min_y = raw_min_y.min(*y);
                    raw_max_y = raw_max_y.max(*y);
                }
            }
            views.push((inst_id.as_str(), points, color));
        }
        if views.is_empty() {
            views.push(("N/A", Cow::Borrowed(EMPTY_SERIES), Color::White));
        }
        let (label_min_y, label_max_y, bounds_min_y, bounds_max_y) =
            if !raw_min_y.is_finite() || !raw_max_y.is_finite() {
                (0.0, 1.0, 0.0, 1.0)
            } else if (raw_max_y - raw_min_y).abs() < f64::EPSILON {
                let padding = (raw_max_y.abs() * 0.05).max(1.0);
                (
                    raw_min_y,
                    raw_max_y,
                    raw_min_y - padding,
                    raw_max_y + padding,
                )
            } else {
                let padding = (raw_max_y - raw_min_y) * 0.05;
                (
                    raw_min_y,
                    raw_max_y,
                    raw_min_y - padding,
                    raw_max_y + padding,
                )
            };
        let y_mid = f64::midpoint(label_min_y, label_max_y);
        let y_labels = vec![
            Span::styled(
                self.format_axis_value(label_min_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.format_axis_value(y_mid)),
            Span::styled(
                self.format_axis_value(label_max_y),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let x_bounds = if self.window[0] < self.window[1] {
            self.window
        } else if (self.window[0] - self.window[1]).abs() < f64::EPSILON {
            [self.window[0] - 1.0, self.window[1] + 1.0]
        } else {
            [self.window[1], self.window[0]]
        };
        let y_bounds = self.apply_y_zoom(bounds_min_y, bounds_max_y);
        let datasets: Vec<Dataset> = views
            .iter()
            .map(|(inst_id, points, color)| {
                let d = Dataset::default();
                if !(self.normalize || multi_axis_active) {
                    d.name(self.legend_label(inst_id))
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::default().fg(*color))
                        .data(points.as_ref())
                } else {
                    d.marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::default().fg(*color))
                        .data(points.as_ref())
                }
            })
            .collect();
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(self.chart_title_line()))
            .x_axis(
                Axis::default()
                    .title("Time")
                    .style(Style::default().fg(Color::Gray))
                    .labels(x_labels)
                    .labels_alignment(Alignment::Left)
                    .bounds(x_bounds),
            )
            .y_axis(
                Axis::default()
                    .title(self.axis_title())
                    .style(Style::default().fg(Color::Gray))
                    .labels(y_labels)
                    .bounds(y_bounds),
            );

        enum PanelKind {
            MultiAxis,
            Price,
        }
        let panel_kind = if multi_axis_active && !axis_infos.is_empty() {
            Some(PanelKind::MultiAxis)
        } else if self.normalize && !price_entries.is_empty() {
            Some(PanelKind::Price)
        } else {
            None
        };
        let (chart_area, axis_area) = if panel_kind.is_some() && area.width > 20 {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(20)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        frame.render_widget(chart, chart_area);
        if let (Some(axis_area), Some(panel_kind)) = (axis_area, panel_kind) {
            match panel_kind {
                PanelKind::MultiAxis => self.render_multi_axis(frame, axis_area, &axis_infos),
                PanelKind::Price => self.render_price_panel(frame, axis_area, &price_entries),
            }
        }
    }
    fn chart_title_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            self.chart_title_text(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        for badge in self.mode_badges() {
            spans.push(Span::raw(" "));
            spans.push(badge);
        }
        Line::from(spans)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        if self.exit_confirmation {
            return self.handle_exit_confirmation_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char('c') = key.code {
                self.prompt_exit_confirmation();
                return Ok(false);
            }
        }
        if self.loading_blocks_input() && !Self::is_exit_key(&key) {
            self.set_status_message("币种数据正在加载，完成后即可操作. 按 Q/Esc 可退出");
            return Ok(false);
        }
        if self.trade.input.is_some() {
            self.handle_order_input_key(key);
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                self.prompt_exit_confirmation();
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.view_mode = match self.view_mode {
                    ViewMode::Chart => {
                        self.set_status_message("进入交易页面 (T)");
                        ViewMode::Trade
                    }
                    ViewMode::Trade => {
                        self.set_status_message("返回图表页面 (T)");
                        ViewMode::Chart
                    }
                };
            }
            _ => match self.view_mode {
                ViewMode::Chart => self.handle_chart_key(key),
                ViewMode::Trade => self.handle_trade_key(key),
            },
        }
        Ok(false)
    }

    fn prompt_exit_confirmation(&mut self) {
        if self.exit_confirmation {
            return;
        }
        self.exit_confirmation = true;
        self.set_status_message("确认退出？Y/Enter 确认 · N/Esc 取消");
    }

    fn handle_exit_confirmation_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char('c') = key.code {
                self.exit_confirmation = false;
                return Ok(true);
            }
        }
        match key.code {
            KeyCode::Char('y')
            | KeyCode::Char('Y')
            | KeyCode::Char('q')
            | KeyCode::Char('Q')
            | KeyCode::Enter => {
                self.exit_confirmation = false;
                Ok(true)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.exit_confirmation = false;
                self.set_status_message("已取消退出");
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn handle_chart_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.normalize = !self.normalize;
                self.y_zoom = 1.0;
                self.set_status_message(match (self.normalize, self.multi_axis) {
                    (true, true) => {
                        "Relative change mode enabled; multi Y resumes once you exit (N)"
                            .to_string()
                    }
                    (true, false) => "Relative change mode enabled (N)".to_string(),
                    (false, true) => {
                        "Absolute price mode enabled; multi Y restored (N)".to_string()
                    }
                    (false, false) => "Absolute price mode enabled (N)".to_string(),
                });
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                self.multi_axis = !self.multi_axis;
                self.y_zoom = 1.0;
                self.set_status_message(match (self.multi_axis, self.normalize) {
                    (true, true) => {
                        "Multi Y axis pre-enabled; activates once you leave relative mode (M)"
                            .to_string()
                    }
                    (true, false) => "Multi Y axis mode enabled (M)".to_string(),
                    (false, true) => {
                        "Multi Y axis disabled; still in relative mode (M)".to_string()
                    }
                    (false, false) => "Multi Y axis mode disabled (M)".to_string(),
                });
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.y_zoom = (self.y_zoom * 1.25).min(100.0);
                self.set_status_message(format!("Zoomed in Y axis (Zoom {:.2}x)", self.y_zoom));
            }
            KeyCode::Char('-') => {
                self.y_zoom = (self.y_zoom / 1.25).max(0.05);
                self.set_status_message(format!("Zoomed out Y axis (Zoom {:.2}x)", self.y_zoom));
            }
            KeyCode::Char('0') => {
                self.y_zoom = 1.0;
                self.set_status_message("Reset Y axis (0)");
            }
            _ => {}
        }
    }

    fn handle_trade_key(&mut self, key: KeyEvent) {
        if self.trade.ai_detail_active() {
            match key.code {
                KeyCode::Char('o') | KeyCode::Char('O') | KeyCode::Esc => {
                    self.trade.toggle_ai_detail();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.trade.scroll_ai_detail(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.trade.scroll_ai_detail(-1);
                }
                KeyCode::PageDown => {
                    let step = self.trade.ai_detail_view_height.max(1) as i16;
                    self.trade.scroll_ai_detail(step);
                }
                KeyCode::PageUp => {
                    let step = self.trade.ai_detail_view_height.max(1) as i16;
                    self.trade.scroll_ai_detail(-step);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Tab => {
                self.trade.cycle_focus(false);
            }
            KeyCode::BackTab => {
                self.trade.cycle_focus(true);
            }
            KeyCode::Up => {
                self.trade.move_focus(&self.inst_ids, -1);
            }
            KeyCode::Down => {
                self.trade.move_focus(&self.inst_ids, 1);
            }
            KeyCode::Char('j') => {
                self.trade.move_focus(&self.inst_ids, 1);
            }
            KeyCode::Char('k') => {
                self.trade.move_focus(&self.inst_ids, -1);
            }
            KeyCode::Char('b') | KeyCode::Char('B') => {
                self.start_order_entry(TradeSide::Buy);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.start_order_entry(TradeSide::Sell);
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                if self.trade.focus == TradeFocus::Positions {
                    self.start_position_close(OrderIntent::TakeProfit);
                }
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                if self.trade.focus == TradeFocus::Positions {
                    self.start_position_close(OrderIntent::StopLoss);
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                if self.trade.focus == TradeFocus::Orders {
                    self.cancel_selected_order();
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if self.trade.focus == TradeFocus::Orders {
                    self.start_order_replace();
                }
            }
            KeyCode::Char('o') | KeyCode::Char('O') => match self.trade.focus {
                TradeFocus::Logs => self.trade.toggle_log_detail(),
                TradeFocus::AiInsights => self.trade.toggle_ai_detail(),
                _ => {}
            },
            KeyCode::PageUp => match self.trade.focus {
                TradeFocus::Positions => self.trade.page_scroll_positions(-1),
                TradeFocus::Orders => self.trade.page_scroll_orders(-1),
                TradeFocus::Logs => self.trade.page_scroll_logs(-1),
                TradeFocus::AiInsights => self.trade.page_scroll_ai(-1),
                TradeFocus::Instruments => {}
            },
            KeyCode::PageDown => match self.trade.focus {
                TradeFocus::Positions => self.trade.page_scroll_positions(1),
                TradeFocus::Orders => self.trade.page_scroll_orders(1),
                TradeFocus::Logs => self.trade.page_scroll_logs(1),
                TradeFocus::AiInsights => self.trade.page_scroll_ai(1),
                TradeFocus::Instruments => {}
            },
            KeyCode::Home => match self.trade.focus {
                TradeFocus::Positions => self.trade.scroll_positions_to_start(),
                TradeFocus::Orders => self.trade.scroll_orders_to_start(),
                TradeFocus::Logs => self.trade.scroll_logs_to_start(),
                TradeFocus::AiInsights => self.trade.scroll_ai_to_start(),
                TradeFocus::Instruments => {}
            },
            KeyCode::End => match self.trade.focus {
                TradeFocus::Positions => self.trade.scroll_positions_to_end(),
                TradeFocus::Orders => self.trade.scroll_orders_to_end(),
                TradeFocus::Logs => self.trade.scroll_logs_to_end(),
                TradeFocus::AiInsights => self.trade.scroll_ai_to_end(),
                TradeFocus::Instruments => {}
            },
            _ => {}
        }
    }

    fn start_order_entry(&mut self, side: TradeSide) {
        if !self.trade.trading_enabled() {
            self.set_error_status_message("未配置 OKX API，无法下单");
            return;
        }
        if self.inst_ids.is_empty() {
            self.set_error_status_message("暂无可交易的合约");
            return;
        }
        self.trade.ensure_selection(&self.inst_ids);
        let inst_idx = self
            .trade
            .selected_inst_idx
            .min(self.inst_ids.len().saturating_sub(1));
        let inst_id = self
            .inst_ids
            .get(inst_idx)
            .cloned()
            .unwrap_or_else(|| "BTC-USDT-SWAP".to_string());
        let leverage = self.trade.leverage_for_inst(&inst_id, None);
        let price = self
            .latest_prices
            .get(&inst_id)
            .map(|value| self.format_price_for(&inst_id, *value))
            .unwrap_or_else(|| "".to_string());
        self.open_order_dialog(
            inst_id,
            side,
            price,
            "1".to_string(),
            None,
            OrderIntent::Manual,
            TradeOrderKind::Regular,
            false,
            None,
            None,
            leverage,
        );
    }

    fn start_position_close(&mut self, intent: OrderIntent) {
        if !self.trade.trading_enabled() {
            self.set_error_status_message("未配置 OKX API，无法下单");
            return;
        }
        let position = match self.trade.selected_position() {
            Some(position) => position.clone(),
            None => {
                self.set_error_status_message("当前无可操作的持仓");
                return;
            }
        };
        let side = Self::closing_side_for_position(&position);
        let inst_id = position.inst_id.clone();
        let price = self
            .latest_prices
            .get(&inst_id)
            .map(|value| self.format_price_for(&inst_id, *value))
            .unwrap_or_else(|| "".to_string());
        let size = self.format_contract_size(&position.inst_id, position.size.abs());
        let pos_side = Self::pos_side_for_position(&position);
        let tag = match intent {
            OrderIntent::TakeProfit => Some("tp".to_string()),
            OrderIntent::StopLoss => Some("sl".to_string()),
            _ => None,
        };
        let leverage = position.lever;
        self.open_order_dialog(
            inst_id,
            side,
            price,
            size,
            pos_side,
            intent,
            match intent {
                OrderIntent::TakeProfit => TradeOrderKind::TakeProfit,
                OrderIntent::StopLoss => TradeOrderKind::StopLoss,
                _ => TradeOrderKind::Regular,
            },
            true,
            tag,
            None,
            leverage,
        );
    }

    fn start_order_replace(&mut self) {
        if !self.trade.trading_enabled() {
            self.set_error_status_message("未配置 OKX API，无法改单");
            return;
        }
        if self.trade.focus != TradeFocus::Orders {
            self.set_error_status_message("请先切换焦点到挂单列表");
            return;
        }
        let order = match self.trade.selected_order() {
            Some(order) => order.clone(),
            None => {
                self.set_error_status_message("当前无挂单可改单");
                return;
            }
        };
        let side = match Self::parse_order_side(order.side.as_str()) {
            Some(side) => side,
            None => {
                self.set_error_status_message("无法识别挂单方向，暂不支持改单");
                return;
            }
        };
        let price = order
            .trigger_price
            .or(order.price)
            .map(|value| self.format_price_for(&order.inst_id, value))
            .or_else(|| {
                self.latest_prices
                    .get(&order.inst_id)
                    .map(|value| self.format_price_for(&order.inst_id, *value))
            })
            .unwrap_or_default();
        let size = self.format_contract_size(&order.inst_id, order.size.abs());
        let leverage = order.lever.or_else(|| {
            self.trade
                .leverage_for_inst(&order.inst_id, order.pos_side.as_deref())
        });
        self.open_order_dialog(
            order.inst_id.clone(),
            side,
            price,
            size,
            order.pos_side.clone(),
            OrderIntent::Modify,
            order.kind,
            order.reduce_only,
            order.tag.clone(),
            Some(order.ord_id.clone()),
            leverage,
        );
    }

    fn cancel_selected_order(&mut self) {
        if !self.trade.trading_enabled() {
            self.set_error_status_message("未配置 OKX API，无法撤单");
            return;
        }
        let order = match self.trade.selected_order() {
            Some(order) => order.clone(),
            None => {
                self.set_error_status_message("当前无挂单可撤");
                return;
            }
        };
        let sender = match self.trade.order_sender() {
            Some(sender) => sender,
            None => {
                self.set_error_status_message("交易通道不可用");
                return;
            }
        };
        let request = TradingCommand::Cancel(CancelOrderRequest {
            inst_id: order.inst_id.clone(),
            ord_id: order.ord_id.clone(),
            operator: TradeOperator::Manual,
            pos_side: order.pos_side.clone(),
            kind: order.kind,
        });
        match sender.try_send(request) {
            Ok(_) => {
                self.set_status_message(format!(
                    "已提交撤单请求 {}",
                    Self::short_order_id(&order.ord_id)
                ));
            }
            Err(TrySendError::Closed(_)) => {
                self.set_error_status_message("交易通道已关闭");
            }
            Err(TrySendError::Full(_)) => {
                self.set_error_status_message("交易请求过多，请稍后再试");
            }
        }
    }

    fn open_order_dialog(
        &mut self,
        inst_id: String,
        side: TradeSide,
        price: String,
        size: String,
        pos_side: Option<String>,
        intent: OrderIntent,
        order_kind: TradeOrderKind,
        reduce_only: bool,
        tag: Option<String>,
        replace_order_id: Option<String>,
        initial_leverage: Option<f64>,
    ) {
        let leverage_value = initial_leverage
            .map(Self::leverage_input_value)
            .unwrap_or_default();
        self.trade.input = Some(OrderInputState {
            side,
            inst_id: inst_id.clone(),
            price,
            size,
            leverage: leverage_value,
            initial_leverage,
            active_field: OrderInputField::Price,
            error: None,
            pos_side,
            intent,
            order_kind,
            reduce_only,
            tag,
            replace_order_id,
        });
        self.clear_status_message();
    }

    fn handle_order_input_key(&mut self, key: KeyEvent) {
        if let Some(input) = self.trade.input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.trade.input = None;
                    self.set_status_message("已取消下单");
                }
                KeyCode::Enter => {
                    self.finalize_order_input();
                }
                KeyCode::Tab => {
                    input.focus_next_field();
                }
                KeyCode::BackTab => {
                    input.focus_prev_field();
                }
                KeyCode::Left => {
                    input.focus_prev_field();
                }
                KeyCode::Right => {
                    input.focus_next_field();
                }
                KeyCode::Backspace => {
                    let field = input.active_value_mut();
                    field.pop();
                }
                KeyCode::Char(c) => {
                    if c.is_ascii_digit() || c == '.' {
                        let field = input.active_value_mut();
                        if c == '.' && field.contains('.') {
                            return;
                        }
                        field.push(c);
                    }
                }
                _ => {}
            }
        }
    }

    fn finalize_order_input(&mut self) {
        let (request, intent, replace_ord_id, leverage_request) = {
            let input = match self.trade.input.as_mut() {
                Some(value) => value,
                None => return,
            };
            let order_kind = input.order_kind;

            let price = match input.price.trim().parse::<f64>() {
                Ok(value) if value > 0.0 => value,
                _ => {
                    input.error = Some("价格必须为正数".to_string());
                    return;
                }
            };
            let size = match input.size.trim().parse::<f64>() {
                Ok(value) if value > 0.0 => value,
                _ => {
                    input.error = Some("数量必须为正数".to_string());
                    return;
                }
            };
            let size = if let Some(market) = self.trade.markets.get(&input.inst_id) {
                size / market.ct_val
            } else {
                input.error = Some("无法获取合约信息，无法提交平仓单".to_string());
                return;
            };
            if let Some(position) = self.trade.positions.iter().find(|pos| {
                pos.inst_id == input.inst_id && Self::pos_side_for_position(pos) == input.pos_side
            }) {
                if size > position.size.abs() {
                    input.error = Some("平仓数量不能大于持仓数量".to_string());
                    return;
                }
            } else {
                input.error = Some("未找到对应持仓，无法提交平仓单".to_string());
                return;
            }
            let leverage_value = {
                let trimmed = input.leverage.trim();
                if trimmed.is_empty() {
                    input.initial_leverage
                } else {
                    match trimmed.parse::<f64>() {
                        Ok(value) if value > 0.0 => Some(value),
                        Ok(_) => {
                            input.error = Some("杠杆必须为正数".to_string());
                            return;
                        }
                        Err(_) => {
                            input.error = Some("杠杆格式无效".to_string());
                            return;
                        }
                    }
                }
            };
            let leverage_request = leverage_value.and_then(|value| {
                let changed = input
                    .initial_leverage
                    .map(|prev| (prev - value).abs() > LEVERAGE_EPSILON)
                    .unwrap_or(true);
                if changed {
                    Some(SetLeverageRequest {
                        inst_id: input.inst_id.clone(),
                        lever: value,
                        pos_side: input.pos_side.clone(),
                    })
                } else {
                    None
                }
            });
            (
                TradeRequest {
                    inst_id: input.inst_id.clone(),
                    side: input.side,
                    price,
                    size,
                    pos_side: input.pos_side.clone(),
                    reduce_only: input.reduce_only,
                    tag: input.tag.clone(),
                    operator: TradeOperator::Manual,
                    leverage: leverage_value,
                    kind: order_kind,
                },
                input.intent,
                input.replace_order_id.clone(),
                leverage_request,
            )
        };
        self.trade.input = None;
        if let Some(tx) = self.trade.order_sender() {
            if let Some(leverage_req) = leverage_request {
                match tx.try_send(TradingCommand::SetLeverage(leverage_req)) {
                    Ok(_) => {}
                    Err(TrySendError::Closed(_)) => {
                        self.set_error_status_message("交易通道已关闭，无法调整杠杆");
                        return;
                    }
                    Err(TrySendError::Full(_)) => {
                        self.set_error_status_message("交易请求繁忙，请稍候再试 (调杠杆)");
                        return;
                    }
                }
            }
            if let Some(ord_id) = replace_ord_id {
                let cancel_request = TradingCommand::Cancel(CancelOrderRequest {
                    inst_id: request.inst_id.clone(),
                    ord_id,
                    operator: TradeOperator::Manual,
                    pos_side: request.pos_side.clone(),
                    kind: request.kind,
                });
                match tx.try_send(cancel_request) {
                    Ok(_) => {}
                    Err(TrySendError::Full(_)) => {
                        self.set_error_status_message("交易请求繁忙，请稍候再试 (改单取消)");
                        return;
                    }
                    Err(TrySendError::Closed(_)) => {
                        self.set_error_status_message("交易通道已关闭，无法提交改单请求");
                        return;
                    }
                }
            }
            match tx.try_send(TradingCommand::Place(request.clone())) {
                Ok(_) => {
                    let price_fmt = self.format_price_for(&request.inst_id, request.price);
                    let size_fmt = self.format_contract_size(&request.inst_id, request.size);
                    self.set_status_message(format!(
                        "{} 已发送{} {} {} @ {}",
                        intent.action_label(),
                        Self::side_label(request.side),
                        size_fmt,
                        request.inst_id,
                        price_fmt
                    ));
                }
                Err(TrySendError::Full(_)) => {
                    self.set_error_status_message("交易请求繁忙，请稍候重试");
                }
                Err(TrySendError::Closed(_)) => {
                    self.set_error_status_message("交易通道已关闭，无法下单");
                }
            }
        } else {
            self.set_error_status_message("未配置 OKX API，无法下单");
        }
    }

    fn chart_title_text(&self) -> String {
        let base = if self.normalize {
            "Mark Δ%"
        } else if self.multi_axis_active() {
            "Mark Price (Multi Y)"
        } else {
            "Mark Price"
        };
        if self.inst_ids.is_empty() {
            base.to_string()
        } else {
            format!("{base} [{}]", self.inst_ids.join(", "))
        }
    }

    fn mode_badges(&self) -> Vec<Span<'static>> {
        let mut badges = Vec::new();
        if self.normalize {
            badges.push(Span::styled(
                "[Normalized]",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if self.multi_axis {
            let (label, style) = if self.multi_axis_active() {
                (
                    "[Multi Y]",
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("[Multi Y Pending]", Style::default().fg(Color::DarkGray))
            };
            badges.push(Span::styled(label, style));
        }
        badges
    }

    fn axis_title(&self) -> String {
        if self.normalize {
            "Δ% (Relative Change)".to_string()
        } else if self.multi_axis_active() {
            "Scaled (0-1)".to_string()
        } else {
            "Mark Price".to_string()
        }
    }

    fn legend_label(&self, inst_id: &str) -> String {
        if let Some(value) = self.latest_display_value(inst_id) {
            format!("{} {}", inst_id, self.format_value(inst_id, value))
        } else {
            inst_id.to_string()
        }
    }

    fn color_for(&self, inst_id: &str) -> Color {
        self.colors.get(inst_id).copied().unwrap_or(Color::White)
    }

    fn update_window(&mut self) {
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        for series in self.data.values() {
            if let Some((first_x, _)) = series.first() {
                min_x = min_x.min(*first_x);
            }
            if let Some((last_x, _)) = series.last() {
                max_x = max_x.max(*last_x);
            }
        }
        if min_x.is_finite() && max_x.is_finite() {
            self.window = [min_x, max_x];
        } else {
            self.window = [0.0, 100.0];
        }
    }
    fn render_status(&self, frame: &mut Frame, area: Rect) {
        if let Some(message) = &self.status_message {
            let color = if self.status_is_error {
                Color::Red
            } else {
                Color::Yellow
            };
            let block = Block::bordered().title("Status");
            let status = Paragraph::new(message.as_str())
                .style(Style::default().fg(color))
                .alignment(Alignment::Left)
                .block(block);
            frame.render_widget(status, area);
        }
    }

    fn render_multi_axis(&self, frame: &mut Frame, area: Rect, infos: &[AxisInfo]) {
        if infos.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        for info in infos {
            lines.push(Line::from(vec![Span::styled(
                info.inst_id.as_str(),
                Style::default().fg(info.color).add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(format!(
                "↑ {}",
                self.format_price_for(&info.inst_id, info.max)
            )));
            lines.push(Line::from(format!(
                "• {}",
                self.format_price_for(&info.inst_id, info.mid)
            )));
            lines.push(Line::from(format!(
                "↓ {}",
                self.format_price_for(&info.inst_id, info.min)
            )));
            lines.push(Line::from(" "));
        }
        let block = Block::bordered().title("Y Axis");
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_price_panel(&self, frame: &mut Frame, area: Rect, entries: &[PricePanelEntry]) {
        if entries.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        for entry in entries {
            lines.push(Line::from(vec![Span::styled(
                entry.inst_id.as_str(),
                Style::default()
                    .fg(entry.color)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(format!(
                "Price {}",
                self.format_price_for(&entry.inst_id, entry.price)
            )));
            if let Some(change) = entry.change_pct {
                lines.push(Line::from(format!("Δ {}", self.format_percent(change))));
            }
            lines.push(Line::from(" "));
        }
        let block = Block::bordered().title("Live Prices");
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn update_precision(&mut self, inst_id: &str, precision: usize) {
        self.price_precision
            .entry(inst_id.to_string())
            .and_modify(|existing| {
                if (*existing == 0 && precision > 0) || precision > *existing {
                    *existing = precision;
                }
            })
            .or_insert(precision);
    }

    fn normalized_series(series: &[(f64, f64)]) -> Vec<(f64, f64)> {
        if series.is_empty() {
            return Vec::new();
        }
        let base = series[0].1;
        if base.abs() < f64::EPSILON {
            return Vec::new();
        }
        series
            .iter()
            .map(|(x, y)| (*x, ((*y / base) - 1.0) * 100.0))
            .collect()
    }

    fn scaled_series(series: &[(f64, f64)], min: f64, max: f64) -> Vec<(f64, f64)> {
        if series.is_empty() {
            return Vec::new();
        }
        let range = max - min;
        if range.abs() < f64::EPSILON {
            return series.iter().map(|(x, _)| (*x, 0.5)).collect();
        }
        series
            .iter()
            .map(|(x, y)| (*x, (y - min) / range))
            .collect()
    }

    fn series_bounds(series: &[(f64, f64)]) -> Option<(f64, f64)> {
        series.iter().fold(None, |acc, (_, y)| {
            if !y.is_finite() {
                acc
            } else {
                Some(match acc {
                    Some((min, max)) => (min.min(*y), max.max(*y)),
                    None => (*y, *y),
                })
            }
        })
    }

    fn series_view<'a>(
        &self,
        inst_id: &str,
        source: &'a [(f64, f64)],
        multi_axis_active: bool,
        color: Color,
    ) -> (Cow<'a, [(f64, f64)]>, Option<AxisInfo>) {
        if source.is_empty() {
            return (Cow::Borrowed(source), None);
        }
        if self.normalize {
            return (Cow::Owned(Self::normalized_series(source)), None);
        }
        if multi_axis_active {
            if let Some((min, max)) = Self::series_bounds(source) {
                let scaled = Self::scaled_series(source, min, max);
                let info = AxisInfo {
                    inst_id: inst_id.to_string(),
                    color,
                    min,
                    mid: (min + max) / 2.0,
                    max,
                };
                return (Cow::Owned(scaled), Some(info));
            }
        }
        (Cow::Borrowed(source), None)
    }

    fn latest_display_value(&self, inst_id: &str) -> Option<f64> {
        if self.normalize {
            self.normalized_latest_value(inst_id)
        } else {
            self.latest_prices.get(inst_id).copied()
        }
    }

    fn normalized_latest_value(&self, inst_id: &str) -> Option<f64> {
        let series = self.data.get(inst_id)?;
        let (_, first) = series.first()?;
        let (_, last) = series.last()?;
        if first.abs() < f64::EPSILON {
            None
        } else {
            Some(((last / first) - 1.0) * 100.0)
        }
    }

    fn price_precision_for(&self, inst_id: &str) -> usize {
        self.price_precision
            .get(inst_id)
            .copied()
            .unwrap_or_else(|| self.price_precision())
    }

    fn format_price_for(&self, inst_id: &str, value: f64) -> String {
        let precision = self.price_precision_for(inst_id);
        format!("{value:.prec$}", value = value, prec = precision)
    }

    fn format_value(&self, inst_id: &str, value: f64) -> String {
        if self.normalize {
            self.format_percent(value)
        } else {
            self.format_price_for(inst_id, value)
        }
    }

    fn format_axis_value(&self, value: f64) -> String {
        if self.normalize {
            self.format_percent(value)
        } else if self.multi_axis_active() {
            format!("{value:.2}")
        } else {
            self.format_axis_price(value)
        }
    }

    fn format_percent(&self, value: f64) -> String {
        format!("{value:+.2}%", value = value)
    }

    fn format_pnl(value: f64) -> String {
        const PNL_PRECISION: usize = 8;
        let formatted = format!("{value:+.prec$}", value = value, prec = PNL_PRECISION);
        Self::trim_formatted_number(formatted)
    }

    fn format_pnl_ratio(value: f64) -> String {
        let percent = value * 100.0;
        format!("{percent:+.2}%", percent = percent)
    }

    fn format_contract_size(&self, inst_id: &str, value: f64) -> String {
        let mut value = value;
        if let Some(mkt) = self.trade.markets.get(inst_id) {
            if mkt.ct_val > 0.0 {
                value *= mkt.ct_val;
            }
        }
        const SIZE_PRECISION: usize = 8;
        let formatted = format!("{value:.prec$}", value = value, prec = SIZE_PRECISION);
        Self::trim_formatted_number(formatted)
    }

    fn format_imr(value: f64) -> String {
        format!("{:.2}", value)
    }

    fn trim_formatted_number(mut formatted: String) -> String {
        if formatted.contains('.') {
            formatted = formatted
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string();
        }
        if formatted == "-0" || formatted == "+0" {
            formatted = "0".to_string();
        }
        formatted
    }

    fn apply_y_zoom(&self, min: f64, max: f64) -> [f64; 2] {
        if !min.is_finite() || !max.is_finite() {
            return [min, max];
        }
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        if (max - min).abs() < f64::EPSILON {
            return [min - 1.0, max + 1.0];
        }
        let center = (min + max) / 2.0;
        let half_range = (max - min) / 2.0;
        let zoom = self.y_zoom.clamp(0.05, 100.0);
        let adjusted_half = (half_range / zoom).max(half_range * 0.01);
        [center - adjusted_half, center + adjusted_half]
    }

    fn multi_axis_active(&self) -> bool {
        self.multi_axis && !self.normalize
    }

    fn poll_input(&mut self) -> Result<bool> {
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if self.handle_key_event(key)? {
                        return Ok(true);
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn snapshot_time_label(&self, timestamp: Option<i64>) -> String {
        self.format_optional_timestamp(timestamp, "%H:%M:%S", "--:--:--")
    }

    fn format_timestamp_label(&self, ts_ms: f64) -> String {
        if !ts_ms.is_finite() {
            return "--:--:--".to_string();
        }
        let rounded = ts_ms.round() as i64;
        self.format_timestamp_or_default(rounded, "%H:%M:%S", "--:--:--")
    }

    fn format_optional_timestamp(
        &self,
        timestamp: Option<i64>,
        fmt: &str,
        fallback: &str,
    ) -> String {
        timestamp
            .filter(|ts| *ts > 0)
            .and_then(|ts| self.timezone.format_timestamp(ts, fmt))
            .unwrap_or_else(|| fallback.to_string())
    }

    fn format_timestamp_or_default(&self, timestamp_ms: i64, fmt: &str, fallback: &str) -> String {
        if timestamp_ms <= 0 {
            return fallback.to_string();
        }
        self.timezone
            .format_timestamp(timestamp_ms, fmt)
            .unwrap_or_else(|| fallback.to_string())
    }

    fn price_precision(&self) -> usize {
        self.price_precision.values().copied().max().unwrap_or(2)
    }

    fn format_axis_price(&self, value: f64) -> String {
        let precision = self.price_precision();
        format!("{value:.prec$}", value = value, prec = precision)
    }
}

#[derive(Clone, Copy)]
enum ColumnAlign {
    Left,
    Right,
}

fn row_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .bg(Color::LightCyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn format_columns(columns: &[(&str, ColumnAlign, usize)]) -> String {
    let mut row = String::new();
    for (idx, (value, align, width)) in columns.iter().enumerate() {
        let clipped = clip_to_width(value, *width);
        let padded = pad_to_width(&clipped, *width, *align);
        row.push_str(&padded);
        if idx + 1 != columns.len() {
            row.push(' ');
        }
    }
    row
}

fn format_column_value(value: &str, align: ColumnAlign, width: usize) -> String {
    let clipped = clip_to_width(value, width);
    pad_to_width(&clipped, width, align)
}

fn clip_to_width(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    let mut result = String::new();
    let mut remaining = width.saturating_sub(1);
    for ch in value.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if ch_width > remaining {
            break;
        }
        result.push(ch);
        remaining = remaining.saturating_sub(ch_width);
    }
    result.push('…');
    result
}

fn pad_to_width(value: &str, width: usize, align: ColumnAlign) -> String {
    let current = UnicodeWidthStr::width(value);
    if current >= width {
        return value.to_string();
    }
    let padding = " ".repeat(width - current);
    match align {
        ColumnAlign::Left => format!("{value}{padding}"),
        ColumnAlign::Right => format!("{padding}{value}"),
    }
}

fn clamp_index(idx: usize, len: usize) -> usize {
    if len == 0 { 0 } else { idx.min(len - 1) }
}

fn visible_range(len: usize, visible: usize, selected: usize) -> (usize, usize) {
    if len == 0 || visible == 0 {
        return (0, 0);
    }
    if len <= visible {
        return (0, len);
    }
    let max_start = len - visible;
    let clamped = clamp_index(selected, len);
    let start = clamped.min(max_start);
    (start, start + visible)
}
