mod command;
mod config;
mod monitor;
mod notify;
mod okx;
mod tui;

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
    let pok = param.clone();
    let ttx = tx.clone();
    task::spawn(async move {
        OkxWsClient::new(ttx)
            .await
            .unwrap()
            .subscribe_mark_price(&pok.inst_id)
            .await
            .unwrap();
    });
    let nrx = tx.subscribe();
    task::spawn(async move {
        OsNotification::new(nrx).run().await.unwrap();
    });
    let mtx = tx.clone();
    let mrx = tx.subscribe();
    task::spawn(async move {
        let mut monitor =
            monitor::Monitor::new(param.lower_threshold, param.upper_threshold, mtx, mrx);
        monitor.run().await.unwrap();
    });
    let mut app = TuiApp::new(&param.inst_id);
    app.run(&mut rx).await.unwrap();
    app.dispose();
    Ok(())
}
