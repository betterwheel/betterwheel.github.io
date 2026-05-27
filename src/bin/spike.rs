//! Connectivity + data-path spike.
//!
//! Validates that the app can talk to IB Gateway and pull everything the wheel
//! engine needs, BEFORE any TUI is built on top (see the plan's milestone 1).
//! It only *reads* and runs *what-if* previews — it never transmits an order.
//!
//! Usage:
//!   1. Start IB Gateway (paper), enable API, socket port 4002, trust 127.0.0.1.
//!   2. `cargo run --bin spike -- [SYMBOL]`   (defaults to AAPL)

use anyhow::Result;
use chrono::{Local, NaiveDate};
use std::path::Path;

use thewheel::config::Config;
use thewheel::ibkr::{Ibkr, Tradability};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).try_init();

    let symbol = std::env::args().nth(1).unwrap_or_else(|| "AAPL".to_string());
    let cfg = Config::load(Path::new("config.toml"))?;
    let addr = cfg.connection.address();

    println!("== TheWheel spike ==");
    println!("symbol: {symbol}");
    println!("connecting to IB Gateway at {addr} (mode {:?}, market data {:?}) ...\n",
        cfg.connection.mode, cfg.connection.market_data);

    let ibkr = match Ibkr::connect(&cfg.connection).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connection failed: {e}");
            eprintln!("\nIs IB Gateway running with the API enabled on {addr}?");
            return Err(e);
        }
    };
    println!("connected.\n");

    // --- Account ---
    println!("-- account summary --");
    match ibkr.account_summary().await {
        Ok(a) => println!(
            "  net liq:   {}\n  cash:      {}\n  buying pwr:{}\n  avail:     {}",
            fmt_money(a.net_liquidation),
            fmt_money(a.total_cash),
            fmt_money(a.buying_power),
            fmt_money(a.available_funds),
        ),
        Err(e) => eprintln!("  account_summary error: {e}"),
    }

    // --- Positions ---
    println!("\n-- positions --");
    match ibkr.positions().await {
        Ok(rows) if rows.is_empty() => println!("  (none)"),
        Ok(rows) => {
            for p in rows {
                println!(
                    "  {:6} {:8} qty {:>7.0} @ {:.2} {}{}",
                    p.symbol,
                    p.security_type,
                    p.position,
                    p.average_cost,
                    if p.right.is_empty() { String::new() } else { format!("{} ", p.right) },
                    if p.strike > 0.0 { format!("{} {:.1}", p.expiry, p.strike) } else { String::new() },
                );
            }
        }
        Err(e) => eprintln!("  positions error: {e}"),
    }

    // --- Option chain ---
    println!("\n-- option chain: {symbol} --");
    let conid = ibkr.underlying_contract_id(&symbol).await?;
    println!("  underlying conid: {conid}");
    let chain = ibkr.option_chain(&symbol, conid).await?;
    println!(
        "  exchange {} · class {} · x{} · {} expirations · {} strikes",
        chain.exchange,
        chain.trading_class,
        chain.multiplier,
        chain.expirations.len(),
        chain.strikes.len()
    );
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        eprintln!("  chain is empty — cannot continue.");
        return Ok(());
    }

    // --- Underlying price ---
    let und = ibkr.underlying_snapshot(&symbol).await?;
    let spot = und.last.unwrap_or_else(|| median(&chain.strikes));
    println!(
        "  spot: {} {}",
        und.last.map(|p| format!("{p:.2}")).unwrap_or_else(|| "(no quote)".into()),
        if und.last.is_none() { format!("(using median strike {spot:.2})") } else { String::new() },
    );

    // --- Pick a ~35 DTE expiry ---
    let today = Local::now().date_naive();
    let Some((expiry, dte)) = pick_expiry(&chain.expirations, today) else {
        eprintln!("  no parseable future expiration found.");
        return Ok(());
    };
    println!("  target expiry: {expiry} ({dte} DTE)");

    // --- Snapshot a few OTM puts ---
    let otm_puts = otm_put_strikes(&chain.strikes, spot, 5);
    println!("\n-- put greeks @ {expiry} (delayed/realtime per config) --");
    for k in &otm_puts {
        match ibkr.option_snapshot(&symbol, &expiry, *k, "P").await {
            Ok(s) => {
                let (delta, iv, price) = match &s.comp {
                    Some(c) => (c.delta, c.implied_volatility, c.option_price),
                    None => (None, None, None),
                };
                println!(
                    "  {:>7.1}P  Δ {:>6}  IV {:>6}  px {:>6}{}",
                    k,
                    opt(delta, 2),
                    opt(iv, 3),
                    opt(price, 2),
                    if s.comp.is_none() { "  (no computation — check market-data entitlement)" } else { "" },
                );
            }
            Err(e) => eprintln!("  {k:.1}P error: {e}"),
        }
    }

    // --- Tradability probe (EU/PRIIPs) via what-if ---
    if let Some(probe_strike) = otm_puts.last().copied() {
        println!("\n-- tradability probe (what-if sell {probe_strike:.1}P, NOT transmitted) --");
        match ibkr.tradability(&symbol, &expiry, probe_strike).await {
            Tradability::Allowed { init_margin, commission } => println!(
                "  ALLOWED · init margin {} · commission {}",
                fmt_money(init_margin),
                fmt_money(commission)
            ),
            Tradability::Blocked(reason) => println!("  BLOCKED · {reason}"),
        }
    }

    println!("\nspike complete.");
    Ok(())
}

fn fmt_money(v: Option<f64>) -> String {
    v.map(|x| format!("{x:>12.2}")).unwrap_or_else(|| "         n/a".into())
}

fn opt(v: Option<f64>, prec: usize) -> String {
    v.map(|x| format!("{x:.prec$}")).unwrap_or_else(|| "  -".into())
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// Pick the expiration closest to ~35 DTE (within the future).
fn pick_expiry(expirations: &[String], today: NaiveDate) -> Option<(String, i64)> {
    expirations
        .iter()
        .filter_map(|e| {
            NaiveDate::parse_from_str(e, "%Y%m%d")
                .ok()
                .map(|d| (e.clone(), (d - today).num_days()))
        })
        .filter(|(_, dte)| *dte >= 1)
        .min_by_key(|(_, dte)| (dte - 35).abs())
}

/// Up to `n` OTM put strikes nearest to (and below) spot, deepest-OTM last.
fn otm_put_strikes(strikes: &[f64], spot: f64, n: usize) -> Vec<f64> {
    let mut below: Vec<f64> = strikes.iter().copied().filter(|k| *k < spot).collect();
    below.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Closest-to-spot first, then take n, then reverse so deepest-OTM is last.
    let mut picked: Vec<f64> = below.into_iter().rev().take(n).collect();
    picked.reverse();
    picked
}
