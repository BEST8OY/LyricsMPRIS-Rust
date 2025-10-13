// Event loop module: orchestrates MPRIS event handling and periodic polling
// to maintain synchronized lyrics display with media player state.
//
// Design philosophy:
// - Separation of concerns: events, polling, and state management are distinct
// - Resilience: D-Bus failures don't crash the loop; polling provides fallback
// - Predictable timing: polls run on fixed intervals regardless of event flow

use crate::event::{self, Event, MprisEvent, process_event, send_update};
use crate::mpris::{TrackMetadata, events::MprisEventHandler};
use crate::state::{StateBundle, Update};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Configuration for the event loop, wrapping the main app config.
struct LoopConfig {
    inner: Arc<crate::Config>,
    providers: Vec<String>,
}

impl LoopConfig {
    fn new(mut config: crate::Config) -> Self {
        let providers = if config.providers.is_empty() {
            vec!["lrclib".to_string(), "musixmatch".to_string()]
        } else {
            std::mem::take(&mut config.providers)
        };

        Self {
            inner: Arc::new(config),
            providers,
        }
    }

    fn debug_log(&self) -> bool {
        self.inner.debug_log
    }

    fn block_list(&self) -> &[String] {
        &self.inner.block
    }
}

/// Encapsulates the runtime state needed by the event loop.
struct LoopState {
    state_bundle: StateBundle,
    was_playing: bool,
}

impl LoopState {
    fn new() -> Self {
        Self {
            state_bundle: StateBundle::new(),
            was_playing: false,
        }
    }

    fn update_playing_status(&mut self) {
        self.was_playing = self.state_bundle.player_state.playing;
    }
}

/// Main event loop entry point. Coordinates MPRIS event monitoring and
/// periodic polling to keep lyrics synchronized with playback.
///
/// # Arguments
/// * `update_tx` - Channel for sending state updates to UI/consumers
/// * `poll_interval` - Duration between periodic state checks
/// * `shutdown_rx` - Receives shutdown signal to terminate loop
/// * `config` - Application configuration including provider settings
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    shutdown_rx: mpsc::Receiver<()>,
    config: crate::Config,
) {
    let loop_config = LoopConfig::new(config);
    let mut loop_state = LoopState::new();
    
    let event_rx = initialize_loop(&mut loop_state, &update_tx, &loop_config).await;

    run_event_loop(
        loop_state,
        event_rx,
        update_tx,
        shutdown_rx,
        loop_config,
    ).await;
}

/// Initializes the event loop: discovers active player, fetches initial state,
/// spawns MPRIS watcher, and returns the event receiver.
async fn initialize_loop(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) -> mpsc::Receiver<Event> {
    let (event_tx, event_rx) = mpsc::channel::<Event>(16);
    
    let active_service = discover_active_player(config).await;
    
    if active_service.is_none() {
        handle_no_player(loop_state, update_tx).await;
        spawn_mpris_watcher(event_tx, config);
        return event_rx;
    }

    let service = active_service.as_ref().unwrap();
    let initial_metadata = fetch_initial_metadata(service, config).await;
    
    initialize_lyrics_state(
        loop_state,
        &initial_metadata,
        service,
        config,
    ).await;
    
    spawn_mpris_watcher(event_tx, config);
    
    event_rx
}

/// Discovers the first active, non-blocked media player service.
async fn discover_active_player(config: &LoopConfig) -> Option<String> {
    match crate::mpris::get_active_player_names().await {
        Ok(names) => {
            names.into_iter()
                .find(|service| !crate::mpris::is_blocked(service, config.block_list()))
        }
        Err(e) => {
            if config.debug_log() {
                eprintln!("[EventLoop] Failed to enumerate players: {}", e);
            }
            None
        }
    }
}

/// Handles the case where no active player is found: clears state and notifies UI.
async fn handle_no_player(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
) {
    loop_state.state_bundle.clear_lyrics();
    loop_state.state_bundle.player_state = Default::default();
    let _ = send_update(&loop_state.state_bundle, update_tx, true).await;
}

/// Fetches initial metadata for the discovered player service.
async fn fetch_initial_metadata(
    service: &str,
    config: &LoopConfig,
) -> TrackMetadata {
    match crate::mpris::metadata::get_metadata(service).await {
        Ok(metadata) => metadata,
        Err(e) => {
            if config.debug_log() {
                eprintln!("[EventLoop] Failed to fetch metadata for {}: {}", service, e);
            }
            TrackMetadata::default()
        }
    }
}

/// Initializes lyrics state based on initial metadata and sets player position.
async fn initialize_lyrics_state(
    loop_state: &mut LoopState,
    metadata: &TrackMetadata,
    service: &str,
    config: &LoopConfig,
) {
    let position = event::fetch_and_update_lyrics(
        metadata,
        &mut loop_state.state_bundle,
        config.debug_log(),
        &config.providers,
        Some(service),
    ).await;
    
    loop_state.state_bundle.player_state.set_position(position);
    loop_state.update_playing_status();
}

/// Spawns a background task to watch for MPRIS events and forward them
/// to the event channel.
fn spawn_mpris_watcher(
    event_tx: mpsc::Sender<Event>,
    config: &LoopConfig,
) {
    let update_tx = event_tx.clone();
    let seek_tx = event_tx;
    let block_list = config.block_list().to_vec();
    let debug = config.debug_log();

    tokio::spawn(async move {
        let handler_result = MprisEventHandler::with_closures(
            move |meta, pos, service| {
                let _ = update_tx.try_send(Event::Mpris(
                    MprisEvent::PlayerUpdate(meta, pos, service)
                ));
            },
            move |meta, pos, service| {
                let _ = seek_tx.try_send(Event::Mpris(
                    MprisEvent::Seeked(meta, pos, service)
                ));
            },
            block_list,
        ).await;

        match handler_result {
            Ok(mut handler) => {
                if let Err(e) = handler.handle_events().await && debug {
                    eprintln!("[EventLoop] MPRIS handler terminated: {}", e);
                }
            }
            Err(e) => {
                if debug {
                    eprintln!("[EventLoop] Failed to initialize MPRIS handler: {}", e);
                }
            }
        }
    });
}

/// Main event processing loop: handles shutdown signals, MPRIS events,
/// and periodic polling.
async fn run_event_loop(
    mut loop_state: LoopState,
    mut event_rx: mpsc::Receiver<Event>,
    update_tx: mpsc::Sender<Update>,
    mut shutdown_rx: mpsc::Receiver<()>,
    config: LoopConfig,
) {
    loop {
        tokio::select! {
            // Shutdown signal received
            _ = shutdown_rx.recv() => {
                handle_shutdown(&mut loop_state, &update_tx, &config).await;
                break;
            }

            // MPRIS event received from watcher
            maybe_event = event_rx.recv() => {
                handle_mpris_event(maybe_event, &mut loop_state, &update_tx, &config).await;
            }

            // No periodic polling: rely solely on MPRIS events
        }
    }
}

/// Processes a shutdown event and cleans up state.
async fn handle_shutdown(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    let _ = process_event(
        Event::Shutdown,
        &mut loop_state.state_bundle,
        update_tx,
        config.debug_log(),
        &config.providers,
    ).await;
}

/// Handles an incoming MPRIS event, if present.
async fn handle_mpris_event(
    maybe_event: Option<Event>,
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    match maybe_event {
        Some(event) => {
            let _ = process_event(
                event,
                &mut loop_state.state_bundle,
                update_tx,
                config.debug_log(),
                &config.providers,
            ).await;
            
            loop_state.update_playing_status();
        }
        None => {
            // Event channel closed; nothing else to do
            if config.debug_log() {
                eprintln!("[EventLoop] MPRIS channel closed");
            }
        }
    }
}

// periodic polling removed — rely on MPRIS events only