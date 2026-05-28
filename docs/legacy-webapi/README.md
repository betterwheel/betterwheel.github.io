# Legacy: IBKR Web API + first-party OAuth 2.0

This directory archives the broker layer we built around the IBKR **Web API**
with **first-party OAuth 2.0** (`private_key_jwt`, RS256). It was a
security-driven pivot from the TWS socket API — the TWS socket has no
per-connection auth, so any local process can ride the logged-in session, while
OAuth would have signed each request with our RSA key and not opened a local
port at all.

## Why we reverted

IBKR's onboarding response did **not** confirm that first-party OAuth 2.0 is
available to individual retail accounts (it's framed for institutional /
third-party developers, and the self-service portal isn't exposed in the
retail Client Portal). Without that guarantee, every available path (TWS,
Client Portal Gateway, OAuth) collapses to the **same local-trust model** — so
the security advantage that justified the pivot disappears.

Given equal trust models, the TWS API wins on **Rust ergonomics** (the mature
`ibapi` crate vs hand-rolling REST + JWT + session keepalive), so we reverted
the broker layer to TWS in commit *after* `e559145`. The engine, store, and
TUI were unaffected by either pivot — they sit behind plain types.

## What's preserved here

| File | What it does |
|---|---|
| `ibkr/auth.rs` | RS256-signed `client_assertion` JWT + token exchange |
| `ibkr/client.rs` | `WebApi` session (token refresh, `ssodh/init`, tickle) + REST endpoints (accounts, summary, positions, secdef search/strikes/info, snapshot, what-if, place/cancel), with lenient JSON parsing |
| `ibkr/models.rs` | Broker-agnostic output types + `dig_money`/`lenient_f64` helpers |
| `ibkr/mod.rs` | Module wiring + re-exports |
| `spike.rs` | Read-only Web API connectivity probe (was `src/bin/spike.rs`) |
| `config.rs` | Config with `[connection.oauth]` and `[connection.fields]` (snapshot field codes) |
| `config.toml.example` | Sample config for the OAuth path |
| `SETUP.md` | OAuth onboarding walkthrough (RSA keygen, Self-Service portal, etc.) |

## If first-party OAuth ever opens up

1. Restore the files into their original locations (`git mv` them back).
2. Add deps: `cargo add reqwest --features json,query,form && cargo add serde_json jsonwebtoken && cargo add uuid --features v4`.
3. Optionally remove `ibapi` (or keep a dual backend behind `cfg` features).
4. Follow `SETUP.md` to onboard with IBKR.

The unverified bits flagged at the time were the exact token request
parameters and the snapshot greek field codes — both are config-driven in
`config.rs`, so calibration is editing a TOML, not code.
