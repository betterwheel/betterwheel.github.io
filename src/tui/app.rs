//! TUI application state and update logic.
//!
//! Key handling is synchronous and returns an [`Action`]; the run loop performs
//! any async work (store writes, data refresh) via [`App::dispatch`]. Rendering
//! is a pure function of this state (see [`super::ui`]).

use anyhow::Result;
use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::demo;
use crate::config::{Config, TradingMode};
use crate::engine::types::Suggestion;
use crate::ibkr::AccountSnapshot;
use crate::store::{JournalRow, Store, WatchlistRow, WheelPositionRow};

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
}

/// All TUI state.
pub struct App {
    pub cfg: Config,
    pub tab: Tab,
    pub watchlist: Vec<WatchlistRow>,
    pub suggestions: Vec<Suggestion>,
    pub journal: Vec<JournalRow>,
    pub positions: Vec<WheelPositionRow>,
    pub account: Option<AccountSnapshot>,
    /// Selected row index within the active tab's list.
    pub selected: usize,
    pub input: InputMode,
    pub status: String,
    /// `false` = offline/demo (no live IBKR connection yet).
    pub connected: bool,
    pub should_quit: bool,
}

impl App {
    pub async fn new(cfg: Config, store: &Store) -> Result<Self> {
        let mut app = Self {
            cfg,
            tab: Tab::Dashboard,
            watchlist: Vec::new(),
            suggestions: Vec::new(),
            journal: Vec::new(),
            positions: Vec::new(),
            account: None,
            selected: 0,
            input: InputMode::Normal,
            status: "offline — showing demo data. See SETUP.md to connect.".to_string(),
            connected: false,
            should_quit: false,
        };
        app.reload(store).await?;
        Ok(app)
    }

    /// Number of selectable rows on the active tab.
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
            KeyCode::Char(d @ '1'..='5') => {
                Some(Action::JumpTab(d as usize - '1' as usize))
            }
            KeyCode::Char('j') | KeyCode::Down => Some(Action::Down),
            KeyCode::Char('k') | KeyCode::Up => Some(Action::Up),
            KeyCode::Char('a') => Some(Action::StartAddSymbol),
            KeyCode::Char('d') => Some(Action::DeleteSelected),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char('?') => Some(Action::JumpTab(Tab::Help.index())),
            _ => None,
        }
    }

    /// Apply an action, performing any async work against the store.
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
                    && (c.is_ascii_alphanumeric() || c == '.') {
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
                    && let Some(row) = self.watchlist.get(self.selected) {
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
        if self.connected {
            "connected".into()
        } else {
            "offline — showing demo data. See SETUP.md to connect.".into()
        }
    }

    /// Reload persisted state and recompute suggestions.
    async fn reload(&mut self, store: &Store) -> Result<()> {
        self.watchlist = store.list_watchlist().await?;
        self.journal = store.recent_journal(200).await?;
        self.positions = store.list_positions().await?;

        // Suggestions: offline → demo data; live wiring comes after the spike.
        let symbols: Vec<String> = self
            .watchlist
            .iter()
            .filter(|r| r.is_enabled())
            .map(|r| r.symbol.clone())
            .collect();
        let today = Local::now().date_naive();
        self.suggestions = demo::demo_suggestions(&symbols, &self.cfg.engine, today);

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
