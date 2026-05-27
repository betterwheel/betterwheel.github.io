//! TheWheel binary entrypoint.
//!
//! For now this only proves the crate wires together; the IBKR service, store,
//! and ratatui TUI are added in subsequent milestones.

use anyhow::Result;

fn main() -> Result<()> {
    let cfg = thewheel::config::Config::default();
    println!(
        "thewheel — engine scaffold ready (mode: {:?}, target Δ {:.2}, {}–{} DTE)",
        cfg.connection.mode, cfg.engine.target_delta, cfg.engine.min_dte, cfg.engine.max_dte
    );
    Ok(())
}
