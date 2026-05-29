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
use crate::config::{Config, TradingMode};
use crate::engine::types::{
    ActionKind, EngineConfig, OpenShortOption, OptionQuote, Right, SharePosition, Suggestion,
    UnderlyingQuote, WheelState,
};
use crate::engine::math::round_cents;
use crate::engine::{self, SymbolContext};
use crate::ibkr::{
    AccountSnapshot, Ibkr, OpenOrderInfo, OptionOrder, OrderEvent, OrderOutcome, PositionRow, Side,
    Tradability,
};
use crate::positions;
use crate::store::{
    JournalRow, NewJournalEntry, PendingRollRow, Store, WatchlistRow, WheelPositionRow,
};

/// Keep only OTM strikes within this fraction of spot when building a chain
/// (bounds how far OTM we'll quote for entries / covered calls).
const MAX_OTM_MONEYNESS: f64 = 0.15;
/// Cap on per-symbol option snapshots, to bound market-data requests.
const MAX_CHAIN_STRIKES: usize = 5;
/// Far-OTM put target (fraction of spot) used by the tradability permission probe.
const PROBE_OTM_FRACTION: f64 = 0.85;

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
    /// Rolls whose close leg is live but not yet filled; the open leg is sent
    /// only once the matching close fills (see [`App::apply_order_event`]).
    pending_rolls: Vec<PendingRoll>,
    pub should_quit: bool,
}

/// A roll awaiting its close leg to fill before the open leg is transmitted.
/// Persisted to the store so it survives a restart (see [`PendingRollRow`]).
struct PendingRoll {
    symbol: String,
    /// IBKR right code (`"P"`/`"C"`) of both legs.
    right: &'static str,
    /// The near (closing) leg, used on restart to tell a filled close from a
    /// cancelled one by checking whether the short is still held.
    near_strike: f64,
    near_expiry: String,
    to_strike: f64,
    /// Resolved, listed expiry of the new leg (`YYYYMMDD`).
    to_expiry: String,
    quantity: i32,
    far_limit: f64,
    /// Order id of the buy-to-close leg we're waiting on.
    close_oid: String,
    /// `true` if loaded from the store at startup (a prior session's roll).
    /// Only these are orphan-reconciled — this-session rolls are driven entirely
    /// by the order stream, so they're never spuriously dropped.
    reconstructed: bool,
}

impl PendingRoll {
    fn from_row(r: PendingRollRow) -> Self {
        Self {
            symbol: r.symbol,
            // Map the persisted right back to a static literal.
            right: if r.right == "C" { "C" } else { "P" },
            near_strike: r.near_strike,
            near_expiry: r.near_expiry,
            to_strike: r.to_strike,
            to_expiry: r.to_expiry,
            quantity: r.quantity as i32,
            far_limit: r.far_limit,
            close_oid: r.close_oid,
            reconstructed: true,
        }
    }

    fn to_row(&self) -> PendingRollRow {
        PendingRollRow {
            close_oid: self.close_oid.clone(),
            symbol: self.symbol.clone(),
            right: self.right.to_string(),
            near_strike: self.near_strike,
            near_expiry: self.near_expiry.clone(),
            to_strike: self.to_strike,
            to_expiry: self.to_expiry.clone(),
            quantity: self.quantity as i64,
            far_limit: self.far_limit,
            created_at: String::new(), // assigned by the store on insert
        }
    }
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
            pending_rolls: Vec::new(),
            should_quit: false,
        };
        // Reconstruct rolls left in flight by a prior session; reload then
        // reconciles them against the broker's live open orders.
        app.pending_rolls = store
            .list_pending_rolls()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(PendingRoll::from_row)
            .collect();
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
        if let ActionKind::Roll { to_expiry, to_strike } = sug.kind {
            return self.preview_roll(&ibkr, sug, to_expiry, to_strike, store).await;
        }
        let Some((side, right_str)) = side_and_right(sug) else {
            self.status = "this action can't be previewed".into();
            return Ok(());
        };
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let order = OptionOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &expiry,
            strike: sug.strike,
            right: right_str,
            side,
            quantity: sug.quantity,
            limit: sug.limit_price,
        };
        let result = ibkr.submit_or_preview(&order, true).await;
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
        if let ActionKind::Roll { to_expiry, to_strike } = sug.kind {
            return self.execute_roll(&ibkr, sug, to_expiry, to_strike, store).await;
        }
        let Some((side, right_str)) = side_and_right(sug) else {
            self.status = "this action can't be executed".into();
            return Ok(());
        };
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let order = OptionOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &expiry,
            strike: sug.strike,
            right: right_str,
            side,
            quantity: sug.quantity,
            limit: sug.limit_price,
        };
        let result = ibkr.submit_or_preview(&order, false).await;
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

    /// Preview a roll as two what-if legs — buy-to-close the near (tested) short
    /// and sell-to-open the far one — reporting the net credit/debit. Read-only.
    /// The far leg is resolved to a real listed contract first.
    async fn preview_roll(
        &mut self,
        ibkr: &Ibkr,
        sug: &Suggestion,
        to_expiry: NaiveDate,
        to_strike: f64,
        store: &Store,
    ) -> Result<()> {
        if self.is_symbol_blocked(&sug.symbol) {
            self.status =
                format!("roll {}: blocked for new positions — close it instead", sug.symbol);
            return Ok(());
        }
        let right = right_char(sug.right);
        let near_expiry = sug.expiry.format("%Y%m%d").to_string();
        let today = Local::now().date_naive();

        let Some((far_expiry, far_strike, far_credit)) =
            resolve_roll_target(ibkr, &sug.symbol, right, to_expiry, to_strike, today).await?
        else {
            self.status = format!(
                "roll {}: couldn't resolve/price a listed target leg yet — retry when quotes are live",
                sug.symbol
            );
            return Ok(());
        };

        // Re-price the near (close) leg from a live quote (see execute_roll), so
        // the previewed net credit/debit reflects the current cost to close.
        let near_cost = price_leg(ibkr, &sug.symbol, &near_expiry, sug.strike, right)
            .await
            .unwrap_or(sug.limit_price);

        let near = OptionOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &near_expiry,
            strike: sug.strike,
            right,
            side: Side::Buy,
            quantity: sug.quantity,
            limit: near_cost,
        };
        let far = OptionOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &far_expiry,
            strike: far_strike,
            right,
            side: Side::Sell,
            quantity: sug.quantity,
            limit: far_credit,
        };
        let near_res = ibkr.submit_or_preview(&near, true).await;
        let far_res = ibkr.submit_or_preview(&far, true).await;

        let net = far_credit - near_cost; // per share; > 0 = net credit
        self.status = format!(
            "preview roll {}: close {:.1}{right}@{near_cost:.2} [{}] → open {far_strike:.1}{right} {far_expiry}@{far_credit:.2} [{}]; net {} ${:.2}/sh",
            sug.symbol,
            sug.strike,
            preview_summary(&near_res),
            preview_summary(&far_res),
            if net >= 0.0 { "credit" } else { "debit" },
            net.abs(),
        );

        let note = format!(
            "close {:.1}{right} {near_expiry}@{near_cost:.2} / open {far_strike:.1}{right} {far_expiry}@{far_credit:.2}; net {}{:.2}",
            sug.strike,
            if net >= 0.0 { "+" } else { "-" },
            net.abs(),
        );
        store
            .record(&NewJournalEntry {
                action: "Roll".into(),
                status: "previewed".into(),
                premium: Some(net * 100.0 * sug.quantity as f64),
                note: Some(note),
                ..roll_leg_journal(&sug.symbol, right, far_strike, &far_expiry, sug.quantity, far_credit)
            })
            .await?;
        self.journal = store.recent_journal(200).await?;
        Ok(())
    }

    /// Execute a roll by **closing first**: submit the buy-to-close near leg, then
    /// record a [`PendingRoll`] so the sell-to-open far leg is sent only once the
    /// close is *confirmed filled* (in [`App::apply_order_event`]). Submitting the
    /// open before the close fills could leave both shorts live (double exposure),
    /// so we never do that. Auto-disarms once the close goes live.
    async fn execute_roll(
        &mut self,
        ibkr: &Ibkr,
        sug: &Suggestion,
        to_expiry: NaiveDate,
        to_strike: f64,
        store: &Store,
    ) -> Result<()> {
        if self.is_symbol_blocked(&sug.symbol) {
            // Rolling opens a new short the account isn't permitted to take, which
            // would close the near leg and then fail to re-open — leave it to a
            // plain close instead. (Suggestions already filter these out.)
            self.status =
                format!("roll {}: blocked for new positions — close it instead", sug.symbol);
            return Ok(());
        }
        if self.pending_rolls.iter().any(|p| p.symbol == sug.symbol) {
            // A roll close is already in flight for this symbol; re-executing
            // would submit a second buy-to-close and risk over-closing the short.
            self.status =
                format!("roll {}: already in flight — wait for the close to fill", sug.symbol);
            return Ok(());
        }
        let right = right_char(sug.right);
        let near_expiry = sug.expiry.format("%Y%m%d").to_string();
        let today = Local::now().date_naive();

        // Resolve + price the new leg before touching anything. Abort cleanly if
        // we can't (we won't close a position we can't re-open as intended).
        let Some((far_expiry, far_strike, far_credit)) =
            resolve_roll_target(ibkr, &sug.symbol, right, to_expiry, to_strike, today).await?
        else {
            self.status = format!(
                "roll {}: can't resolve/price the new leg — aborted, nothing transmitted",
                sug.symbol
            );
            return Ok(());
        };

        // Re-price the near (close) leg now: `sug.limit_price` was captured at
        // plan time, but a tested short moves, so a stale limit risks an
        // unmarketable close (the roll would hang) and a debit/credit check
        // against mismatched-vintage prices. Fall back to the plan price only if
        // the leg can't be quoted right now.
        let near_cost = price_leg(ibkr, &sug.symbol, &near_expiry, sug.strike, right)
            .await
            .unwrap_or(sug.limit_price);

        // Refuse a net-debit roll: a roll is meant to bring in credit, so if the
        // new leg pays less than the buy-to-close costs, don't silently close the
        // short and pay to re-open. The user can close manually if they must
        // defend. (Preview still shows the debit so the choice is informed.)
        if far_credit < near_cost {
            self.status = format!(
                "roll {}: market offers only a net debit (${:.2}/sh) — not transmitted; close manually to defend",
                sug.symbol,
                near_cost - far_credit
            );
            return Ok(());
        }

        // Verify the new leg is actually *openable* (margin / permission) with a
        // what-if BEFORE closing anything — a priced-but-rejected far leg would
        // otherwise leave the account flat after the close fills.
        let far = OptionOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &far_expiry,
            strike: far_strike,
            right,
            side: Side::Sell,
            quantity: sug.quantity,
            limit: far_credit,
        };
        if let Err(e) = ibkr.submit_or_preview(&far, true).await {
            self.status = format!(
                "roll {}: new leg not openable ({e}) — aborted, nothing transmitted",
                sug.symbol
            );
            return Ok(());
        }

        let near = OptionOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &near_expiry,
            strike: sug.strike,
            right,
            side: Side::Buy,
            quantity: sug.quantity,
            limit: near_cost,
        };
        match ibkr.submit_or_preview(&near, false).await {
            Ok(OrderOutcome::Submitted(close_id)) => {
                self.armed = false; // a live order went out
                // Register the pending roll *before* any further await. Order
                // events are processed only at the run loop's select point (never
                // while this handler runs), so the close's fill can't be observed
                // before this; registering first keeps that guarantee robust.
                let pending = PendingRoll {
                    symbol: sug.symbol.clone(),
                    right,
                    near_strike: sug.strike,
                    near_expiry: near_expiry.clone(),
                    to_strike: far_strike,
                    to_expiry: far_expiry.clone(),
                    quantity: sug.quantity,
                    far_limit: far_credit,
                    close_oid: close_id.clone(),
                    reconstructed: false,
                };
                // Persist before in-memory push so a crash before the fill still
                // leaves a durable record to reconstruct/reconcile from.
                if let Err(e) = store.add_pending_roll(&pending.to_row()).await {
                    tracing::warn!("persist pending roll {}: {e}", pending.close_oid);
                }
                self.pending_rolls.push(pending);
                store
                    .record(&NewJournalEntry {
                        action: "RollClose".into(),
                        status: "submitted".into(),
                        ibkr_order_id: Some(close_id.clone()),
                        ..roll_leg_journal(&sug.symbol, right, sug.strike, &near_expiry, sug.quantity, near_cost)
                    })
                    .await?;
                self.status = format!(
                    "roll {}: closing near leg (id {close_id}); opening {far_strike:.1}{right} {far_expiry}@{far_credit:.2} once it fills",
                    sug.symbol
                );
            }
            Ok(OrderOutcome::Preview(_)) => {
                self.status = "roll: execute unexpectedly returned a preview".into();
            }
            Err(e) => {
                store
                    .record(&NewJournalEntry {
                        action: "RollClose".into(),
                        status: "rejected".into(),
                        note: Some(e.to_string()),
                        ..roll_leg_journal(&sug.symbol, right, sug.strike, &near_expiry, sug.quantity, sug.limit_price)
                    })
                    .await?;
                self.status = format!("roll aborted: couldn't close {} near leg: {e}", sug.symbol);
            }
        }
        self.journal = store.recent_journal(200).await?;
        Ok(())
    }

    /// Transmit the sell-to-open leg of a roll whose close has filled. Returns a
    /// status line; journals the open leg's outcome.
    /// Transmit the sell-to-open leg of a roll whose close has filled. The
    /// persisted pending-roll row is removed only on a *successful* open, so a
    /// rejected open leaves a durable record that a later refresh/restart retries
    /// (rather than silently stranding the account flat). Returns a status line.
    async fn open_rolled_leg(&mut self, pr: &PendingRoll, store: &Store) -> Result<String> {
        let Some(ibkr) = self.ibkr.clone() else {
            // Keep the persisted row so a connected session retries.
            return Ok(format!("roll {}: not connected — new leg not opened", pr.symbol));
        };
        let far = OptionOrder {
            symbol: &pr.symbol,
            expiry_yyyymmdd: &pr.to_expiry,
            strike: pr.to_strike,
            right: pr.right,
            side: Side::Sell,
            quantity: pr.quantity,
            limit: pr.far_limit,
        };
        match ibkr.submit_or_preview(&far, false).await {
            Ok(OrderOutcome::Submitted(far_id)) => {
                // Open succeeded — the roll is complete; drop the durable record.
                let _ = store.remove_pending_roll(&pr.close_oid).await;
                store
                    .record(&NewJournalEntry {
                        action: "RollOpen".into(),
                        status: "submitted".into(),
                        ibkr_order_id: Some(far_id.clone()),
                        ..roll_leg_journal(&pr.symbol, pr.right, pr.to_strike, &pr.to_expiry, pr.quantity, pr.far_limit)
                    })
                    .await?;
                Ok(format!(
                    "rolled {}: close filled, opened {:.1}{} {}→{far_id}",
                    pr.symbol, pr.to_strike, pr.right, pr.to_expiry
                ))
            }
            Ok(OrderOutcome::Preview(_)) => {
                Ok(format!("roll {}: open leg unexpectedly returned a preview", pr.symbol))
            }
            Err(e) => {
                // Leave the persisted row in place so the open is retried later.
                store
                    .record(&NewJournalEntry {
                        action: "RollOpen".into(),
                        status: "rejected".into(),
                        note: Some(e.to_string()),
                        ..roll_leg_journal(&pr.symbol, pr.right, pr.to_strike, &pr.to_expiry, pr.quantity, pr.far_limit)
                    })
                    .await?;
                Ok(format!(
                    "roll {}: close filled but FAILED to open new leg — now FLAT, will retry: {e}",
                    pr.symbol
                ))
            }
        }
    }

    /// Reconcile *reconstructed* pending rolls (from a prior session) against the
    /// broker's live open orders + positions. For a reconstructed roll whose
    /// close leg is no longer open, the close terminated while we weren't
    /// tracking it; we tell a fill from a cancel by the *position*:
    /// - the near short is gone → the close filled → complete the roll (open the
    ///   far leg), the restart-survival path the table exists for;
    /// - the near short is still held → the close didn't fill → drop the roll and
    ///   leave the short for normal management.
    ///
    /// This-session rolls (`reconstructed == false`) are left to the order stream.
    async fn reconcile_pending_rolls(
        &mut self,
        open: &[OpenOrderInfo],
        positions: &[PositionRow],
        store: &Store,
    ) {
        if !self.pending_rolls.iter().any(|p| p.reconstructed) {
            return;
        }
        let open_ids: std::collections::HashSet<&str> =
            open.iter().map(|o| o.order_id.as_str()).collect();

        // Partition out reconstructed rolls whose close is no longer open.
        let mut resolved: Vec<PendingRoll> = Vec::new();
        for pr in std::mem::take(&mut self.pending_rolls) {
            if pr.reconstructed && !open_ids.contains(pr.close_oid.as_str()) {
                resolved.push(pr);
            } else {
                self.pending_rolls.push(pr);
            }
        }

        for pr in resolved {
            let near_held =
                position_has_short(positions, &pr.symbol, pr.right, pr.near_strike, &pr.near_expiry);
            if near_held {
                // Close didn't fill — the short is still held; abandon the roll and
                // leave the short for normal management.
                let _ = store.remove_pending_roll(&pr.close_oid).await;
                let _ = store
                    .update_journal_status(
                        &pr.close_oid,
                        "cancelled",
                        Some("roll close didn't fill (resolved offline); short still open"),
                    )
                    .await;
                self.status =
                    format!("roll {}: close didn't fill while offline — short still open", pr.symbol);
            } else {
                // The near short is gone — the close filled while we were away, so
                // complete the roll by transmitting the sell-to-open leg now.
                // open_rolled_leg removes the persisted row only on a successful
                // open, so a rejected open is retried on the next reconcile.
                let _ = store
                    .update_journal_status(&pr.close_oid, "filled", Some("close filled while offline"))
                    .await;
                self.status = self
                    .open_rolled_leg(&pr, store)
                    .await
                    .unwrap_or_else(|e| format!("roll {}: open leg error: {e}", pr.symbol));
            }
        }
    }

    /// Apply a live order-activity event from the broker stream: transition the
    /// matching journal row and surface it. A fill changes holdings, so it also
    /// triggers a refresh of positions / wheel state / suggestions.
    pub async fn apply_order_event(&mut self, ev: OrderEvent, store: &Store) -> Result<()> {
        match ev {
            OrderEvent::Status { order_id, status, filled, avg_fill_price, .. } => {
                let oid = order_id.to_string();
                // `None` for working states (Submitted/PreSubmitted/Pending*); a
                // partial fill arrives as one of those with `filled > 0`.
                let journal_status = journal_status_for(&status);

                // Record any terminal transition on our journal row, and learn
                // whether this order is one we placed.
                let is_ours = if let Some(js) = journal_status {
                    let note = (js == "filled")
                        .then(|| format!("filled {filled:.0} @ {avg_fill_price:.2}"));
                    store.update_journal_status(&oid, js, note.as_deref()).await? > 0
                } else {
                    store.journal_order_exists(&oid).await?
                };
                // A *reconstructed* roll's `close_oid` is from a prior session and
                // IBKR recycles order ids, so only trust it as a pending-roll match
                // when the journal also confirms the id is ours; this-session rolls
                // (`!reconstructed`) can't suffer id recycling.
                let is_pending_roll = self
                    .pending_rolls
                    .iter()
                    .any(|p| p.close_oid == oid && (is_ours || !p.reconstructed));
                if !is_ours && !is_pending_roll {
                    return Ok(()); // an order from elsewhere (e.g. placed in TWS)
                }

                // Complete or abandon a pending roll on its close leg's terminal
                // status. Opening the far leg only after the close *fully fills*
                // is what prevents a doubled-up position; a partial/working close
                // fill is left pending until it completes.
                let mut roll_status: Option<String> = None;
                if let Some(idx) = self
                    .pending_rolls
                    .iter()
                    .position(|p| p.close_oid == oid && (is_ours || !p.reconstructed))
                {
                    match journal_status {
                        Some("filled") => {
                            // open_rolled_leg removes the persisted row on success
                            // (and leaves it for retry on failure).
                            let pr = self.pending_rolls.remove(idx);
                            roll_status = Some(self.open_rolled_leg(&pr, store).await?);
                        }
                        Some(js @ ("cancelled" | "rejected")) => {
                            let pr = self.pending_rolls.remove(idx);
                            let _ = store.remove_pending_roll(&pr.close_oid).await;
                            // A close can cancel *after* a partial fill, so report
                            // honestly: contracts may have traded. The reload below
                            // (filled > 0 ⇒ traded) re-syncs the residual short for
                            // normal management.
                            roll_status = Some(if filled > 0.0 {
                                format!(
                                    "roll {}: close {js} after partial fill ({filled:.0} closed); residual short re-managed",
                                    pr.symbol
                                )
                            } else {
                                format!(
                                    "roll {}: close {js}; new leg not opened (original short unchanged)",
                                    pr.symbol
                                )
                            });
                        }
                        _ => {}
                    }
                }

                // A plain working ack (no terminal status, nothing filled, not a
                // roll) needs no UI change — ignore it to avoid status noise.
                let traded = journal_status == Some("filled") || filled > 0.0;
                if journal_status.is_none() && !traded && roll_status.is_none() {
                    return Ok(());
                }

                // Refresh holdings whenever contracts actually traded — a terminal
                // fill *or* a partial fill on a still-working status — so live
                // exposure is never left stale; otherwise just refresh the journal.
                if self.ibkr.is_some() && traded {
                    self.reload(store).await?; // also reloads the journal
                } else {
                    self.journal = store.recent_journal(200).await?;
                }

                self.status = roll_status.unwrap_or_else(|| match journal_status {
                    Some(js) => format!("order {oid}: {js}"),
                    None => format!("order {oid}: partial fill {filled:.0}"),
                });
            }
            OrderEvent::Notice(msg) => {
                self.status = format!("broker: {msg}");
            }
        }
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
    ///
    /// When connected, broker holdings drive the wheel-state machine: positions
    /// are reconciled into the local store *before* suggestions are computed, so
    /// each symbol is advised in the leg it is actually in (entry, manage, or
    /// covered call) rather than always being treated as idle.
    async fn reload(&mut self, store: &Store) -> Result<()> {
        self.watchlist = store.list_watchlist().await?;
        self.journal = store.recent_journal(200).await?;
        let today = Local::now().date_naive();

        if let Some(ibkr) = self.ibkr.clone() {
            self.account = ibkr.account_summary().await.ok();
            // Probe tradability for still-unknown symbols *before* planning, then
            // refresh the watchlist, so a symbol the probe blocks (PRIIPs /
            // permissions) is excluded from this pass's suggestions rather than
            // surfacing an executable order for an instrument we can't trade.
            probe_unknown_tradability(&ibkr, store, &self.watchlist, today).await;
            self.watchlist = store.list_watchlist().await?;
            // Only sync + recompute on a *complete* positions snapshot. A failed
            // or partial fetch must not be mistaken for "all positions closed",
            // which would wipe local state and suggest new entries against
            // symbols that still have open positions — so we keep the last known
            // state untouched on error.
            match ibkr.positions().await {
                Ok(positions) => {
                    self.broker_positions = positions;
                    sync_wheel_state(store, &self.broker_positions).await;
                    // Authoritative open orders drive both: which symbols to skip
                    // (a live order in flight) and reconciling pending rolls. On a
                    // failed snapshot, fall back to the journal's "submitted" rows.
                    let working = match ibkr.open_orders_snapshot().await {
                        Ok(open) => {
                            let bp = self.broker_positions.clone();
                            self.reconcile_pending_rolls(&open, &bp, store).await;
                            let mut w: Vec<String> = open.into_iter().map(|o| o.symbol).collect();
                            // Also suppress symbols with an in-flight roll whose
                            // close order may not be in the snapshot yet (just
                            // submitted), closing the broker-ack race window.
                            w.extend(self.pending_rolls.iter().map(|p| p.symbol.clone()));
                            w
                        }
                        Err(e) => {
                            tracing::warn!("open orders unavailable ({e}); using journal fallback");
                            store.symbols_with_working_orders().await.unwrap_or_default()
                        }
                    };
                    self.suggestions = live_suggestions(
                        &ibkr,
                        store,
                        &self.watchlist,
                        &self.broker_positions,
                        &working,
                        &self.cfg,
                        today,
                    )
                    .await;
                }
                Err(e) => {
                    // Positions are unknown: preserve the stored wheel state for
                    // the dashboard, but clear suggestions so no stale, still-
                    // executable action survives against an account we can no
                    // longer see.
                    tracing::warn!("positions fetch failed; clearing suggestions, keeping stored state: {e}");
                    self.suggestions.clear();
                    self.status =
                        "broker positions unavailable — suggestions cleared until refresh".into();
                }
            }
        } else {
            let symbols: Vec<String> = self
                .watchlist
                .iter()
                .filter(|r| r.is_enabled())
                .map(|r| r.symbol.clone())
                .collect();
            self.suggestions = demo::demo_suggestions(&symbols, &self.cfg.engine, today);
        }

        // Load the (possibly just-synced) wheel positions last so the dashboard
        // reflects holdings.
        self.positions = store.list_positions().await?;

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

    /// Count of tracked symbols currently in an open wheel leg (not `Idle`).
    /// Parsing through [`WheelState`] also folds any unrecognized stored state to
    /// `Idle` rather than miscounting it as open.
    pub fn open_position_count(&self) -> usize {
        self.positions
            .iter()
            .filter(|p| WheelState::parse(&p.state) != WheelState::Idle)
            .count()
    }

    /// Whether the tradability probe has marked this symbol blocked.
    fn is_symbol_blocked(&self, symbol: &str) -> bool {
        self.watchlist
            .iter()
            .any(|w| w.symbol == symbol && w.tradable == Some(0))
    }
}

fn initial_status(connected: bool) -> String {
    if connected {
        "connected — `r` refreshes live data, `p` previews, `A` arms, `x` executes".into()
    } else {
        "offline — showing demo data. Start IB Gateway and configure config.toml to connect.".into()
    }
}

/// Map an IBKR order-status string to the journal's vocabulary, or `None` for
/// non-terminal "working" states (PreSubmitted, Submitted, Pending*) that don't
/// warrant a journal change. See `ibapi`'s `OrderStatus::status` docs.
fn journal_status_for(ibkr_status: &str) -> Option<&'static str> {
    match ibkr_status {
        "Filled" => Some("filled"),
        "Cancelled" | "ApiCancelled" => Some("cancelled"),
        "Inactive" => Some("rejected"),
        _ => None,
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

/// A base journal entry for one leg of a roll. The caller fills `action`,
/// `status`, and (for live legs) `ibkr_order_id` / `note` via struct update.
fn roll_leg_journal(
    symbol: &str,
    right: &str,
    strike: f64,
    expiry: &str,
    quantity: i32,
    limit: f64,
) -> NewJournalEntry {
    NewJournalEntry {
        symbol: symbol.to_string(),
        action: String::new(),
        right: Some(right.to_string()),
        strike: Some(strike),
        expiry: Some(expiry.to_string()),
        quantity: quantity as i64,
        limit_price: Some(limit),
        status: String::new(),
        ibkr_order_id: None,
        premium: None,
        note: None,
    }
}

/// One-shot price for an option leg (used to value a roll's new leg): model
/// price if present, else last, rounded to the cent. `None` if unpriced.
async fn price_leg(ibkr: &Ibkr, symbol: &str, expiry: &str, strike: f64, right: &str) -> Option<f64> {
    let snap = ibkr.option_snapshot(symbol, expiry, strike, right).await.ok()?;
    let price = snap.comp.as_ref().and_then(|c| c.option_price).or(snap.last)?;
    (price > 0.0).then(|| round_cents(price))
}

/// One-line summary of a what-if leg for the status bar (margin or error).
fn preview_summary(res: &Result<OrderOutcome>) -> String {
    match res {
        Ok(OrderOutcome::Preview(state)) => format!(
            "margin {}",
            state
                .initial_margin_after
                .map(|v| format!("${v:.0}"))
                .unwrap_or_else(|| "?".into())
        ),
        Ok(OrderOutcome::Submitted(_)) => "?".into(),
        Err(e) => format!("err: {e}"),
    }
}

/// Reconcile broker holdings into the local `wheel_positions` store.
///
/// Every symbol that has a current holding *or* an already-tracked row is
/// re-derived (so a closed position falls back to `Idle`), preserving each
/// row's `cumulative_premium` (which the broker can't report).
async fn sync_wheel_state(store: &Store, broker_positions: &[PositionRow]) {
    use std::collections::BTreeSet;
    let mut symbols: BTreeSet<String> =
        broker_positions.iter().map(|p| p.symbol.clone()).collect();
    if let Ok(existing) = store.list_positions().await {
        symbols.extend(existing.into_iter().map(|p| p.symbol));
    }
    for symbol in symbols {
        let r = positions::reconcile(&symbol, broker_positions);
        let (shares, cost_basis) = r.shares.map_or((0, 0.0), |s| (s.shares, s.cost_basis));
        if let Err(e) = store
            .upsert_wheel_state(&symbol, r.state.as_str(), shares, cost_basis)
            .await
        {
            tracing::warn!("sync wheel state for {symbol}: {e}");
        }
    }
}

/// Probe tradability (EU/PRIIPs permission) for any enabled watchlist symbol
/// whose status is still unknown, persisting Allowed/Blocked. One-shot per
/// symbol: once set it isn't re-probed, so the cost is paid once. Uses a far-OTM
/// put what-if (never transmitted), mirroring the spike's probe.
async fn probe_unknown_tradability(
    ibkr: &Ibkr,
    store: &Store,
    watchlist: &[WatchlistRow],
    today: NaiveDate,
) {
    for w in watchlist
        .iter()
        .filter(|w| w.is_enabled() && w.tradable.is_none())
    {
        if let Err(e) = probe_one_tradability(ibkr, store, w, today).await {
            tracing::warn!("tradability probe for {}: {e}", w.symbol);
        }
    }
}

/// Probe and persist one symbol's tradability. A definitively optionless symbol
/// is marked blocked; transient failures (no expiry / no spot) leave it unknown
/// so a later refresh retries.
async fn probe_one_tradability(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    today: NaiveDate,
) -> Result<()> {
    let symbol = w.symbol.as_str();
    let conid = resolve_conid(ibkr, store, w).await?;

    let chain = ibkr.option_chain(symbol, conid).await?;
    // An empty chain is more likely a transient/timed-out fetch than a stock
    // with no options at all, so leave it unknown and retry rather than sticking
    // a permanent "blocked".
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(());
    }

    let Some((expiry, _)) = pick_expiry(&chain.expirations, today, 35) else {
        return Ok(()); // no future expiry right now — retry next refresh
    };
    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    let Some(strike) = far_otm_put_strike(&chain.strikes, spot) else {
        return Ok(());
    };

    match ibkr.tradability(symbol, &expiry, strike).await {
        Tradability::Allowed { .. } => store.set_tradable(symbol, true, None).await?,
        // Only persist "blocked" for a recognized permission/PRIIPs rejection.
        // A transient what-if failure (timeout, connection, no market data) is
        // left unknown so a later refresh retries instead of blocking forever.
        Tradability::Blocked(reason) if is_permission_block(&reason) => {
            store.set_tradable(symbol, false, Some(&reason)).await?;
        }
        Tradability::Blocked(reason) => {
            tracing::info!("{symbol}: tradability probe inconclusive ({reason}); will retry");
        }
    }
    Ok(())
}

/// Whether a what-if rejection reads like a *trading-permission* block (PRIIPs /
/// missing entitlement) — a durable "no" — as opposed to a transient failure we
/// should retry. Deliberately conservative: unmatched reasons stay "unknown".
///
/// Market-data permission errors are explicitly *not* trade blocks: a user can
/// be entitled to trade an instrument while lacking its data subscription.
fn is_permission_block(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    if r.contains("market data") || r.contains("market-data") {
        return false;
    }
    [
        "priips",
        "kid",
        "prohibited",
        "trading permission",
        "not allowed",
        "not permitted",
        "professional",
    ]
    .iter()
    .any(|kw| r.contains(kw))
}

/// A clearly-OTM listed put strike for a permission probe: the strike nearest
/// 85% of spot, or — with no spot quote — the median listed strike.
fn far_otm_put_strike(strikes: &[f64], spot: f64) -> Option<f64> {
    let target = if spot > 0.0 {
        spot * PROBE_OTM_FRACTION
    } else {
        let mut s: Vec<f64> = strikes.iter().copied().filter(|k| *k > 0.0).collect();
        if s.is_empty() {
            return None;
        }
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        s[s.len() / 2]
    };
    nearest_strike(strikes, target)
}

/// The listed strike nearest `target` (positive strikes only).
fn nearest_strike(strikes: &[f64], target: f64) -> Option<f64> {
    strikes
        .iter()
        .copied()
        .filter(|k| *k > 0.0)
        .min_by(|a, b| {
            (a - target)
                .abs()
                .partial_cmp(&(b - target).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Resolve the engine's *ideal* roll target (`to_expiry`, `to_strike`) to a
/// real **listed** contract and price it: nearest listed expiry to the ideal
/// DTE, nearest listed strike, and its current credit. `None` if the chain is
/// empty or the leg can't be priced. Without this, the target often falls on a
/// non-trading date and the order would reference a nonexistent contract.
async fn resolve_roll_target(
    ibkr: &Ibkr,
    symbol: &str,
    right: &str,
    to_expiry: NaiveDate,
    to_strike: f64,
    today: NaiveDate,
) -> Result<Option<(String, f64, f64)>> {
    let conid = ibkr.underlying_contract_id(symbol).await?;
    let chain = ibkr.option_chain(symbol, conid).await?;
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(None);
    }
    let target_dte = (to_expiry - today).num_days().max(1);
    let Some((expiry, _)) = pick_expiry(&chain.expirations, today, target_dte) else {
        return Ok(None);
    };
    let Some(strike) = nearest_strike(&chain.strikes, to_strike) else {
        return Ok(None);
    };
    let Some(credit) = price_leg(ibkr, symbol, &expiry, strike, right).await else {
        return Ok(None);
    };
    Ok(Some((expiry, strike, credit)))
}

/// Owned per-symbol inputs for one planning pass; [`SymbolContext`] borrows the
/// quote vec from this, so instances are kept alive across [`engine::plan`].
struct SymbolInputs {
    symbol: String,
    state: WheelState,
    underlying: UnderlyingQuote,
    quotes: Vec<OptionQuote>,
    open_short: Option<(OpenShortOption, OptionQuote)>,
    shares: Option<SharePosition>,
    committed_call_contracts: i32,
    max_collateral: f64,
}

/// Compute suggestions across the enabled watchlist using live data, with each
/// symbol advised in the wheel leg its holdings put it in.
async fn live_suggestions(
    ibkr: &Ibkr,
    store: &Store,
    watchlist: &[WatchlistRow],
    broker_positions: &[PositionRow],
    working: &[String],
    cfg: &Config,
    today: NaiveDate,
) -> Vec<Suggestion> {
    let active: Vec<&WatchlistRow> = watchlist.iter().filter(|w| w.is_enabled()).collect();
    if active.is_empty() {
        return Vec::new();
    }
    // Size new-entry collateral against the symbols actually eligible to open
    // one (enabled and not blocked); blocked symbols are managed only.
    let openable = active.iter().filter(|w| w.tradable != Some(0)).count().max(1);
    let budget = (cfg.guardrails.max_total_deployed / openable as f64).max(1000.0);

    // `working` = symbols with a live broker order; skip them entirely this pass
    // so we never stack a second action (e.g. a fresh entry while a roll-open is
    // still working, or a duplicate CSP) on a symbol with an in-flight order.
    let mut inputs: Vec<SymbolInputs> = Vec::with_capacity(active.len());
    for &w in &active {
        if working.iter().any(|s| s == &w.symbol) {
            continue;
        }
        let reconciled = positions::reconcile(&w.symbol, broker_positions);
        // A blocked symbol (PRIIPs / no permission) may still hold an open short
        // that we must be able to close, so keep managing existing positions;
        // only suppress *new opening* legs (entry / covered call). Rolls — which
        // open a new leg the account can't take — are filtered out below.
        let manages_existing =
            matches!(reconciled.state, WheelState::ShortPut | WheelState::ShortCall);
        if w.tradable == Some(0) && !manages_existing {
            continue;
        }
        match gather_inputs(ibkr, store, w, &reconciled, &cfg.engine, today, budget).await {
            Ok(Some(si)) => inputs.push(si),
            Ok(None) => {}
            Err(e) => tracing::warn!("live inputs for {}: {e}", w.symbol),
        }
    }

    let contexts: Vec<SymbolContext> = inputs
        .iter()
        .map(|si| SymbolContext {
            symbol: si.symbol.clone(),
            state: si.state,
            underlying: si.underlying,
            option_quotes: &si.quotes,
            open_short: si.open_short.clone(),
            shares: si.shares,
            committed_call_contracts: si.committed_call_contracts,
            max_collateral: si.max_collateral,
        })
        .collect();

    let mut suggestions = engine::plan(&contexts, &cfg.engine, today).suggestions;
    // A blocked symbol can be closed but not (re)opened, so drop rolls (which
    // open a new leg) for it; its buy-to-close take-profit action still stands.
    suggestions.retain(|s| {
        !(matches!(s.kind, ActionKind::Roll { .. })
            && watchlist
                .iter()
                .any(|w| w.symbol == s.symbol && w.tradable == Some(0)))
    });
    suggestions
}

/// Fetch the market data the engine needs for one symbol, dispatched on the
/// wheel leg its holdings put it in:
/// - `Idle` → an OTM put chain for a new cash-secured put
/// - `LongShares` → an OTM call chain for a covered call
/// - `ShortPut` / `ShortCall` → a fresh quote for the open short, to manage it
async fn gather_inputs(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    reconciled: &positions::ReconciledPosition,
    cfg: &EngineConfig,
    today: NaiveDate,
    budget: f64,
) -> Result<Option<SymbolInputs>> {
    match reconciled.state {
        WheelState::Idle => {
            let Some((spot, quotes)) =
                gather_chain_quotes(ibkr, store, w, Right::Put, cfg, today).await?
            else {
                return Ok(None);
            };
            Ok(Some(SymbolInputs {
                symbol: w.symbol.clone(),
                state: WheelState::Idle,
                underlying: UnderlyingQuote { last: spot },
                quotes,
                open_short: None,
                shares: None,
                committed_call_contracts: 0,
                max_collateral: budget,
            }))
        }
        WheelState::LongShares => {
            let Some((spot, quotes)) =
                gather_chain_quotes(ibkr, store, w, Right::Call, cfg, today).await?
            else {
                return Ok(None);
            };
            Ok(Some(SymbolInputs {
                symbol: w.symbol.clone(),
                state: WheelState::LongShares,
                underlying: UnderlyingQuote { last: spot },
                quotes,
                open_short: None,
                shares: reconciled.shares,
                committed_call_contracts: reconciled.committed_call_contracts,
                max_collateral: 0.0,
            }))
        }
        WheelState::ShortPut | WheelState::ShortCall => {
            gather_manage_inputs(ibkr, w, reconciled).await
        }
    }
}

/// Resolve (and cache in the watchlist) the underlying's IBKR contract id.
async fn resolve_conid(ibkr: &Ibkr, store: &Store, w: &WatchlistRow) -> Result<i32> {
    match w.conid {
        Some(c) => Ok(c as i32),
        None => {
            let c = ibkr.underlying_contract_id(&w.symbol).await?;
            let _ = store.set_conid(&w.symbol, i64::from(c)).await;
            Ok(c)
        }
    }
}

/// Fetch spot plus a bounded set of OTM option quotes (one right) ~target DTE
/// out: the chain → nearest-to-spot OTM strike pre-filter → per-contract greek
/// snapshot pipeline shared by the entry and covered-call legs.
async fn gather_chain_quotes(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    right: Right,
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Result<Option<(f64, Vec<OptionQuote>)>> {
    let symbol = w.symbol.as_str();
    let conid = resolve_conid(ibkr, store, w).await?;

    let chain = ibkr.option_chain(symbol, conid).await?;
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(None);
    }

    let target_dte = (cfg.min_dte + cfg.max_dte) / 2;
    let Some((expiry_str, _)) = pick_expiry(&chain.expirations, today, target_dte) else {
        return Ok(None);
    };
    let expiry_date = NaiveDate::parse_from_str(&expiry_str, "%Y%m%d")?;

    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    // Keep only OTM strikes within 15% of spot, nearest-to-spot first, capped at
    // 5 so per-contract market-data requests stay bounded.
    let mut strikes: Vec<f64> = chain
        .strikes
        .iter()
        .copied()
        .filter(|k| {
            *k > 0.0 && is_otm(right, *k, spot) && moneyness(right, *k, spot) <= MAX_OTM_MONEYNESS
        })
        .collect();
    strikes.sort_by(|a, b| {
        (a - spot)
            .abs()
            .partial_cmp(&(b - spot).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    strikes.truncate(MAX_CHAIN_STRIKES);
    if strikes.is_empty() {
        return Ok(None);
    }

    let right_char = right_char(right);
    let mut quotes: Vec<OptionQuote> = Vec::with_capacity(strikes.len());
    for k in strikes {
        if let Ok(snap) = ibkr.option_snapshot(symbol, &expiry_str, k, right_char).await
            && let Some(comp) = snap.comp
        {
            let price = comp.option_price.or(snap.last).unwrap_or(0.0);
            if price > 0.0 {
                quotes.push(OptionQuote {
                    right,
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

    Ok(Some((spot, quotes)))
}

/// Fetch the inputs to manage an open short: spot + a fresh quote for the exact
/// short contract. Returns `None` (no suggestion) when the short can't be priced
/// this cycle — better to stay quiet than risk a bogus take-profit at $0.
async fn gather_manage_inputs(
    ibkr: &Ibkr,
    w: &WatchlistRow,
    reconciled: &positions::ReconciledPosition,
) -> Result<Option<SymbolInputs>> {
    let Some(short) = reconciled.open_short.clone() else {
        return Ok(None);
    };
    let symbol = w.symbol.as_str();

    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    let expiry_str = short.expiry.format("%Y%m%d").to_string();
    let snap = ibkr
        .option_snapshot(symbol, &expiry_str, short.strike, right_char(short.right))
        .await?;
    let comp = snap.comp.as_ref();
    let price = comp.and_then(|c| c.option_price).or(snap.last).unwrap_or(0.0);
    if price <= 0.0 {
        tracing::info!("{symbol}: open short unpriced this cycle; skipping management");
        return Ok(None);
    }

    let quote = OptionQuote {
        right: short.right,
        strike: short.strike,
        expiry: short.expiry,
        bid: price,
        ask: price,
        delta: comp.and_then(|c| c.delta),
        implied_volatility: comp.and_then(|c| c.implied_volatility),
        open_interest: None,
        volume: None,
    };

    Ok(Some(SymbolInputs {
        symbol: symbol.to_string(),
        state: reconciled.state,
        underlying: UnderlyingQuote { last: spot },
        quotes: Vec::new(),
        open_short: Some((short, quote)),
        shares: reconciled.shares,
        committed_call_contracts: reconciled.committed_call_contracts,
        max_collateral: 0.0,
    }))
}

/// IBKR right code for a snapshot/order request.
fn right_char(right: Right) -> &'static str {
    match right {
        Right::Put => "P",
        Right::Call => "C",
    }
}

/// Whether broker `positions` still hold a *short* option matching this leg.
/// `right` is an IBKR code (`"P"`/`"C"`); IBKR may report `right` as `PUT`/`CALL`
/// too, so we match on the leading letter.
fn position_has_short(
    positions: &[PositionRow],
    symbol: &str,
    right: &str,
    strike: f64,
    expiry: &str,
) -> bool {
    positions.iter().any(|p| {
        p.symbol == symbol
            && p.security_type == "Option"
            && p.position < 0.0
            && (p.strike - strike).abs() < 1e-6
            && p.expiry == expiry
            && p.right.to_ascii_uppercase().starts_with(right)
    })
}

/// Whether `strike` is out-of-the-money for `right` given `spot`.
fn is_otm(right: Right, strike: f64, spot: f64) -> bool {
    match right {
        Right::Put => strike < spot,
        Right::Call => strike > spot,
    }
}

/// OTM moneyness as a positive fraction of spot (0 at-the-money).
fn moneyness(right: Right, strike: f64, spot: f64) -> f64 {
    if spot <= 0.0 {
        return f64::INFINITY;
    }
    match right {
        Right::Put => (spot - strike) / spot,
        Right::Call => (strike - spot) / spot,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[tokio::test]
    async fn app_boots_offline_with_demo_data() {
        let store = Store::open_in_memory().await.unwrap();
        store.add_symbol("AAPL", "STK").await.unwrap();
        let app = App::new(Config::default(), None, &store).await.unwrap();
        assert!(!app.connected);
        assert!(!app.armed);
        assert_eq!(app.open_position_count(), 0);
        // Offline → the real engine runs over demo chains, so the enabled symbol
        // yields at least one suggestion.
        assert!(!app.suggestions.is_empty());
    }

    // --- shared test helpers ---

    async fn offline_app(store: &Store) -> App {
        App::new(Config::default(), None, store).await.unwrap()
    }

    fn sell_put(qty: i32) -> Suggestion {
        let mut s = sug(ActionKind::SellPut, Right::Put);
        s.quantity = qty;
        s
    }

    async fn seed_submitted(store: &Store, symbol: &str, oid: &str) {
        store
            .record(&NewJournalEntry {
                symbol: symbol.into(),
                action: "SellPut".into(),
                quantity: 1,
                limit_price: Some(1.80),
                status: "submitted".into(),
                ibkr_order_id: Some(oid.into()),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    fn status_event(order_id: i32, status: &str, filled: f64, avg: f64) -> OrderEvent {
        OrderEvent::Status {
            order_id,
            status: status.into(),
            filled,
            remaining: 0.0,
            avg_fill_price: avg,
        }
    }

    fn watch_row(symbol: &str, tradable: Option<i64>) -> WatchlistRow {
        WatchlistRow {
            symbol: symbol.into(),
            sec_type: "STK".into(),
            enabled: 1,
            tradable,
            tradable_reason: None,
            conid: None,
            notes: None,
            added_at: String::new(),
        }
    }

    fn pending(close_oid: &str, reconstructed: bool) -> PendingRoll {
        PendingRoll {
            symbol: "AAPL".into(),
            right: "P",
            near_strike: 100.0,
            near_expiry: "20260619".into(),
            to_strike: 95.0,
            to_expiry: "20260717".into(),
            quantity: 1,
            far_limit: 2.00,
            close_oid: close_oid.into(),
            reconstructed,
        }
    }

    // --- money-safety: execute_suggestion guardrails ---
    // These checks short-circuit BEFORE any broker call, so they're fully
    // testable offline. A regression here could transmit an unintended or
    // oversized live order, so each gate is pinned down.

    #[tokio::test]
    async fn execute_blocked_when_disarmed() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.armed = false;
        app.execute_suggestion(&sell_put(1), &store).await.unwrap();
        assert!(app.status.contains("disarmed"), "status: {}", app.status);
        assert!(store.recent_journal(10).await.unwrap().is_empty(), "nothing journaled");
    }

    #[tokio::test]
    async fn execute_blocked_when_read_only() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.armed = true;
        app.cfg.guardrails.read_only = true;
        app.execute_suggestion(&sell_put(1), &store).await.unwrap();
        assert!(app.status.contains("read_only"), "status: {}", app.status);
        assert!(store.recent_journal(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn execute_blocked_when_quantity_exceeds_max() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.armed = true;
        app.cfg.guardrails.read_only = false;
        app.cfg.guardrails.max_contracts_per_order = 2;
        app.execute_suggestion(&sell_put(5), &store).await.unwrap();
        assert!(
            app.status.contains("exceeds max_contracts_per_order"),
            "status: {}",
            app.status
        );
        assert!(store.recent_journal(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn execute_passes_guards_then_halts_without_connection() {
        // Armed + not read-only + within the contract cap → all guards pass; with
        // no broker it must halt at the connection check (proving the guards did
        // NOT block it, and that nothing transmits offline).
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.armed = true;
        app.cfg.guardrails.read_only = false;
        app.cfg.guardrails.max_contracts_per_order = 10;
        app.execute_suggestion(&sell_put(1), &store).await.unwrap();
        assert!(app.status.contains("not connected"), "status: {}", app.status);
        assert!(store.recent_journal(10).await.unwrap().is_empty());
    }

    // --- order-event → journal transitions (the fill/cancel tracking path) ---

    #[tokio::test]
    async fn fill_event_transitions_journal_to_filled() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "42").await;
        app.apply_order_event(status_event(42, "Filled", 1.0, 1.85), &store)
            .await
            .unwrap();
        let row = store
            .recent_journal(10)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.ibkr_order_id.as_deref() == Some("42"))
            .unwrap();
        assert_eq!(row.status, "filled");
        assert_eq!(row.note.as_deref(), Some("filled 1 @ 1.85"));
        assert!(app.status.contains("filled"), "status: {}", app.status);
    }

    #[tokio::test]
    async fn cancel_event_transitions_journal_to_cancelled() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "7").await;
        app.apply_order_event(status_event(7, "Cancelled", 0.0, 0.0), &store)
            .await
            .unwrap();
        let row = store.recent_journal(10).await.unwrap().into_iter().next().unwrap();
        assert_eq!(row.status, "cancelled");
    }

    #[tokio::test]
    async fn inactive_event_is_recorded_as_rejected() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "9").await;
        app.apply_order_event(status_event(9, "Inactive", 0.0, 0.0), &store)
            .await
            .unwrap();
        assert_eq!(store.recent_journal(10).await.unwrap()[0].status, "rejected");
    }

    #[tokio::test]
    async fn event_for_foreign_order_leaves_journal_untouched() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "42").await;
        // A fill for an order id we never placed (e.g. placed directly in TWS).
        app.apply_order_event(status_event(999, "Filled", 1.0, 2.0), &store)
            .await
            .unwrap();
        assert_eq!(store.recent_journal(10).await.unwrap()[0].status, "submitted");
    }

    #[tokio::test]
    async fn working_ack_with_no_fill_is_ignored() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "42").await;
        // A plain Submitted ack (working, nothing filled) must not change anything.
        app.apply_order_event(status_event(42, "Submitted", 0.0, 0.0), &store)
            .await
            .unwrap();
        assert_eq!(store.recent_journal(10).await.unwrap()[0].status, "submitted");
    }

    #[tokio::test]
    async fn notice_event_surfaces_on_status_line() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.apply_order_event(OrderEvent::Notice("heads up".into()), &store)
            .await
            .unwrap();
        assert!(app.status.contains("broker: heads up"), "status: {}", app.status);
    }

    // --- pending-roll lifecycle ---

    #[tokio::test]
    async fn filled_close_completes_pending_roll_and_keeps_record_on_offline_open() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "50").await;
        store.add_pending_roll(&pending("50", false).to_row()).await.unwrap();
        app.pending_rolls.push(pending("50", false));

        app.apply_order_event(status_event(50, "Filled", 1.0, 5.0), &store)
            .await
            .unwrap();

        // Matched and removed from memory; offline open can't transmit, so the
        // persisted row is retained for a later retry.
        assert!(app.pending_rolls.is_empty(), "in-memory pending roll cleared");
        assert_eq!(store.list_pending_rolls().await.unwrap().len(), 1, "persisted for retry");
        assert!(app.status.contains("not connected"), "status: {}", app.status);
    }

    #[tokio::test]
    async fn cancelled_close_abandons_pending_roll() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        seed_submitted(&store, "AAPL", "60").await;
        store.add_pending_roll(&pending("60", false).to_row()).await.unwrap();
        app.pending_rolls.push(pending("60", false));

        app.apply_order_event(status_event(60, "Cancelled", 0.0, 0.0), &store)
            .await
            .unwrap();

        assert!(app.pending_rolls.is_empty());
        assert!(store.list_pending_rolls().await.unwrap().is_empty(), "persisted roll dropped");
        assert!(app.status.contains("not opened"), "status: {}", app.status);
    }

    #[tokio::test]
    async fn is_symbol_blocked_reads_watchlist() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.watchlist = vec![
            watch_row("AAPL", Some(0)), // blocked
            watch_row("MSFT", Some(1)), // allowed
            watch_row("TSLA", None),    // unknown
        ];
        assert!(app.is_symbol_blocked("AAPL"));
        assert!(!app.is_symbol_blocked("MSFT"));
        assert!(!app.is_symbol_blocked("TSLA"));
        assert!(!app.is_symbol_blocked("NVDA")); // not on the list
    }

    #[tokio::test]
    async fn open_position_count_ignores_idle_and_unknown_states() {
        let store = Store::open_in_memory().await.unwrap();
        store.upsert_wheel_state("AAPL", "ShortPut", 0, 0.0).await.unwrap();
        store.upsert_wheel_state("MSFT", "Idle", 0, 0.0).await.unwrap();
        store.upsert_wheel_state("NVDA", "garbage", 0, 0.0).await.unwrap(); // unknown → Idle
        let app = offline_app(&store).await;
        assert_eq!(app.open_position_count(), 1, "only the ShortPut counts as open");
    }

    // --- broker-positions → wheel-state store sync (Phase 1 core) ---

    fn short_put_pos(symbol: &str) -> PositionRow {
        PositionRow {
            account: "DU".into(),
            symbol: symbol.into(),
            security_type: "Option".into(),
            right: "P".into(),
            strike: 90.0,
            expiry: "20260116".into(),
            position: -1.0,
            average_cost: 150.0,
            multiplier: "100".into(),
        }
    }

    #[tokio::test]
    async fn sync_wheel_state_persists_reconciled_leg() {
        let store = Store::open_in_memory().await.unwrap();
        sync_wheel_state(&store, &[short_put_pos("AAPL")]).await;
        let row = store
            .list_positions()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.symbol == "AAPL")
            .expect("AAPL synced");
        assert_eq!(row.state, "ShortPut");
    }

    #[tokio::test]
    async fn sync_wheel_state_closes_vanished_position_but_keeps_premium() {
        let store = Store::open_in_memory().await.unwrap();
        // A previously-tracked short put with collected premium.
        store
            .upsert_position(&WheelPositionRow {
                symbol: "AAPL".into(),
                state: "ShortPut".into(),
                shares: 0,
                cost_basis: 0.0,
                cumulative_premium: 1.50,
                updated_at: String::new(),
            })
            .await
            .unwrap();
        // No broker positions anymore → the leg is closed (Idle)…
        sync_wheel_state(&store, &[]).await;
        let row = store
            .list_positions()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.symbol == "AAPL")
            .unwrap();
        assert_eq!(row.state, "Idle");
        // …but the locally-tracked premium is preserved (broker can't report it).
        assert!((row.cumulative_premium - 1.50).abs() < 1e-9);
    }

    #[test]
    fn journal_status_mapping() {
        assert_eq!(journal_status_for("Filled"), Some("filled"));
        assert_eq!(journal_status_for("Cancelled"), Some("cancelled"));
        assert_eq!(journal_status_for("ApiCancelled"), Some("cancelled"));
        assert_eq!(journal_status_for("Inactive"), Some("rejected"));
        // Non-terminal / working states are intentionally ignored.
        assert_eq!(journal_status_for("Submitted"), None);
        assert_eq!(journal_status_for("PreSubmitted"), None);
        assert_eq!(journal_status_for("PendingSubmit"), None);
        assert_eq!(journal_status_for("whatever"), None);
    }

    fn sug(kind: ActionKind, right: Right) -> Suggestion {
        Suggestion {
            symbol: "AAPL".into(),
            kind,
            right,
            strike: 100.0,
            expiry: NaiveDate::from_ymd_opt(2026, 6, 19).unwrap(),
            dte: 30,
            quantity: 1,
            limit_price: 1.0,
            delta: None,
            premium_total: 100.0,
            capital_required: 0.0,
            annualized_yield: 0.0,
            rationale: String::new(),
        }
    }

    #[test]
    fn side_and_right_covers_executable_kinds() {
        assert!(matches!(
            side_and_right(&sug(ActionKind::SellPut, Right::Put)),
            Some((Side::Sell, "P"))
        ));
        assert!(matches!(
            side_and_right(&sug(ActionKind::SellCall, Right::Call)),
            Some((Side::Sell, "C"))
        ));
        assert!(matches!(
            side_and_right(&sug(ActionKind::CloseForProfit, Right::Put)),
            Some((Side::Buy, "P"))
        ));
        assert!(matches!(
            side_and_right(&sug(ActionKind::CloseForProfit, Right::Call)),
            Some((Side::Buy, "C"))
        ));
    }

    #[test]
    fn otm_and_moneyness_by_right() {
        assert!(is_otm(Right::Put, 95.0, 100.0));
        assert!(!is_otm(Right::Put, 105.0, 100.0));
        assert!(is_otm(Right::Call, 105.0, 100.0));
        assert!(!is_otm(Right::Call, 95.0, 100.0));
        assert!((moneyness(Right::Put, 90.0, 100.0) - 0.10).abs() < 1e-9);
        assert!((moneyness(Right::Call, 110.0, 100.0) - 0.10).abs() < 1e-9);
    }

    #[test]
    fn pick_expiry_chooses_nearest_to_target() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let exps = vec![
            "20260605".to_string(), // 4 DTE
            "20260703".to_string(), // 32 DTE — nearest to 35
            "20260919".to_string(), // 110 DTE
        ];
        let (chosen, dte) = pick_expiry(&exps, today, 35).expect("an expiry");
        assert_eq!(chosen, "20260703");
        assert_eq!(dte, 32);
    }

    fn opt_pos(symbol: &str, right: &str, strike: f64, expiry: &str, position: f64) -> PositionRow {
        PositionRow {
            account: "DU1".into(),
            symbol: symbol.into(),
            security_type: "Option".into(),
            right: right.into(),
            strike,
            expiry: expiry.into(),
            position,
            average_cost: 100.0,
            multiplier: "100".into(),
        }
    }

    #[test]
    fn position_has_short_matches_leg() {
        let positions = vec![
            opt_pos("AAPL", "P", 100.0, "20260619", -1.0), // our short put
            opt_pos("AAPL", "PUT", 90.0, "20260619", 1.0), // a long put (ignored)
        ];
        assert!(position_has_short(&positions, "AAPL", "P", 100.0, "20260619"));
        // Long position is not a short.
        assert!(!position_has_short(&positions, "AAPL", "P", 90.0, "20260619"));
        // Different strike / expiry / right / symbol don't match.
        assert!(!position_has_short(&positions, "AAPL", "P", 95.0, "20260619"));
        assert!(!position_has_short(&positions, "AAPL", "C", 100.0, "20260619"));
        assert!(!position_has_short(&positions, "MSFT", "P", 100.0, "20260619"));
    }

    #[test]
    fn nearest_strike_picks_closest_listed() {
        let strikes = vec![80.0, 90.0, 95.0, 100.0];
        assert_eq!(nearest_strike(&strikes, 93.0), Some(95.0));
        assert_eq!(nearest_strike(&strikes, 81.0), Some(80.0));
        assert_eq!(nearest_strike(&[], 90.0), None);
    }

    #[test]
    fn permission_block_only_for_recognized_reasons() {
        assert!(is_permission_block("Order rejected: PRIIPs KID required"));
        assert!(is_permission_block("No trading permission for this product"));
        assert!(is_permission_block("Product not allowed for retail"));
        // Transient / unrelated failures must NOT be treated as a permanent block.
        assert!(!is_permission_block("request timed out"));
        assert!(!is_permission_block("connection reset"));
        assert!(!is_permission_block("no market data subscription"));
        // A market-data *permission* error is a data issue, not a trade block.
        assert!(!is_permission_block("No market data permissions for ISLAND"));
    }

    #[test]
    fn far_otm_put_strike_targets_85pct_of_spot() {
        let strikes = vec![70.0, 80.0, 85.0, 90.0, 95.0, 100.0, 110.0];
        // 85% of 100 = 85 → exact listed strike.
        assert_eq!(far_otm_put_strike(&strikes, 100.0), Some(85.0));
    }

    #[test]
    fn far_otm_put_strike_uses_median_without_spot() {
        let strikes = vec![70.0, 80.0, 90.0, 100.0, 110.0];
        // No spot → median listed strike (index 2 of 5).
        assert_eq!(far_otm_put_strike(&strikes, 0.0), Some(90.0));
        assert_eq!(far_otm_put_strike(&[], 0.0), None);
    }

    #[test]
    fn pick_expiry_skips_past_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let exps = vec!["20260101".to_string(), "20260630".to_string()];
        let (chosen, _) = pick_expiry(&exps, today, 35).expect("a future expiry");
        assert_eq!(chosen, "20260630");
    }
}
