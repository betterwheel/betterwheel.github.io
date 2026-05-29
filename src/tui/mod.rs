//! Terminal UI: an async event loop over a [`ratatui`] terminal.
//!
//! The loop merges keyboard input (crossterm `EventStream`) and a render tick
//! with [`tokio::select!`]. Key handling is synchronous ([`app::App::handle_key`]);
//! async work runs via [`app::App::dispatch`]. Runs offline with demo data until
//! a live IBKR connection is wired in.

pub mod app;
pub mod demo;
pub mod ui;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::ibkr::Ibkr;
use crate::store::Store;
use app::App;

/// Run the TUI until the user quits.
pub async fn run(
    mut terminal: DefaultTerminal,
    cfg: Config,
    store: Store,
    ibkr: Option<Arc<Ibkr>>,
) -> Result<()> {
    let mut app = App::new(cfg, ibkr, &store).await?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    // When connected, run one background consumer of the broker's order-activity
    // stream. It forwards fills/cancellations as `OrderEvent`s; the loop below
    // applies them to the journal live. Keeping the original `tx` alive in this
    // scope means `rx.recv()` parks (never returns `None`) when offline.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let consumer = app.ibkr.clone().map(|ib| {
        let tx = tx.clone();
        tokio::spawn(async move { ib.stream_order_events(tx).await })
    });

    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, &app))?;

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = app.handle_key(key) {
                        app.dispatch(action, &store).await?;
                    }
                }
                Some(Ok(_)) => {}              // resize, mouse, focus, etc.
                Some(Err(_)) | None => break,  // input stream closed
            },
            maybe_ev = rx.recv() => {
                if let Some(ev) = maybe_ev {
                    app.apply_order_event(ev, &store).await?;
                }
            }
            _ = tick.tick() => {}              // periodic redraw
        }
    }

    if let Some(handle) = consumer {
        handle.abort();
    }
    Ok(())
}
