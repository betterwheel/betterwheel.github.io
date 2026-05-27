//! TheWheel — launches the terminal UI.
//!
//! Logs go to a file (never stdout) so they can't corrupt the alternate screen.
//! Runs offline with demo data until OAuth is configured (see `SETUP.md`).

use std::path::Path;

use anyhow::Result;
use thewheel::{config::Config, store::Store, tui};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load(Path::new("config.toml"))?;
    let data_dir = cfg.resolved_data_dir();
    std::fs::create_dir_all(&data_dir).ok();

    // Keep the guard alive for the program's lifetime so logs flush.
    let _log_guard = init_logging(&data_dir);

    let store = Store::open(&data_dir.join("thewheel.db")).await?;

    let terminal = ratatui::init();
    let result = tui::run(terminal, cfg, store).await;
    ratatui::restore();

    if let Err(e) = &result {
        eprintln!("thewheel error: {e:#}");
    }
    result
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
