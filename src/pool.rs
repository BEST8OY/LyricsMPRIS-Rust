// Minimal central event loop for polling and event-based updates.
//
// This module listens for MPRIS events and drives periodic polling. The
// poll handler is intentionally lightweight and always run on the interval so
// pending metadata/lyrics fetches aren't missed simply because playback is
// paused.

use crate::event::{self, Event, MprisEvent, process_event, send_update};
use std::time::Duration;
use crate::mpris::TrackMetadata;
use crate::mpris::events::MprisEventHandler;
use crate::state::{StateBundle, Update};
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    mut shutdown_rx: mpsc::Receiver<()>,
    mut mpris_config: crate::Config,
) {
    // Provider order: either from config or sensible default.
    let providers: Vec<String> = if mpris_config.providers.is_empty() {
        vec!["lrclib".to_string(), "musixmatch".to_string()]
    } else {
        mpris_config.providers.clone()
    };

    let mut state = StateBundle::new();
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut latest_meta: Option<(TrackMetadata, f64, String)> = None;

    // Find first unblocked player at startup
    let service = match crate::mpris::get_active_player_names().await {
        Ok(names) => names.into_iter().find(|s| !crate::mpris::is_blocked(s, &mpris_config.block)),
        Err(e) => {
            if mpris_config.debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting active players: {}", e);
            }
            None
        }
    };

    mpris_config.player_service = service.clone();
    let mpris_config_arc = Arc::new(mpris_config);

    // If no service present, clear state and notify UI.
    if service.is_none() {
        state.clear_lyrics();
        state.player_state = Default::default();
        send_update(&state, &update_tx, true).await;
    }

    // Initial metadata fetch only when a service is present. This avoids calling
    // metadata APIs with empty service strings.
    let meta = if let Some(ref svc) = service {
        match crate::mpris::metadata::get_metadata(svc).await {
            Ok(m) => m,
            Err(e) => {
                if mpris_config_arc.debug_log {
                    eprintln!("[LyricsMPRIS] D-Bus error getting metadata: {}", e);
                }
                Default::default()
            }
        }
    } else {
        Default::default()
    };

    let position = event::fetch_and_update_lyrics(
        &meta,
        &mut state,
        mpris_config_arc.debug_log,
        &providers,
    )
    .await;
    state.player_state.position = position;

    // Track previous playing state for bookkeeping; initialize from state.
    let mut was_playing = state.player_state.playing;

    // Spawn MPRIS watcher. If creation fails, log and continue (we'll still poll).
    {
        let event_tx_update = event_tx.clone();
        let event_tx_seek = event_tx.clone();
        let block_list = mpris_config_arc.block.clone();
        let debug = mpris_config_arc.debug_log;
        tokio::spawn(async move {
            match MprisEventHandler::new(
                move |meta, pos, service| {
                    let _ = event_tx_update.try_send(Event::Mpris(MprisEvent::PlayerUpdate(meta, pos, service)));
                },
                move |meta, pos, service| {
                    let _ = event_tx_seek.try_send(Event::Mpris(MprisEvent::Seeked(meta, pos, service)));
                },
                block_list,
            ).await {
                Ok(mut handler) => {
                    let _ = handler.handle_events().await;
                }
                Err(e) => {
                    if debug {
                        eprintln!("[LyricsMPRIS] Failed to create MPRIS event handler: {}", e);
                    }
                }
            }
        });
    }

    // Main loop: handle shutdown, incoming events, and timed polls.
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                process_event(Event::Shutdown, &mut state, &update_tx, &mut latest_meta).await;
                break;
            }
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    let prev_playing = was_playing;
                    process_event(event, &mut state, &update_tx, &mut latest_meta).await;
                    was_playing = state.player_state.playing;
                    if prev_playing != was_playing {
                        // could add metrics/logging here if desired
                    }
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                // Always run poll handler; it internally decides if anything
                // needs to be done (fetch lyrics, sync position, send updates).
                event::handle_poll(
                    &mut state,
                    &update_tx,
                    mpris_config_arc.debug_log,
                    &mut latest_meta,
                    &providers,
                ).await;
            }
        }
    }
}
