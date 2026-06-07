//! Row types for the SQLite store (see `migrations/`).

use sqlx::FromRow;

/// The `journal.status` vocabulary. These strings are matched in safety-relevant
/// queries (e.g. [`crate::store::Store::symbols_with_working_orders`], which
/// suppresses new suggestions while an order is working), so every writer must use
/// these constants rather than bare literals — a typo would silently break the
/// double-exposure guard.
pub mod journal_status {
    pub const PREVIEWED: &str = "previewed";
    pub const SUBMITTED: &str = "submitted";
    pub const FILLED: &str = "filled";
    pub const CANCELLED: &str = "cancelled";
    pub const REJECTED: &str = "rejected";
}

/// The `zerodte_positions.status` lifecycle vocabulary. Drives auto-management
/// reconcile (`pending` → `open` → `closing` → `closed`); use these constants
/// everywhere the status is written or compared.
pub mod zerodte_status {
    pub const PENDING: &str = "pending";
    pub const OPEN: &str = "open";
    pub const CLOSING: &str = "closing";
    pub const CLOSED: &str = "closed";
    pub const CANCELLED: &str = "cancelled";
}

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

/// An auto-managed 0DTE structure position (see the `zerodte_positions` table).
/// Lives from entry submission (`pending`) through fill (`open`, profit-close
/// placed) to `closed`. `legs` is the entry combo encoded as
/// `ACTION:strike:RIGHT:ratio` segments joined by `;`.
#[derive(Debug, Clone, FromRow)]
pub struct ZeroDtePositionRow {
    pub entry_oid: String,
    pub slot: i64,
    pub strategy: String,
    pub underlying: String,
    pub expiry: String,
    pub legs: String,
    pub entry_credit: f64,
    pub quantity: i64,
    pub max_loss: f64,
    pub profit_target_pct: f64,
    pub status: String,
    pub close_oid: Option<String>,
    pub entry_date: String,
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
