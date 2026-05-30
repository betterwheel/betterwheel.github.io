//! Local persistence: watchlist, per-symbol wheel state, and an order journal.
//!
//! IBKR reports *current* positions, but not the wheel metadata we care about
//! (which leg a symbol is in, adjusted cost basis, premium collected to date),
//! so we keep that — plus the watchlist and a trade journal — in SQLite.

pub mod models;
pub mod repo;

pub use models::{
    JournalRow, NewJournalEntry, PendingRollRow, WatchlistRow, WheelPositionRow, ZeroDtePositionRow,
};
pub use repo::Store;
