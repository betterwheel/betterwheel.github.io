//! User configuration: connection target, engine tuning, and safety guardrails.
//!
//! Loaded from a TOML file (see `config.toml.example`). Everything has a sane
//! default so a missing or partial file still yields a usable config.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::structures::StructureParams;
use crate::engine::types::{EngineConfig, StructureKind};

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub connection: ConnectionConfig,
    pub engine: EngineConfig,
    pub guardrails: Guardrails,
    /// 0DTE/short-dated structure roster + the four 0DTE-tab quadrant slots.
    pub zerodte: ZeroDteConfig,
    /// Where the SQLite db and logs live. Defaults to the OS data dir.
    pub data_dir: Option<PathBuf>,
}

/// The two strategy knobs the user can tune live from the app's Settings tab.
/// Persisted to the store (overriding the `[engine]` TOML defaults) and kept as
/// whole percents for a friendly UI; [`UserSettings::apply_to`] maps them back
/// to engine units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UserSettings {
    /// Target probability the trade wins (the short expires worthless), as a
    /// percent (e.g. `70`). Maps to the short-option delta: `delta ≈ 1 − win`.
    pub target_win_pct: f64,
    /// Buy-to-close once this percent of the max premium is captured (e.g. `50`).
    pub take_profit_pct: f64,
}

impl UserSettings {
    /// Win-rate bounds the UI allows: closer than ~50Δ stops being income and
    /// becomes a directional bet; further than ~10Δ is dust.
    pub const WIN_MIN: f64 = 50.0;
    pub const WIN_MAX: f64 = 90.0;
    /// Take-profit bounds: closing under 10% is noise, over 90% rarely fills.
    pub const TP_MIN: f64 = 10.0;
    pub const TP_MAX: f64 = 90.0;

    /// Derive the editable knobs from a (TOML-loaded) engine config, so the
    /// Settings tab opens showing the configured defaults on first run.
    pub fn from_engine(cfg: &EngineConfig) -> Self {
        Self {
            target_win_pct: ((1.0 - cfg.target_delta) * 100.0).round(),
            take_profit_pct: (cfg.take_profit_pct * 100.0).round(),
        }
    }

    /// Parse a persisted blob; `None` (or malformed) lets the caller fall back to
    /// the config default so corrupt settings never wedge startup.
    pub fn parse(blob: &str) -> Option<Self> {
        toml::from_str(blob).ok()
    }

    /// Serialize for persistence.
    pub fn to_blob(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// Overlay these knobs onto an engine config: win% sets the target short
    /// delta and re-centers the entry band (±10Δ) around it; take-profit% maps
    /// directly. Called whenever settings load or change, before re-ranking.
    pub fn apply_to(&self, cfg: &mut EngineConfig) {
        let target_delta = (1.0 - self.target_win_pct / 100.0).clamp(0.05, 0.95);
        cfg.target_delta = target_delta;
        cfg.min_delta = (target_delta - 0.10).max(0.05);
        cfg.max_delta = (target_delta + 0.10).min(0.95);
        cfg.take_profit_pct = (self.take_profit_pct / 100.0).clamp(0.05, 0.95);
    }

    /// Clamp both knobs back into their UI ranges after an adjustment.
    pub fn clamp(&mut self) {
        self.target_win_pct = self.target_win_pct.clamp(Self::WIN_MIN, Self::WIN_MAX);
        self.take_profit_pct = self.take_profit_pct.clamp(Self::TP_MIN, Self::TP_MAX);
    }
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

/// The 0DTE/short-dated structure roster and the four quadrant assignments shown
/// on the 0DTE tab. `strategies` are named, parameterized structures (TOML
/// `[[zerodte.strategy]]`); `slots` picks, by roster index, which structure each
/// of the four quadrants displays.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ZeroDteConfig {
    #[serde(rename = "strategy")]
    pub strategies: Vec<StructureParams>,
    /// Quadrant → roster-index assignments (the first four are shown).
    pub slots: Vec<usize>,
}

impl Default for ZeroDteConfig {
    fn default() -> Self {
        // Seeded from the r/thetagang 0DTE thread: a neutral condor, a one-sided
        // credit spread (with a stop), a no-upside-risk broken-wing fly, and a
        // max-premium iron fly. Stops default to 0 ("the wings are the stop")
        // except the directional spread.
        Self {
            strategies: vec![
                StructureParams {
                    name: "Iron Condor".into(),
                    kind: StructureKind::IronCondor,
                    dte: 0,
                    short_delta: 0.13,
                    call_delta: 0.13,
                    wing_points: 25.0,
                    min_credit: 0.80,
                    // SPX 25pt wings risk ~$2–2.8k/contract; give headroom so one
                    // contract clears the budget on the index's wide strikes.
                    max_risk: 3500.0,
                    entry_minutes_after_open: 45,
                    profit_target_pct: 0.40,
                    stop_loss_mult: 0.0,
                    ..Default::default()
                },
                StructureParams {
                    name: "Put Credit Spread".into(),
                    kind: StructureKind::PutCreditSpread,
                    dte: 1,
                    short_delta: 0.11,
                    wing_points: 10.0,
                    min_credit: 0.30,
                    max_risk: 1500.0,
                    entry_minutes_after_open: 45,
                    profit_target_pct: 0.40,
                    stop_loss_mult: 1.2,
                    ..Default::default()
                },
                StructureParams {
                    name: "Broken-Wing Fly".into(),
                    kind: StructureKind::BrokenWingButterfly,
                    dte: 0,
                    // Near-the-money body with a narrow upper wing is what lets a
                    // 1-2-1 put fly price to a credit (the lower skip is 3×).
                    short_delta: 0.32,
                    wing_points: 10.0,
                    min_credit: 0.10,
                    max_risk: 2000.0,
                    entry_minutes_after_open: 30,
                    profit_target_pct: 0.50,
                    stop_loss_mult: 0.0,
                    ..Default::default()
                },
                StructureParams {
                    name: "Iron Fly".into(),
                    kind: StructureKind::IronFly,
                    dte: 0,
                    wing_points: 30.0,
                    min_credit: 2.00,
                    max_risk: 2500.0,
                    entry_minutes_after_open: 30,
                    profit_target_pct: 0.25,
                    stop_loss_mult: 0.0,
                    ..Default::default()
                },
            ],
            slots: vec![0, 1, 2, 3],
        }
    }
}

impl ZeroDteConfig {
    /// The structure assigned to quadrant `i` (0–3), if the slot and roster index
    /// both resolve.
    pub fn slot(&self, i: usize) -> Option<&StructureParams> {
        self.slots.get(i).and_then(|&idx| self.strategies.get(idx))
    }

    /// How many quadrants to render (the 2×2 grid shows at most four).
    pub fn slot_count(&self) -> usize {
        self.slots.len().min(4)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_pct_maps_to_delta_and_recenters_band() {
        let mut eng = EngineConfig::default();
        UserSettings { target_win_pct: 70.0, take_profit_pct: 50.0 }.apply_to(&mut eng);
        // 70% win ⇒ ~0.30Δ short, band re-centered ±0.10 around it.
        assert!((eng.target_delta - 0.30).abs() < 1e-9, "delta {}", eng.target_delta);
        assert!((eng.min_delta - 0.20).abs() < 1e-9);
        assert!((eng.max_delta - 0.40).abs() < 1e-9);
        assert!((eng.take_profit_pct - 0.50).abs() < 1e-9);
    }

    #[test]
    fn from_engine_inverts_apply() {
        // Round-trip: derive knobs from a config, re-apply, get the same delta.
        let eng = EngineConfig::default(); // target_delta 0.30 ⇒ 70% win
        let s = UserSettings::from_engine(&eng);
        assert!((s.target_win_pct - 70.0).abs() < 1e-9);
        assert!((s.take_profit_pct - 50.0).abs() < 1e-9);
        let mut eng2 = EngineConfig::default();
        s.apply_to(&mut eng2);
        assert!((eng2.target_delta - eng.target_delta).abs() < 1e-9);
    }

    #[test]
    fn clamp_holds_knobs_in_range() {
        let mut s = UserSettings { target_win_pct: 999.0, take_profit_pct: -5.0 };
        s.clamp();
        assert_eq!(s.target_win_pct, UserSettings::WIN_MAX);
        assert_eq!(s.take_profit_pct, UserSettings::TP_MIN);
        // Extreme win% still yields a delta inside the engine's sane bounds.
        let mut eng = EngineConfig::default();
        UserSettings { target_win_pct: 99.0, take_profit_pct: 50.0 }.apply_to(&mut eng);
        assert!(eng.min_delta >= 0.05 && eng.max_delta <= 0.95);
    }

    #[test]
    fn settings_blob_roundtrips() {
        let s = UserSettings { target_win_pct: 65.0, take_profit_pct: 40.0 };
        let parsed = UserSettings::parse(&s.to_blob()).expect("roundtrip");
        assert_eq!(parsed, s);
        // Garbage falls back to None so corrupt settings never wedge startup.
        assert!(UserSettings::parse("not valid toml = = =").is_none());
    }

    #[test]
    fn zerodte_defaults_seed_four_slots() {
        let z = ZeroDteConfig::default();
        assert_eq!(z.strategies.len(), 4);
        assert_eq!(z.slot_count(), 4);
        assert_eq!(z.slot(0).unwrap().kind, StructureKind::IronCondor);
        assert_eq!(z.slot(3).unwrap().kind, StructureKind::IronFly);
        assert!(z.slot(9).is_none());
    }

    #[test]
    fn config_loads_partial_zerodte_toml() {
        // A roster entry needs only the fields it overrides; the rest default.
        let toml = r#"
[[zerodte.strategy]]
name = "My Condor"
kind = "IronCondor"
short_delta = 0.10
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.zerodte.strategies.len(), 1);
        assert_eq!(cfg.zerodte.strategies[0].name, "My Condor");
        assert!((cfg.zerodte.strategies[0].short_delta - 0.10).abs() < 1e-9);
        assert_eq!(cfg.zerodte.strategies[0].kind, StructureKind::IronCondor);
        // A defaulted field still carries the StructureParams default.
        assert_eq!(cfg.zerodte.strategies[0].profit_target_pct, 0.40);
    }
}
