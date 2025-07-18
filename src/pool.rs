// pool.rs: Central event loop for polling and event-based updates
// Orchestrates state, event, and player updates for the application.

use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};
use crate::event::{Event, process_event, handle_poll};
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use tokio::time::{Duration, Instant};

// --- Main Listener ---
/// Listens for player and lyric updates, sending them to the update channel.
///
/// - Spawns a watcher for MPRIS events, sending them to the event channel.
/// - Handles shutdown, player events, and periodic polling.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    mut db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mut shutdown_rx: mpsc::Receiver<()>,
    mpris_config: crate::Config,
) {
    let mut state = StateBundle::new();
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut latest_meta: Option<(TrackMetadata, f64)> = None;
    let event_tx_track = event_tx.clone();
    let event_tx_seek = event_tx.clone();
    let mpris_config_arc = Arc::new(mpris_config);

    // --- Pause/Resume Resource Management ---
    let mut paused_since: Option<Instant> = None;
    let pause_release_threshold = Duration::from_secs(60); // 1 minute
    let mut was_playing = true;

    // --- Spawn MPRIS watcher task ---
    let mpris_config_track = mpris_config_arc.clone();
    tokio::spawn(async move {
        let _ = crate::mpris::watch_and_handle_events(
            move |meta, pos| {
                let _ = event_tx_track.try_send(Event::PlayerUpdate(meta, pos, true));
            },
            move |meta, pos| {
                let _ = event_tx_seek.try_send(Event::PlayerUpdate(meta, pos, false));
            },
            Some(&mpris_config_track),
        ).await;
    });

    // --- Main event loop: handle shutdown, player events, and polling ---
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                process_event(Event::Shutdown, &mut state, &update_tx, &mut latest_meta, &mpris_config_arc).await;
                break;
            },
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    // Check for playback status change
                    let prev_playing = was_playing;
                    process_event(event, &mut state, &update_tx, &mut latest_meta, &mpris_config_arc).await;
                    was_playing = state.player_state.playing;
                    if prev_playing != was_playing {
                        if !was_playing {
                            // Just paused
                            paused_since = Some(Instant::now());
                        } else {
                            // Just resumed
                            paused_since = None;
                            // Reacquire DB if needed
                            if db.is_none() {
                                if let Some(ref path) = db_path {
                                    if let Ok(new_db) = LyricsDB::load(path) {
                                        db = Some(Arc::new(Mutex::new(new_db)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                // Only poll if playing
                if state.player_state.playing {
                    handle_poll(
                        &mut state,
                        db.as_ref(),
                        db_path.as_deref(),
                        &mpris_config_arc,
                        &update_tx,
                        &mut latest_meta,
                    ).await;
                } else {
                    // If paused for a long time, release DB
                    if let Some(paused_at) = paused_since {
                        if paused_at.elapsed() > pause_release_threshold {
                            db = None;
                        }
                    }
                }
            }
        }
    }
}