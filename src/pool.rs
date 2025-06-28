// pool.rs: Central event loop for polling and event-based updates
// Orchestrates state, event, and player updates for the application.

use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};
use crate::event::{Event, process_event, handle_poll};
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use tokio::time::Duration;

/// Listens for player and lyric updates, sending them to the update channel.
///
/// - Spawns a watcher for MPRIS events, sending them to the event channel.
/// - Handles shutdown, player events, and periodic polling.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mut shutdown_rx: mpsc::Receiver<()>,
    mpris_config: crate::Config,
) {
    let mut state = StateBundle::new();
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut latest_meta: Option<(TrackMetadata, f64)> = None;
    let event_tx_clone = event_tx.clone();
    let mpris_config_clone = mpris_config.clone();
    // Spawn a task to watch for player events and send to event channel
    tokio::spawn(async move {
        let _ = crate::mpris::watch_and_handle_events(
            move |meta, pos| {
                let _ = event_tx_clone.try_send(Event::PlayerUpdate(meta, pos, true));
            },
            move |meta, pos| {
                let _ = event_tx.try_send(Event::PlayerUpdate(meta, pos, false));
            },
            Some(&mpris_config_clone),
        ).await;
    });
    // Main event loop: handle shutdown, player events, and polling
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                process_event(Event::Shutdown, &mut state, &update_tx, &mut latest_meta, &mpris_config).await;
                break;
            },
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    process_event(event, &mut state, &update_tx, &mut latest_meta, &mpris_config).await;
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                handle_poll(
                    &mut state,
                    db.as_ref(),
                    db_path.as_deref(),
                    &mpris_config,
                    &update_tx,
                    &mut latest_meta,
                ).await;
            }
        }
    }
}