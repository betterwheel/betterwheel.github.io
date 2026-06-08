//! BetterWheel desktop (Tauri) — a native front-end over the shared `betterwheel` core.
//!
//! Phase 1 is **read-only**: a background task drives `betterwheel::data::gather`
//! (connected) or the demo chains (offline), caches a render-ready [`Snapshot`],
//! and emits it to the webview. No order transmit lives here — the arm/execute
//! safety flow stays in the TUI and lands in Phase 2.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::{Mutex, RwLock};

use betterwheel::config::Config;
use betterwheel::data::gather;
use betterwheel::engine::structures;
use betterwheel::engine::types::{ActionKind, StructureLeg, Suggestion};
use betterwheel::ibkr::{AccountSnapshot, Ibkr};
use betterwheel::store::{JournalRow, Store, WheelPositionRow};
use betterwheel::tui::demo;

const REFRESH_SECS: u64 = 30;
const RECONNECT_SECS: u64 = 15;
const HEALTH_SECS: u64 = 5;
const CONNECT_TIMEOUT_SECS: u64 = 5;

// ---- render-ready view structs (Serialize → the webview) ----

#[derive(Serialize, Clone, Default)]
struct AccountView {
    net_liq: Option<f64>,
    cash: Option<f64>,
    buying_power: Option<f64>,
}
impl From<&AccountSnapshot> for AccountView {
    fn from(a: &AccountSnapshot) -> Self {
        Self { net_liq: a.net_liquidation, cash: a.total_cash, buying_power: a.buying_power }
    }
}

#[derive(Serialize, Clone)]
struct PositionView {
    symbol: String,
    state: String,
    shares: i64,
    cost_basis: f64,
    premium: f64,
}

#[derive(Serialize, Clone)]
struct JournalView {
    ts: String,
    symbol: String,
    action: String,
    strike: Option<f64>,
    quantity: i64,
    status: String,
}

/// A suggestion flattened for the webview: a ready display label + the scalar
/// fields the tables/cards show (so the frontend never parses the `ActionKind`
/// enum shape).
#[derive(Serialize, Clone)]
struct SuggestionView {
    symbol: String,
    action: String,
    right: String,
    strike: f64,
    expiry: String,
    dte: i64,
    quantity: i32,
    limit_price: f64,
    delta: Option<f64>,
    premium_total: f64,
    capital_required: f64,
    annualized_yield: f64,
    rationale: String,
}

fn sug_view(s: &Suggestion) -> SuggestionView {
    SuggestionView {
        symbol: s.symbol.clone(),
        action: s.kind.display_label().to_string(),
        right: s.right.code().to_string(),
        strike: s.strike,
        expiry: s.expiry.format("%Y-%m-%d").to_string(),
        dte: s.dte,
        quantity: s.quantity,
        limit_price: s.limit_price,
        delta: s.delta,
        premium_total: s.premium_total,
        capital_required: s.capital_required,
        annualized_yield: s.annualized_yield,
        rationale: s.rationale.clone(),
    }
}

/// One 0DTE quadrant slot: its structure (or `None`) plus derived risk/reward and
/// the payoff curve the webview draws.
#[derive(Serialize, Clone)]
struct SlotView {
    name: String,
    suggestion: Option<SuggestionView>,
    breakevens: Vec<f64>,
    pop: Option<f64>,
    payoff_xs: Vec<f64>,
    payoff_ys: Vec<f64>,
}

#[derive(Serialize, Clone, Default)]
struct Snapshot {
    connected: bool,
    account: Option<AccountView>,
    suggestions: Vec<SuggestionView>,
    hedged: Vec<SuggestionView>,
    zerodte: Vec<SlotView>,
    positions: Vec<PositionView>,
    journal: Vec<JournalView>,
    updated: String,
    note: String,
}

struct AppState {
    cfg: Config,
    ibkr: Mutex<Option<Arc<Ibkr>>>,
    store: Store,
    snapshot: RwLock<Snapshot>,
}
type Shared = Arc<AppState>;

// ---- data assembly (mirrors the TUI's read-only refresh) ----

/// Sample a structure's expiry P&L (total $ for `qty` contracts) across a price
/// range spanning its strikes — the series the webview plots. Returns `(xs, ys)`.
fn payoff_curve(legs: &[StructureLeg], qty: i32) -> (Vec<f64>, Vec<f64>) {
    let lo = legs.iter().map(|l| l.strike).fold(f64::INFINITY, f64::min);
    let hi = legs.iter().map(|l| l.strike).fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() || !hi.is_finite() {
        return (Vec::new(), Vec::new());
    }
    let pad = ((hi - lo) * 0.5).max(hi * 0.02);
    let start = (lo - pad).max(0.0);
    let end = hi + pad;
    let n = 80usize;
    let mult = 100.0 * qty as f64;
    let mut xs = Vec::with_capacity(n + 1);
    let mut ys = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let s = start + (end - start) * i as f64 / n as f64;
        xs.push((s * 100.0).round() / 100.0);
        ys.push((structures::payoff_at(legs, s) * mult * 100.0).round() / 100.0);
    }
    (xs, ys)
}

fn slot_views(cfg: &Config, zsug: &[Option<Suggestion>]) -> Vec<SlotView> {
    (0..cfg.zerodte.slot_count())
        .map(|i| {
            let name = cfg
                .zerodte
                .slot(i)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| format!("Slot {i}"));
            let sug = zsug.get(i).and_then(|o| o.clone());
            let mut breakevens = Vec::new();
            let mut pop = None;
            let mut payoff_xs = Vec::new();
            let mut payoff_ys = Vec::new();
            if let Some(s) = &sug {
                if let ActionKind::OpenStructure { kind, legs } = &s.kind {
                    breakevens = structures::breakevens(legs);
                    if kind.pop_is_meaningful() {
                        pop = structures::estimate_pop(legs);
                    }
                    let (xs, ys) = payoff_curve(legs, s.quantity);
                    payoff_xs = xs;
                    payoff_ys = ys;
                }
            }
            SlotView {
                name,
                suggestion: sug.as_ref().map(sug_view),
                breakevens,
                pop,
                payoff_xs,
                payoff_ys,
            }
        })
        .collect()
}

fn pos_view(p: &WheelPositionRow) -> PositionView {
    PositionView {
        symbol: p.symbol.clone(),
        state: p.state.clone(),
        shares: p.shares,
        cost_basis: p.cost_basis,
        premium: p.cumulative_premium,
    }
}

fn jrn_view(j: &JournalRow) -> JournalView {
    JournalView {
        ts: j.ts.clone(),
        symbol: j.symbol.clone(),
        action: j.action.clone(),
        strike: j.strike,
        quantity: j.quantity,
        status: j.status.clone(),
    }
}

/// Build the snapshot from the broker (connected) or demo data (offline). The
/// connected path uses `gather`, which is read-only — it syncs wheel state and
/// ranks suggestions but never transmits or reconciles orders.
async fn build_snapshot(st: &AppState) -> Snapshot {
    let today = Local::now().date_naive();
    let updated = Local::now().format("%H:%M:%S").to_string();
    let ibkr = st.ibkr.lock().await.clone();

    if let Some(ibkr) = ibkr {
        let d = gather(&ibkr, &st.store, &st.cfg, &[], today).await;
        let ok = d.positions_ok;
        Snapshot {
            connected: true,
            account: d.account.as_ref().map(AccountView::from),
            suggestions: if ok { d.suggestions.iter().map(sug_view).collect() } else { Vec::new() },
            hedged: if ok { d.hedged_suggestions.iter().map(sug_view).collect() } else { Vec::new() },
            zerodte: slot_views(&st.cfg, if ok { &d.zerodte_suggestions } else { &[] }),
            positions: d.positions.iter().filter(|p| p.state != "Idle").map(pos_view).collect(),
            journal: d.journal.iter().map(jrn_view).collect(),
            updated,
            note: if ok {
                String::new()
            } else {
                "broker positions unavailable — suggestions hidden until the next refresh".into()
            },
        }
    } else {
        let watch = st.store.list_watchlist().await.unwrap_or_default();
        let symbols: Vec<String> =
            watch.iter().filter(|w| w.is_enabled()).map(|w| w.symbol.clone()).collect();
        let zsug = demo::demo_zerodte(&st.cfg.zerodte, &st.cfg.engine, today);
        let positions = st.store.list_positions().await.unwrap_or_default();
        let journal = st.store.recent_journal(200).await.unwrap_or_default();
        Snapshot {
            connected: false,
            account: None,
            suggestions: demo::demo_suggestions(&symbols, &st.cfg.engine, today)
                .iter()
                .map(sug_view)
                .collect(),
            hedged: Vec::new(),
            zerodte: slot_views(&st.cfg, &zsug),
            positions: positions.iter().filter(|p| p.state != "Idle").map(pos_view).collect(),
            journal: journal.iter().map(jrn_view).collect(),
            updated,
            note: "offline — showing demo data (start IB Gateway to go live)".into(),
        }
    }
}

async fn try_connect(st: &AppState) -> bool {
    match tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        Ibkr::connect(&st.cfg.connection),
    )
    .await
    {
        Ok(Ok(ib)) => {
            tracing::info!("connected to IB Gateway");
            *st.ibkr.lock().await = Some(Arc::new(ib));
            true
        }
        Ok(Err(e)) => {
            tracing::warn!("Gateway connect failed: {e}");
            false
        }
        Err(_) => {
            tracing::warn!("Gateway connect timed out");
            false
        }
    }
}

async fn do_refresh(app: &tauri::AppHandle, st: &AppState) {
    let snap = build_snapshot(st).await;
    *st.snapshot.write().await = snap.clone();
    let _ = app.emit("snapshot", &snap);
}

/// The read-only background driver: initial connect + refresh, then periodic
/// refresh, reconnect-while-offline, and a health poll that drops a dead socket.
async fn run_background(app: tauri::AppHandle, st: Shared) {
    try_connect(&st).await;
    do_refresh(&app, &st).await;

    let mut refresh_iv = tokio::time::interval(Duration::from_secs(REFRESH_SECS));
    let mut reconnect_iv = tokio::time::interval(Duration::from_secs(RECONNECT_SECS));
    let mut health_iv = tokio::time::interval(Duration::from_secs(HEALTH_SECS));
    // Consume each interval's immediate first tick so the loop ticks are spaced
    // (we already did the initial connect + refresh above).
    refresh_iv.tick().await;
    reconnect_iv.tick().await;
    health_iv.tick().await;

    loop {
        tokio::select! {
            _ = refresh_iv.tick() => do_refresh(&app, &st).await,
            _ = reconnect_iv.tick() => {
                let connected = st.ibkr.lock().await.is_some();
                if !connected && try_connect(&st).await {
                    do_refresh(&app, &st).await;
                }
            }
            _ = health_iv.tick() => {
                let mut guard = st.ibkr.lock().await;
                if guard.as_ref().is_some_and(|ib| !ib.is_connected()) {
                    tracing::warn!("IB Gateway connection lost — will reconnect");
                    *guard = None;
                }
            }
        }
    }
}

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, Shared>) -> Result<Snapshot, String> {
    Ok(state.snapshot.read().await.clone())
}

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg = Config::load(Path::new("config.toml")).unwrap_or_default();
    let data_dir = cfg.resolved_data_dir();
    std::fs::create_dir_all(&data_dir).ok();
    let store = tauri::async_runtime::block_on(Store::open(&data_dir.join("betterwheel.db")))
        .expect("open store");
    let state: Shared = Arc::new(AppState {
        cfg,
        ibkr: Mutex::new(None),
        store,
        snapshot: RwLock::new(Snapshot::default()),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![get_snapshot])
        .setup(|app| {
            let handle = app.handle().clone();
            let st = app.state::<Shared>().inner().clone();
            tauri::async_runtime::spawn(run_background(handle, st));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running BetterWheel desktop");
}
