// Minimal central event loop for polling and event-based updates

use crate::event::{Event, MprisEvent, handle_poll, process_event, send_update};
use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::mpris::events::MprisEventHandler;
use crate::state::{StateBundle, Update};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, Instant};

pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    mut db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mut shutdown_rx: mpsc::Receiver<()>,
    mpris_config: crate::Config,
) {
    // Determine provider order from config (default is lrclib then musixmatch)
    let providers: Vec<String> = if mpris_config.providers.is_empty() {
        vec!["lrclib".to_string(), "musixmatch".to_string()]
    } else {
        mpris_config.providers.clone()
    };
    let mut state = StateBundle::new();
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut latest_meta: Option<(TrackMetadata, f64, String)> = None;
    let mut mpris_config = mpris_config;

    // Find first unblocked player at startup
    let service = match crate::mpris::get_active_player_names().await {
        Ok(names) => names
            .into_iter()
            .find(|s| !crate::mpris::is_blocked(s, &mpris_config.block)),
        Err(e) => {
            if mpris_config.debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting active players: {}", e);
            }
            None
        }
    };
    mpris_config.player_service = service.clone();
    let mpris_config_arc = Arc::new(mpris_config);

    if service.is_none() {
        state.clear_lyrics();
        state.player_state = Default::default();
        send_update(&state, &update_tx, true).await;
    }

    // Initial fetch (refactored for efficiency)
    let meta = match crate::mpris::metadata::get_metadata(service.as_deref().unwrap_or("")).await {
        Ok(meta) => meta,
        Err(e) => {
            if mpris_config_arc.debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting metadata: {}", e);
            }
            Default::default()
        }
    };
    let position = crate::event::fetch_and_update_lyrics(
        &meta,
        &mut state,
        db.as_ref(),
        db_path.as_deref(),
        mpris_config_arc.debug_log,
        &providers,
        service.as_deref().unwrap_or(""),
    )
    .await;
    state.player_state.position = position;

    let mut paused_since: Option<Instant> = None;
    let pause_release_threshold = Duration::from_secs(60);
    let mut was_playing = true;

    // Spawn MPRIS watcher
    let event_tx_update = event_tx.clone();
    let event_tx_seek = event_tx.clone();
    let block_list = mpris_config_arc.block.clone();
    tokio::spawn(async move {
        let mut event_handler = MprisEventHandler::new(
            move |meta, pos, service| {
                let _ = event_tx_update
                    .try_send(Event::Mpris(MprisEvent::PlayerUpdate(meta, pos, service)));
            },
            move |meta, pos, service| {
                let _ =
                    event_tx_seek.try_send(Event::Mpris(MprisEvent::Seeked(meta, pos, service)));
            },
            block_list,
        )
        .await
        .expect("Failed to create MPRIS event handler");
        let _ = event_handler.handle_events().await;
    });

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                process_event(Event::Shutdown, &mut state, &update_tx, &mut latest_meta).await;
                break;
            },
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    let prev_playing = was_playing;
                    process_event(event, &mut state, &update_tx, &mut latest_meta).await;
                    was_playing = state.player_state.playing;
                    if prev_playing != was_playing {
                        match was_playing {
                            false => paused_since = Some(Instant::now()),
                            true => {
                                paused_since = None;
                                if db.is_none() && let Some(ref path) = db_path && let Ok(new_db) = LyricsDB::load(path) {
                                    db = Some(Arc::new(Mutex::new(new_db)));
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                if state.player_state.playing {
                    handle_poll(
                        &mut state,
                        db.as_ref(),
                        db_path.as_deref(),
                        &update_tx,
                        mpris_config_arc.debug_log,
                        &mut latest_meta,
                        &providers,
                    ).await;
                } else if let Some(paused_at) = paused_since && paused_at.elapsed() > pause_release_threshold {
                    db = None;
                }
            }
        }
    }
}
