//! BetterWheel desktop (Tauri) — a native front-end over the shared `betterwheel`
//! core. It **drives the same `tui::app::App`** the terminal UI runs: a background
//! task wires the broker order-event stream, the 0DTE scheduler, reloads, and
//! reconnect/health into `App`, and the webview renders a snapshot of its state.
//! The preview→arm→execute→live-confirm safety flow goes through `App`'s exact,
//! deep-review-hardened guardrail code (via the `ui_*` facade) — nothing about the
//! order path is reimplemented here.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use betterwheel::config::{Config, ConnectionConfig};
use betterwheel::engine::structures;
use betterwheel::engine::types::{ActionKind, StructureLeg, Suggestion};
use betterwheel::ibkr::{AccountSnapshot, Ibkr, OrderEvent};
use betterwheel::store::{JournalRow, Store, WheelPositionRow};
use betterwheel::tui::app::{App, BrokerUpdate, SugList};

const RECONNECT_SECS: u64 = 15;
const HEALTH_SECS: u64 = 5;
const SCHEDULER_SECS: u64 = 30;
const AUTORELOAD_SECS: u64 = 180;
const HEARTBEAT_SECS: u64 = 3;
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
/// enum shape). Its array position is its command index.
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
    mode: String,
    armed: bool,
    live_confirmed: bool,
    needs_live_confirm: bool,
    auto_trading: usize,
    account: Option<AccountView>,
    suggestions: Vec<SuggestionView>,
    hedged: Vec<SuggestionView>,
    zerodte: Vec<SlotView>,
    positions: Vec<PositionView>,
    journal: Vec<JournalView>,
    status: String,
    updated: String,
    note: String,
}

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

/// A render-ready snapshot of the live `App` state (pure read; no I/O).
fn build_snapshot(app: &App) -> Snapshot {
    Snapshot {
        connected: app.connected,
        mode: app.mode_label().to_string(),
        armed: app.armed,
        live_confirmed: app.live_confirmed,
        needs_live_confirm: !app.live_gate_ok(),
        auto_trading: app.zerodte_automating(),
        account: app.account.as_ref().map(AccountView::from),
        suggestions: app.suggestions.iter().map(sug_view).collect(),
        hedged: app.hedged_suggestions.iter().map(sug_view).collect(),
        zerodte: slot_views(&app.cfg, &app.zerodte_suggestions),
        positions: app.positions.iter().filter(|p| p.state != "Idle").map(pos_view).collect(),
        journal: app.journal.iter().map(jrn_view).collect(),
        status: app.status.clone(),
        updated: Local::now().format("%H:%M:%S").to_string(),
        note: if app.connected {
            String::new()
        } else {
            app.offline_reason
                .clone()
                .unwrap_or_else(|| "offline — showing demo data (start IB Gateway to go live)".into())
        },
    }
}

/// Shared server state: the live `App` (behind a lock) and the store handle.
struct DeskState {
    app: Mutex<App>,
    store: Store,
}
type Shared = Arc<DeskState>;

/// Build a snapshot under the lock, then emit it to the webview (lock released
/// before the emit). The single place the frontend is notified of state changes.
async fn emit(app_handle: &tauri::AppHandle, st: &DeskState) -> Snapshot {
    let snap = {
        let app = st.app.lock().await;
        build_snapshot(&app)
    };
    let _ = app_handle.emit("snapshot", &snap);
    snap
}

// ---- the broker background driver (mirrors `tui::mod::run`, minus the terminal) ----

fn spawn_order_consumer(
    ibkr: Option<Arc<Ibkr>>,
    tx: &mpsc::UnboundedSender<OrderEvent>,
) -> Option<JoinHandle<()>> {
    ibkr.map(|ib| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = ib.stream_order_events(tx).await;
        })
    })
}

fn spawn_reconnect(
    conn: ConnectionConfig,
    tx: &mpsc::UnboundedSender<BrokerUpdate>,
) -> Option<JoinHandle<()>> {
    if conn.reconnect_secs == 0 {
        return None;
    }
    let tx = tx.clone();
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(conn.reconnect_secs)).await;
            if tx.is_closed() {
                return;
            }
            match tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), Ibkr::connect(&conn)).await {
                Ok(Ok(ib)) => {
                    let _ = tx.send(BrokerUpdate::Connected(Arc::new(ib)));
                    return;
                }
                Ok(Err(e)) => {
                    let _ = tx.send(BrokerUpdate::ConnectFailed(betterwheel::ibkr::connect_failure_hint(&e)));
                }
                Err(_) => {
                    let _ = tx.send(BrokerUpdate::ConnectFailed(
                        "IB Gateway connection timed out — is it running?".into(),
                    ));
                }
            }
        }
    }))
}

/// Drive the `App`: fan in broker order events, off-loop reloads, the 0DTE
/// scheduler, reconnect, and a health poll — emitting a fresh snapshot after each.
async fn run_loop(app_handle: tauri::AppHandle, st: Shared) {
    let (upd_tx, mut upd_rx) = mpsc::unbounded_channel();
    let (order_tx, mut order_rx) = mpsc::unbounded_channel();

    let (mut order_consumer, mut reconnect) = {
        let mut app = st.app.lock().await;
        app.set_update_sender(upd_tx.clone());
        let oc = spawn_order_consumer(app.ibkr.clone(), &order_tx);
        let rc = if app.ibkr.is_none() {
            spawn_reconnect(app.cfg.connection.clone(), &upd_tx)
        } else {
            None
        };
        // Kick the first live load (off-loop) so suggestions populate.
        if app.ibkr.is_some() {
            app.request_reload(&st.store).await;
        }
        (oc, rc)
    };
    emit(&app_handle, &st).await;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    let mut health = tokio::time::interval(Duration::from_secs(HEALTH_SECS));
    let mut scheduler = tokio::time::interval(Duration::from_secs(SCHEDULER_SECS));
    let mut autoreload = tokio::time::interval(Duration::from_secs(AUTORELOAD_SECS));
    let mut reconnect_iv = tokio::time::interval(Duration::from_secs(RECONNECT_SECS));
    heartbeat.tick().await;
    health.tick().await;
    scheduler.tick().await;
    autoreload.tick().await;
    reconnect_iv.tick().await;

    loop {
        tokio::select! {
            Some(ev) = order_rx.recv() => {
                let _ = st.app.lock().await.apply_order_event(ev, &st.store).await;
                emit(&app_handle, &st).await;
            }
            Some(upd) = upd_rx.recv() => {
                {
                    let mut app = st.app.lock().await;
                    match upd {
                        BrokerUpdate::Connected(ib) => {
                            if let Some(h) = order_consumer.take() { h.abort(); }
                            app.set_connected(ib);
                            order_consumer = spawn_order_consumer(app.ibkr.clone(), &order_tx);
                            reconnect = None;
                            app.request_reload(&st.store).await;
                        }
                        BrokerUpdate::ConnectFailed(reason) => app.set_offline_reason(reason),
                        BrokerUpdate::Reloaded(data) => app.apply_live_data(*data, &st.store).await,
                    }
                }
                emit(&app_handle, &st).await;
            }
            _ = health.tick() => {
                let restart = {
                    let mut app = st.app.lock().await;
                    let dropped = app.connected
                        && app.ibkr.as_ref().is_some_and(|ib| !ib.is_connected());
                    if dropped {
                        app.set_disconnected("IB Gateway connection lost — reconnecting…".into());
                        Some(app.cfg.connection.clone())
                    } else {
                        None
                    }
                };
                if let Some(conn) = restart {
                    if let Some(h) = order_consumer.take() { h.abort(); }
                    if let Some(h) = reconnect.take() { h.abort(); }
                    reconnect = spawn_reconnect(conn, &upd_tx);
                    emit(&app_handle, &st).await;
                }
            }
            _ = reconnect_iv.tick() => {
                // Belt-and-suspenders: if offline and no reconnect task is running, start one.
                let need = {
                    let app = st.app.lock().await;
                    app.ibkr.is_none()
                };
                if need && reconnect.is_none() {
                    let conn = st.app.lock().await.cfg.connection.clone();
                    reconnect = spawn_reconnect(conn, &upd_tx);
                }
            }
            _ = scheduler.tick() => {
                st.app.lock().await.tick_zerodte(&st.store).await;
                emit(&app_handle, &st).await;
            }
            _ = autoreload.tick() => {
                let mut app = st.app.lock().await;
                if app.ibkr.is_some() {
                    app.request_reload(&st.store).await;
                }
            }
            _ = heartbeat.tick() => {
                // Periodic emit so the "updated" clock advances and a landed reload shows.
                emit(&app_handle, &st).await;
            }
        }
    }
}

// ---- Tauri commands ----

fn parse_list(s: &str) -> Result<SugList, String> {
    match s {
        "classic" => Ok(SugList::Classic),
        "hedged" => Ok(SugList::Hedged),
        "zerodte" => Ok(SugList::ZeroDte),
        other => Err(format!("unknown list {other}")),
    }
}

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, Shared>) -> Result<Snapshot, String> {
    Ok(build_snapshot(&*state.app.lock().await))
}

#[tauri::command]
async fn preview(
    list: String,
    index: usize,
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, Shared>,
) -> Result<Snapshot, String> {
    let sl = parse_list(&list)?;
    state.app.lock().await.ui_preview(sl, index, &state.store).await.map_err(|e| e.to_string())?;
    Ok(emit(&app_handle, &state).await)
}

#[tauri::command]
async fn execute(
    list: String,
    index: usize,
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, Shared>,
) -> Result<Snapshot, String> {
    let sl = parse_list(&list)?;
    state.app.lock().await.ui_execute(sl, index, &state.store).await.map_err(|e| e.to_string())?;
    Ok(emit(&app_handle, &state).await)
}

#[tauri::command]
async fn set_armed(
    on: bool,
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, Shared>,
) -> Result<Snapshot, String> {
    state.app.lock().await.ui_set_armed(on);
    Ok(emit(&app_handle, &state).await)
}

#[tauri::command]
async fn confirm_live(
    phrase: String,
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, Shared>,
) -> Result<Snapshot, String> {
    state.app.lock().await.ui_confirm_live(&phrase);
    Ok(emit(&app_handle, &state).await)
}

#[tauri::command]
async fn refresh(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, Shared>,
) -> Result<Snapshot, String> {
    state.app.lock().await.request_reload(&state.store).await;
    Ok(emit(&app_handle, &state).await)
}

async fn try_connect(cfg: &Config) -> Option<Arc<Ibkr>> {
    match tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), Ibkr::connect(&cfg.connection)).await {
        Ok(Ok(ib)) => {
            tracing::info!("connected to IB Gateway");
            Some(Arc::new(ib))
        }
        Ok(Err(e)) => {
            tracing::warn!("Gateway connect failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("Gateway connect timed out");
            None
        }
    }
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

    let (app, store) = tauri::async_runtime::block_on(async {
        let store = Store::open(&data_dir.join("betterwheel.db")).await.expect("open store");
        let ibkr = try_connect(&cfg).await;
        let app = App::new(cfg, ibkr, &store).await.expect("init app");
        (app, store)
    });
    let state: Shared = Arc::new(DeskState { app: Mutex::new(app), store });

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
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            preview,
            execute,
            set_armed,
            confirm_live,
            refresh
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let st = app.state::<Shared>().inner().clone();
            tauri::async_runtime::spawn(run_loop(handle, st));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running BetterWheel desktop");
}
