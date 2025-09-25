// Central event loop: listens for MPRIS events and drives periodic polling.
//
// The loop intentionally keeps the poll handler lightweight and always runs
// on the configured interval so that pending metadata/lyrics fetches aren't
// missed while playback is paused or events aren't emitted.

use crate::event::{self, Event, MprisEvent, process_event, send_update};
use std::time::Duration;
use crate::mpris::TrackMetadata;
use crate::mpris::events::MprisEventHandler;
use crate::state::Update;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Listen for MPRIS events and run periodic polling.
///
/// Arguments mirror the previous implementation and are kept for compatibility
/// with the rest of the codebase:
/// - `update_tx`: channel sender to push `Update` values to the UI/state
/// - `poll_interval`: how often to run passive polls
/// - `shutdown_rx`: receiver that, when closed or receives a unit, triggers shutdown
/// - `mpris_config`: configuration (consumed and wrapped in Arc inside)
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    mut shutdown_rx: mpsc::Receiver<()>,
    mut mpris_config: crate::Config,
) {
    // Determine provider order: use configured list if present, otherwise a sane default.
    let providers: Vec<String> = if mpris_config.providers.is_empty() {
        vec!["lrclib".into(), "musixmatch".into()]
    } else {
        // Take ownership of the configured vector to avoid extra clones later.
        std::mem::take(&mut mpris_config.providers)
    };

    // Prepare runtime state and event channel used by the MPRIS watcher.
    let mut state = crate::state::StateBundle::new();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
    let mut latest_meta: Option<(TrackMetadata, f64, String)> = None;

    // Discover first unblocked player service, logging any D-Bus errors if configured.
    let initial_service = match crate::mpris::get_active_player_names().await {
        Ok(names) => names.into_iter().find(|s| !crate::mpris::is_blocked(s, &mpris_config.block)),
        Err(e) => {
            if mpris_config.debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting active players: {}", e);
            }
            None
        }
    };

    // Record the discovered player service in the config and wrap config in an Arc.
    mpris_config.player_service = initial_service.clone();
    let mpris_config = Arc::new(mpris_config);

    // If no player was found, clear state and notify listeners immediately.
    if initial_service.is_none() {
        state.clear_lyrics();
        state.player_state = Default::default();
        let _ = send_update(&state, &update_tx, true).await;
    }

    // Optionally fetch initial metadata for the selected service. Keep errors
    // non-fatal and fall back to default metadata.
    let initial_meta = if let Some(ref svc) = initial_service {
        match crate::mpris::metadata::get_metadata(svc).await {
            Ok(m) => m,
            Err(e) => {
                if mpris_config.debug_log {
                    eprintln!("[LyricsMPRIS] D-Bus error getting metadata: {}", e);
                }
                Default::default()
            }
        }
    } else {
        Default::default()
    };

    // Fetch/update lyrics based on initial metadata and set initial position.
    let pos = event::fetch_and_update_lyrics(
        &initial_meta,
        &mut state,
        mpris_config.debug_log,
        &providers,
        initial_service.as_deref(),
    )
    .await;
    state.player_state.set_position(pos);

    // Keep track of whether playback was previously playing for bookkeeping.
    let mut was_playing = state.player_state.playing;

    // Spawn the MPRIS event watcher. Failure to create the watcher is logged
    // but not fatal; polling will keep the app functional.
    {
        let tx_for_update = event_tx.clone();
        let tx_for_seek = event_tx.clone();
        let block_list = mpris_config.block.clone();
        let debug = mpris_config.debug_log;

        tokio::spawn(async move {
            match MprisEventHandler::new(
                move |meta, pos, service| {
                    // Try to send an update event. If the channel is full or closed,
                    // ignore the error — the poller will pick up missed state soon.
                    let _ = tx_for_update.try_send(Event::Mpris(MprisEvent::PlayerUpdate(
                        meta, pos, service,
                    )));
                },
                move |meta, pos, service| {
                    let _ = tx_for_seek.try_send(Event::Mpris(MprisEvent::Seeked(meta, pos, service)));
                },
                block_list,
            )
            .await
            {
                Ok(mut handler) => {
                    // Run until the handler completes or errors.
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

    // Main loop: respond to shutdown, incoming MPRIS events, and timed polls.
    loop {
        tokio::select! {
            // Shutdown requested: process a final shutdown event and exit.
            _ = shutdown_rx.recv() => {
                let _ = process_event(Event::Shutdown, &mut state, &update_tx, &mut latest_meta).await;
                break;
            }

            // Incoming MPRIS event from the watcher. If the sender side closed
            // and the channel returns None, treat it as no-op and continue.
            maybe_event = event_rx.recv() => {
                if let Some(ev) = maybe_event {
                    let prev_playing = was_playing;
                    let _ = process_event(ev, &mut state, &update_tx, &mut latest_meta).await;
                    was_playing = state.player_state.playing;

                    if prev_playing != was_playing {
                        // Potential place for metrics or debug logging.
                    }
                } else {
                    // Event channel closed; continue relying on polls.
                    if mpris_config.debug_log {
                        eprintln!("[LyricsMPRIS] MPRIS event channel closed, falling back to polling");
                    }
                }
            }

            // Periodic poll: ensure position/lyrics stay in sync even without events.
            _ = tokio::time::sleep(poll_interval) => {
                let _ = event::handle_poll(
                    &mut state,
                    &update_tx,
                    mpris_config.debug_log,
                    &mut latest_meta,
                    &providers,
                ).await;
            }
        }
    }
}
