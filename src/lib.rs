//! TheWheel — an options *wheel strategy* assistant for Interactive Brokers.
//!
//! The crate is split into independent layers so the strategy logic can be
//! tested without a broker connection:
//! - [`engine`]: pure strategy logic (no I/O). Given chains, greeks, account
//!   and config it produces ranked [`engine::Suggestion`]s.
//! - [`config`]: user configuration (connection, engine tuning, guardrails).
//!
//! Later layers (`ibkr`, `store`, `tui`) wrap this core with IO.

pub mod config;
pub mod engine;
pub mod ibkr;
pub mod positions;
pub mod store;
pub mod tui;
