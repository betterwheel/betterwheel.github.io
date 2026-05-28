//! TUI application state and update logic.
//!
//! Key handling is synchronous and returns an [`Action`]; the run loop performs
//! any async work (store writes, broker calls, data refresh) via
//! [`App::dispatch`]. Rendering is a pure function of this state
//! (see [`super::ui`]).
//!
//! When connected to IB Gateway, `App::reload` pulls real balances, broker
//! positions, and runs the engine over live option chains. Without a
//! connection it falls back to Black-Scholes-consistent demo data so the UI is
//! always populated.

use std::sync::Arc;

use anyhow::Result;
use chrono::{Local, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::demo;
use crate::config::{Config, Guardrails, TradingMode};
use crate::engine::csp;
use crate::engine::types::{
    ActionKind, EngineConfig, OptionQuote, Right, Suggestion, UnderlyingQuote,
};
use crate::ibkr::{AccountSnapshot, Ibkr, OrderOutcome, PositionRow, Side};
use crate::store::{JournalRow, NewJournalEntry, Store, WatchlistRow, WheelPositionRow};

/// Top-level tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Watchlist,
    Suggestions,
    Journal,
    Help,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Dashboard,
        Tab::Watchlist,
        Tab::Suggestions,
        Tab::Journal,
        Tab::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Watchlist => "Watchlist",
            Tab::Suggestions => "Suggestions",
            Tab::Journal => "Journal",
            Tab::Help => "Help",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Keyboard input mode.
pub enum InputMode {
    Normal,
    /// Typing a symbol to add to the watchlist (holds the buffer).
    AddSymbol(String),
}

/// An intent produced by a keypress, applied by [`App::dispatch`].
pub enum Action {
    Quit,
    NextTab,
    PrevTab,
    JumpTab(usize),
    Up,
    Down,
    StartAddSymbol,
    InputChar(char),
    Backspace,
    CancelInput,
    SubmitInput,
    DeleteSelected,
    Refresh,
    ToggleArm,
    Preview,
    Execute,
}

/// All TUI state.
pub struct App {
    pub cfg: Config,
    /// `Some` when a Gateway connection succeeded at startup; `None` = offline.
    pub ibkr: Option<Arc<Ibkr>>,
    pub tab: Tab,
    pub watchlist: Vec<WatchlistRow>,
    pub suggestions: Vec<Suggestion>,
    pub journal: Vec<JournalRow>,
    /// Per-symbol wheel state from the local store.
    pub positions: Vec<WheelPositionRow>,
    /// Live broker positions (populated only when connected).
    pub broker_positions: Vec<PositionRow>,
    pub account: Option<AccountSnapshot>,
    pub selected: usize,
    pub input: InputMode,
    pub status: String,
    /// `true` when `ibkr.is_some()` and the spike-path queries are usable.
    pub connected: bool,
    /// When `true`, `Execute` will actually transmit. Toggled with `A`.
    pub armed: bool,
    pub should_quit: bool,
}

impl App {
    pub async fn new(cfg: Config, ibkr: Option<Arc<Ibkr>>, store: &Store) -> Result<Self> {
        let connected = ibkr.is_some();
        let mut app = Self {
            cfg,
            ibkr,
            tab: Tab::Dashboard,
            watchlist: Vec::new(),
            suggestions: Vec::new(),
            journal: Vec::new(),
            positions: Vec::new(),
            broker_positions: Vec::new(),
            account: None,
            selected: 0,
            input: InputMode::Normal,
            status: initial_status(connected),
            connected,
            armed: false,
            should_quit: false,
        };
        app.reload(store).await?;
        Ok(app)
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Watchlist => self.watchlist.len(),
            Tab::Suggestions => self.suggestions.len(),
            Tab::Journal => self.journal.len(),
            _ => 0,
        }
    }

    /// Map a keypress to an [`Action`] (mode-aware).
    pub fn handle_key(&self, key: KeyEvent) -> Option<Action> {
        if let InputMode::AddSymbol(_) = self.input {
            return match key.code {
                KeyCode::Enter => Some(Action::SubmitInput),
                KeyCode::Esc => Some(Action::CancelInput),
                KeyCode::Backspace => Some(Action::Backspace),
                KeyCode::Char(c) => Some(Action::InputChar(c)),
                _ => None,
            };
        }

        match key.code {
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
            KeyCode::Tab | KeyCode::Right => Some(Action::NextTab),
            KeyCode::BackTab | KeyCode::Left => Some(Action::PrevTab),
            KeyCode::Char(d @ '1'..='5') => Some(Action::JumpTab(d as usize - '1' as usize)),
            KeyCode::Char('j') | KeyCode::Down => Some(Action::Down),
            KeyCode::Char('k') | KeyCode::Up => Some(Action::Up),
            KeyCode::Char('a') => Some(Action::StartAddSymbol),
            KeyCode::Char('d') => Some(Action::DeleteSelected),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char('?') => Some(Action::JumpTab(Tab::Help.index())),
            KeyCode::Char('A') => Some(Action::ToggleArm),
            KeyCode::Char('p') => Some(Action::Preview),
            KeyCode::Char('x') => Some(Action::Execute),
            _ => None,
        }
    }

    /// Apply an action, performing any async work against the store / broker.
    pub async fn dispatch(&mut self, action: Action, store: &Store) -> Result<()> {
        match action {
            Action::Quit => self.should_quit = true,
            Action::NextTab => self.switch_tab(1),
            Action::PrevTab => self.switch_tab(-1),
            Action::JumpTab(i) => {
                if let Some(t) = Tab::ALL.get(i) {
                    self.tab = *t;
                    self.selected = 0;
                }
            }
            Action::Up => self.move_selection(-1),
            Action::Down => self.move_selection(1),
            Action::StartAddSymbol => {
                self.tab = Tab::Watchlist;
                self.input = InputMode::AddSymbol(String::new());
                self.status = "add symbol — type a ticker, Enter to confirm, Esc to cancel".into();
            }
            Action::InputChar(c) => {
                if let InputMode::AddSymbol(buf) = &mut self.input
                    && (c.is_ascii_alphanumeric() || c == '.')
                {
                    buf.push(c.to_ascii_uppercase());
                }
            }
            Action::Backspace => {
                if let InputMode::AddSymbol(buf) = &mut self.input {
                    buf.pop();
                }
            }
            Action::CancelInput => {
                self.input = InputMode::Normal;
                self.status = self.default_status();
            }
            Action::SubmitInput => {
                if let InputMode::AddSymbol(buf) = &self.input {
                    let sym = buf.trim().to_string();
                    if !sym.is_empty() {
                        store.add_symbol(&sym, "STK").await?;
                        self.status = format!("added {sym}");
                    }
                }
                self.input = InputMode::Normal;
                self.reload(store).await?;
            }
            Action::DeleteSelected => {
                if self.tab == Tab::Watchlist
                    && let Some(row) = self.watchlist.get(self.selected)
                {
                    let sym = row.symbol.clone();
                    store.remove_symbol(&sym).await?;
                    self.status = format!("removed {sym}");
                    self.reload(store).await?;
                }
            }
            Action::Refresh => {
                self.reload(store).await?;
                self.status = "refreshed".into();
            }
            Action::ToggleArm => {
                self.armed = !self.armed;
                self.status = if self.armed {
                    "ARMED — `x` will transmit a real order".into()
                } else {
                    "disarmed".into()
                };
            }
            Action::Preview => {
                if self.tab == Tab::Suggestions
                    && let Some(sug) = self.suggestions.get(self.selected).cloned()
                {
                    self.preview_suggestion(&sug, store).await?;
                }
            }
            Action::Execute => {
                if self.tab == Tab::Suggestions
                    && let Some(sug) = self.suggestions.get(self.selected).cloned()
                {
                    self.execute_suggestion(&sug, store).await?;
                }
            }
        }
        Ok(())
    }

    /// Submit a what-if for the selected suggestion; journal it; show the result.
    async fn preview_suggestion(&mut self, sug: &Suggestion, store: &Store) -> Result<()> {
        let Some(ibkr) = self.ibkr.clone() else {
            self.status = "not connected — start IB Gateway first".into();
            return Ok(());
        };
        let Some((side, right_str)) = side_and_right(sug) else {
            self.status = "rolls aren't supported yet".into();
            return Ok(());
        };
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let result = ibkr
            .submit_or_preview(
                &sug.symbol,
                &expiry,
                sug.strike,
                right_str,
                side,
                sug.quantity,
                sug.limit_price,
                true,
            )
            .await;
        let entry_base = journal_entry_for(sug, &expiry, right_str);
        match result {
            Ok(OrderOutcome::Preview(state)) => {
                let margin = state
                    .initial_margin_after
                    .map(|v| format!("${v:.0}"))
                    .unwrap_or_else(|| "?".into());
                let commission = state
                    .commission
                    .map(|v| format!("${v:.2}"))
                    .unwrap_or_else(|| "?".into());
                self.status = format!(
                    "preview {} {} {:.1}{}@{:.2}: margin {} · commission {} · {}",
                    sug.symbol,
                    format_kind(&sug.kind),
                    sug.strike,
                    right_str,
                    sug.limit_price,
                    margin,
                    commission,
                    state.status
                );
                store
                    .record(&NewJournalEntry {
                        status: "previewed".into(),
                        premium: Some(sug.premium_total),
                        ..entry_base
                    })
                    .await?;
            }
            Ok(OrderOutcome::Submitted(_)) => {
                self.status = "preview unexpectedly returned a submission id".into();
            }
            Err(e) => {
                self.status = format!("preview error: {e}");
                store
                    .record(&NewJournalEntry {
                        status: "rejected".into(),
                        note: Some(e.to_string()),
                        ..entry_base
                    })
                    .await?;
            }
        }
        self.journal = store.recent_journal(200).await?;
        Ok(())
    }

    /// Transmit the selected suggestion (live order). Gated on armed +
    /// connected + not read_only; auto-disarms after a successful submission.
    async fn execute_suggestion(&mut self, sug: &Suggestion, store: &Store) -> Result<()> {
        if !self.armed {
            self.status = "disarmed — press `A` to arm before executing".into();
            return Ok(());
        }
        if self.cfg.guardrails.read_only {
            self.status = "read_only = true in config — disable to transmit".into();
            return Ok(());
        }
        if sug.quantity > self.cfg.guardrails.max_contracts_per_order {
            self.status = format!(
                "blocked: quantity {} exceeds max_contracts_per_order {}",
                sug.quantity, self.cfg.guardrails.max_contracts_per_order
            );
            return Ok(());
        }
        let Some(ibkr) = self.ibkr.clone() else {
            self.status = "not connected — start IB Gateway first".into();
            return Ok(());
        };
        let Some((side, right_str)) = side_and_right(sug) else {
            self.status = "rolls aren't supported yet".into();
            return Ok(());
        };
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let result = ibkr
            .submit_or_preview(
                &sug.symbol,
                &expiry,
                sug.strike,
                right_str,
                side,
                sug.quantity,
                sug.limit_price,
                false,
            )
            .await;
        let entry_base = journal_entry_for(sug, &expiry, right_str);
        match result {
            Ok(OrderOutcome::Submitted(oid)) => {
                self.status = format!(
                    "submitted {} {} {:.1}{}@{:.2} → id {}",
                    sug.symbol, format_kind(&sug.kind), sug.strike, right_str, sug.limit_price, oid
                );
                store
                    .record(&NewJournalEntry {
                        status: "submitted".into(),
                        ibkr_order_id: Some(oid),
                        premium: Some(sug.premium_total),
                        ..entry_base
                    })
                    .await?;
                // Safety: a successful transmit auto-disarms.
                self.armed = false;
            }
            Ok(OrderOutcome::Preview(_)) => {
                self.status = "execute unexpectedly returned a preview".into();
            }
            Err(e) => {
                self.status = format!("execute error: {e}");
                store
                    .record(&NewJournalEntry {
                        status: "rejected".into(),
                        note: Some(e.to_string()),
                        ..entry_base
                    })
                    .await?;
            }
        }
        self.journal = store.recent_journal(200).await?;
        Ok(())
    }

    fn switch_tab(&mut self, delta: isize) {
        let n = Tab::ALL.len() as isize;
        let i = (self.tab.index() as isize + delta).rem_euclid(n) as usize;
        self.tab = Tab::ALL[i];
        self.selected = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.list_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let i = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.selected = i;
    }

    fn default_status(&self) -> String {
        initial_status(self.connected)
    }

    /// Reload local state + (when connected) refresh broker data + suggestions.
    async fn reload(&mut self, store: &Store) -> Result<()> {
        self.watchlist = store.list_watchlist().await?;
        self.journal = store.recent_journal(200).await?;
        self.positions = store.list_positions().await?;
        let today = Local::now().date_naive();

        if let Some(ibkr) = self.ibkr.clone() {
            self.account = ibkr.account_summary().await.ok();
            self.broker_positions = ibkr.positions().await.unwrap_or_default();
            self.suggestions = live_suggestions(
                &ibkr,
                store,
                &self.watchlist,
                &self.cfg.engine,
                &self.cfg.guardrails,
                today,
            )
            .await;
        } else {
            let symbols: Vec<String> = self
                .watchlist
                .iter()
                .filter(|r| r.is_enabled())
                .map(|r| r.symbol.clone())
                .collect();
            self.suggestions = demo::demo_suggestions(&symbols, &self.cfg.engine, today);
        }

        if self.selected >= self.list_len() {
            self.selected = self.list_len().saturating_sub(1);
        }
        Ok(())
    }

    pub fn mode_label(&self) -> &'static str {
        match self.cfg.connection.mode {
            TradingMode::Paper => "paper",
            TradingMode::Live => "live",
        }
    }
}

fn initial_status(connected: bool) -> String {
    if connected {
        "connected — `r` refreshes live data, `p` previews, `A` arms, `x` executes".into()
    } else {
        "offline — showing demo data. Start IB Gateway and configure config.toml to connect.".into()
    }
}

fn format_kind(k: &ActionKind) -> String {
    match k {
        ActionKind::SellPut => "SellPut".into(),
        ActionKind::SellCall => "SellCall".into(),
        ActionKind::CloseForProfit => "Close".into(),
        ActionKind::Roll { .. } => "Roll".into(),
    }
}

fn side_and_right(sug: &Suggestion) -> Option<(Side, &'static str)> {
    match (&sug.kind, sug.right) {
        (ActionKind::SellPut, _) => Some((Side::Sell, "P")),
        (ActionKind::SellCall, _) => Some((Side::Sell, "C")),
        (ActionKind::CloseForProfit, Right::Put) => Some((Side::Buy, "P")),
        (ActionKind::CloseForProfit, Right::Call) => Some((Side::Buy, "C")),
        (ActionKind::Roll { .. }, _) => None,
    }
}

fn journal_entry_for(sug: &Suggestion, expiry: &str, right_str: &str) -> NewJournalEntry {
    NewJournalEntry {
        symbol: sug.symbol.clone(),
        action: format_kind(&sug.kind),
        right: Some(right_str.to_string()),
        strike: Some(sug.strike),
        expiry: Some(expiry.to_string()),
        quantity: sug.quantity as i64,
        limit_price: Some(sug.limit_price),
        status: String::new(),
        ibkr_order_id: None,
        premium: None,
        note: None,
    }
}

/// Compute CSP suggestions across the enabled watchlist using live data.
async fn live_suggestions(
    ibkr: &Ibkr,
    store: &Store,
    watchlist: &[WatchlistRow],
    cfg: &EngineConfig,
    guardrails: &Guardrails,
    today: NaiveDate,
) -> Vec<Suggestion> {
    let active: Vec<&WatchlistRow> = watchlist.iter().filter(|w| w.is_enabled()).collect();
    if active.is_empty() {
        return Vec::new();
    }
    let budget = (guardrails.max_total_deployed / active.len() as f64).max(1000.0);

    let mut out = Vec::with_capacity(active.len());
    for w in active {
        match build_live_suggestion(ibkr, store, w, cfg, today, budget).await {
            Ok(Some(s)) => out.push(s),
            Ok(None) => {}
            Err(e) => tracing::warn!("live suggestion for {}: {e}", w.symbol),
        }
    }
    out.sort_by(|a, b| {
        b.annualized_yield
            .partial_cmp(&a.annualized_yield)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// One symbol's live pipeline: chain → moneyness pre-filter → snapshot greeks
/// → assemble [`OptionQuote`]s → run the engine's CSP selector.
async fn build_live_suggestion(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    cfg: &EngineConfig,
    today: NaiveDate,
    budget: f64,
) -> Result<Option<Suggestion>> {
    let symbol = w.symbol.as_str();

    // Resolve underlying conid (cached in the watchlist).
    let conid = match w.conid {
        Some(c) => c as i32,
        None => {
            let c = ibkr.underlying_contract_id(symbol).await?;
            let _ = store.set_conid(symbol, i64::from(c)).await;
            c
        }
    };

    let chain = ibkr.option_chain(symbol, conid).await?;
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(None);
    }

    let target_dte = (cfg.min_dte + cfg.max_dte) / 2;
    let Some((expiry_str, _)) = pick_expiry(&chain.expirations, today, target_dte) else {
        return Ok(None);
    };
    let expiry_date = NaiveDate::parse_from_str(&expiry_str, "%Y%m%d")?;

    let spot = ibkr
        .underlying_snapshot(symbol)
        .await?
        .last
        .unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    // Pre-filter strikes to keep market-data requests bounded.
    let mut strikes: Vec<f64> = chain
        .strikes
        .iter()
        .copied()
        .filter(|k| *k > 0.0 && *k < spot && (spot - *k) / spot <= 0.15)
        .collect();
    strikes.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    strikes.truncate(5);
    if strikes.is_empty() {
        return Ok(None);
    }

    let mut quotes: Vec<OptionQuote> = Vec::with_capacity(strikes.len());
    for k in strikes {
        if let Ok(snap) = ibkr.option_snapshot(symbol, &expiry_str, k, "P").await
            && let Some(comp) = snap.comp
        {
            let price = comp.option_price.or(snap.last).unwrap_or(0.0);
            if price > 0.0 {
                quotes.push(OptionQuote {
                    right: Right::Put,
                    strike: k,
                    expiry: expiry_date,
                    bid: price,
                    ask: price,
                    delta: comp.delta,
                    implied_volatility: comp.implied_volatility,
                    open_interest: None,
                    volume: None,
                });
            }
        }
    }

    Ok(csp::select_csp(
        symbol,
        UnderlyingQuote { last: spot },
        &quotes,
        budget,
        cfg,
        today,
    ))
}

/// Pick the expiration closest to `target_dte` days out.
fn pick_expiry(expirations: &[String], today: NaiveDate, target_dte: i64) -> Option<(String, i64)> {
    expirations
        .iter()
        .filter_map(|e| {
            NaiveDate::parse_from_str(e, "%Y%m%d")
                .ok()
                .map(|d| (e.clone(), (d - today).num_days()))
        })
        .filter(|(_, dte)| *dte >= 1)
        .min_by_key(|(_, dte)| (dte - target_dte).abs())
}
