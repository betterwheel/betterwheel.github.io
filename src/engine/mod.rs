//! The strategy engine: pure functions turning market + account state into a
//! ranked [`ActionPlan`]. No I/O lives here, so it is fully unit-testable.

pub mod covered_call;
pub mod csp;
pub mod manage;
pub mod math;
pub mod structures;
pub mod types;

pub use types::*;

use chrono::NaiveDate;

/// Everything needed to advise on a single symbol for one planning pass.
pub struct SymbolContext<'a> {
    pub symbol: String,
    pub state: WheelState,
    pub underlying: UnderlyingQuote,
    /// Candidate chain (already narrowed by moneyness) for new entries.
    pub option_quotes: &'a [OptionQuote],
    /// The open short option and its current quote (for `ShortPut`/`ShortCall`).
    pub open_short: Option<(OpenShortOption, OptionQuote)>,
    /// Held shares (for `LongShares`/`ShortCall`).
    pub shares: Option<SharePosition>,
    /// Calls already written against held shares.
    pub committed_call_contracts: i32,
    /// Max collateral the app may deploy on a new CSP for this symbol.
    pub max_collateral: f64,
}

/// Produce suggestions for one symbol based on its current wheel state.
pub fn plan_for_symbol(ctx: &SymbolContext, cfg: &EngineConfig, today: NaiveDate) -> Vec<Suggestion> {
    plan_for_symbol_mode(ctx, cfg, today, false)
}

/// Like [`plan_for_symbol`], but `hedged` makes the entry a defined-risk put
/// credit spread (Hedged Wheel) instead of a cash-secured put. Management of
/// already-open positions is identical in both modes.
fn plan_for_symbol_mode(
    ctx: &SymbolContext,
    cfg: &EngineConfig,
    today: NaiveDate,
    hedged: bool,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    match ctx.state {
        WheelState::Idle => {
            let entry = if hedged {
                csp::select_put_spread(
                    &ctx.symbol,
                    ctx.underlying,
                    ctx.option_quotes,
                    ctx.max_collateral,
                    cfg,
                    today,
                )
            } else {
                csp::select_csp(
                    &ctx.symbol,
                    ctx.underlying,
                    ctx.option_quotes,
                    ctx.max_collateral,
                    cfg,
                    today,
                )
            };
            if let Some(s) = entry {
                out.push(s);
            }
        }
        // A hedged short put is managed exactly like a bare short put on its
        // short leg — close-for-profit or roll the short; the long put is left
        // alone as protection (spread close/roll as a combo is future work).
        WheelState::ShortPut | WheelState::HedgedShortPut | WheelState::ShortCall => {
            if let Some((pos, quote)) = &ctx.open_short
                && let Some(s) =
                    manage::manage_short_option(&ctx.symbol, pos, quote, ctx.underlying, cfg, today)
                {
                    out.push(s);
                }
        }
        WheelState::LongShares => {
            if let Some(shares) = ctx.shares
                && let Some(s) = covered_call::select_covered_call(
                    &ctx.symbol,
                    ctx.underlying,
                    shares,
                    ctx.committed_call_contracts,
                    ctx.option_quotes,
                    cfg,
                    today,
                ) {
                    out.push(s);
                }
        }
    }
    out
}

/// Build a full plan across many symbols, ordered so time-sensitive management
/// actions (closes, rolls) come before new entries, then by yield. Classic Wheel
/// (cash-secured put entries).
pub fn plan(contexts: &[SymbolContext], cfg: &EngineConfig, today: NaiveDate) -> ActionPlan {
    plan_with(contexts, cfg, today, false)
}

/// Like [`plan`], but entries are defined-risk put credit spreads (Hedged Wheel).
pub fn plan_hedged(contexts: &[SymbolContext], cfg: &EngineConfig, today: NaiveDate) -> ActionPlan {
    plan_with(contexts, cfg, today, true)
}

fn plan_with(
    contexts: &[SymbolContext],
    cfg: &EngineConfig,
    today: NaiveDate,
    hedged: bool,
) -> ActionPlan {
    let mut suggestions: Vec<Suggestion> = contexts
        .iter()
        .flat_map(|ctx| plan_for_symbol_mode(ctx, cfg, today, hedged))
        .collect();

    suggestions.sort_by(|a, b| {
        kind_priority(&a.kind)
            .cmp(&kind_priority(&b.kind))
            .then(
                b.annualized_yield
                    .partial_cmp(&a.annualized_yield)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    ActionPlan { suggestions }
}

/// Lower value sorts first.
fn kind_priority(k: &ActionKind) -> u8 {
    match k {
        ActionKind::CloseForProfit => 0,
        ActionKind::Roll { .. } => 1,
        ActionKind::SellPut
        | ActionKind::SellCall
        | ActionKind::SellPutSpread { .. }
        | ActionKind::OpenStructure { .. } => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    #[test]
    fn idle_symbol_yields_a_csp() {
        let quotes = vec![OptionQuote {
            right: Right::Put,
            strike: 95.0,
            expiry: day(30),
            bid: 1.75,
            ask: 1.85,
            delta: Some(-0.30),
            implied_volatility: Some(0.3),
            open_interest: Some(500),
            volume: Some(100),
        }];
        let ctx = SymbolContext {
            symbol: "AAPL".to_string(),
            state: WheelState::Idle,
            underlying: UnderlyingQuote { last: 100.0 },
            option_quotes: &quotes,
            open_short: None,
            shares: None,
            committed_call_contracts: 0,
            max_collateral: 10_000.0,
        };
        let plan = plan(std::slice::from_ref(&ctx), &EngineConfig::default(), day(0));
        assert_eq!(plan.suggestions.len(), 1);
        assert_eq!(plan.suggestions[0].kind, ActionKind::SellPut);
    }

    #[test]
    fn management_sorts_before_entries() {
        // One idle symbol (entry) and one with a profitable short (close).
        let put_quotes = vec![OptionQuote {
            right: Right::Put,
            strike: 95.0,
            expiry: day(30),
            bid: 1.75,
            ask: 1.85,
            delta: Some(-0.30),
            implied_volatility: Some(0.3),
            open_interest: Some(500),
            volume: Some(100),
        }];
        let entry = SymbolContext {
            symbol: "AAPL".to_string(),
            state: WheelState::Idle,
            underlying: UnderlyingQuote { last: 100.0 },
            option_quotes: &put_quotes,
            open_short: None,
            shares: None,
            committed_call_contracts: 0,
            max_collateral: 10_000.0,
        };

        let close_quote = OptionQuote {
            right: Right::Put,
            strike: 50.0,
            expiry: day(15),
            bid: 0.20,
            ask: 0.24,
            delta: Some(-0.10),
            implied_volatility: Some(0.3),
            open_interest: Some(500),
            volume: Some(100),
        };
        let manage = SymbolContext {
            symbol: "MSFT".to_string(),
            state: WheelState::ShortPut,
            underlying: UnderlyingQuote { last: 60.0 },
            option_quotes: &[],
            open_short: Some((
                OpenShortOption {
                    right: Right::Put,
                    strike: 50.0,
                    expiry: day(15),
                    entry_credit: 1.00,
                    quantity: 1,
                },
                close_quote,
            )),
            shares: None,
            committed_call_contracts: 0,
            max_collateral: 10_000.0,
        };

        let plan = plan(&[entry, manage], &EngineConfig::default(), day(0));
        assert_eq!(plan.suggestions.len(), 2);
        // Close (management) must come first.
        assert_eq!(plan.suggestions[0].kind, ActionKind::CloseForProfit);
        assert_eq!(plan.suggestions[1].kind, ActionKind::SellPut);
    }
}
