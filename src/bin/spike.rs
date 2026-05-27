//! Connectivity + data-path spike for the IBKR **Web API** (first-party OAuth2).
//!
//! Validates the whole path the wheel engine needs — token, brokerage session,
//! accounts, option chain, greeks, and a what-if/tradability probe — BEFORE any
//! TUI is built on top. It only *reads* and runs *what-if* previews; it never
//! transmits an order.
//!
//! Prereq: complete the one-time OAuth onboarding in `SETUP.md` and fill in
//! `[connection.oauth]` in `config.toml`.
//!
//! Usage: `cargo run --bin spike -- [SYMBOL]`   (defaults to AAPL)

use anyhow::Result;
use chrono::{Datelike, Duration, Local};
use std::path::Path;

use thewheel::config::Config;
use thewheel::ibkr::{Tradability, WebApi};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).try_init();

    let symbol = std::env::args().nth(1).unwrap_or_else(|| "AAPL".to_string());
    let cfg = Config::load(Path::new("config.toml"))?;

    println!("== TheWheel spike (Web API / OAuth2) ==");
    println!("symbol: {symbol}");
    println!("base:   {}", cfg.connection.base_url);
    println!("token:  {}\n", cfg.connection.token_url());

    if !cfg.connection.oauth.is_configured() {
        eprintln!("OAuth is not configured. Fill [connection.oauth] in config.toml — see SETUP.md.");
        return Ok(());
    }

    let mut api = match WebApi::connect(&cfg.connection).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("connection/auth failed: {e:#}");
            return Err(e);
        }
    };
    println!("authenticated. account: {}\n", api.account().unwrap_or("?"));

    // --- Account ---
    println!("-- account summary --");
    match api.account_summary().await {
        Ok(a) => println!(
            "  net liq {}  cash {}  buying pwr {}  avail {}",
            money(a.net_liquidation),
            money(a.total_cash),
            money(a.buying_power),
            money(a.available_funds),
        ),
        Err(e) => eprintln!("  error: {e:#}"),
    }

    // --- Positions ---
    println!("\n-- positions --");
    match api.positions().await {
        Ok(rows) if rows.is_empty() => println!("  (none)"),
        Ok(rows) => {
            for p in rows {
                println!(
                    "  {:8} {:5} qty {:>7.0} @ {:.2}  mkt {:.2}  uPnL {:.2}",
                    p.symbol, p.asset_class, p.position, p.avg_price, p.mkt_price, p.unrealized_pnl
                );
            }
        }
        Err(e) => eprintln!("  error: {e:#}"),
    }

    // --- Underlying conid + price ---
    println!("\n-- {symbol} --");
    let conid = api.underlying_conid(&symbol).await?;
    println!("  underlying conid: {conid}");
    let spot = api.option_snapshot(conid).await.ok().and_then(|s| s.last);
    println!("  spot: {}", spot.map(|p| format!("{p:.2}")).unwrap_or_else(|| "(no quote)".into()));

    // --- Strikes for a ~35-DTE month ---
    let month = target_month();
    println!("  month: {month}");
    let (_, puts) = api.strikes(conid, &month).await?;
    if puts.is_empty() {
        eprintln!("  no put strikes returned (try a different `month` format — see SETUP.md).");
        return Ok(());
    }
    let spot = spot.unwrap_or_else(|| median(&puts));
    let otm = otm_put_strikes(&puts, spot, 5);
    println!("  {} put strikes; sampling OTM near {:.2}: {:?}", puts.len(), spot, otm);

    // --- Greeks per OTM put ---
    println!("\n-- put greeks @ {month} --");
    let mut probe_conid: Option<i64> = None;
    for k in &otm {
        match api.option_conid(conid, &month, *k, "P").await {
            Ok(oc) => {
                probe_conid = Some(oc);
                match api.option_snapshot(oc).await {
                    Ok(s) => println!(
                        "  {:>7.1}P  Δ {:>6}  IV {:>6}  bid {:>6}  ask {:>6}",
                        k, optf(s.delta, 3), optf(s.implied_volatility, 3), optf(s.bid, 2), optf(s.ask, 2)
                    ),
                    Err(e) => eprintln!("  {k:.1}P snapshot error: {e:#}"),
                }
            }
            Err(e) => eprintln!("  {k:.1}P conid error: {e:#}"),
        }
    }

    // --- Tradability probe (EU/PRIIPs) ---
    if let Some(oc) = probe_conid {
        println!("\n-- tradability probe (what-if SELL 1 put, NOT transmitted) --");
        match api.tradability(oc).await {
            Tradability::Allowed(p) => println!(
                "  ALLOWED · margin {} · commission {}",
                p.init_margin_change.as_deref().unwrap_or("?"),
                p.commission.as_deref().unwrap_or("?")
            ),
            Tradability::Blocked(reason) => println!("  BLOCKED · {reason}"),
        }
    }

    println!("\nspike complete.");
    Ok(())
}

fn money(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "n/a".into())
}

fn optf(v: Option<f64>, prec: usize) -> String {
    v.map(|x| format!("{x:.prec$}")).unwrap_or_else(|| "-".into())
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// `YYYYMM` roughly 35 days out (a typical wheel expiry month).
fn target_month() -> String {
    let d = Local::now().date_naive() + Duration::days(35);
    format!("{:04}{:02}", d.year(), d.month())
}

/// Up to `n` OTM put strikes nearest to (and below) spot, deepest-OTM last.
fn otm_put_strikes(strikes: &[f64], spot: f64, n: usize) -> Vec<f64> {
    let mut below: Vec<f64> = strikes.iter().copied().filter(|k| *k < spot).collect();
    below.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut picked: Vec<f64> = below.into_iter().rev().take(n).collect();
    picked.reverse();
    picked
}
