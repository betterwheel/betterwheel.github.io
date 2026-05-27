//! Terminal UI: an async event loop over a [`ratatui`] terminal.
//!
//! The loop merges keyboard input (crossterm `EventStream`) and a render tick
//! with [`tokio::select!`]. Key handling is synchronous ([`app::App::handle_key`]);
//! async work runs via [`app::App::dispatch`]. Runs offline with demo data until
//! a live IBKR connection is wired in.

pub mod app;
pub mod demo;
pub mod ui;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;

use crate::config::Config;
use crate::store::Store;
use app::App;

/// Run the TUI until the user quits.
pub async fn run(mut terminal: DefaultTerminal, cfg: Config, store: Store) -> Result<()> {
    let mut app = App::new(cfg, &store).await?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

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
            _ = tick.tick() => {}              // periodic redraw
        }
    }
    Ok(())
}
