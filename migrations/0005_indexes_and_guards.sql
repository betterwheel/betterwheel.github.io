-- Index/constraint hardening (deep-review).

-- `journal.ts` is never filtered or ordered on (recent_journal orders by id);
-- the hot lookup is by ibkr_order_id (every inbound order event). Swap the dead
-- index for the one the access pattern actually justifies.
DROP INDEX IF EXISTS idx_journal_ts;
CREATE INDEX IF NOT EXISTS idx_journal_ibkr_order_id ON journal (ibkr_order_id);

-- Make the "one live auto-managed structure per slot per trading day" guarantee
-- structural rather than dependent on the Rust "already entered today" check: a
-- second concurrently-live entry on the same (slot, entry_date) now fails the
-- insert instead of silently double-exposing the account. Scoped to non-terminal
-- statuses so historical/closed rows (and a legitimate next-day re-entry) are
-- unaffected.
CREATE UNIQUE INDEX IF NOT EXISTS uq_zerodte_live_slot_day
    ON zerodte_positions (slot, entry_date)
    WHERE status IN ('pending', 'open', 'closing');
