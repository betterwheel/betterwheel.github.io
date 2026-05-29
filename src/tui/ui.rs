//! Rendering — a pure function of [`App`] state.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Tabs, Wrap};

use super::app::{App, InputMode, Tab};
use crate::engine::types::{ActionKind, Suggestion};

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
        Tab::Journal => render_journal(frame, app, mid),
        Tab::Help => render_help(frame, mid),
    }
    render_status(frame, app, bottom);
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = Tab::ALL.iter().map(|t| Line::from(format!(" {} ", t.title())));
    let conn = if app.connected { "● live" } else { "○ offline" };
    let sync = if app.is_loading() { "  ⟳ syncing" } else { "" };
    let armed = if app.armed { "  ⚡ ARMED" } else { "" };
    let title = format!(
        "TheWheel  [{}]  {conn}{sync}  ·  {} open{armed}",
        app.mode_label(),
        app.open_position_count()
    );
    let mut block = Block::bordered().title(title);
    if app.armed {
        // A loud, hard-to-miss cue that `x` will transmit a live order.
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
        InputMode::Normal => format!(
            " {}   ·   q quit · tab switch · j/k move · a add · d delete · r refresh · ? help",
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
        Line::from(Span::styled(
            "Add symbols on the Watchlist tab; the Suggestions tab ranks cash-secured puts.",
            Style::new().fg(Color::DarkGray),
        )),
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
    let header =
        Row::new(["Symbol", "Action", "Strike", "Expiry", "DTE", "Δ", "Premium", "Ann%", "Capital"])
            .style(HEAD);
    let rows = app.suggestions.iter().enumerate().map(|(i, s)| {
        styled_row(suggestion_cells(s), app.tab == Tab::Suggestions && i == app.selected)
    });
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
    let title = if app.suggestions.is_empty() {
        " Suggestions — add watchlist symbols, then press 'r' ".to_string()
    } else {
        format!(" Suggestions ({}) — cash-secured puts, best yield first ", app.suggestions.len())
    };
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
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
        Line::from("  Tab / → / ←      switch tabs        1–5   jump to tab"),
        Line::from("  j / k  or ↑ / ↓  move selection"),
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

fn suggestion_cells(s: &Suggestion) -> Vec<String> {
    vec![
        s.symbol.clone(),
        action_label(&s.kind).to_string(),
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

fn action_label(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::SellPut => "Sell Put",
        ActionKind::SellCall => "Sell Call",
        ActionKind::CloseForProfit => "Close",
        ActionKind::Roll { .. } => "Roll",
    }
}

fn styled_row<'a>(cells: Vec<String>, selected: bool) -> Row<'a> {
    let row = Row::new(cells.into_iter().map(Cell::from).collect::<Vec<_>>());
    if selected { row.style(SEL) } else { row }
}

fn money(v: Option<f64>) -> String {
    v.map(|x| format!("${x:.0}")).unwrap_or_else(|| "—".into())
}
