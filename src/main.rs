//! TheWheel — launches the terminal UI.
//!
//! Logs go to a file (never stdout) so they can't corrupt the alternate screen.
//! Tries to connect to IB Gateway at startup with a short timeout; on failure
//! the TUI runs offline with demo data.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use thewheel::{config::Config, ibkr::Ibkr, store::Store, tui};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load(Path::new("config.toml"))?;
    let data_dir = cfg.resolved_data_dir();
    std::fs::create_dir_all(&data_dir).ok();

    // Keep the guard alive for the program's lifetime so logs flush.
    let _log_guard = init_logging(&data_dir);

    let store = Store::open(&data_dir.join("thewheel.db")).await?;
    let ibkr = try_connect_ibkr(&cfg).await;

    let terminal = ratatui::init();
    let result = tui::run(terminal, cfg, store, ibkr).await;
    ratatui::restore();

    if let Err(e) = &result {
        eprintln!("thewheel error: {e:#}");
    }
    result
}

/// Try to connect to IB Gateway. Returns `None` after a 5s timeout / failure
/// so the TUI can still start offline.
async fn try_connect_ibkr(cfg: &Config) -> Option<Arc<Ibkr>> {
    let addr = cfg.connection.address();
    match tokio::time::timeout(Duration::from_secs(5), Ibkr::connect(&cfg.connection)).await {
        Ok(Ok(ib)) => {
            tracing::info!("connected to IB Gateway at {addr}");
            Some(Arc::new(ib))
        }
        Ok(Err(e)) => {
            tracing::warn!("Gateway connect to {addr} failed: {e}; running offline");
            None
        }
        Err(_) => {
            tracing::warn!("Gateway connect to {addr} timed out; running offline");
            None
        }
    }
}

/// Initialise file-based logging. Returns a guard that must be kept alive.
fn init_logging(data_dir: &Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir).ok()?;
    let appender = tracing_appender::rolling::never(&log_dir, "thewheel.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
    Some(guard)
}
