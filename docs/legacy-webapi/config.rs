//! User configuration: IBKR Web API / OAuth connection, engine tuning, guardrails.
//!
//! Loaded from a TOML file (see `config.toml.example`). Everything has a sane
//! default so a missing or partial file still yields a usable config — except
//! the OAuth credentials, which you must fill in (see `SETUP.md`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::types::EngineConfig;

/// Top-level application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub connection: ConnectionConfig,
    pub engine: EngineConfig,
    pub guardrails: Guardrails,
    /// Where the SQLite db and logs live. Defaults to the OS data dir.
    pub data_dir: Option<PathBuf>,
}

/// Paper vs live trading. The wheel app is built paper-first. With the Web API
/// this reflects which account the OAuth app is linked to (it is not a port).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradingMode {
    Paper,
    Live,
}

/// Preferred market-data type. Realtime needs an OPRA subscription; delayed is
/// free (~15 min) and still returns model greeks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketDataPref {
    Realtime,
    Frozen,
    Delayed,
    DelayedFrozen,
}

/// IBKR Web API connection + OAuth settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    /// REST base URL. First-party OAuth talks directly to IBKR (no local gateway).
    pub base_url: String,
    pub mode: TradingMode,
    /// Account id (e.g. "DU1234567" paper / "U1234567" live). Discovered if absent.
    pub account: Option<String>,
    pub market_data: MarketDataPref,
    pub oauth: OAuthConfig,
    pub fields: FieldCodes,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.ibkr.com/v1/api".to_string(),
            mode: TradingMode::Paper,
            account: None,
            market_data: MarketDataPref::Delayed,
            oauth: OAuthConfig::default(),
            fields: FieldCodes::default(),
        }
    }
}

impl ConnectionConfig {
    pub fn is_live(&self) -> bool {
        matches!(self.mode, TradingMode::Live)
    }

    /// OAuth2 token endpoint (explicit override, else derived from `base_url`).
    pub fn token_url(&self) -> String {
        self.oauth
            .token_url
            .clone()
            .unwrap_or_else(|| format!("{}/oauth2/token", self.base_url.trim_end_matches('/')))
    }
}

/// First-party OAuth 2.0 (`private_key_jwt`) credentials. Obtain these from the
/// IBKR Self-Service OAuth portal (see `SETUP.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthConfig {
    /// Consumer key / client id assigned by IBKR.
    pub client_id: String,
    /// Key id (`kid`) returned when you registered your public key.
    pub kid: String,
    /// Subject — typically your IBKR username/credential.
    pub credential: String,
    /// Path to your RSA private key (PEM). Keep this file locked down.
    pub private_key_path: PathBuf,
    /// Override the token endpoint; otherwise derived from `base_url`.
    pub token_url: Option<String>,
    /// Optional OAuth scope.
    pub scope: Option<String>,
    /// JWT-bearer grant type (config-driven: IBKR onboarding may specify another).
    pub grant_type: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            kid: String::new(),
            credential: String::new(),
            private_key_path: PathBuf::from("private_key.pem"),
            token_url: None,
            scope: None,
            grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
        }
    }
}

impl OAuthConfig {
    /// Whether the minimum credentials are present to attempt a connection.
    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.kid.is_empty() && !self.credential.is_empty()
    }
}

/// Numeric `fields` ids for the market-data snapshot endpoint. Config-driven
/// because IBKR's codes are version-specific and sparsely documented; defaults
/// are the commonly-cited values (bid/ask/last confirmed; greeks likely).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct FieldCodes {
    pub last: u32,
    pub bid: u32,
    pub ask: u32,
    pub implied_volatility: u32,
    pub delta: u32,
    pub gamma: u32,
    pub theta: u32,
    pub vega: u32,
    pub open_interest: u32,
    pub volume: u32,
}

impl Default for FieldCodes {
    fn default() -> Self {
        Self {
            last: 31,
            bid: 84,
            ask: 86,
            implied_volatility: 7633,
            delta: 7308,
            gamma: 7309,
            theta: 7310,
            vega: 7311,
            open_interest: 7638,
            volume: 87,
        }
    }
}

impl FieldCodes {
    /// All codes as a comma-separated list for the `fields` query param.
    pub fn csv(&self) -> String {
        [
            self.last,
            self.bid,
            self.ask,
            self.implied_volatility,
            self.delta,
            self.gamma,
            self.theta,
            self.vega,
            self.open_interest,
            self.volume,
        ]
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",")
    }
}

/// Hard limits enforced regardless of the engine's suggestions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Guardrails {
    /// Maximum total capital (sum of CSP collateral) the app will deploy.
    pub max_total_deployed: f64,
    /// Maximum contracts allowed on a single order.
    pub max_contracts_per_order: i32,
    /// Require typing a confirmation phrase before enabling live trading.
    pub require_live_confirmation: bool,
    /// When true, all order-transmit paths are disabled (preview only).
    pub read_only: bool,
}

impl Default for Guardrails {
    fn default() -> Self {
        Self {
            max_total_deployed: 50_000.0,
            max_contracts_per_order: 10,
            require_live_confirmation: true,
            read_only: true,
        }
    }
}

impl Config {
    /// Load config from `path`. A missing file yields defaults.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    /// Resolved data directory (db + logs), creating nothing.
    pub fn resolved_data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("thewheel")
        })
    }
}
