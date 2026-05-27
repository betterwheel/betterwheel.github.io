//! SQLite-backed persistence (watchlist, wheel state, journal, settings).

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::models::{JournalRow, NewJournalEntry, WatchlistRow, WheelPositionRow};

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
}
