//! IBKR broker layer — the **IBKR Web API** via **first-party OAuth 2.0**.
//!
//! [`WebApi`] is the authenticated client. It talks directly to IBKR's hosted
//! REST host (`https://api.ibkr.com/v1/api`) with requests signed by your RSA
//! key — there is no local gateway/port. Submodules:
//! - [`auth`]: OAuth `private_key_jwt` token exchange.
//! - [`client`]: session management + REST endpoints.
//! - [`models`]: broker-agnostic output types + lenient JSON parsing.

pub mod auth;
pub mod client;
pub mod models;

pub use client::WebApi;
pub use models::{
    AccountSnapshot, ContractMatch, OptionQuoteSnap, OrderPreview, PositionRow, Tradability,
};
