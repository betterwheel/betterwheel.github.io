-- TheWheel local state. Timestamps are RFC3339 TEXT.

CREATE TABLE IF NOT EXISTS settings (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

-- Symbols the user wants to wheel.
CREATE TABLE IF NOT EXISTS watchlist (
    symbol          TEXT PRIMARY KEY,
    sec_type        TEXT NOT NULL DEFAULT 'STK',
    enabled         INTEGER NOT NULL DEFAULT 1,   -- 0/1
    tradable        INTEGER,                       -- NULL=unknown, 0=blocked, 1=allowed
    tradable_reason TEXT,
    conid           INTEGER,
    notes           TEXT,
    added_at        TEXT NOT NULL
);

-- Per-symbol wheel state the broker can't tell us (leg, basis, premium collected).
CREATE TABLE IF NOT EXISTS wheel_positions (
    symbol             TEXT PRIMARY KEY,
    state              TEXT NOT NULL,             -- Idle|ShortPut|LongShares|ShortCall
    shares             INTEGER NOT NULL DEFAULT 0,
    cost_basis         REAL NOT NULL DEFAULT 0,
    cumulative_premium REAL NOT NULL DEFAULT 0,
    updated_at         TEXT NOT NULL
);

-- Order/trade journal for P&L and audit.
CREATE TABLE IF NOT EXISTS journal (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ts            TEXT NOT NULL,
    symbol        TEXT NOT NULL,
    action        TEXT NOT NULL,                  -- SellPut|SellCall|Close|Roll
    right         TEXT,                            -- P|C
    strike        REAL,
    expiry        TEXT,
    quantity      INTEGER NOT NULL,
    limit_price   REAL,
    status        TEXT NOT NULL,                  -- previewed|submitted|filled|cancelled|rejected
    ibkr_order_id TEXT,
    premium       REAL,
    note          TEXT
);

CREATE INDEX IF NOT EXISTS idx_journal_ts ON journal (ts);
