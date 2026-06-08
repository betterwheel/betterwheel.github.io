# BetterWheel

BetterWheel is an assistant for running the options **wheel strategy** on Interactive
Brokers. It ranks — and, behind a paper-first, three-step safety gate
(preview → arm → execute), places — the strategy's moves: selling cash-secured puts,
defending tested shorts by rolling, and writing covered calls after assignment, scored
by annualized return on collateral. Alongside the wheel it surfaces short-dated 0DTE
index structures (iron condors, put/call credit spreads, broken-wing flies, iron flies)
with full payoff, breakeven, and capped-risk analysis. It's built in Rust — a pure,
broker-agnostic strategy engine, an IB Gateway/TWS broker layer (via the `ibapi` crate),
and local SQLite persistence — and ships as both a terminal UI and a native desktop app
for macOS and Windows that auto-updates itself.

More info, screenshots, and downloads: **https://www.salamacchine.it/projects/betterwheel/**

Licensed under the GNU Affero General Public License v3.0 or later (see `LICENSE`).
