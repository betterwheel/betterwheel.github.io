//! Plain data types consumed and produced by the strategy engine.
//!
//! Nothing here performs I/O. The IBKR layer adapts `ibapi` types into these,
//! and the engine turns them into [`Suggestion`]s.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Option right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Right {
    Put,
    Call,
}

impl Right {
    /// The single-letter IBKR/wire code for this right (`"P"` / `"C"`). The one
    /// source of truth for the P/C strings that flow into combo legs, order
    /// requests, journal rows, and the leg-encoding blob.
    pub fn code(self) -> &'static str {
        match self {
            Right::Put => "P",
            Right::Call => "C",
        }
    }

    /// Parse an IBKR right code. IBKR emits several spellings (`P`/`PUT`,
    /// `C`/`CALL`); anything whose first letter is `C` is a call, everything else
    /// (including our own `"P"`) is a put — matching the historical "non-`C` means
    /// put" convention used at the encode/decode and roll sites.
    pub fn from_code(s: &str) -> Right {
        if s.starts_with('C') || s.starts_with('c') {
            Right::Call
        } else {
            Right::Put
        }
    }
}

/// A single option contract's market snapshot, as the engine needs it.
#[derive(Debug, Clone)]
pub struct OptionQuote {
    pub right: Right,
    pub strike: f64,
    pub expiry: NaiveDate,
    pub bid: f64,
    pub ask: f64,
    /// Delta as IBKR reports it: calls positive, puts negative. `None` if the
    /// greek is unavailable (no subscription / illiquid), in which case the
    /// engine may fall back to a Black-Scholes estimate.
    pub delta: Option<f64>,
    pub implied_volatility: Option<f64>,
    pub open_interest: Option<i64>,
}

impl OptionQuote {
    /// Mid price; falls back to bid when the ask is missing or non-positive.
    pub fn mid(&self) -> f64 {
        if self.ask > 0.0 && self.bid > 0.0 {
            (self.bid + self.ask) / 2.0
        } else {
            self.bid.max(0.0)
        }
    }

    /// Absolute delta in `[0, 1]` if known.
    pub fn abs_delta(&self) -> Option<f64> {
        self.delta.map(f64::abs)
    }
}

/// Latest price of the underlying instrument.
#[derive(Debug, Clone, Copy)]
pub struct UnderlyingQuote {
    pub last: f64,
}

/// The wheel leg a symbol is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WheelState {
    /// No position — eligible to sell a cash-secured put.
    #[default]
    Idle,
    /// A short cash-secured put is open.
    ShortPut,
    /// A short put hedged by a long put below it (a defined-risk put credit
    /// spread — the Hedged Wheel's open position). Managed like [`Self::ShortPut`]
    /// on the short leg; the long leg is protection that rides along.
    HedgedShortPut,
    /// Assigned: holding >= 100 shares, eligible to sell covered calls.
    LongShares,
    /// A covered call is open against held shares.
    ShortCall,
}

impl WheelState {
    /// Stable string used for persistence and display.
    pub fn as_str(self) -> &'static str {
        match self {
            WheelState::Idle => "Idle",
            WheelState::ShortPut => "ShortPut",
            WheelState::HedgedShortPut => "HedgedShortPut",
            WheelState::LongShares => "LongShares",
            WheelState::ShortCall => "ShortCall",
        }
    }

    /// Parse a persisted state string, falling back to [`WheelState::Idle`].
    pub fn parse(s: &str) -> WheelState {
        match s {
            "ShortPut" => WheelState::ShortPut,
            "HedgedShortPut" => WheelState::HedgedShortPut,
            "LongShares" => WheelState::LongShares,
            "ShortCall" => WheelState::ShortCall,
            _ => WheelState::Idle,
        }
    }
}

/// An open short option we already hold (used by the management logic).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenShortOption {
    pub right: Right,
    pub strike: f64,
    pub expiry: NaiveDate,
    /// Premium received per share when the position was opened.
    pub entry_credit: f64,
    /// Number of contracts (positive).
    pub quantity: i32,
}

/// A stock position held after assignment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharePosition {
    pub shares: i64,
    /// Per-share cost basis, ideally reduced by premium already collected.
    pub cost_basis: f64,
}

/// Which way a single structure leg trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegSide {
    Buy,
    Sell,
}

/// One leg of a multi-leg structure (iron condor, butterfly, …). Plain data; the
/// IBKR layer maps these into combo (BAG) legs at order time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StructureLeg {
    pub right: Right,
    pub strike: f64,
    pub side: LegSide,
    /// Per-share price (mid) of this leg when the structure was built.
    pub price: f64,
    /// Absolute delta of this leg, if known.
    pub delta: Option<f64>,
}

/// A named multi-leg, short-dated (0–2 DTE) options structure. Unlike the wheel
/// (which trades one short leg through an assignment lifecycle), these are
/// market-neutral premium structures opened and closed within the trade's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StructureKind {
    /// Short put spread + short call spread (defined risk both sides).
    IronCondor,
    /// One-sided bull put spread.
    PutCreditSpread,
    /// One-sided bear call spread.
    CallCreditSpread,
    /// Unequal-wing put butterfly, structured for a net credit (no upside risk).
    BrokenWingButterfly,
    /// ATM short straddle hedged by wings (max premium, narrow profit zone).
    IronFly,
    /// Naked short put + short call (undefined risk — gated behind config).
    ShortStrangle,
}

impl StructureKind {
    /// Human label for headers and journal rows.
    pub fn label(self) -> &'static str {
        match self {
            StructureKind::IronCondor => "Iron Condor",
            StructureKind::PutCreditSpread => "Put Credit Spread",
            StructureKind::CallCreditSpread => "Call Credit Spread",
            StructureKind::BrokenWingButterfly => "Broken-Wing Butterfly",
            StructureKind::IronFly => "Iron Fly",
            StructureKind::ShortStrangle => "Short Strangle",
        }
    }

    /// Whether this structure carries undefined (naked) risk and so needs the
    /// `allow_naked` gate and a naked option-trading permission tier.
    pub fn is_naked(self) -> bool {
        matches!(self, StructureKind::ShortStrangle)
    }

    /// Whether the delta-sum probability-of-profit estimate is meaningful here.
    /// It only holds for OTM-short structures; a near-ATM body (iron fly) or a
    /// butterfly needs a richer model, so the UI should not surface POP for those.
    pub fn pop_is_meaningful(self) -> bool {
        matches!(
            self,
            StructureKind::IronCondor
                | StructureKind::PutCreditSpread
                | StructureKind::CallCreditSpread
                | StructureKind::ShortStrangle
        )
    }
}

/// The action the engine recommends.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionKind {
    /// Open a cash-secured put.
    SellPut,
    /// Open a covered call against held shares.
    SellCall,
    /// Buy to close an open short for profit.
    CloseForProfit,
    /// Defend a tested short by rolling out (and possibly down/up).
    Roll { to_expiry: NaiveDate, to_strike: f64 },
    /// Open a defined-risk put credit spread: sell `strike`, buy `long_strike`
    /// (further OTM) as protection. Caps max loss to the spread width minus the
    /// net credit. The Hedged Wheel's entry.
    SellPutSpread { long_strike: f64, long_price: f64 },
    /// Open a multi-leg 0DTE/short-dated structure. `legs` is the full leg set in
    /// execution order; the `Suggestion`'s scalar fields describe the primary
    /// short leg (`strike`/`right`/`delta`), the net credit (`limit_price`), and
    /// the defined max loss (`capital_required`). Breakevens / max-loss / POP are
    /// derived from `legs` via [`crate::engine::structures`] helpers.
    OpenStructure { kind: StructureKind, legs: Vec<StructureLeg> },
}

impl ActionKind {
    /// Stable identifier for the journal `action` column (persisted, parsed). Kept
    /// next to [`Self::display_label`] so adding a variant forces both to be
    /// handled rather than silently diverging.
    pub fn persist_key(&self) -> String {
        match self {
            ActionKind::SellPut => "SellPut".into(),
            ActionKind::SellCall => "SellCall".into(),
            ActionKind::CloseForProfit => "Close".into(),
            ActionKind::Roll { .. } => "Roll".into(),
            ActionKind::SellPutSpread { .. } => "SellPutSpread".into(),
            ActionKind::OpenStructure { kind, .. } => kind.label().into(),
        }
    }

    /// Human-readable label for tables and headers (distinct from the persisted
    /// [`Self::persist_key`]).
    pub fn display_label(&self) -> &'static str {
        match self {
            ActionKind::SellPut => "Sell Put",
            ActionKind::SellCall => "Sell Call",
            ActionKind::CloseForProfit => "Close",
            ActionKind::Roll { .. } => "Roll",
            ActionKind::SellPutSpread { .. } => "Put Spread",
            ActionKind::OpenStructure { kind, .. } => kind.label(),
        }
    }
}

/// A single recommended action with everything needed to preview/execute it.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub symbol: String,
    pub kind: ActionKind,
    pub right: Right,
    pub strike: f64,
    /// Underlying spot price when this suggestion was produced. Drives the
    /// detail panel's "P&L if the stock falls X%" scenarios; `0.0` if unknown.
    pub underlying_price: f64,
    pub expiry: NaiveDate,
    pub dte: i64,
    pub quantity: i32,
    /// Suggested limit price (premium per share).
    pub limit_price: f64,
    /// Absolute delta of the chosen contract, if known.
    pub delta: Option<f64>,
    /// Total premium = `limit_price * 100 * quantity`.
    pub premium_total: f64,
    /// Collateral required (`strike * 100 * qty` for a CSP; 0 for a covered call).
    pub capital_required: f64,
    /// Annualized return on collateral as a fraction (0.30 = 30%/yr).
    pub annualized_yield: f64,
    /// Human-readable explanation of why this was chosen.
    pub rationale: String,
}

/// An ordered set of suggested actions.
#[derive(Debug, Clone, Default)]
pub struct ActionPlan {
    pub suggestions: Vec<Suggestion>,
}

/// Tunable strategy parameters. Defaults follow common r/thetagang norms.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Ideal absolute delta for new short options.
    pub target_delta: f64,
    /// Acceptable delta band for entries: `[min_delta, max_delta]`.
    pub min_delta: f64,
    pub max_delta: f64,
    /// Acceptable days-to-expiration band for entries.
    pub min_dte: i64,
    pub max_dte: i64,
    /// Buy-to-close once this fraction of max premium is captured.
    pub take_profit_pct: f64,
    /// Defend (roll) when a short's absolute delta exceeds this.
    pub roll_delta: f64,
    /// Also consider rolling an in-the-money short under this many DTE.
    pub roll_dte: i64,
    /// Skip entries whose annualized yield is below this fraction.
    pub min_annualized_yield: f64,
    /// Liquidity floor: skip strikes with open interest below this (if known).
    pub min_open_interest: i64,
    /// Ignore option prices below this (dust).
    pub min_premium: f64,
    /// Risk-free rate used by the Black-Scholes delta fallback.
    pub risk_free_rate: f64,
    /// Covered-call strikes must sit at least this fraction above cost basis.
    pub cc_min_pct_above_basis: f64,
    /// Hedged Wheel: the protective long put sits at most this fraction below the
    /// short strike (e.g. 0.05 = within 5%), capping the spread width so the hedge
    /// stays tight rather than a far-away one that barely caps risk.
    pub hedge_pct_below: f64,
    /// Hedged Wheel: skip spreads whose net credit (per share) is below this.
    pub hedge_min_credit: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            target_delta: 0.30,
            min_delta: 0.16,
            max_delta: 0.35,
            min_dte: 25,
            max_dte: 45,
            take_profit_pct: 0.50,
            roll_delta: 0.50,
            roll_dte: 21,
            min_annualized_yield: 0.20,
            min_open_interest: 100,
            min_premium: 0.05,
            risk_free_rate: 0.04,
            cc_min_pct_above_basis: 0.0,
            hedge_pct_below: 0.05,
            hedge_min_credit: 0.10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_state_string_roundtrips() {
        for s in [
            WheelState::Idle,
            WheelState::ShortPut,
            WheelState::LongShares,
            WheelState::ShortCall,
        ] {
            assert_eq!(WheelState::parse(s.as_str()), s, "roundtrip {:?}", s);
        }
        // Unknown / corrupt stored states fold to Idle (never miscounted as open).
        assert_eq!(WheelState::parse("nonsense"), WheelState::Idle);
        assert_eq!(WheelState::parse(""), WheelState::Idle);
        assert_eq!(WheelState::default(), WheelState::Idle);
    }

    #[test]
    fn option_quote_mid_falls_back_to_bid() {
        let q = OptionQuote {
            right: Right::Put, strike: 100.0,
            expiry: NaiveDate::from_ymd_opt(2026, 6, 19).unwrap(),
            bid: 1.0, ask: 1.4, delta: Some(-0.3), implied_volatility: Some(0.3),
            open_interest: None,
        };
        assert!((q.mid() - 1.2).abs() < 1e-9);
        // Missing ask → fall back to bid, not a bogus midpoint.
        let no_ask = OptionQuote { ask: 0.0, ..q.clone() };
        assert!((no_ask.mid() - 1.0).abs() < 1e-9);
        assert_eq!(q.abs_delta(), Some(0.3));
    }
}
