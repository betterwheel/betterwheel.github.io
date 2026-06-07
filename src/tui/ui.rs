//! Rendering — a pure function of [`App`] state.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

use super::app::{App, InputMode, Tab};
use crate::engine::math::short_put_pnl_at;
use crate::engine::structures;
use crate::engine::types::{ActionKind, LegSide, Right, StructureKind, StructureLeg, Suggestion};

const SEL: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
const HEAD: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

pub fn render(frame: &mut Frame, app: &App) {
    let [top, mid, bottom] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());

    render_tabs(frame, app, top);
    match app.tab {
        Tab::Dashboard => render_dashboard(frame, app, mid),
        Tab::Watchlist => render_watchlist(frame, app, mid),
        Tab::Suggestions => render_suggestions(frame, app, mid),
        Tab::HedgedWheel => render_hedged_suggestions(frame, app, mid),
        Tab::ZeroDte => render_zerodte(frame, app, mid),
        Tab::Journal => render_journal(frame, app, mid),
        Tab::Settings => render_settings(frame, app, mid),
        Tab::Help => render_help(frame, mid),
    }
    render_status(frame, app, bottom);

    // Modal detail panel over the selected suggestion, drawn last so it's on top.
    if app.detail_open {
        render_suggestion_detail(frame, app, frame.area());
    }
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = Tab::ALL.iter().map(|t| Line::from(format!(" {} ", t.title())));
    let conn = if app.connected { "● live" } else { "○ offline" };
    let sync = match app.loading_elapsed() {
        Some(d) => {
            // Braille spinner advanced off elapsed time so it animates each tick.
            const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let f = FRAMES[(d.as_millis() / 120) as usize % FRAMES.len()];
            format!("  {f} syncing {}s", d.as_secs())
        }
        None => String::new(),
    };
    let armed = if app.armed { "  ⚡ ARMED" } else { "" };
    let auto = match app.zerodte_automating() {
        0 => String::new(),
        n => format!("  ⚡ {n} AUTO-TRADING"),
    };
    let title = format!(
        "TheWheel  [{}]  {conn}{sync}  ·  {} open{armed}{auto}",
        app.mode_label(),
        app.open_position_count()
    );
    let mut block = Block::bordered().title(title);
    if app.armed || app.zerodte_automating() > 0 {
        // A loud, hard-to-miss cue that the app may transmit a live order: while
        // armed (`x`) or whenever a 0DTE slot is auto-trading unattended.
        block = block.border_style(Style::new().fg(Color::Red).add_modifier(Modifier::BOLD));
    }
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .block(block)
        .highlight_style(SEL.add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let text = match &app.input {
        InputMode::AddSymbol(buf) => format!(" add symbol: {buf}_ "),
        InputMode::ConfirmLive(buf) => {
            format!(" confirm LIVE: {buf}_   (type {} then Enter, Esc cancels) ", super::app::LIVE_CONFIRM_PHRASE)
        }
        InputMode::Normal => format!(
            " {}   ·   q quit · tab switch · j/k move · enter details · a add · d del · r refresh · ? help",
            app.status
        ),
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn render_dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let acct = match &app.account {
        Some(a) => format!(
            "net liq {}   cash {}   buying power {}",
            money(a.net_liquidation),
            money(a.total_cash),
            money(a.buying_power)
        ),
        None if app.connected => "—  (fetching…)".into(),
        None => match &app.offline_reason {
            Some(r) => format!("—  ({r})"),
            None => "—  (offline; start IB Gateway/TWS and set [connection] in config.toml)".into(),
        },
    };
    let armed = if app.armed {
        Span::styled(
            "ARMED — `x` transmits a live order",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("disarmed (`A` to arm)", Style::new().fg(Color::DarkGray))
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Mode:        ", HEAD),
            Span::raw(format!("{}   ", app.mode_label())),
            Span::styled("Connection:  ", HEAD),
            Span::raw(if app.connected {
                "live".to_string()
            } else if app.cfg.connection.reconnect_secs > 0 {
                format!(
                    "offline / demo — retrying every {}s",
                    app.cfg.connection.reconnect_secs
                )
            } else {
                "offline / demo data".to_string()
            }),
        ]),
        Line::from(vec![Span::styled("Account:     ", HEAD), Span::raw(acct)]),
        Line::from(vec![Span::styled("Armed:       ", HEAD), armed]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Watchlist:   ", HEAD),
            Span::raw(format!("{} symbols", app.watchlist.len())),
        ]),
        Line::from(vec![
            Span::styled("Suggestions: ", HEAD),
            Span::raw(format!("{} ready", app.suggestions.len())),
        ]),
        Line::from(vec![
            Span::styled("Positions:   ", HEAD),
            Span::raw(format!(
                "{} open / {} tracked",
                app.open_position_count(),
                app.positions.len()
            )),
        ]),
        Line::from(""),
        match app.loading_elapsed() {
            Some(d) => Line::from(Span::styled(
                format!(
                    "⟳ Syncing live data from IB Gateway… {}s   (first sync is slow on delayed data)",
                    d.as_secs()
                ),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            None => Line::from(Span::styled(
                "Add symbols on the Watchlist tab; the Suggestions tab ranks cash-secured puts.",
                Style::new().fg(Color::DarkGray),
            )),
        },
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Overview "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_watchlist(frame: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["Symbol", "Type", "On", "Tradable", "ConID", "Notes"]).style(HEAD);
    let rows = app.watchlist.iter().enumerate().map(|(i, r)| {
        let cells = vec![
            r.symbol.clone(),
            r.sec_type.clone(),
            if r.is_enabled() { "yes".into() } else { "no".into() },
            r.tradable_label().to_string(),
            r.conid.map(|c| c.to_string()).unwrap_or_default(),
            r.notes.clone().unwrap_or_default(),
        ];
        styled_row(cells, app.tab == Tab::Watchlist && i == app.selected)
    });
    let widths = [
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Min(6),
    ];
    let title = if app.watchlist.is_empty() {
        " Watchlist — empty; press 'a' to add a symbol ".to_string()
    } else {
        format!(" Watchlist ({}) ", app.watchlist.len())
    };
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
}

fn render_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    render_suggestion_table(frame, app, area, &app.suggestions, Tab::Suggestions, "cash-secured puts");
}

/// Hedged Wheel tab: defined-risk put credit spreads (see [`render_suggestions`]).
fn render_hedged_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    render_suggestion_table(
        frame,
        app,
        area,
        &app.hedged_suggestions,
        Tab::HedgedWheel,
        "defined-risk put spreads",
    );
}

/// Shared table renderer for the Classic Suggestions and Hedged Wheel tabs.
fn render_suggestion_table(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    suggestions: &[Suggestion],
    tab: Tab,
    noun: &str,
) {
    let header =
        Row::new(["Symbol", "Action", "Strike", "Expiry", "DTE", "Δ", "Premium", "Ann%", "Capital"])
            .style(HEAD);
    let rows = suggestions
        .iter()
        .enumerate()
        .map(|(i, s)| styled_row(suggestion_cells(s), app.tab == tab && i == app.selected));
    let widths = [
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(10),
    ];
    let title = if !suggestions.is_empty() {
        format!(" {} ({}) — {noun}, best yield first ", tab.title(), suggestions.len())
    } else if let Some(d) = app.loading_elapsed() {
        format!(" {} — ⟳ syncing live data… {}s ", tab.title(), d.as_secs())
    } else {
        format!(" {} — none yet (press 'r' to refresh, or add tickers) ", tab.title())
    };
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
}

/// The 0DTE tab: a 2×2 grid of strategy quadrants, one per configured slot. Each
/// shows the slot's current structure (legs, credit, max loss, breakevens, POP)
/// and its automation state. The focused quadrant is `app.selected`.
fn render_zerodte(frame: &mut Frame, app: &App, area: Rect) {
    if app.cfg.zerodte.slot_count() == 0 {
        frame.render_widget(
            Paragraph::new(
                "No 0DTE strategies configured. Add [[zerodte.strategy]] entries to config.toml.",
            )
            .block(Block::bordered().title(" 0DTE "))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let [q0, q1] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    let [q2, q3] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);
    for (i, rect) in [q0, q1, q2, q3].into_iter().enumerate() {
        render_zerodte_quadrant(frame, app, rect, i);
    }
}

/// One quadrant of the 0DTE grid: the structure assigned to slot `i`.
fn render_zerodte_quadrant(frame: &mut Frame, app: &App, area: Rect, i: usize) {
    let focused = app.tab == Tab::ZeroDte && app.selected == i;
    let border = if focused && app.armed {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };

    let Some(params) = app.cfg.zerodte.slot(i) else {
        let block = Block::bordered()
            .border_style(border)
            .title(format!(" slot {} — empty ", i + 1));
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    };

    // Title: name + underlying/DTE, with a loud AUTO badge when the slot is armed
    // for unattended trading.
    let auto = if params.automate {
        Span::styled(" ⚡AUTO ", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" manual ", Style::new().fg(Color::DarkGray))
    };
    let title = Line::from(vec![
        Span::styled(
            format!(" {} · {} {}DTE ", params.name, params.underlying, params.dte),
            if focused { HEAD } else { Style::new().fg(Color::Gray) },
        ),
        auto,
    ]);

    let dim = Style::new().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match app.zerodte_suggestions.get(i).and_then(|o| o.as_ref()) {
        Some(s) => {
            lines.push(Line::from(vec![
                Span::styled("credit ", HEAD),
                Span::styled(format!("${:.0}", s.premium_total), Style::new().fg(Color::Green)),
                Span::raw("   "),
                Span::styled("max loss ", HEAD),
                Span::styled(format!("${:.0}", s.capital_required), Style::new().fg(Color::Red)),
                Span::styled(format!("   ×{}", s.quantity), dim),
            ]));
            if let ActionKind::OpenStructure { kind, legs } = &s.kind {
                let bes = structures::breakevens(legs);
                let be = match bes.as_slice() {
                    [lo, .., hi] => format!("{lo:.0}–{hi:.0}"),
                    [b] => format!("{b:.0}"),
                    [] => "—".into(),
                };
                let pop = kind
                    .pop_is_meaningful()
                    .then(|| structures::estimate_pop(legs))
                    .flatten()
                    .map(|p| format!("  ·  est win ~{:.0}%", p * 100.0))
                    .unwrap_or_default();
                lines.push(Line::styled(
                    format!("breakeven {be}  ·  ror {:.0}%{pop}", s.annualized_yield * 100.0),
                    dim,
                ));
                lines.push(Line::from(""));
                for leg in legs.iter() {
                    let (label, st) = match leg.side {
                        LegSide::Sell => ("sell", Style::new().fg(Color::Yellow)),
                        LegSide::Buy => ("buy ", dim),
                    };
                    let r = leg.right.code();
                    lines.push(Line::styled(
                        format!("  {label} {:.0}{r} @ {:.2}", leg.strike, leg.price),
                        st,
                    ));
                }
            }
            if focused {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "[enter] details · [p] preview · [A] arm · [x] execute",
                    dim,
                ));
                lines.push(Line::styled(
                    "[t] automate · [+/−] risk · [ [ / ] ] profit target",
                    dim,
                ));
            }
        }
        None => {
            if let Some(d) = app.loading_elapsed() {
                lines.push(Line::styled(format!("⟳ syncing… {}s", d.as_secs()), Style::new().fg(Color::Yellow)));
            } else {
                lines.push(Line::styled("no structure fits right now", dim));
                lines.push(Line::styled("(tune delta / credit / max_risk, or 'r' to refresh)", dim));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().border_style(border).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// A `pct_x` × `pct_y` percent rectangle centered in `area`, for modal popups.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let [_, v, _] = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .areas(area);
    let [_, h, _] = Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .areas(v);
    h
}

/// Short human label for an action kind.
fn action_title(kind: &ActionKind) -> &'static str {
    match kind {
        ActionKind::SellPut => "Cash-Secured Put",
        ActionKind::SellCall => "Covered Call",
        ActionKind::CloseForProfit => "Close for Profit",
        ActionKind::Roll { .. } => "Roll (defend)",
        ActionKind::SellPutSpread { .. } => "Put Credit Spread",
        ActionKind::OpenStructure { kind, .. } => kind.label(),
    }
}

/// Modal detail panel for the selected suggestion: plain-English mechanics plus
/// the gated actions. Drawn over everything else.
fn render_suggestion_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.selected_suggestion() else {
        return;
    };
    let popup = centered_rect(82, 86, area);
    frame.render_widget(Clear, popup);
    let border = if app.armed {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Cyan)
    };
    let block = Block::bordered()
        .title(format!(" {} — {} ", s.symbol, action_title(&s.kind)))
        .border_style(border);
    frame.render_widget(
        Paragraph::new(suggestion_detail_lines(s, app))
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// Plain-English explanation of a suggestion (what you sell, collect, and risk)
/// plus an action bar reflecting the preview → arm → execute gate.
/// One detail line for the take-profit target: the buy-back price and the
/// profit banked when `take_profit_pct` of the credit is captured early. Uses
/// the live (Settings-tunable) `take_profit_pct`, so it tracks the user's knob.
fn take_profit_line(s: &Suggestion, app: &App) -> Line<'static> {
    let tp = app.cfg.engine.take_profit_pct;
    Line::from(format!(
        "  Take-profit   {:.0}% → buy back ~${:.2}/sh, bank ~${:.0}",
        tp * 100.0,
        s.limit_price * (1.0 - tp),
        s.premium_total * tp
    ))
}

/// P&L-at-expiry rows for a series of downward moves in the underlying — the
/// concrete "what do I actually lose if it drops X%" that a strike-to-zero line
/// never answered. `long_strike` `Some(..)` caps the loss (a put spread); `None`
/// is a bare short put. Empty if the spot is unknown. Profit green, loss red.
fn downside_scenarios(
    spot: f64,
    short_strike: f64,
    credit: f64,
    long_strike: Option<f64>,
    shares: f64,
) -> Vec<Line<'static>> {
    if spot <= 0.0 {
        return Vec::new();
    }
    let dim = Style::new().fg(Color::DarkGray);
    let good = Style::new().fg(Color::Green);
    let bad = Style::new().fg(Color::Red);
    // Header anchors the drops to the *current* price and how far the strike sits
    // below it — without that, a −10% row that still keeps the credit reads as
    // impossible (the strike is already well below today's price).
    let header = if short_strike < spot {
        let pct_below = (spot - short_strike) / spot * 100.0;
        format!("  Stock ${spot:.2} now — the ${short_strike:.1} strike is {pct_below:.0}% below:")
    } else {
        "  If the stock falls, your P&L at expiry:".to_string()
    };
    let mut out = vec![Line::styled(header, dim)];
    for mv in [0.05f64, 0.10, 0.20, 0.30, 0.40] {
        let s_t = spot * (1.0 - mv);
        let pnl = short_put_pnl_at(s_t, short_strike, credit, long_strike, shares);
        let (style, sign) = if pnl >= 0.0 { (good, "+") } else { (bad, "−") };
        // Tag the rows that never reach the strike — that's *why* the credit is kept.
        let tag = if s_t >= short_strike { "   · above strike" } else { "" };
        out.push(Line::styled(
            format!("    −{:>2.0}%  →  ${s_t:>8.2}   {sign}${:>9.0}{tag}", mv * 100.0, pnl.abs()),
            style,
        ));
    }
    out
}

/// Two-sided "what if it closes here" P&L table for a multi-leg structure: the
/// actual profit/loss (green/red) if the underlying settles at a spread of moves
/// around the current spot. The concrete answer to "what am I betting on".
fn structure_scenarios(legs: &[StructureLeg], spot: f64, symbol: &str, shares: f64) -> Vec<Line<'static>> {
    if spot <= 0.0 {
        return Vec::new();
    }
    let dim = Style::new().fg(Color::DarkGray);
    let good = Style::new().fg(Color::Green);
    let bad = Style::new().fg(Color::Red);
    let mut out = vec![Line::styled(
        format!("  {symbol} is ${spot:.0} now — where it CLOSES decides your P&L:"),
        dim,
    )];
    for mv in [-0.02f64, -0.01, -0.005, 0.0, 0.005, 0.01, 0.02] {
        let close = spot * (1.0 + mv);
        let pnl = structures::payoff_at(legs, close) * shares;
        let (style, sign) = if pnl >= 0.0 { (good, "+") } else { (bad, "−") };
        let here = if mv == 0.0 { "  (flat)" } else { "" };
        out.push(Line::styled(
            format!("    {:>+5.1}%  →  ${close:>8.0}   {sign}${:>7.0}{here}", mv * 100.0, pnl.abs()),
            style,
        ));
    }
    out
}

fn suggestion_detail_lines(s: &Suggestion, app: &App) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let good = Style::new().fg(Color::Green);
    let warn = Style::new().fg(Color::Yellow);
    let qty = s.quantity;
    let shares = qty * 100;
    let prem = s.premium_total;
    let right = match s.right {
        Right::Put => "put",
        Right::Call => "call",
    };
    let mut lines: Vec<Line<'static>> = Vec::new();

    match &s.kind {
        ActionKind::SellPut => {
            let breakeven = s.strike - s.limit_price;
            lines.push(Line::from(format!(
                "SELL {qty} {} ${:.1} put, expiring {} ({}d).",
                s.symbol, s.strike, s.expiry, s.dte
            )));
            lines.push(Line::styled(
                "You're selling insurance against the stock falling below the strike.",
                dim,
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "  Collect now   ${prem:.0}   (premium — yours to keep)"
            )));
            lines.push(Line::from(format!(
                "  Set aside     ${:.0}   (cash collateral)",
                s.capital_required
            )));
            lines.push(Line::from(format!(
                "  Breakeven     ${breakeven:.2}   (strike − premium)"
            )));
            if let Some(d) = s.delta {
                lines.push(Line::from(format!(
                    "  Win chance    ~{:.0}%   ({d:.2}Δ → ~{:.0}% chance of assignment)",
                    (1.0 - d) * 100.0,
                    d * 100.0
                )));
            }
            lines.push(take_profit_line(s, app));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!(
                    "If above ${:.1} at expiry → expires worthless, keep ${prem:.0} (~{:.0}% annualized).",
                    s.strike,
                    s.annualized_yield * 100.0
                ),
                good,
            ));
            lines.push(Line::styled(
                format!(
                    "If below ${:.1} → assigned: BUY {shares} shares @ ${:.1} (${:.0}), cost ${breakeven:.2}/sh.",
                    s.strike, s.strike, s.capital_required
                ),
                warn,
            ));
            lines.push(Line::from(""));
            lines.extend(downside_scenarios(
                s.underlying_price,
                s.strike,
                s.limit_price,
                None,
                shares as f64,
            ));
            lines.push(Line::styled(
                "  (then the wheel sells covered calls on the shares to lower the basis)",
                dim,
            ));
        }
        ActionKind::SellCall => {
            lines.push(Line::from(format!(
                "SELL {qty} {} ${:.1} call, expiring {} ({}d), against your shares.",
                s.symbol, s.strike, s.expiry, s.dte
            )));
            lines.push(Line::styled(
                "You own the shares; you're selling someone the right to buy them at the strike.",
                dim,
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "  Collect now   ${prem:.0}   (premium — yours to keep)"
            )));
            if let Some(d) = s.delta {
                lines.push(Line::from(format!(
                    "  Win chance    ~{:.0}%   ({d:.2}Δ → ~{:.0}% chance of being called away)",
                    (1.0 - d) * 100.0,
                    d * 100.0
                )));
            }
            lines.push(take_profit_line(s, app));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!(
                    "If below ${:.1} at expiry → expires worthless: keep the premium AND your shares.",
                    s.strike
                ),
                good,
            ));
            lines.push(Line::styled(
                format!(
                    "If above ${:.1} → shares sold @ ${:.1}: keep premium + gains to the strike, miss upside above.",
                    s.strike, s.strike
                ),
                warn,
            ));
        }
        ActionKind::CloseForProfit => {
            lines.push(Line::from(format!(
                "BUY TO CLOSE {qty} {} ${:.1} {right}, ~${:.2}/sh (${prem:.0} total).",
                s.symbol, s.strike, s.limit_price
            )));
            lines.push(Line::styled(
                "This short has lost most of its value — buying it back locks in the gain and frees the collateral early.",
                dim,
            ));
        }
        ActionKind::Roll { to_expiry, to_strike } => {
            lines.push(Line::from(format!(
                "ROLL {qty} {} ${:.1} {right} → ${:.1} @ {}.",
                s.symbol, s.strike, to_strike, to_expiry
            )));
            lines.push(Line::styled(
                "This short is being tested. Buy it back and sell a later one (restruck) for a net credit — buys time to defend instead of taking assignment now.",
                dim,
            ));
        }
        ActionKind::SellPutSpread { long_strike, long_price } => {
            let width = s.strike - *long_strike;
            let max_loss = s.capital_required - prem;
            let breakeven = s.strike - s.limit_price;
            lines.push(Line::from(format!(
                "SELL {qty} {} ${:.1}/${long_strike:.1} put spread, expiring {} ({}d).",
                s.symbol, s.strike, s.expiry, s.dte
            )));
            lines.push(Line::styled(
                format!(
                    "Defined-risk hedge: sell the ${:.1} put, buy the ${long_strike:.1} put (${long_price:.2}) as protection.",
                    s.strike
                ),
                dim,
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "  Net credit    ${prem:.0}   (premium in − protection cost)"
            )));
            lines.push(Line::from(format!(
                "  Max loss      ${max_loss:.0}   (CAPPED — ${:.0} width minus credit)",
                width * 100.0 * qty as f64
            )));
            lines.push(Line::from(format!(
                "  Breakeven     ${breakeven:.2}   (short strike − net credit)"
            )));
            if let Some(d) = s.delta {
                lines.push(Line::from(format!(
                    "  Win chance    ~{:.0}%   ({d:.2}Δ → ~{:.0}% chance the short is ITM)",
                    (1.0 - d) * 100.0,
                    d * 100.0
                )));
            }
            lines.push(take_profit_line(s, app));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!(
                    "If above ${:.1} at expiry → both expire worthless, keep ${prem:.0} (~{:.0}% annualized).",
                    s.strike,
                    s.annualized_yield * 100.0
                ),
                good,
            ));
            lines.push(Line::styled(
                format!("If it falls → loss is CAPPED at ${max_loss:.0}; the ${long_strike:.1} put stops further downside."),
                warn,
            ));
            lines.push(Line::from(""));
            lines.extend(downside_scenarios(
                s.underlying_price,
                s.strike,
                s.limit_price,
                Some(*long_strike),
                shares as f64,
            ));
        }
        ActionKind::OpenStructure { kind, legs } => {
            lines.extend(structure_detail_lines(s, *kind, legs));
        }
    }

    lines.push(Line::from(""));
    if app.armed {
        lines.push(Line::styled(
            "⚡ ARMED — [x] EXECUTE LIVE   ·   [A] disarm   ·   [p] preview   ·   [Esc] back",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("[p]", HEAD),
            Span::raw(" preview what-if    "),
            Span::styled("[A]", HEAD),
            Span::raw(" arm    "),
            Span::styled("[x]", HEAD),
            Span::raw(" execute (arm first)    "),
            Span::styled("[Esc]", HEAD),
            Span::raw(" back"),
        ]));
    }
    lines.push(Line::styled(format!("  {}", app.status), dim));
    lines
}

/// The detail-panel body for a 0DTE/short-dated structure: the "what you're
/// betting" thesis + win/lose zones, the combo legs, the money summary, and
/// concrete close-here P&L. Split out of [`suggestion_detail_lines`] so the
/// per-`StructureKind` rendering is one focused unit instead of a nested match.
fn structure_detail_lines(
    s: &Suggestion,
    kind: StructureKind,
    legs: &[StructureLeg],
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let good = Style::new().fg(Color::Green);
    let red_bold = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let qty = s.quantity;
    let shares = qty * 100;
    let prem = s.premium_total;
    let sym = s.symbol.as_str();
    let exp = s.expiry;
    let spot = s.underlying_price;
    let max_loss = s.capital_required;
    let when = if s.dte == 0 { "expires TODAY".to_string() } else { format!("expires in {}d", s.dte) };
    let bes = structures::breakevens(legs);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(format!("OPEN {qty} {sym} {} — {when} ({exp}).", kind.label())));
    lines.push(Line::from(""));

    // Plain-English "what you're betting", then the concrete WIN / LOSE zones.
    lines.extend(structure_thesis_lines(s, kind, legs));
    if kind.is_naked() {
        lines.push(Line::styled("⚠ NAKED — needs a naked-options permission tier; size small.", red_bold));
    }

    // The combo legs, annotated.
    lines.push(Line::from(""));
    lines.push(Line::styled("The legs (sent as ONE combo order):", dim));
    for leg in legs.iter() {
        let side = match leg.side {
            LegSide::Buy => "BUY ",
            LegSide::Sell => "SELL",
        };
        let r = leg.right.code();
        let role = match leg.side {
            LegSide::Buy => " (wing — your protection)",
            LegSide::Sell => " (short — the bet)",
        };
        lines.push(Line::from(format!("  {side}  ${:.0}{r}  @ ${:.2}{role}", leg.strike, leg.price)));
    }

    // The money, in plain terms.
    lines.push(Line::from(""));
    lines.push(Line::from(format!("  Collect now   ${prem:.0}   (the credit — yours to keep if you win)")));
    if kind.is_naked() {
        lines.push(Line::from(format!("  Risk          ${max_loss:.0}   (NAKED — effectively uncapped on a gap)")));
    } else {
        lines.push(Line::from(format!("  Risk up to    ${max_loss:.0}   (the MOST you can lose — capped by the wings)")));
    }
    match bes.as_slice() {
        [lo, .., hi] => lines.push(Line::from(format!("  Breakevens    ${lo:.0}  /  ${hi:.0}   (profit only between these)"))),
        [be] => lines.push(Line::from(format!("  Breakeven     ${be:.0}"))),
        [] => {}
    }
    if kind.pop_is_meaningful()
        && let Some(p) = structures::estimate_pop(legs)
    {
        lines.push(Line::from(format!("  Est. win      ~{:.0}%   (chance it lands in the win zone)", p * 100.0)));
    }

    // Concrete "if it closes here" P&L, both directions.
    lines.push(Line::from(""));
    lines.extend(structure_scenarios(legs, spot, sym, shares as f64));

    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Take profit at the slot's target; for defined risk, let the wings be the stop.",
        good,
    ));
    lines
}

/// The per-`StructureKind` "you're betting…" thesis and WIN/LOSE zone lines —
/// the one place that knows each structure's shape in plain English.
fn structure_thesis_lines(
    s: &Suggestion,
    kind: StructureKind,
    legs: &[StructureLeg],
) -> Vec<Line<'static>> {
    let good = Style::new().fg(Color::Green);
    let warn = Style::new().fg(Color::Yellow);
    let thesis = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let red_bold = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let sym = s.symbol.as_str();
    let spot = s.underlying_price;
    let prem = s.premium_total;
    let max_loss = s.capital_required;
    let close_ref = if s.dte == 0 { "today's close".to_string() } else { format!("the {} close", s.expiry) };

    // Nearest-the-money short put / short call and the protective wings.
    let sp = legs.iter().filter(|l| l.side == LegSide::Sell && l.right == Right::Put).map(|l| l.strike).reduce(f64::max);
    let sc = legs.iter().filter(|l| l.side == LegSide::Sell && l.right == Right::Call).map(|l| l.strike).reduce(f64::min);
    let lp = legs.iter().filter(|l| l.side == LegSide::Buy && l.right == Right::Put).map(|l| l.strike).reduce(f64::min);
    let lc = legs.iter().filter(|l| l.side == LegSide::Buy && l.right == Right::Call).map(|l| l.strike).reduce(f64::max);
    let bes = structures::breakevens(legs);
    let be_lo = bes.first().copied();
    let be_hi = bes.last().copied();

    let mut lines: Vec<Line<'static>> = Vec::new();
    match kind {
        StructureKind::IronCondor => {
            if let (Some(sp), Some(sc)) = (sp, sc) {
                lines.push(Line::styled(format!("YOU'RE BETTING {sym} (now ${spot:.0}) goes nowhere — settles between ${sp:.0} and ${sc:.0} by {close_ref}."), thesis));
                if let (Some(lo), Some(hi)) = (be_lo, be_hi) {
                    lines.push(Line::styled(format!("  WIN  → keep up to ${prem:.0}: full credit anywhere ${sp:.0}–${sc:.0}, still green ${lo:.0}–${hi:.0}."), good));
                }
                if let (Some(lp), Some(lc)) = (lp, lc) {
                    lines.push(Line::styled(format!("  LOSE → if it breaks out; loss CAPPED at ${max_loss:.0} once below ${lp:.0} or above ${lc:.0}."), warn));
                }
            }
        }
        StructureKind::IronFly => {
            if let Some(body) = sp.or(sc) {
                lines.push(Line::styled(format!("YOU'RE BETTING {sym} (now ${spot:.0}) PINS near ${body:.0} — a tight range — by {close_ref}."), thesis));
                if let (Some(lo), Some(hi)) = (be_lo, be_hi) {
                    lines.push(Line::styled(format!("  WIN  → profitable ${lo:.0}–${hi:.0}; most of the ${prem:.0} if it lands right at ${body:.0}."), good));
                }
                if let (Some(lp), Some(lc)) = (lp, lc) {
                    lines.push(Line::styled(format!("  LOSE → outside the breakevens; CAPPED at ${max_loss:.0} past ${lp:.0} / ${lc:.0}."), warn));
                }
            }
        }
        StructureKind::ShortStrangle => {
            if let (Some(sp), Some(sc)) = (sp, sc) {
                lines.push(Line::styled(format!("YOU'RE BETTING {sym} (now ${spot:.0}) stays between ${sp:.0} and ${sc:.0} by {close_ref}."), thesis));
                lines.push(Line::styled(format!("  WIN  → keep ${prem:.0} if it settles between the breakevens ${:.0}–${:.0}.", be_lo.unwrap_or(sp), be_hi.unwrap_or(sc)), good));
                lines.push(Line::styled("  LOSE → UNCAPPED past the shorts (no wings) — a big gap can be ruinous.".to_string(), red_bold));
            }
        }
        StructureKind::PutCreditSpread => {
            if let Some(sp) = sp {
                lines.push(Line::styled(format!("YOU'RE BETTING {sym} (now ${spot:.0}) STAYS ABOVE ${sp:.0} (doesn't fall much) by {close_ref}."), thesis));
                if let Some(lo) = be_lo {
                    lines.push(Line::styled(format!("  WIN  → keep the full ${prem:.0} above ${sp:.0}; profitable above ${lo:.0}."), good));
                }
                if let Some(lp) = lp {
                    lines.push(Line::styled(format!("  LOSE → if it falls below ${:.0}; CAPPED at ${max_loss:.0} once below ${lp:.0}.", be_lo.unwrap_or(sp)), warn));
                }
            }
        }
        StructureKind::CallCreditSpread => {
            if let Some(sc) = sc {
                lines.push(Line::styled(format!("YOU'RE BETTING {sym} (now ${spot:.0}) STAYS BELOW ${sc:.0} (doesn't rally much) by {close_ref}."), thesis));
                if let Some(hi) = be_hi {
                    lines.push(Line::styled(format!("  WIN  → keep the full ${prem:.0} below ${sc:.0}; profitable below ${hi:.0}."), good));
                }
                if let Some(lc) = lc {
                    lines.push(Line::styled(format!("  LOSE → if it rises above ${:.0}; CAPPED at ${max_loss:.0} once above ${lc:.0}.", be_hi.unwrap_or(sc)), warn));
                }
            }
        }
        StructureKind::BrokenWingButterfly => {
            lines.push(Line::styled(format!("YOU'RE BETTING {sym} (now ${spot:.0}) DOESN'T CRASH — stays up by {close_ref}. NO risk if it rises."), thesis));
            if let Some(lo) = be_lo {
                lines.push(Line::styled(format!("  WIN  → keep ${prem:.0} as long as it closes above ${lo:.0}; best near ${:.0}.", sp.unwrap_or(lo)), good));
            }
            lines.push(Line::styled(format!("  LOSE → only on a drop below breakeven; CAPPED at ${max_loss:.0}."), warn));
        }
    }
    lines
}

fn render_journal(frame: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["Time", "Symbol", "Action", "Strike", "Qty", "Status"]).style(HEAD);
    let rows = app.journal.iter().enumerate().map(|(i, j)| {
        let cells = vec![
            j.ts.split('T').nth(1).unwrap_or(&j.ts).chars().take(8).collect::<String>(),
            j.symbol.clone(),
            j.action.clone(),
            j.strike.map(|s| format!("{s:.1}")).unwrap_or_default(),
            j.quantity.to_string(),
            j.status.clone(),
        ];
        styled_row(cells, app.tab == Tab::Journal && i == app.selected)
    });
    let widths = [
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Min(8),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(" Journal (most recent first) ")),
        area,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("Navigation", HEAD)),
        Line::from("  Tab / → / ←      switch tabs        1–8   jump to tab"),
        Line::from("  j / k  or ↑ / ↓  move selection"),
        Line::from(""),
        Line::from(Span::styled("Settings tab", HEAD)),
        Line::from("  Enter            edit selected knob; then ↑/↓ change it, Enter/Esc to confirm"),
        Line::from(""),
        Line::from(Span::styled("Watchlist", HEAD)),
        Line::from("  a                add a symbol (type ticker, Enter)"),
        Line::from("  d                delete selected symbol"),
        Line::from(""),
        Line::from(Span::styled("Trading (Suggestions tab)", HEAD)),
        Line::from("  p                preview (what-if): margin / commission, no transmit"),
        Line::from("  A                arm / disarm — while armed, `x` sends a LIVE order"),
        Line::from("  x                execute the selected suggestion (needs arm + not read_only)"),
        Line::from(""),
        Line::from(Span::styled("General", HEAD)),
        Line::from("  r                refresh / recompute suggestions"),
        Line::from("  q  or  Ctrl-C    quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Offline mode shows demo data. Start IB Gateway and set [connection] in config.toml to go live.",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Help ")),
        area,
    );
}

/// The Settings tab: two live-tunable strategy knobs, the selected one marked.
fn render_settings(frame: &mut Frame, app: &App, area: Rect) {
    let s = &app.settings;
    let on_tab = app.tab == Tab::Settings;
    let target_delta = (1.0 - s.target_win_pct / 100.0).clamp(0.05, 0.95);
    let band_lo = (target_delta - 0.10).max(0.05);
    let band_hi = (target_delta + 0.10).min(0.95);

    let knob = |idx: usize, label: &str, value: String, detail: String| -> Line<'static> {
        let active = on_tab && app.selected == idx;
        let editing = active && app.settings_editing;
        let marker = if active { "▶ " } else { "  " };
        let label_style = if active {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        // While editing, reverse-video the value with a ↕ cue so it's obvious
        // ↑/↓ now move it; otherwise it's a plain bold number.
        let value_span = if editing {
            Span::styled(
                format!(" {value} ↕ "),
                SEL.add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("{value:>5}"), label_style.add_modifier(Modifier::BOLD))
        };
        Line::from(vec![
            Span::styled(format!("{marker}{label:<17}"), label_style),
            value_span,
            Span::styled(format!("    {detail}"), Style::new().fg(Color::DarkGray)),
        ])
    };

    let hint = if app.settings_editing {
        "Editing — ↑/↓ (or k/j) change the value · Enter or Esc when done."
    } else {
        "j/k pick a row · Enter to edit · ←/→ or Tab switch tabs · auto-saved."
    };
    let lines = vec![
        Line::styled(hint, Style::new().fg(Color::DarkGray)),
        Line::from(""),
        knob(
            0,
            "Target win rate",
            format!("{:.0}%", s.target_win_pct),
            format!("sell ≈ {target_delta:.2}Δ puts  (band {band_lo:.2}–{band_hi:.2}Δ)"),
        ),
        knob(
            1,
            "Take-profit",
            format!("{:.0}%", s.take_profit_pct),
            "buy back once this much of the credit is captured".to_string(),
        ),
        Line::from(""),
        Line::styled(
            "Higher win rate → further OTM → smaller credit, but wins more often.",
            Style::new().fg(Color::DarkGray),
        ),
        Line::styled(
            "Take-profit closes early to bank the gain and free capital before expiry.",
            Style::new().fg(Color::DarkGray),
        ),
        Line::from(""),
        Line::styled(
            "These override [engine] in config.toml and persist across restarts.",
            Style::new().fg(Color::DarkGray),
        ),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Settings ")),
        area,
    );
}

fn suggestion_cells(s: &Suggestion) -> Vec<String> {
    vec![
        s.symbol.clone(),
        s.kind.display_label().to_string(),
        format!("{:.1}", s.strike),
        s.expiry.format("%Y-%m-%d").to_string(),
        s.dte.to_string(),
        s.delta.map(|d| format!("{d:.2}")).unwrap_or_else(|| "-".into()),
        format!("{:.2}", s.limit_price),
        format!("{:.0}%", s.annualized_yield * 100.0),
        if s.capital_required > 0.0 {
            format!("{:.0}", s.capital_required)
        } else {
            "—".into()
        },
    ]
}

fn styled_row<'a>(cells: Vec<String>, selected: bool) -> Row<'a> {
    let row = Row::new(cells.into_iter().map(Cell::from).collect::<Vec<_>>());
    if selected { row.style(SEL) } else { row }
}

fn money(v: Option<f64>) -> String {
    v.map(|x| format!("${x:.0}")).unwrap_or_else(|| "—".into())
}
