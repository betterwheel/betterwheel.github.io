//! Terminal UI: an async event loop over a [`ratatui`] terminal.
//!
//! The loop merges keyboard input (crossterm `EventStream`) and a render tick
//! with [`tokio::select!`]. Key handling is synchronous ([`app::App::handle_key`]);
//! async work runs via [`app::App::dispatch`]. Runs offline with demo data until
//! a live IBKR connection is wired in.

pub mod app;
pub mod demo;
mod live;
pub mod ui;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::{Config, ConnectionConfig};
use crate::ibkr::{Ibkr, OrderEvent};
use crate::store::Store;
use app::{App, BrokerUpdate};

/// Run the TUI until the user quits.
pub async fn run(
    mut terminal: DefaultTerminal,
    cfg: Config,
    store: Store,
    ibkr: Option<Arc<Ibkr>>,
    offline_reason: Option<String>,
) -> Result<()> {
    let mut app = App::new(cfg, ibkr, &store).await?;
    app.offline_reason = offline_reason;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    // Off-loop broker results — auto-reconnects and background reloads report
    // here so the select! loop never blocks on broker I/O. Keeping `upd_tx` alive
    // in this scope means `upd_rx.recv()` parks rather than returning `None`.
    let (upd_tx, mut upd_rx) = mpsc::unbounded_channel();
    app.set_update_sender(upd_tx.clone());

    // One background consumer of the broker's order-activity stream, forwarding
    // fills/cancellations as `OrderEvent`s. Re-spawned whenever auto-reconnect
    // establishes a new connection. `order_tx` stays alive here so `recv()` parks.
    let (order_tx, mut order_rx) = mpsc::unbounded_channel();
    let mut order_consumer = spawn_order_consumer(app.ibkr.clone(), &order_tx);

    // While offline, retry connecting on an interval — off the UI thread.
    let mut reconnect = if app.ibkr.is_none() {
        spawn_reconnect(app.cfg.connection.clone(), &upd_tx)
    } else {
        None
    };

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
            maybe_ev = order_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    app.apply_order_event(ev, &store).await?;
                }
            }
            maybe_upd = upd_rx.recv() => {
                if let Some(upd) = maybe_upd {
                    match upd {
                        BrokerUpdate::Connected(ib) => {
                            // Restart the order-activity consumer on the new client,
                            // adopt the connection, then refresh data off-loop.
                            if let Some(h) = order_consumer.take() {
                                h.abort();
                            }
                            app.set_connected(ib);
                            order_consumer = spawn_order_consumer(app.ibkr.clone(), &order_tx);
                            reconnect = None; // the reconnect task returns on success
                            app.request_reload(&store).await;
                        }
                        BrokerUpdate::ConnectFailed(reason) => app.set_offline_reason(reason),
                        BrokerUpdate::Reloaded(data) => app.apply_live_data(*data, &store).await,
                    }
                }
            }
            _ = tick.tick() => {}              // periodic redraw
        }
    }

    if let Some(handle) = order_consumer {
        handle.abort();
    }
    if let Some(handle) = reconnect {
        handle.abort();
    }
    Ok(())
}

/// Spawn the broker's order-activity stream consumer, forwarding events to the
/// run loop. `None` when offline (no client to stream from).
fn spawn_order_consumer(
    ibkr: Option<Arc<Ibkr>>,
    tx: &mpsc::UnboundedSender<OrderEvent>,
) -> Option<JoinHandle<()>> {
    ibkr.map(|ib| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = ib.stream_order_events(tx).await;
        })
    })
}

/// Spawn an off-loop task that retries connecting every `reconnect_secs` while
/// offline, reporting each outcome over `tx`. `None` if reconnect is disabled
/// (`reconnect_secs == 0`); the task exits on the first success.
fn spawn_reconnect(
    conn: ConnectionConfig,
    tx: &mpsc::UnboundedSender<BrokerUpdate>,
) -> Option<JoinHandle<()>> {
    if conn.reconnect_secs == 0 {
        return None;
    }
    let tx = tx.clone();
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(conn.reconnect_secs)).await;
            if tx.is_closed() {
                return; // run loop gone
            }
            match tokio::time::timeout(Duration::from_secs(5), Ibkr::connect(&conn)).await {
                Ok(Ok(ib)) => {
                    let _ = tx.send(BrokerUpdate::Connected(Arc::new(ib)));
                    return;
                }
                Ok(Err(e)) => {
                    let _ = tx.send(BrokerUpdate::ConnectFailed(crate::ibkr::connect_failure_hint(&e)));
                }
                Err(_) => {
                    let _ = tx.send(BrokerUpdate::ConnectFailed(
                        "IB Gateway connection timed out — is it running?".into(),
                    ));
                }
            }
        }
    }))
}
