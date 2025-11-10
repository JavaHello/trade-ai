mod command;
mod config;
mod monitor;
mod notify;
mod okx;
mod tui;

use anyhow::anyhow;
use clap::Parser;
use color_eyre::Result;
use tokio::task;

use crate::command::Command;
use crate::notify::OsNotification;
use crate::okx::OkxWsClient;
use crate::tui::TuiApp;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let param = config::CliParams::parse();
    use tokio::sync::broadcast;

    let (tx, mut rx) = broadcast::channel::<Command>(16);
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

    let mut app = TuiApp::new(&param.inst_ids, history_window);
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
