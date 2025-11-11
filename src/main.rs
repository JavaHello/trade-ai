mod command;
mod config;
mod monitor;
mod notify;
mod okx;
mod tui;

use anyhow::anyhow;
use clap::Parser;
use color_eyre::Result;
use tokio::sync::{broadcast, mpsc};
use tokio::task;

use crate::command::{Command, TradingCommand};
use crate::notify::OsNotification;
use crate::okx::{OkxPrivateWsClient, OkxTradingClient, OkxWsClient};
use crate::tui::TuiApp;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let param = config::CliParams::parse();
    let (tx, mut rx) = broadcast::channel::<Command>(16);
    let trading_cfg = param.trading_config();
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
    if let Some(snapshot_cfg) = trading_cfg.clone() {
        let inst_ids = param.inst_ids.clone();
        let snapshot_tx = tx.clone();
        task::spawn(async move {
            match okx::fetch_account_snapshot(&snapshot_cfg, &inst_ids).await {
                Ok(snapshot) => {
                    let _ = snapshot_tx.send(Command::AccountSnapshot(snapshot));
                }
                Err(err) => {
                    let _ = snapshot_tx.send(Command::Error(format!("okx snapshot error: {err}")));
                }
            }
        });
    }
    if let Some(private_cfg) = trading_cfg.clone() {
        let inst_ids = param.inst_ids.clone();
        let account_tx = tx.clone();
        task::spawn(async move {
            let result = async {
                let client = OkxPrivateWsClient::new(private_cfg, account_tx.clone())?;
                client.stream_account(&inst_ids).await
            }
            .await;
            if let Err(err) = result {
                let _ = account_tx.send(Command::Error(format!("okx account ws error: {err}")));
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
    task::spawn(async move {
        if let Err(err) = OsNotification::new(nrx).run().await {
            let _ = notify_tx.send(Command::Error(format!("notification error: {err}")));
        }
    });
    let monitor_error_tx = tx.clone();
    let mtx = tx.clone();
    let mrx = tx.subscribe();
    let thresholds = param.threshold_map();
    task::spawn(async move {
        let mut monitor = monitor::Monitor::new(thresholds, mtx, mrx);
        if let Err(err) = monitor.run().await {
            let _ = monitor_error_tx.send(Command::Error(format!("monitor error: {err}")));
        }
    });

    let mut app = TuiApp::new(&param.inst_ids, history_window, order_tx);
    if !history_points.is_empty() {
        app.preload_history(&history_points);
    }
    let app_result = tokio::select! {
        result = app.run(&mut rx) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    let _ = tx.send(Command::Exit);
    app.dispose();
    app_result.map_err(|err| anyhow!(err.to_string()))?;
    Ok(())
}
