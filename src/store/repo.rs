//! SQLite-backed persistence (watchlist, wheel state, journal, settings).

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::models::{JournalRow, NewJournalEntry, PendingRollRow, WatchlistRow, WheelPositionRow};

/// Handle to the local database. Cheap to clone (shares the pool).
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if needed) the database at `path` and run migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening database {}", path.display()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// An ephemeral in-memory database (for tests).
    pub async fn open_in_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("running migrations")?;
        Ok(())
    }

    // --- watchlist ---

    pub async fn add_symbol(&self, symbol: &str, sec_type: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO watchlist (symbol, sec_type, enabled, added_at)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(symbol) DO NOTHING",
        )
        .bind(symbol)
        .bind(sec_type)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_symbol(&self, symbol: &str) -> Result<()> {
        sqlx::query("DELETE FROM watchlist WHERE symbol = ?1")
            .bind(symbol)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_watchlist(&self) -> Result<Vec<WatchlistRow>> {
        let rows = sqlx::query_as::<_, WatchlistRow>(
            "SELECT symbol, sec_type, enabled, tradable, tradable_reason, conid, notes, added_at
             FROM watchlist ORDER BY symbol",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn set_tradable(&self, symbol: &str, tradable: bool, reason: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE watchlist SET tradable = ?2, tradable_reason = ?3 WHERE symbol = ?1")
            .bind(symbol)
            .bind(i64::from(tradable))
            .bind(reason)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_conid(&self, symbol: &str, conid: i64) -> Result<()> {
        sqlx::query("UPDATE watchlist SET conid = ?2 WHERE symbol = ?1")
            .bind(symbol)
            .bind(conid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- wheel positions ---

    pub async fn upsert_position(&self, p: &WheelPositionRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO wheel_positions (symbol, state, shares, cost_basis, cumulative_premium, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(symbol) DO UPDATE SET
               state = excluded.state, shares = excluded.shares,
               cost_basis = excluded.cost_basis,
               cumulative_premium = excluded.cumulative_premium,
               updated_at = excluded.updated_at",
        )
        .bind(&p.symbol)
        .bind(&p.state)
        .bind(p.shares)
        .bind(p.cost_basis)
        .bind(p.cumulative_premium)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sync the broker-derived leg of a position (state / shares / cost basis)
    /// without disturbing the locally-tracked `cumulative_premium`, which the
    /// broker can't report. On first insert the premium starts at 0.
    pub async fn upsert_wheel_state(
        &self,
        symbol: &str,
        state: &str,
        shares: i64,
        cost_basis: f64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO wheel_positions (symbol, state, shares, cost_basis, cumulative_premium, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)
             ON CONFLICT(symbol) DO UPDATE SET
               state = excluded.state, shares = excluded.shares,
               cost_basis = excluded.cost_basis, updated_at = excluded.updated_at",
        )
        .bind(symbol)
        .bind(state)
        .bind(shares)
        .bind(cost_basis)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_positions(&self) -> Result<Vec<WheelPositionRow>> {
        let rows = sqlx::query_as::<_, WheelPositionRow>(
            "SELECT symbol, state, shares, cost_basis, cumulative_premium, updated_at
             FROM wheel_positions ORDER BY symbol",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // --- journal ---

    pub async fn record(&self, e: &NewJournalEntry) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO journal
               (ts, symbol, action, right, strike, expiry, quantity, limit_price, status, ibkr_order_id, premium, note)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        )
        .bind(now())
        .bind(&e.symbol)
        .bind(&e.action)
        .bind(&e.right)
        .bind(e.strike)
        .bind(&e.expiry)
        .bind(e.quantity)
        .bind(e.limit_price)
        .bind(&e.status)
        .bind(&e.ibkr_order_id)
        .bind(e.premium)
        .bind(&e.note)
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    /// Update the status (and optionally append a note) of the journal row for
    /// `ibkr_order_id`. Returns the number of rows changed — 0 means the id is
    /// not one we recorded (e.g. an order placed directly in TWS), which the
    /// caller can safely ignore. A `None` note leaves the existing note intact.
    pub async fn update_journal_status(
        &self,
        ibkr_order_id: &str,
        status: &str,
        note: Option<&str>,
    ) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE journal SET status = ?2, note = COALESCE(?3, note)
             WHERE ibkr_order_id = ?1",
        )
        .bind(ibkr_order_id)
        .bind(status)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Distinct symbols with a live (`status = "submitted"`) order we placed —
    /// used to avoid stacking another suggestion on a symbol while an order is
    /// still working (which could double exposure).
    pub async fn symbols_with_working_orders(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT symbol FROM journal WHERE status = 'submitted'")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    /// Whether any journal row records this IBKR order id (i.e. it's an order we
    /// placed). Used to ignore activity for orders originating elsewhere.
    pub async fn journal_order_exists(&self, ibkr_order_id: &str) -> Result<bool> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM journal WHERE ibkr_order_id = ?1 LIMIT 1")
                .bind(ibkr_order_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    // --- pending rolls ---

    /// Persist a pending roll (its close leg is live; the open leg fires on fill).
    pub async fn add_pending_roll(&self, r: &PendingRollRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO pending_rolls
               (close_oid, symbol, right, near_strike, near_expiry, to_strike, to_expiry, quantity, far_limit, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(close_oid) DO NOTHING",
        )
        .bind(&r.close_oid)
        .bind(&r.symbol)
        .bind(&r.right)
        .bind(r.near_strike)
        .bind(&r.near_expiry)
        .bind(r.to_strike)
        .bind(&r.to_expiry)
        .bind(r.quantity)
        .bind(r.far_limit)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_pending_rolls(&self) -> Result<Vec<PendingRollRow>> {
        let rows = sqlx::query_as::<_, PendingRollRow>(
            "SELECT close_oid, symbol, right, near_strike, near_expiry, to_strike, to_expiry,
                    quantity, far_limit, created_at
             FROM pending_rolls",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn remove_pending_roll(&self, close_oid: &str) -> Result<()> {
        sqlx::query("DELETE FROM pending_rolls WHERE close_oid = ?1")
            .bind(close_oid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recent_journal(&self, limit: i64) -> Result<Vec<JournalRow>> {
        let rows = sqlx::query_as::<_, JournalRow>(
            "SELECT id, ts, symbol, action, right, strike, expiry, quantity, limit_price,
                    status, ibkr_order_id, premium, note
             FROM journal ORDER BY id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watchlist_roundtrip() {
        let s = Store::open_in_memory().await.unwrap();
        s.add_symbol("AAPL", "STK").await.unwrap();
        s.add_symbol("MSFT", "STK").await.unwrap();
        s.add_symbol("AAPL", "STK").await.unwrap(); // idempotent

        let list = s.list_watchlist().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].symbol, "AAPL");
        assert_eq!(list[0].tradable_label(), "unknown");

        s.set_tradable("AAPL", true, None).await.unwrap();
        s.set_tradable("MSFT", false, Some("PRIIPs")).await.unwrap();
        s.set_conid("AAPL", 265598).await.unwrap();

        let list = s.list_watchlist().await.unwrap();
        let aapl = list.iter().find(|r| r.symbol == "AAPL").unwrap();
        assert_eq!(aapl.tradable_label(), "tradable");
        assert_eq!(aapl.conid, Some(265598));
        let msft = list.iter().find(|r| r.symbol == "MSFT").unwrap();
        assert_eq!(msft.tradable_label(), "blocked");
        assert_eq!(msft.tradable_reason.as_deref(), Some("PRIIPs"));

        s.remove_symbol("MSFT").await.unwrap();
        assert_eq!(s.list_watchlist().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn positions_and_journal() {
        let s = Store::open_in_memory().await.unwrap();
        s.upsert_position(&WheelPositionRow {
            symbol: "AAPL".into(),
            state: "ShortPut".into(),
            shares: 0,
            cost_basis: 0.0,
            cumulative_premium: 1.80,
            updated_at: String::new(),
        })
        .await
        .unwrap();
        // upsert again updates in place
        s.upsert_position(&WheelPositionRow {
            symbol: "AAPL".into(),
            state: "LongShares".into(),
            shares: 100,
            cost_basis: 93.2,
            cumulative_premium: 1.80,
            updated_at: String::new(),
        })
        .await
        .unwrap();
        let pos = s.list_positions().await.unwrap();
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].state, "LongShares");
        assert_eq!(pos[0].shares, 100);

        let id = s
            .record(&NewJournalEntry {
                symbol: "AAPL".into(),
                action: "SellPut".into(),
                right: Some("P".into()),
                strike: Some(95.0),
                quantity: 1,
                limit_price: Some(1.80),
                status: "previewed".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(id >= 1);
        let recent = s.recent_journal(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].action, "SellPut");
        assert_eq!(recent[0].strike, Some(95.0));
    }

    #[tokio::test]
    async fn upsert_wheel_state_preserves_cumulative_premium() {
        let s = Store::open_in_memory().await.unwrap();
        // Seed a position with collected premium (as a fill handler would).
        s.upsert_position(&WheelPositionRow {
            symbol: "AAPL".into(),
            state: "ShortPut".into(),
            shares: 0,
            cost_basis: 0.0,
            cumulative_premium: 2.50,
            updated_at: String::new(),
        })
        .await
        .unwrap();

        // A broker sync updates the leg but must not clobber cumulative_premium.
        s.upsert_wheel_state("AAPL", "LongShares", 100, 93.2).await.unwrap();
        let p = &s.list_positions().await.unwrap()[0];
        assert_eq!(p.state, "LongShares");
        assert_eq!(p.shares, 100);
        assert!((p.cost_basis - 93.2).abs() < 1e-9);
        assert!((p.cumulative_premium - 2.50).abs() < 1e-9, "premium preserved");

        // First sync of a brand-new symbol starts premium at 0.
        s.upsert_wheel_state("MSFT", "ShortPut", 0, 0.0).await.unwrap();
        let msft = s.list_positions().await.unwrap().into_iter().find(|p| p.symbol == "MSFT").unwrap();
        assert_eq!(msft.cumulative_premium, 0.0);
    }

    #[tokio::test]
    async fn journal_status_update_and_existence() {
        let s = Store::open_in_memory().await.unwrap();
        s.record(&NewJournalEntry {
            symbol: "AAPL".into(),
            action: "SellPut".into(),
            quantity: 1,
            status: "submitted".into(),
            ibkr_order_id: Some("42".into()),
            note: Some("orig".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        assert!(s.journal_order_exists("42").await.unwrap());
        assert!(!s.journal_order_exists("999").await.unwrap());

        // A None note leaves the existing note intact; a Some note overwrites.
        let n = s.update_journal_status("42", "filled", None).await.unwrap();
        assert_eq!(n, 1);
        let row = &s.recent_journal(10).await.unwrap()[0];
        assert_eq!(row.status, "filled");
        assert_eq!(row.note.as_deref(), Some("orig"));

        s.update_journal_status("42", "filled", Some("filled 1 @ 1.80")).await.unwrap();
        let row = &s.recent_journal(10).await.unwrap()[0];
        assert_eq!(row.note.as_deref(), Some("filled 1 @ 1.80"));

        // Unknown order id changes nothing.
        assert_eq!(s.update_journal_status("999", "filled", None).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn working_orders_tracks_submitted() {
        let s = Store::open_in_memory().await.unwrap();
        for (sym, status, oid) in [
            ("AAPL", "submitted", "1"),
            ("MSFT", "previewed", "2"),
            ("NVDA", "filled", "3"),
        ] {
            s.record(&NewJournalEntry {
                symbol: sym.into(),
                action: "SellPut".into(),
                quantity: 1,
                status: status.into(),
                ibkr_order_id: Some(oid.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let working = s.symbols_with_working_orders().await.unwrap();
        assert_eq!(working, vec!["AAPL".to_string()]); // only the submitted one
    }

    #[tokio::test]
    async fn pending_roll_roundtrip() {
        let s = Store::open_in_memory().await.unwrap();
        s.add_pending_roll(&PendingRollRow {
            close_oid: "55".into(),
            symbol: "AAPL".into(),
            right: "P".into(),
            near_strike: 100.0,
            near_expiry: "20260619".into(),
            to_strike: 95.0,
            to_expiry: "20260717".into(),
            quantity: 1,
            far_limit: 2.10,
            created_at: String::new(),
        })
        .await
        .unwrap();

        let rolls = s.list_pending_rolls().await.unwrap();
        assert_eq!(rolls.len(), 1);
        assert_eq!(rolls[0].close_oid, "55");
        assert_eq!(rolls[0].near_strike, 100.0);
        assert_eq!(rolls[0].to_strike, 95.0);
        assert!(!rolls[0].created_at.is_empty(), "created_at stamped on insert");

        s.remove_pending_roll("55").await.unwrap();
        assert!(s.list_pending_rolls().await.unwrap().is_empty());
    }
}
