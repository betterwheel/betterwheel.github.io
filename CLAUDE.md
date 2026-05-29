# TheWheel

Rust TUI assistant for running the options **wheel strategy** on Interactive
Brokers. Connects to IB Gateway/TWS via the `ibapi` crate; persists local state
in SQLite. Paper-first and safety-gated.

## Commands

```bash
cargo build
cargo test                       # unit tests live next to the code (#[cfg(test)])
cargo clippy --all-targets
cargo run                        # launch the TUI (reads ./config.toml)
cargo run --bin spike -- AAPL    # read-only Gateway connectivity probe (default AAPL)
```

Edition 2024; no pinned toolchain. Tests are pure and need no Gateway/network
(`engine`, `positions`, and parts of `tui::app`); `Store::open_in_memory()`
backs store tests.

## Architecture (layers, strictly separated)

- `engine/` — **pure strategy logic, zero I/O.** Selectors `csp` (entry),
  `covered_call` (post-assignment income), `manage` (take-profit / roll), a
  Black-Scholes delta fallback in `math` (used when IBKR reports no greek), and
  plain `types`. `plan()` ranks suggestions: management (close, roll) before new
  entries, then by annualized yield. Fully unit-testable; keep it broker-agnostic.
- `ibkr/` — **the SOLE `ibapi` boundary.** Owns the `ibapi::Client` and maps
  `ibapi` types into plain structs (`PositionRow`, `ChainMeta`, `SnapshotData`,
  `OrderEvent`, …). Do not import `ibapi` anywhere else. Every streaming request
  is bounded by a timeout. `submit_or_preview(order, preview)` is the single
  order entry point so preview and live paths can't diverge (`preview=true` →
  what-if `analyze()`; `false` → `submit()`).
- `positions.rs` — **pure broker→wheel-state reconciliation.** Flattened
  holdings → `WheelState` + share lot + open short. No I/O; exhaustively tested
  (it's the safety net for the connection-only path).
- `store/` — SQLite persistence via `sqlx` (tables: `watchlist`,
  `wheel_positions`, `journal`, `settings`; see `migrations/`). Migrations run
  automatically on `Store::open`. Holds the wheel metadata IBKR can't report
  (which leg, cost basis, cumulative premium). Broker-agnostic.
- `tui/` — `ratatui` app. `app.rs` = state + key→`Action` dispatch (async work),
  `ui.rs` = **pure render function of `App`**, `mod.rs` = `tokio::select!` run
  loop (key events + broker order-event stream + redraw tick).
- `config.rs` — TOML config (connection, engine tuning, guardrails); every field
  defaults, so a missing `config.toml` still runs. See `config.toml.example`.

Data flow when connected: `ibkr.positions()` → `positions::reconcile` → sync into
`store` → `engine::plan` over live chains → suggestions.

## Safety model (do not weaken)

- **Paper-first.** `connection.mode = "paper"` by default (port 4002).
- **Transmit is a 3-step gate:** preview/what-if (`p`) → **arm** (`A` toggles
  `armed`) → execute (`x`). A successful live submit **auto-disarms**.
- **Guardrails** (config, enforced in `app::execute_suggestion` regardless of
  engine output): `read_only` blocks all transmits; `max_contracts_per_order`
  caps order size; `max_total_deployed` caps total CSP collateral (split across
  the active watchlist when sizing).
- `ibkr.positions()` returns `Err` on an **incomplete** snapshot (stream error /
  timeout before `PositionEnd`). Callers must treat that as "unknown", never as
  "account is empty" — a failed fetch must not wipe wheel state or surface stale
  executable suggestions. Preserve this distinction in any refactor.

## Conventions & gotchas

- **Logging is file-only** (`<data_dir>/logs/thewheel.log`). Never log to stdout/
  stderr from the TUI path — it corrupts ratatui's alternate screen. (The `spike`
  binary logs to stderr because it has no TUI.)
- **Money is `f64`** throughout (prices, premium, collateral). No decimal type.
- **Offline fallback:** if Gateway isn't reachable within 5s at startup, the TUI
  runs with Black-Scholes-consistent demo data (`tui/demo.rs`) so it's always
  usable. `App.connected` / `ibkr: Option` gate all live paths.
- **Greeks may be missing** (paper accounts, illiquid strikes): the engine falls
  back to `math::bs_delta` from implied volatility to filter by moneyness.
- **Live market data needs IBKR web-portal setup first.** Even connected on
  paper, the API returns no option prices/greeks — codes `10091`/`10167`, so the
  Suggestions tab stays empty — until you complete, in the IBKR web portal
  (Client Portal): the **"Market Data API access configuration"**, the
  **"Non-Commercial Form"**, and your **Market Data Subscriber Status**, *and*
  hold the actual subscription (OPRA for US options). The app connects fine
  without these; it just can't rank anything. Offline/demo mode is unaffected.
- IBKR right strings vary (`P`/`PUT`/`C`/`CALL`); option `average_cost` includes
  the contract multiplier, so per-share credit = `average_cost / multiplier`
  (see `positions.rs`). Expiries are `YYYYMMDD`; contract-month-only expiries are
  dropped (can't be dated).
- Secrets are gitignored: `config.toml` and `*.pem` are never committed.
- `docs/legacy-webapi/` is **archived** (an abandoned Web API/OAuth broker layer);
  the live broker layer is TWS-via-`ibapi`. Don't treat it as current code.
