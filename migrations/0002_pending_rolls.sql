-- A roll whose buy-to-close leg is live but not yet filled. The sell-to-open
-- leg is transmitted only once the close fills, so this must survive a restart:
-- without it, a close that fills while the app is down would never re-open.
CREATE TABLE IF NOT EXISTS pending_rolls (
    close_oid    TEXT PRIMARY KEY, -- order id of the buy-to-close leg
    symbol       TEXT NOT NULL,
    right        TEXT NOT NULL,    -- P|C (both legs share the right)
    near_strike  REAL NOT NULL,    -- the near (closing) leg, used on restart to
    near_expiry  TEXT NOT NULL,    --   tell a filled close from a cancelled one
    to_strike    REAL NOT NULL,
    to_expiry    TEXT NOT NULL,    -- listed expiry of the new leg, YYYYMMDD
    quantity     INTEGER NOT NULL,
    far_limit    REAL NOT NULL,    -- limit credit for the sell-to-open leg
    created_at   TEXT NOT NULL
);
