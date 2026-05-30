-- An auto-managed 0DTE structure: opened by the scheduler, profit-closed on a
-- standing order. Persisted so management resumes after a restart, and so a
-- single-entry slot that already traded today is not re-entered. `legs` is the
-- entry combo encoded "ACTION:strike:RIGHT:ratio" (B/S, P/C) joined by ';'; the
-- profit-close reverses those legs.
CREATE TABLE IF NOT EXISTS zerodte_positions (
    entry_oid          TEXT PRIMARY KEY, -- IBKR order id of the entry combo
    slot               INTEGER NOT NULL, -- 0DTE-tab quadrant slot
    strategy           TEXT NOT NULL,
    underlying         TEXT NOT NULL,
    expiry             TEXT NOT NULL,    -- YYYYMMDD
    legs               TEXT NOT NULL,
    entry_credit       REAL NOT NULL,    -- net credit per share received
    quantity           INTEGER NOT NULL,
    max_loss           REAL NOT NULL,    -- defined max loss, total $
    profit_target_pct  REAL NOT NULL,    -- buy-to-close at this fraction of credit
    status             TEXT NOT NULL,    -- pending | open | closing | closed
    close_oid          TEXT,             -- order id of the profit-close, once placed
    entry_date         TEXT NOT NULL,    -- ET trading date (YYYY-MM-DD)
    created_at         TEXT NOT NULL
);
