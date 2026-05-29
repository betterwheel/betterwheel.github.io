//! Row types for the SQLite store (see `migrations/`).

use sqlx::FromRow;

/// A watchlist entry.
#[derive(Debug, Clone, FromRow)]
pub struct WatchlistRow {
    pub symbol: String,
    pub sec_type: String,
    pub enabled: i64,
    /// `None` = unknown, `Some(0)` = blocked, `Some(1)` = allowed.
    pub tradable: Option<i64>,
    pub tradable_reason: Option<String>,
    pub conid: Option<i64>,
    pub notes: Option<String>,
    pub added_at: String,
}

impl WatchlistRow {
    pub fn is_enabled(&self) -> bool {
        self.enabled != 0
    }

    /// Human-readable tradability badge.
    pub fn tradable_label(&self) -> &str {
        match self.tradable {
            Some(1) => "tradable",
            Some(0) => "blocked",
            _ => "unknown",
        }
    }
}

/// Per-symbol wheel state.
#[derive(Debug, Clone, FromRow)]
pub struct WheelPositionRow {
    pub symbol: String,
    pub state: String,
    pub shares: i64,
    pub cost_basis: f64,
    pub cumulative_premium: f64,
    pub updated_at: String,
}

/// A journal entry (order/trade history).
#[derive(Debug, Clone, FromRow)]
pub struct JournalRow {
    pub id: i64,
    pub ts: String,
    pub symbol: String,
    pub action: String,
    pub right: Option<String>,
    pub strike: Option<f64>,
    pub expiry: Option<String>,
    pub quantity: i64,
    pub limit_price: Option<f64>,
    pub status: String,
    pub ibkr_order_id: Option<String>,
    pub premium: Option<f64>,
    pub note: Option<String>,
}

/// A roll awaiting its buy-to-close leg to fill before the sell-to-open leg is
/// transmitted. Persisted so the roll survives a restart (see `pending_rolls`).
#[derive(Debug, Clone, FromRow)]
pub struct PendingRollRow {
    pub close_oid: String,
    pub symbol: String,
    pub right: String,
    /// The near (closing) leg — used on restart to tell a filled close (short
    /// gone from positions) from a cancelled one (short still held).
    pub near_strike: f64,
    pub near_expiry: String,
    pub to_strike: f64,
    pub to_expiry: String,
    pub quantity: i64,
    pub far_limit: f64,
    pub created_at: String,
}

/// Fields for a new journal entry (id/ts are assigned on insert).
#[derive(Debug, Clone, Default)]
pub struct NewJournalEntry {
    pub symbol: String,
    pub action: String,
    pub right: Option<String>,
    pub strike: Option<f64>,
    pub expiry: Option<String>,
    pub quantity: i64,
    pub limit_price: Option<f64>,
    pub status: String,
    pub ibkr_order_id: Option<String>,
    pub premium: Option<f64>,
    pub note: Option<String>,
}
