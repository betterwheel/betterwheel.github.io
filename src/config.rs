//! User configuration: connection target, engine tuning, and safety guardrails.
//!
//! Loaded from a TOML file (see `config.toml.example`). Everything has a sane
//! default so a missing or partial file still yields a usable config.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::types::EngineConfig;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub connection: ConnectionConfig,
    pub engine: EngineConfig,
    pub guardrails: Guardrails,
    /// Where the SQLite db and logs live. Defaults to the OS data dir.
    pub data_dir: Option<PathBuf>,
}


/// Paper vs live trading. The wheel app is built paper-first.
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

/// IB Gateway / TWS connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub host: String,
    pub mode: TradingMode,
    /// IB Gateway paper socket port (TWS paper is 7497).
    pub paper_port: u16,
    /// IB Gateway live socket port (TWS live is 7496).
    pub live_port: u16,
    /// API client id. Each concurrent API client needs a distinct id.
    pub client_id: i32,
    /// Account id (e.g. "DU1234567" for paper). Optional; discovered if absent.
    pub account: Option<String>,
    pub market_data: MarketDataPref,
    /// When offline, retry connecting to Gateway every this many seconds.
    /// `0` disables auto-reconnect. The retry runs off the UI thread.
    pub reconnect_secs: u64,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            mode: TradingMode::Paper,
            paper_port: 4002,
            live_port: 4001,
            client_id: 100,
            account: None,
            market_data: MarketDataPref::Delayed,
            reconnect_secs: 15,
        }
    }
}

impl ConnectionConfig {
    /// The `host:port` address to connect to, based on the active mode.
    pub fn address(&self) -> String {
        let port = match self.mode {
            TradingMode::Paper => self.paper_port,
            TradingMode::Live => self.live_port,
        };
        format!("{}:{}", self.host, port)
    }

    pub fn is_live(&self) -> bool {
        matches!(self.mode, TradingMode::Live)
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
            read_only: false,
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
