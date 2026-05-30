-- Live, in-app overrides for the 0DTE roster (per-slot automate / max_risk /
-- profit target), persisted so toggles survive a restart. A single-row JSON-ish
-- (TOML) blob, like `settings`; applied onto the config.toml roster at startup.
CREATE TABLE IF NOT EXISTS zerodte_settings (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);
