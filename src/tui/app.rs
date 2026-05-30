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
use std::time::Instant;

use anyhow::Result;
use chrono::{Local, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use super::demo;
use super::schedule;
use super::live::{
    gather, live_suggestions, position_has_short, price_leg, probe_unknown_tradability,
    resolve_roll_target, right_char, structure_suggestions, sync_wheel_state, LiveData,
};
use crate::config::{Config, TradingMode, UserSettings, ZeroDteSettings};
use crate::engine::types::{
    ActionKind, LegSide, Right, StructureKind, StructureLeg, Suggestion, WheelState,
};
use crate::ibkr::{
    AccountSnapshot, ComboLeg, ComboOrder, Ibkr, OpenOrderInfo, OptionOrder, OrderEvent,
    OrderOutcome, PositionRow, Side, SpreadOrder,
};
use crate::store::{
    JournalRow, NewJournalEntry, PendingRollRow, Store, WatchlistRow, WheelPositionRow,
    ZeroDtePositionRow,
};


/// Top-level tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Watchlist,
    Suggestions,
    HedgedWheel,
    ZeroDte,
    Journal,
    Settings,
    Help,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::Dashboard,
        Tab::Watchlist,
        Tab::Suggestions,
        Tab::HedgedWheel,
        Tab::ZeroDte,
        Tab::Journal,
        Tab::Settings,
        Tab::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Watchlist => "Watchlist",
            Tab::Suggestions => "Suggestions",
            Tab::HedgedWheel => "Hedged Wheel",
            Tab::ZeroDte => "0DTE",
            Tab::Journal => "Journal",
            Tab::Settings => "Settings",
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
    /// Open the detail panel for the selected suggestion.
    OpenDetail,
    /// Close the detail panel.
    CloseDetail,
    /// Adjust the selected Settings-tab knob by one step (`+1` up, `-1` down).
    SettingAdjust(i32),
    /// Enter/leave edit mode on the selected Settings-tab knob.
    ToggleSettingEdit,
    /// Toggle the focused 0DTE slot's `automate` opt-in (the safety gate).
    ToggleAutomate,
    /// Adjust the focused 0DTE slot's max-risk budget (`+1`/`-1` step).
    SlotRisk(i32),
    /// Adjust the focused 0DTE slot's profit target (`+1`/`-1` step).
    SlotProfit(i32),
}

/// A result from an off-loop broker task, delivered to the run loop so broker
/// I/O (reconnects, data reloads) never blocks the UI thread.
pub(super) enum BrokerUpdate {
    /// An auto-reconnect attempt succeeded.
    Connected(Arc<Ibkr>),
    /// An auto-reconnect attempt failed; carries a short UI hint.
    ConnectFailed(String),
    /// A background reload finished (boxed — it's a large payload).
    Reloaded(Box<LiveData>),
}

/// All TUI state.
pub struct App {
    pub cfg: Config,
    /// `Some` when a Gateway connection succeeded at startup; `None` = offline.
    pub ibkr: Option<Arc<Ibkr>>,
    pub tab: Tab,
    pub watchlist: Vec<WatchlistRow>,
    pub suggestions: Vec<Suggestion>,
    /// Hedged Wheel suggestions (defined-risk put spreads) — the Hedged Wheel tab.
    pub hedged_suggestions: Vec<Suggestion>,
    /// 0DTE-tab structure suggestions, one per configured quadrant slot (`None`
    /// where nothing currently fits). Indexed by slot, parallel to
    /// `cfg.zerodte.slots`; the 0DTE tab's selection (`self.selected`) is a slot.
    pub zerodte_suggestions: Vec<Option<Suggestion>>,
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
    /// Channel to the run loop for off-loop broker results; wired by
    /// [`App::set_update_sender`] once the loop starts.
    update_tx: Option<mpsc::UnboundedSender<BrokerUpdate>>,
    /// A background reload is in flight (coalesces refresh bursts).
    reloading: bool,
    /// A reload was requested while one was running; run one more when it lands.
    reload_pending: bool,
    /// When the in-flight background reload started, for the UI loading timer.
    reload_started: Option<Instant>,
    /// Why we're offline, shown on the dashboard; cleared once connected.
    pub offline_reason: Option<String>,
    /// Whether the suggestion detail panel is open (modal over Suggestions).
    pub detail_open: bool,
    /// User-tunable strategy knobs (win rate, take-profit); editable on the
    /// Settings tab, persisted to the store, and overlaid onto `cfg.engine`.
    pub settings: UserSettings,
    /// `true` while the selected Settings knob is in edit mode (Enter toggles);
    /// only then do ↑/↓ change its value, so arrows otherwise still switch tabs.
    pub settings_editing: bool,
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
        let mut cfg = cfg;
        // Load any persisted user knobs (win rate, take-profit) and overlay them
        // onto the engine config before the first ranking. First run (no saved
        // blob, or a corrupt one) derives them from the TOML defaults.
        let settings = match store.get_settings_blob().await {
            Ok(Some(blob)) => {
                UserSettings::parse(&blob).unwrap_or_else(|| UserSettings::from_engine(&cfg.engine))
            }
            _ => UserSettings::from_engine(&cfg.engine),
        };
        settings.apply_to(&mut cfg.engine);
        // Overlay any in-app 0DTE roster edits (automate / sizing / profit target)
        // onto the config.toml roster, mirroring the UserSettings overlay above.
        if let Ok(Some(blob)) = store.get_zerodte_blob().await
            && let Some(z) = ZeroDteSettings::parse(&blob)
        {
            z.apply_to(&mut cfg.zerodte);
        }
        let mut app = Self {
            cfg,
            ibkr,
            tab: Tab::Dashboard,
            watchlist: Vec::new(),
            suggestions: Vec::new(),
            hedged_suggestions: Vec::new(),
            zerodte_suggestions: Vec::new(),
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
            update_tx: None,
            reloading: false,
            reload_pending: false,
            reload_started: None,
            offline_reason: None,
            detail_open: false,
            settings,
            settings_editing: false,
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
        // Drop prior-day auto-managed 0DTE positions, keeping today's so a
        // single-entry slot that already traded today isn't re-entered.
        let today_et = schedule::eastern_wall(Local::now().naive_utc())
            .date()
            .format("%Y-%m-%d")
            .to_string();
        let _ = store.prune_zerodte_positions_before(&today_et).await;

        // Cheap, broker-free load so the UI draws instantly; the live gather runs
        // off the event loop right after startup (see `tui::run`).
        app.load_local(store).await?;
        Ok(app)
    }

    /// How many 0DTE slots are armed for unattended auto-trading (the `automate`
    /// opt-in). Drives the loud header banner.
    pub fn zerodte_automating(&self) -> usize {
        self.cfg
            .zerodte
            .strategies
            .iter()
            .filter(|p| p.automate)
            .count()
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Watchlist => self.watchlist.len(),
            Tab::Suggestions => self.suggestions.len(),
            Tab::HedgedWheel => self.hedged_suggestions.len(),
            Tab::ZeroDte => self.zerodte_suggestions.len(), // one entry per quadrant slot
            Tab::Journal => self.journal.len(),
            Tab::Settings => 2, // win-rate row + take-profit row
            _ => 0,
        }
    }

    /// The suggestion highlighted on the active suggestions tab (Classic or
    /// Hedged), if any. Drives the detail panel, preview, and execute.
    pub fn selected_suggestion(&self) -> Option<&Suggestion> {
        match self.tab {
            Tab::Suggestions => self.suggestions.get(self.selected),
            Tab::HedgedWheel => self.hedged_suggestions.get(self.selected),
            // On the 0DTE tab the selection is a quadrant slot; some slots may
            // hold no current structure (`None`).
            Tab::ZeroDte => self.zerodte_suggestions.get(self.selected).and_then(|o| o.as_ref()),
            _ => None,
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

        // Detail panel acts as a modal over the selected suggestion.
        if self.detail_open {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('b') | KeyCode::Enter => Some(Action::CloseDetail),
                KeyCode::Char('x') => Some(Action::Execute),
                KeyCode::Char('p') => Some(Action::Preview),
                KeyCode::Char('A') => Some(Action::ToggleArm),
                KeyCode::Char('q') => Some(Action::Quit),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(Action::Quit)
                }
                _ => None,
            };
        }

        // The Settings tab is modal: Enter toggles "edit mode" on the selected
        // knob. Only while editing do ↑/↓ (k/j) change its value (Enter/Esc
        // confirm) — so otherwise arrows keep switching tabs, like every tab.
        if self.tab == Tab::Settings {
            if self.settings_editing {
                return match key.code {
                    KeyCode::Up | KeyCode::Char('k') => Some(Action::SettingAdjust(1)),
                    KeyCode::Down | KeyCode::Char('j') => Some(Action::SettingAdjust(-1)),
                    KeyCode::Enter | KeyCode::Esc => Some(Action::ToggleSettingEdit),
                    KeyCode::Char('q') => Some(Action::Quit),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Some(Action::Quit)
                    }
                    _ => None, // swallow other keys so we never leave mid-edit
                };
            }
            if key.code == KeyCode::Enter {
                return Some(Action::ToggleSettingEdit);
            }
        }

        match key.code {
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
            KeyCode::Tab | KeyCode::Right => Some(Action::NextTab),
            KeyCode::BackTab | KeyCode::Left => Some(Action::PrevTab),
            KeyCode::Char(d @ '1'..='8') => Some(Action::JumpTab(d as usize - '1' as usize)),
            KeyCode::Char('j') | KeyCode::Down => Some(Action::Down),
            KeyCode::Char('k') | KeyCode::Up => Some(Action::Up),
            KeyCode::Char('a') => Some(Action::StartAddSymbol),
            KeyCode::Char('d') => Some(Action::DeleteSelected),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char('?') => Some(Action::JumpTab(Tab::Help.index())),
            KeyCode::Char('A') => Some(Action::ToggleArm),
            KeyCode::Char('p') => Some(Action::Preview),
            KeyCode::Char('x') => Some(Action::Execute),
            KeyCode::Enter if matches!(self.tab, Tab::Suggestions | Tab::HedgedWheel | Tab::ZeroDte) => {
                Some(Action::OpenDetail)
            }
            // 0DTE-tab live edits of the focused slot (persisted): the automate
            // opt-in and the sizing / profit-target knobs.
            KeyCode::Char('t') if self.tab == Tab::ZeroDte => Some(Action::ToggleAutomate),
            KeyCode::Char('+' | '=') if self.tab == Tab::ZeroDte => Some(Action::SlotRisk(1)),
            KeyCode::Char('-' | '_') if self.tab == Tab::ZeroDte => Some(Action::SlotRisk(-1)),
            KeyCode::Char(']') if self.tab == Tab::ZeroDte => Some(Action::SlotProfit(1)),
            KeyCode::Char('[') if self.tab == Tab::ZeroDte => Some(Action::SlotProfit(-1)),
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
                    self.settings_editing = false;
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
                self.refresh_watchlist(store).await?;
                self.request_reload(store).await;
            }
            Action::DeleteSelected => {
                if self.tab == Tab::Watchlist
                    && let Some(row) = self.watchlist.get(self.selected)
                {
                    let sym = row.symbol.clone();
                    store.remove_symbol(&sym).await?;
                    self.status = format!("removed {sym}");
                    self.refresh_watchlist(store).await?;
                    self.request_reload(store).await;
                }
            }
            Action::Refresh => {
                self.request_reload(store).await;
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
                if let Some(sug) = self.selected_suggestion().cloned() {
                    self.preview_suggestion(&sug, store).await?;
                }
            }
            Action::Execute => {
                if let Some(sug) = self.selected_suggestion().cloned() {
                    self.execute_suggestion(&sug, store).await?;
                }
            }
            Action::OpenDetail => {
                if self.selected_suggestion().is_some() {
                    self.detail_open = true;
                }
            }
            Action::CloseDetail => {
                self.detail_open = false;
                self.status = self.default_status();
            }
            Action::SettingAdjust(dir) => {
                self.adjust_setting(dir, store).await;
            }
            Action::ToggleAutomate => self.toggle_slot_automate(store).await,
            Action::SlotRisk(dir) => self.adjust_slot_risk(dir, store).await,
            Action::SlotProfit(dir) => self.adjust_slot_profit(dir, store).await,
            Action::ToggleSettingEdit => {
                self.settings_editing = !self.settings_editing;
                self.status = if self.settings_editing {
                    "editing — ↑/↓ change · Enter or Esc when done".into()
                } else {
                    format!(
                        "win {:.0}% · take-profit {:.0}% — saved",
                        self.settings.target_win_pct, self.settings.take_profit_pct
                    )
                };
            }
        }
        Ok(())
    }

    /// Nudge the Settings-tab knob under the cursor, re-rank, and persist.
    /// Row 0 = target win rate (1-point steps); row 1 = take-profit (5-point
    /// steps). The change overlays onto `cfg.engine` so the very next reload —
    /// live or demo — ranks against it.
    async fn adjust_setting(&mut self, dir: i32, store: &Store) {
        let step = dir as f64;
        if self.selected == 0 {
            self.settings.target_win_pct += step;
        } else {
            self.settings.take_profit_pct += 5.0 * step;
        }
        self.settings.clamp();
        self.settings.apply_to(&mut self.cfg.engine);
        if let Err(e) = store.put_settings_blob(&self.settings.to_blob()).await {
            tracing::warn!("persisting settings: {e}");
        }
        self.status = format!(
            "win {:.0}% · take-profit {:.0}% — saved, re-ranking…",
            self.settings.target_win_pct, self.settings.take_profit_pct
        );
        self.request_reload(store).await;
    }

    /// The roster index behind the focused 0DTE quadrant (`self.selected` is the
    /// quadrant on that tab).
    fn focused_slot_index(&self) -> Option<usize> {
        self.cfg.zerodte.slots.get(self.selected).copied()
    }

    /// Persist the live 0DTE roster edits (a full snapshot of the editable fields).
    async fn persist_zerodte(&self, store: &Store) {
        let blob = ZeroDteSettings::snapshot(&self.cfg.zerodte).to_blob();
        if let Err(e) = store.put_zerodte_blob(&blob).await {
            tracing::warn!("persisting 0DTE settings: {e}");
        }
    }

    /// Toggle the focused slot's `automate` opt-in — the deliberate, persisted gate
    /// that lets the scheduler trade it unattended. Refuses under `read_only` and
    /// for an un-permissioned naked structure.
    async fn toggle_slot_automate(&mut self, store: &Store) {
        let Some(idx) = self.focused_slot_index() else { return };
        if self.cfg.guardrails.read_only {
            self.status = "read_only — disable it in config to allow automation".into();
            return;
        }
        // Mutate within a scope so the &mut borrow ends before we persist (&self).
        let outcome = {
            let Some(p) = self.cfg.zerodte.strategies.get_mut(idx) else { return };
            if p.kind.is_naked() && !p.allow_naked {
                Err(format!("{}: naked structure needs allow_naked in config first", p.name))
            } else {
                p.automate = !p.automate;
                Ok((p.name.clone(), p.automate))
            }
        };
        match outcome {
            Err(msg) => self.status = msg,
            Ok((name, on)) => {
                self.persist_zerodte(store).await;
                self.status = if on {
                    format!("⚡ {name} AUTO-TRADING — it will enter at its scheduled time")
                } else {
                    format!("{name} automation off")
                };
            }
        }
    }

    /// Adjust the focused slot's max-risk budget (±$250/step) and re-rank (sizing
    /// depends on it).
    async fn adjust_slot_risk(&mut self, dir: i32, store: &Store) {
        let Some(idx) = self.focused_slot_index() else { return };
        let edited = {
            let Some(p) = self.cfg.zerodte.strategies.get_mut(idx) else { return };
            p.max_risk = (p.max_risk + dir as f64 * 250.0).clamp(250.0, 1_000_000.0);
            (p.name.clone(), p.max_risk)
        };
        self.persist_zerodte(store).await;
        self.status = format!("{} max risk ${:.0} — re-ranking…", edited.0, edited.1);
        self.request_reload(store).await;
    }

    /// Adjust the focused slot's profit target (±5%/step).
    async fn adjust_slot_profit(&mut self, dir: i32, store: &Store) {
        let Some(idx) = self.focused_slot_index() else { return };
        let edited = {
            let Some(p) = self.cfg.zerodte.strategies.get_mut(idx) else { return };
            p.profit_target_pct = (p.profit_target_pct + dir as f64 * 0.05).clamp(0.05, 0.95);
            (p.name.clone(), p.profit_target_pct)
        };
        self.persist_zerodte(store).await;
        self.status = format!("{} profit target {:.0}%", edited.0, edited.1 * 100.0);
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
        if let ActionKind::SellPutSpread { long_strike, .. } = sug.kind {
            return self.preview_or_execute_spread(&ibkr, sug, long_strike, true, store).await;
        }
        if let ActionKind::OpenStructure { kind, legs } = &sug.kind {
            return self
                .preview_or_execute_structure(&ibkr, sug, *kind, &legs.clone(), true, store)
                .await;
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
        if let ActionKind::SellPutSpread { long_strike, .. } = sug.kind {
            return self.preview_or_execute_spread(&ibkr, sug, long_strike, false, store).await;
        }
        if let ActionKind::OpenStructure { kind, legs } = &sug.kind {
            return self
                .preview_or_execute_structure(&ibkr, sug, *kind, &legs.clone(), false, store)
                .await;
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

    /// Preview (what-if) or transmit a defined-risk **put credit spread** as one
    /// atomic combo, journaled as a single entry. Shared by the preview/execute
    /// paths so they can't drift; the execute caller has already enforced the
    /// arm / read_only / max-contracts guardrails (preview needs none).
    async fn preview_or_execute_spread(
        &mut self,
        ibkr: &Ibkr,
        sug: &Suggestion,
        long_strike: f64,
        preview: bool,
        store: &Store,
    ) -> Result<()> {
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let order = SpreadOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &expiry,
            short_strike: sug.strike,
            long_strike,
            quantity: sug.quantity,
            net_credit: sug.limit_price,
        };
        // One journal row for the whole combo, noting both legs (IBKR returns a
        // single combo order id).
        let note = format!(
            "put credit spread: sell {:.1}P / buy {:.1}P, net credit {:.2}",
            sug.strike, long_strike, sug.limit_price
        );
        let entry_base = NewJournalEntry { note: Some(note), ..journal_entry_for(sug, &expiry, "P") };
        match ibkr.submit_or_preview_spread(&order, preview).await {
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
                    "preview {} put spread {:.1}/{:.1} @{:.2}cr ×{}: margin {} · commission {} · {}",
                    sug.symbol, sug.strike, long_strike, sug.limit_price, sug.quantity, margin,
                    commission, state.status
                );
                store
                    .record(&NewJournalEntry {
                        status: "previewed".into(),
                        premium: Some(sug.premium_total),
                        ..entry_base
                    })
                    .await?;
            }
            Ok(OrderOutcome::Submitted(oid)) => {
                self.status = format!(
                    "submitted {} put spread {:.1}/{:.1} @{:.2}cr ×{} → id {oid}",
                    sug.symbol, sug.strike, long_strike, sug.limit_price, sug.quantity
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
            Err(e) => {
                self.status =
                    format!("{} spread error: {e}", if preview { "preview" } else { "execute" });
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

    /// Preview (what-if) or transmit a 0DTE **structure** (iron condor, credit
    /// spread, broken-wing fly, iron fly, strangle) as one atomic N-leg combo,
    /// journaled as a single entry. Shared by the preview/execute paths so they
    /// can't drift; the execute caller has already enforced the arm / read_only /
    /// max-contracts guardrails (preview needs none).
    async fn preview_or_execute_structure(
        &mut self,
        ibkr: &Ibkr,
        sug: &Suggestion,
        kind: StructureKind,
        legs: &[StructureLeg],
        preview: bool,
        store: &Store,
    ) -> Result<()> {
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let combo_legs = combo_legs_from(legs);
        let order = ComboOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &expiry,
            legs: &combo_legs,
            quantity: sug.quantity,
            net_credit: sug.limit_price,
        };
        // One journal row for the whole combo, noting the structure + leg count.
        let note = format!(
            "{} {}DTE: {}-leg combo, net credit {:.2}, max loss ${:.0}",
            kind.label(),
            sug.dte,
            combo_legs.len(),
            sug.limit_price,
            sug.capital_required,
        );
        let entry_base = NewJournalEntry {
            note: Some(note),
            ..journal_entry_for(sug, &expiry, right_char(sug.right))
        };
        match ibkr.submit_or_preview_combo(&order, preview).await {
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
                    "preview {} {} ×{}: margin {} (≈max loss ${:.0}) · commission {} · {}",
                    sug.symbol,
                    kind.label(),
                    sug.quantity,
                    margin,
                    sug.capital_required,
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
            Ok(OrderOutcome::Submitted(oid)) => {
                self.status = format!(
                    "submitted {} {} ×{} @{:.2}cr → id {oid}",
                    sug.symbol,
                    kind.label(),
                    sug.quantity,
                    sug.limit_price
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
            Err(e) => {
                self.status = format!(
                    "{} {} error: {e}",
                    if preview { "preview" } else { "execute" },
                    kind.label()
                );
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

    /// One scheduler pass for 0DTE auto-management: for each automated slot that's
    /// due (its entry time / MEIC interval in US/Eastern), transmit the entry
    /// combo and record a pending position. Honors the same guardrails as manual
    /// execution — nothing transmits under `read_only`, while offline, or above
    /// `max_contracts_per_order`. The per-slot `automate` flag is the standing,
    /// deliberate opt-in (this app's "friction generator").
    pub(super) async fn tick_zerodte(&mut self, store: &Store) {
        if self.cfg.guardrails.read_only {
            return;
        }
        let Some(ibkr) = self.ibkr.clone() else { return };
        if !self.cfg.zerodte.strategies.iter().any(|p| p.automate) {
            return; // nothing armed for automation
        }
        let now_et = schedule::eastern_wall(Local::now().naive_utc());
        let today_str = now_et.date().format("%Y-%m-%d").to_string();
        let positions = store.list_zerodte_positions().await.unwrap_or_default();
        let mut entered_any = false;

        for i in 0..self.cfg.zerodte.slot_count() {
            let Some(params) = self.cfg.zerodte.slot(i) else { continue };
            if !params.automate {
                continue;
            }
            let last_today: Option<chrono::NaiveDateTime> = positions
                .iter()
                .filter(|p| p.slot == i as i64 && p.entry_date == today_str)
                .filter_map(|p| chrono::DateTime::parse_from_rfc3339(&p.created_at).ok())
                .map(|dt| schedule::eastern_wall(dt.naive_utc()))
                .max();
            if !schedule::should_enter(
                now_et,
                params.entry_minutes_after_open,
                params.meic_interval_min,
                last_today,
            ) {
                continue;
            }
            let Some(sug) = self.zerodte_suggestions.get(i).and_then(|o| o.as_ref()).cloned() else {
                tracing::info!("0DTE slot {i} due but no structure to enter this cycle");
                continue;
            };
            if sug.quantity > self.cfg.guardrails.max_contracts_per_order {
                tracing::warn!("0DTE slot {i}: qty {} over max_contracts_per_order", sug.quantity);
                continue;
            }
            let ActionKind::OpenStructure { kind, legs } = &sug.kind else { continue };
            let combo = combo_legs_from(legs);
            let expiry = sug.expiry.format("%Y%m%d").to_string();
            match self.transmit_structure_combo(&ibkr, &sug, &combo, &expiry).await {
                Ok(oid) => {
                    let _ = store
                        .add_zerodte_position(&ZeroDtePositionRow {
                            entry_oid: oid.clone(),
                            slot: i as i64,
                            strategy: kind.label().to_string(),
                            underlying: sug.symbol.clone(),
                            expiry: expiry.clone(),
                            legs: encode_legs(&combo),
                            entry_credit: sug.limit_price,
                            quantity: sug.quantity as i64,
                            max_loss: sug.capital_required,
                            profit_target_pct: params.profit_target_pct,
                            status: "pending".into(),
                            close_oid: None,
                            entry_date: today_str.clone(),
                            created_at: String::new(),
                        })
                        .await;
                    let _ = store
                        .record(&NewJournalEntry {
                            status: "submitted".into(),
                            ibkr_order_id: Some(oid.clone()),
                            premium: Some(sug.premium_total),
                            note: Some(format!("AUTO entry: {} {}DTE", kind.label(), sug.dte)),
                            ..journal_entry_for(&sug, &expiry, right_char(sug.right))
                        })
                        .await;
                    self.status =
                        format!("⚡ AUTO entered {} {} ×{} → id {oid}", sug.symbol, kind.label(), sug.quantity);
                    tracing::info!("0DTE auto-entry slot {i}: {} id {oid}", kind.label());
                    entered_any = true;
                }
                Err(e) => {
                    tracing::warn!("0DTE auto-entry slot {i} failed: {e}");
                    self.status = format!("⚡ AUTO entry {} failed: {e}", kind.label());
                }
            }
        }
        // Time-stop is advisory in this cut: the defined-risk wings cap the loss,
        // so we log (rather than force-close) when an open position passes its
        // configured close time. An active time-stop (cancel the standing close,
        // resubmit at market) is a follow-up — it needs an order-cancel path.
        for p in positions.iter().filter(|p| p.status == "open") {
            if let Some(ts) = self
                .cfg
                .zerodte
                .slot(p.slot as usize)
                .and_then(|params| params.time_stop_hhmm.as_deref())
                && schedule::past_time_stop(now_et, ts)
            {
                tracing::warn!(
                    "0DTE {} past time-stop {ts} — riding to expiry (wings cap the loss)",
                    p.strategy
                );
            }
        }

        if entered_any {
            self.journal = store.recent_journal(200).await.unwrap_or_default();
        }
    }

    /// Transmit a structure's entry combo (no preview), returning its order id.
    async fn transmit_structure_combo(
        &self,
        ibkr: &Ibkr,
        sug: &Suggestion,
        combo: &[ComboLeg],
        expiry: &str,
    ) -> Result<String> {
        let order = ComboOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: expiry,
            legs: combo,
            quantity: sug.quantity,
            net_credit: sug.limit_price,
        };
        match ibkr.submit_or_preview_combo(&order, false).await? {
            OrderOutcome::Submitted(oid) => Ok(oid),
            OrderOutcome::Preview(_) => Err(anyhow::anyhow!("execute unexpectedly returned a preview")),
        }
    }

    /// React to a fill on a 0DTE auto-managed order: when the *entry* fills, place
    /// the standing profit-close (reversed combo bought at the target debit — "the
    /// wings are the stop", so no separate stop order); when the *close* fills,
    /// mark the position closed. Returns a status line if it acted.
    async fn handle_zerodte_order_event(
        &mut self,
        oid: &str,
        journal_status: Option<&str>,
        store: &Store,
    ) -> Result<Option<String>> {
        let positions = store.list_zerodte_positions().await?;
        // Entry leg → place the profit-close on a full fill.
        if let Some(p) = positions.iter().find(|p| p.entry_oid == oid && p.status == "pending") {
            match journal_status {
                Some("filled") => {
                    let Some(ibkr) = self.ibkr.clone() else { return Ok(None) };
                    let close_debit = (p.entry_credit * (1.0 - p.profit_target_pct)).max(0.0);
                    let close_legs = reverse_legs(&decode_legs(&p.legs));
                    // Buying the reversed package at +debit closes for profit; pass
                    // the debit as a negative "credit" (the combo limit is negated).
                    let order = ComboOrder {
                        symbol: &p.underlying,
                        expiry_yyyymmdd: &p.expiry,
                        legs: &close_legs,
                        quantity: p.quantity as i32,
                        net_credit: -close_debit,
                    };
                    match ibkr.submit_or_preview_combo(&order, false).await {
                        Ok(OrderOutcome::Submitted(close_oid)) => {
                            store.update_zerodte_status(&p.entry_oid, "open", Some(&close_oid)).await?;
                            let _ = store
                                .record(&NewJournalEntry {
                                    symbol: p.underlying.clone(),
                                    action: format!("{} close", p.strategy),
                                    quantity: p.quantity,
                                    limit_price: Some(close_debit),
                                    status: "submitted".into(),
                                    ibkr_order_id: Some(close_oid.clone()),
                                    note: Some(format!(
                                        "AUTO profit-close @ {:.0}% (debit {:.2})",
                                        p.profit_target_pct * 100.0,
                                        close_debit
                                    )),
                                    ..Default::default()
                                })
                                .await;
                            return Ok(Some(format!("⚡ {} filled → profit-close placed", p.strategy)));
                        }
                        Ok(OrderOutcome::Preview(_)) => {
                            return Ok(Some(format!("{} close: unexpected preview", p.strategy)));
                        }
                        Err(e) => {
                            // Entry is on; mark open without a close so a manual/retry
                            // close is possible — the wings still cap the loss.
                            store.update_zerodte_status(&p.entry_oid, "open", None).await?;
                            return Ok(Some(format!("⚡ {} filled, profit-close failed: {e}", p.strategy)));
                        }
                    }
                }
                Some(js @ ("cancelled" | "rejected")) => {
                    store.update_zerodte_status(&p.entry_oid, js, None).await?;
                    return Ok(Some(format!("⚡ {} entry {js}", p.strategy)));
                }
                _ => {}
            }
        }
        // Close leg → mark closed on a full fill.
        if let Some(p) = positions.iter().find(|p| p.close_oid.as_deref() == Some(oid))
            && journal_status == Some("filled")
        {
            store.update_zerodte_status(&p.entry_oid, "closed", None).await?;
            return Ok(Some(format!("⚡ {} profit-closed", p.strategy)));
        }
        Ok(None)
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

                // 0DTE auto-management: an entry fill places the standing
                // profit-close; a close fill marks the position closed (mirrors the
                // pending-roll fill handling above).
                let zerodte_status =
                    self.handle_zerodte_order_event(&oid, journal_status, store).await?;

                // A plain working ack (no terminal status, nothing filled, not a
                // roll or structure event) needs no UI change — ignore the noise.
                let traded = journal_status == Some("filled") || filled > 0.0;
                if journal_status.is_none()
                    && !traded
                    && roll_status.is_none()
                    && zerodte_status.is_none()
                {
                    return Ok(());
                }

                // Refresh holdings whenever contracts actually traded — a terminal
                // fill *or* a partial fill on a still-working status — so live
                // exposure is never left stale; otherwise just refresh the journal.
                if self.ibkr.is_some() && traded {
                    self.request_reload(store).await; // off-loop; also reloads journal
                } else {
                    self.journal = store.recent_journal(200).await?;
                }

                self.status = roll_status.or(zerodte_status).unwrap_or_else(|| match journal_status {
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
        self.settings_editing = false;
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

    /// Load only the cheap, broker-free state (watchlist, journal, wheel
    /// positions; demo suggestions when offline) so the UI can draw immediately
    /// at startup. Live broker data is fetched off the event loop right after
    /// (see [`super::run`]), so startup never blocks on slow broker I/O.
    async fn load_local(&mut self, store: &Store) -> Result<()> {
        self.watchlist = store.list_watchlist().await?;
        self.journal = store.recent_journal(200).await?;
        if self.ibkr.is_none() {
            let today = Local::now().date_naive();
            let symbols: Vec<String> = self
                .watchlist
                .iter()
                .filter(|r| r.is_enabled())
                .map(|r| r.symbol.clone())
                .collect();
            self.suggestions = demo::demo_suggestions(&symbols, &self.cfg.engine, today);
            self.zerodte_suggestions =
                demo::demo_zerodte(&self.cfg.zerodte, &self.cfg.engine, today);
        }
        self.positions = store.list_positions().await?;
        if self.selected >= self.list_len() {
            self.selected = self.list_len().saturating_sub(1);
        }
        Ok(())
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
                    let (classic, hedged) = live_suggestions(
                        &ibkr,
                        store,
                        &self.watchlist,
                        &self.broker_positions,
                        &working,
                        &self.cfg,
                        today,
                    )
                    .await;
                    self.suggestions = classic;
                    self.hedged_suggestions = hedged;
                    self.zerodte_suggestions = structure_suggestions(
                        &ibkr,
                        &self.cfg.zerodte,
                        self.cfg.engine.risk_free_rate,
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
                    self.zerodte_suggestions.clear();
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
            self.zerodte_suggestions =
                demo::demo_zerodte(&self.cfg.zerodte, &self.cfg.engine, today);
        }

        // Load the (possibly just-synced) wheel positions last so the dashboard
        // reflects holdings.
        self.positions = store.list_positions().await?;

        if self.selected >= self.list_len() {
            self.selected = self.list_len().saturating_sub(1);
        }
        Ok(())
    }

    /// Wire the channel the run loop uses to receive off-loop broker results.
    pub(super) fn set_update_sender(&mut self, tx: mpsc::UnboundedSender<BrokerUpdate>) {
        self.update_tx = Some(tx);
    }

    /// Adopt a connection established by auto-reconnect.
    pub(super) fn set_connected(&mut self, ibkr: Arc<Ibkr>) {
        self.ibkr = Some(ibkr);
        self.connected = true;
        self.offline_reason = None;
        self.status = "connected — refreshing live data…".into();
    }

    /// Drop to offline after the live connection vanished (Gateway closed/crashed).
    /// Clears the client and stale account so the UI stops claiming "live"; the
    /// last suggestions stay visible but are non-executable (the execute path is
    /// gated on a live `ibkr`). The caller restarts the reconnect loop.
    pub(super) fn set_disconnected(&mut self, reason: String) {
        self.ibkr = None;
        self.connected = false;
        self.account = None;
        self.reloading = false;
        self.reload_started = None;
        self.offline_reason = Some(reason.clone());
        self.status = reason;
    }

    /// Record why we're offline (shown on the dashboard).
    pub(super) fn set_offline_reason(&mut self, reason: String) {
        self.offline_reason = Some(reason);
    }

    /// Whether a background reload is currently in flight (drives the UI spinner).
    pub fn is_loading(&self) -> bool {
        self.reloading
    }

    /// Elapsed time of the in-flight background reload (for the UI loading timer).
    pub fn loading_elapsed(&self) -> Option<std::time::Duration> {
        self.reload_started.map(|t| t.elapsed())
    }

    /// Re-read the local watchlist into state and keep the selection in range.
    /// Membership is local and user-driven, so add/delete must show up at once —
    /// independent of the slower, off-loop broker refresh.
    async fn refresh_watchlist(&mut self, store: &Store) -> Result<()> {
        self.watchlist = store.list_watchlist().await?;
        if self.selected >= self.list_len() {
            self.selected = self.list_len().saturating_sub(1);
        }
        Ok(())
    }

    /// Refresh data. When connected, the heavy broker gather runs on a spawned
    /// task and lands later via [`App::apply_live_data`], so the UI thread never
    /// blocks; offline it just recomputes demo data inline (cheap, no broker I/O).
    pub(super) async fn request_reload(&mut self, store: &Store) {
        let (Some(ibkr), Some(tx)) = (self.ibkr.clone(), self.update_tx.clone()) else {
            if let Err(e) = self.reload(store).await {
                self.status = format!("refresh failed: {e}");
            }
            return;
        };
        if self.reloading {
            self.reload_pending = true; // coalesce: run once the in-flight one lands
            return;
        }
        self.reloading = true;
        self.reload_started = Some(Instant::now());
        self.status = "refreshing live data…".into();
        let store = store.clone();
        let cfg = self.cfg.clone();
        let pending: Vec<String> = self.pending_rolls.iter().map(|p| p.symbol.clone()).collect();
        tokio::spawn(async move {
            let today = Local::now().date_naive();
            let data = gather(&ibkr, &store, &cfg, &pending, today).await;
            let _ = tx.send(BrokerUpdate::Reloaded(Box::new(data)));
        });
    }

    /// Apply a finished background reload (see [`App::request_reload`]), preserving
    /// the safety rule that an incomplete positions snapshot must not look empty.
    pub(super) async fn apply_live_data(&mut self, d: LiveData, store: &Store) {
        self.account = d.account;
        // Re-read membership on the loop instead of trusting the gather's
        // (possibly pre-deletion) snapshot, so a delete during a slow gather
        // can't repaint the row. The probe persisted its tradable flags, so a
        // fresh read still reflects them.
        self.watchlist = store.list_watchlist().await.unwrap_or(d.watchlist);
        self.journal = d.journal;
        if d.positions_ok {
            self.broker_positions = d.broker_positions;
            self.suggestions = d.suggestions;
            self.hedged_suggestions = d.hedged_suggestions;
            self.zerodte_suggestions = d.zerodte_suggestions;
            // Reconcile prior-session rolls on the loop — a no-op unless some are
            // still reconstructed. This is the stateful/transmitting path, so it
            // must never run on the background task.
            if let Some(open) = &d.open_orders {
                let bp = self.broker_positions.clone();
                self.reconcile_pending_rolls(open, &bp, store).await;
            }
            // Keep any roll status the reconcile just set; otherwise settle to the
            // default connected status.
            if !self.status.starts_with("roll ") {
                self.status = self.default_status();
            }
        } else {
            self.suggestions.clear();
            self.hedged_suggestions.clear();
            self.zerodte_suggestions.clear();
            self.status = "broker positions unavailable — suggestions cleared until refresh".into();
        }
        self.positions = d.positions;
        tracing::info!(
            "reload applied: {} suggestions, {} watchlist rows, {} broker positions",
            self.suggestions.len(),
            self.watchlist.len(),
            self.broker_positions.len()
        );
        if self.selected >= self.list_len() {
            self.selected = self.list_len().saturating_sub(1);
        }
        self.reloading = false;
        self.reload_started = None;
        if self.reload_pending {
            self.reload_pending = false;
            self.request_reload(store).await;
        }
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
        ActionKind::SellPutSpread { .. } => "SellPutSpread".into(),
        ActionKind::OpenStructure { kind, .. } => kind.label().into(),
    }
}

fn side_and_right(sug: &Suggestion) -> Option<(Side, &'static str)> {
    match (&sug.kind, sug.right) {
        (ActionKind::SellPut, _) => Some((Side::Sell, "P")),
        (ActionKind::SellCall, _) => Some((Side::Sell, "C")),
        (ActionKind::CloseForProfit, Right::Put) => Some((Side::Buy, "P")),
        (ActionKind::CloseForProfit, Right::Call) => Some((Side::Buy, "C")),
        (ActionKind::Roll { .. }, _) => None,
        // Neither a spread nor a multi-leg structure is a single-leg order;
        // preview/execute handle those as combos explicitly.
        (ActionKind::SellPutSpread { .. }, _) => None,
        (ActionKind::OpenStructure { .. }, _) => None,
    }
}

/// Encode combo legs for persistence as `ACTION:strike:RIGHT:ratio` (B/S, P/C)
/// segments joined by `;` — enough to rebuild the profit-close after a restart
/// without a serde dependency.
fn encode_legs(legs: &[ComboLeg]) -> String {
    legs.iter()
        .map(|l| {
            let a = if l.action == Side::Buy { "B" } else { "S" };
            format!("{a}:{}:{}:{}", l.strike, l.right, l.ratio)
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Inverse of [`encode_legs`].
fn decode_legs(s: &str) -> Vec<ComboLeg> {
    s.split(';')
        .filter_map(|seg| {
            let mut it = seg.split(':');
            let action = if it.next()? == "B" { Side::Buy } else { Side::Sell };
            let strike: f64 = it.next()?.parse().ok()?;
            let right = if it.next()? == "C" { "C" } else { "P" };
            let ratio: i32 = it.next()?.parse().ok()?;
            Some(ComboLeg { strike, right, action, ratio })
        })
        .collect()
}

/// The profit-close legs: the entry combo with every leg's side flipped. Buying
/// this reversed package (at the target debit) closes the position.
fn reverse_legs(legs: &[ComboLeg]) -> Vec<ComboLeg> {
    legs.iter()
        .map(|l| ComboLeg {
            action: match l.action {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            },
            ..*l
        })
        .collect()
}

/// Collapse a structure's legs into IBKR combo legs, merging identical
/// (strike, right, side) legs into one with the summed ratio — so a butterfly's
/// doubled body becomes a single ratio-2 leg rather than two duplicate BAG legs.
fn combo_legs_from(legs: &[StructureLeg]) -> Vec<ComboLeg> {
    let mut out: Vec<ComboLeg> = Vec::new();
    for l in legs {
        let right = match l.right {
            Right::Put => "P",
            Right::Call => "C",
        };
        let action = match l.side {
            LegSide::Buy => Side::Buy,
            LegSide::Sell => Side::Sell,
        };
        if let Some(c) = out
            .iter_mut()
            .find(|c| (c.strike - l.strike).abs() < 1e-6 && c.right == right && c.action == action)
        {
            c.ratio += 1;
        } else {
            out.push(ComboLeg { strike: l.strike, right, action, ratio: 1 });
        }
    }
    out
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

    #[tokio::test]
    async fn zerodte_tab_renders_structures_offline() {
        use ratatui::{backend::TestBackend, Terminal};
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.tab = Tab::ZeroDte;
        // Offline boot populated the 2×2 grid from demo SPX chains (all four slots).
        assert_eq!(app.zerodte_suggestions.len(), 4);
        assert!(app.zerodte_suggestions.iter().all(|s| s.is_some()));

        let backend = TestBackend::new(140, 44);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::tui::ui::render(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        // The grid draws the flagship condor and the prominent risk metric.
        assert!(text.contains("Iron Condor"), "0DTE tab missing Iron Condor title");
        assert!(text.contains("max loss"), "0DTE tab missing risk metrics");
    }

    #[tokio::test]
    async fn zerodte_automate_toggle_persists_across_restart() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.tab = Tab::ZeroDte;
        app.selected = 0; // focus the first quadrant (Iron Condor)
        assert_eq!(app.zerodte_automating(), 0);

        // Toggle automation on through the real dispatch path.
        app.dispatch(Action::ToggleAutomate, &store).await.unwrap();
        assert!(app.cfg.zerodte.strategies[0].automate);
        assert_eq!(app.zerodte_automating(), 1);

        // A fresh App from the same store restores it (the persisted overlay).
        let app2 = offline_app(&store).await;
        assert!(app2.cfg.zerodte.strategies[0].automate, "automate not restored");
        assert_eq!(app2.zerodte_automating(), 1);

        // Sizing edits persist too and re-rank without panicking.
        app.dispatch(Action::SlotRisk(2), &store).await.unwrap(); // +$500
        assert!((app.cfg.zerodte.strategies[0].max_risk - 4000.0).abs() < 1e-9);
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

    fn sell_put_spread(qty: i32) -> Suggestion {
        let mut s = sug(
            ActionKind::SellPutSpread { long_strike: 95.0, long_price: 1.0 },
            Right::Put,
        );
        s.quantity = qty;
        s
    }

    #[tokio::test]
    async fn spread_execute_blocked_when_disarmed() {
        // The Hedged Wheel's combo path must sit behind the same 3-step gate.
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.armed = false;
        app.execute_suggestion(&sell_put_spread(1), &store).await.unwrap();
        assert!(app.status.contains("disarmed"), "status: {}", app.status);
        assert!(store.recent_journal(10).await.unwrap().is_empty(), "nothing journaled");
    }

    #[tokio::test]
    async fn spread_execute_blocked_when_read_only() {
        let store = Store::open_in_memory().await.unwrap();
        let mut app = offline_app(&store).await;
        app.armed = true;
        app.cfg.guardrails.read_only = true;
        app.execute_suggestion(&sell_put_spread(1), &store).await.unwrap();
        assert!(app.status.contains("read_only"), "status: {}", app.status);
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
            underlying_price: 105.0,
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

}
