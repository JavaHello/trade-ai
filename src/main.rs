mod ai_log;
mod command;
mod config;
mod deepseek;
mod ai_decision;
mod ai_prompt;
mod error_log;
mod monitor;
mod notify;
mod okx;
mod okx_analytics;
mod trade_log;
mod tui;

use std::collections::HashMap;

use anyhow::anyhow;
use clap::Parser;
use color_eyre::Result;
use tokio::sync::{broadcast, mpsc};
use tokio::task;

use crate::command::{Command, TradingCommand};
use crate::deepseek::DeepseekReporter;
use crate::error_log::ErrorLogStore;
use crate::notify::OsNotification;
use crate::okx::{
    OkxBusinessWsClient, OkxPrivateWsClient, OkxTradingClient, OkxWsClient, SharedAccountState,
};
use crate::tui::TuiApp;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let param = config::CliParams::parse();
    let run_config = config::AppRunConfig::load_or_init("config.json")?;
    let run_start_timestamp_ms = run_config.start_timestamp_ms();
    let timezone = run_config.timezone();
    let deepseek_cfg = param.deepseek_config();
    let (tx, mut rx) = broadcast::channel::<Command>(16);
    let (exit_tx, _exit_rx) = broadcast::channel::<()>(1);
    {
        let mut error_rx = tx.subscribe();
        let mut exit_rx = exit_tx.subscribe();
        let error_log_store = ErrorLogStore::new(ErrorLogStore::default_path());
        task::spawn(async move {
            loop {
                tokio::select! {
                    message = error_rx.recv() => match message {
                        Ok(Command::Error(message)) => {
                            if let Err(err) = error_log_store.append_message(message) {
                                eprintln!("failed to persist error log: {err}");
                            }
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    },
                    signal = exit_rx.recv() => match signal {
                        Ok(_) | Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        });
    }
    let trading_cfg = param.trading_config();
    if deepseek_cfg.is_some() && trading_cfg.is_none() {
        let _ = tx.send(Command::Error(
            "已启用 Deepseek 集成，但缺少 OKX API 配置，无法获取账户信息".to_string(),
        ));
    }
    let order_tx = if let Some(trading_cfg) = trading_cfg.clone() {
        let (trade_tx, trade_rx) = mpsc::channel::<TradingCommand>(32);
        let trading_tx = tx.clone();
        task::spawn(async move {
            match OkxTradingClient::new(trading_cfg, trading_tx.clone()) {
                Ok(client) => {
                    if let Err(err) = client.run(trade_rx).await {
                        let _ =
                            trading_tx.send(Command::Error(format!("okx trading error: {err}")));
                    }
                }
                Err(err) => {
                    let _ =
                        trading_tx.send(Command::Error(format!("okx trading init error: {err}")));
                }
            }
        });
        Some(trade_tx)
    } else {
        None
    };
    if let Some(market_cfg) = trading_cfg.clone() {
        let inst_ids = param.inst_ids.clone();
        let td_mode = param.okx_td_mode.clone();
        let market_tx = tx.clone();
        let deepseek_cfg_for_market = deepseek_cfg.clone();
        let deepseek_inst_ids = param.inst_ids.clone();
        let deepseek_start_ms = run_start_timestamp_ms;
        let deepseek_tx = tx.clone();
        let ai_order_tx = order_tx.clone();
        let deepseek_timezone = timezone;
        let deepseek_exit_tx = exit_tx.clone();
        task::spawn(async move {
            match okx::fetch_market_info(&td_mode, &market_cfg, &inst_ids).await {
                Ok(markets) => {
                    let _ = market_tx.send(Command::MarketsLoaded(markets.clone()));
                    if let Some(cfg) = deepseek_cfg_for_market {
                        let state = SharedAccountState::global();
                        match DeepseekReporter::new(
                            cfg,
                            state,
                            deepseek_tx.clone(),
                            deepseek_inst_ids,
                            markets,
                            deepseek_start_ms,
                            ai_order_tx,
                            deepseek_timezone,
                        ) {
                            Ok(reporter) => {
                                let exit_rx = deepseek_exit_tx.subscribe();
                                let reporting_tx = deepseek_tx.clone();
                                task::spawn(async move {
                                    if let Err(err) = reporter.run(exit_rx).await {
                                        let _ = reporting_tx.send(Command::Error(format!(
                                            "Deepseek reporter error: {err}"
                                        )));
                                    }
                                });
                            }
                            Err(err) => {
                                let _ = deepseek_tx
                                    .send(Command::Error(format!("Deepseek 初始化失败: {err}")));
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = market_tx.send(Command::Error(format!("获取币种信息失败: {err}")));
                    let _ = market_tx.send(Command::MarketsLoaded(HashMap::new()));
                }
            }
        });
    }
    if let Some(snapshot_cfg) = trading_cfg.clone() {
        let inst_ids = param.inst_ids.clone();
        let snapshot_tx = tx.clone();
        task::spawn(async move {
            match okx::fetch_account_snapshot(&snapshot_cfg, &inst_ids).await {
                Ok(snapshot) => {
                    let filter = okx::inst_filter(&inst_ids);
                    let state = SharedAccountState::global();
                    state.update_filter(filter).await;
                    state.seed(&snapshot).await;
                    let _ = snapshot_tx.send(Command::AccountSnapshot(snapshot));
                }
                Err(err) => {
                    let _ = snapshot_tx.send(Command::Error(format!("okx snapshot error: {err}")));
                }
            }
        });
    }
    if let Some(private_cfg) = trading_cfg.clone() {
        let account_tx = tx.clone();
        task::spawn(async move {
            let result = async {
                let client = OkxPrivateWsClient::new(private_cfg, account_tx.clone())?;
                client.stream_account().await
            }
            .await;
            if let Err(err) = result {
                let _ = account_tx.send(Command::Error(format!("okx account ws error: {err}")));
            }
        });
    }
    if let Some(business_cfg) = trading_cfg.clone() {
        let business_tx = tx.clone();
        task::spawn(async move {
            let result = async {
                let client = OkxBusinessWsClient::new(business_cfg, business_tx.clone())?;
                client.stream_business().await
            }
            .await;
            if let Err(err) = result {
                let _ = business_tx.send(Command::Error(format!("okx business ws error: {err}")));
            }
        });
    }
    let history_window = param.history_window();
    let history_points =
        match okx::bootstrap_history(&param.inst_ids, history_window, tx.clone()).await {
            Ok(points) => points,
            Err(err) => {
                let _ = tx.send(Command::Error(format!("history bootstrap error: {err}")));
                Vec::new()
            }
        };
    let pok = param.clone();
    let ttx = tx.clone();
    task::spawn(async move {
        let result = async {
            let client = OkxWsClient::new(ttx.clone()).await?;
            client.subscribe_mark_price(&pok.inst_ids).await
        }
        .await;

        if let Err(err) = result {
            let _ = ttx.send(Command::Error(format!("okx websocket error: {err}")));
        }
    });
    let nrx = tx.subscribe();
    let notify_tx = tx.clone();
    let notify_exit_rx = exit_tx.subscribe();
    task::spawn(async move {
        let mut notifier = OsNotification::new(nrx, notify_exit_rx);
        if let Err(err) = notifier.run().await {
            let _ = notify_tx.send(Command::Error(format!("notification error: {err}")));
        }
    });
    let monitor_error_tx = tx.clone();
    let mtx = tx.clone();
    let mrx = tx.subscribe();
    let monitor_exit_rx = exit_tx.subscribe();
    let thresholds = param.threshold_map();
    task::spawn(async move {
        let mut monitor = monitor::Monitor::new(thresholds, mtx, mrx, monitor_exit_rx);
        if let Err(err) = monitor.run().await {
            let _ = monitor_error_tx.send(Command::Error(format!("monitor error: {err}")));
        }
    });

    let mut app = TuiApp::new(
        &param.inst_ids,
        history_window,
        HashMap::new(),
        order_tx.clone(),
        deepseek_cfg.is_some(),
        trading_cfg.is_some(),
        timezone,
    );
    app.preload_trade_logs();
    app.preload_ai_insights();
    if !history_points.is_empty() {
        app.preload_history(&history_points);
    }
    let mut app_exit_rx = exit_tx.subscribe();
    let app_result = tokio::select! {
        result = app.run(&mut rx, &mut app_exit_rx) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    let _ = exit_tx.send(());
    app.dispose();
    app_result.map_err(|err| anyhow!(err.to_string()))?;
    Ok(())
}
