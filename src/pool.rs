//! Event loop module for MPRIS event orchestration.
//!
//! This module coordinates MPRIS event handling to maintain synchronized
//! lyrics display with media player state.
//!
//! # Design Philosophy
//!
//! - **Separation of concerns**: Events, state management, and lyrics fetching are distinct
//! - **Resilience**: D-Bus failures don't crash the loop; state is maintained
//! - **Efficiency**: Event-driven architecture eliminates unnecessary polling
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐
//! │ MPRIS D-Bus     │
//! │ Event Watcher   │
//! └────────┬────────┘
//!          │ Events
//!          ▼
//! ┌─────────────────┐
//! │ Event Channel   │
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐      ┌─────────────────┐
//! │ Event Loop      │─────▶│ State Bundle    │
//! │ (this module)   │      │ (state.rs)      │
//! └────────┬────────┘      └─────────────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │ UI Update       │
//! │ Channel         │
//! └─────────────────┘
//! ```

use crate::event::{self, Event, MprisEvent, process_event, send_update};
use crate::mpris::{TrackMetadata, events::MprisEventHandler};
use crate::state::{StateBundle, Update};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Configuration for the event loop.
///
/// Wraps the main application config and provides convenient accessors
/// for event loop operations.
struct LoopConfig {
    /// Shared reference to main app config
    inner: Arc<crate::Config>,
    /// Ordered list of lyrics providers
    providers: Vec<String>,
}

impl LoopConfig {
    /// Creates a new loop configuration from the main app config.
    ///
    /// If no providers are specified, defaults to ["lrclib", "musixmatch"].
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

    /// Returns the list of blocked player services.
    fn block_list(&self) -> &[String] {
        &self.inner.block
    }

    /// Returns the ordered list of lyrics providers.
    fn providers(&self) -> &[String] {
        &self.providers
    }
}

/// Encapsulates the runtime state needed by the event loop.
///
/// This struct maintains both the shared state bundle and loop-specific
/// tracking variables.
struct LoopState {
    /// Shared state bundle with lyrics and player state
    state_bundle: StateBundle,
    /// Previous playing status for change detection
    was_playing: bool,
}

impl LoopState {
    /// Creates a new loop state with default values.
    fn new() -> Self {
        Self {
            state_bundle: StateBundle::new(),
            was_playing: false,
        }
    }

    /// Updates the cached playing status from the current state.
    ///
    /// Call this after processing events to track playback state changes.
    fn update_playing_status(&mut self) {
        self.was_playing = self.state_bundle.player_state.playing;
    }
}

/// Main event loop entry point.
///
/// Coordinates MPRIS event monitoring to keep lyrics synchronized with playback.
/// This function sets up the event infrastructure and runs the main event loop.
///
/// # Arguments
///
/// * `update_tx` - Channel for sending state updates to UI/consumers
/// * `shutdown_rx` - Receives shutdown signal to terminate loop
/// * `config` - Application configuration including provider settings
///
/// # Architecture
///
/// 1. Initialize loop configuration and state
/// 2. Discover active player and fetch initial state
/// 3. Spawn MPRIS event watcher
/// 4. Run event loop until shutdown
///
/// # Error Handling
///
/// All errors are handled gracefully - D-Bus failures don't crash the loop.
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
    )
    .await;
}

/// Initializes the event loop infrastructure.
///
/// This function:
/// 1. Creates the event channel
/// 2. Discovers active player
/// 3. Fetches initial metadata and lyrics (if player found)
/// 4. Spawns MPRIS event watcher
///
/// # Returns
///
/// The receiver end of the event channel for the main loop to consume.
async fn initialize_loop(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) -> mpsc::Receiver<Event> {
    let (event_tx, event_rx) = mpsc::channel::<Event>(16);
    
    let active_service = discover_active_player(config).await;
    
    if let Some(service) = active_service {
        initialize_with_player(loop_state, &service, config).await;
    } else {
        handle_no_player(loop_state, update_tx).await;
    }
    
    spawn_mpris_watcher(event_tx, config);
    
    event_rx
}

/// Initializes state with an active player.
///
/// Fetches initial metadata and lyrics for the current track.
async fn initialize_with_player(
    loop_state: &mut LoopState,
    service: &str,
    config: &LoopConfig,
) {
    let initial_metadata = fetch_initial_metadata(service, config).await;
    initialize_lyrics_state(loop_state, &initial_metadata, service, config).await;
}

/// Discovers the first active, non-blocked media player service.
///
/// # Returns
///
/// - `Some(service)` if an active, non-blocked player is found
/// - `None` if no players are available or all are blocked
///
/// # Error Handling
///
/// D-Bus enumeration errors are logged and treated as no player.
async fn discover_active_player(config: &LoopConfig) -> Option<String> {
    match crate::mpris::get_active_player_names().await {
        Ok(names) => names
            .into_iter()
            .find(|service| !crate::mpris::is_blocked(service, config.block_list())),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to enumerate MPRIS players"
            );
            None
        }
    }
}

/// Handles the case where no active player is found.
///
/// Clears all state and notifies the UI to display an empty state.
async fn handle_no_player(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
) {
    loop_state.state_bundle.clear_lyrics();
    loop_state.state_bundle.player_state = Default::default();
    send_update(&loop_state.state_bundle, update_tx, true).await;
}

/// Fetches initial metadata for the discovered player service.
///
/// # Returns
///
/// Track metadata, or default metadata if the fetch fails.
///
/// # Error Handling
///
/// Errors are logged and default metadata is returned.
async fn fetch_initial_metadata(
    service: &str,
    _config: &LoopConfig,
) -> TrackMetadata {
    match crate::mpris::metadata::get_metadata(service).await {
        Ok(metadata) => metadata,
        Err(e) => {
            tracing::warn!(
                service = %service,
                error = %e,
                "Failed to fetch initial metadata"
            );
            TrackMetadata::default()
        }
    }
}

/// Initializes lyrics state based on initial metadata.
///
/// This function:
/// 1. Fetches lyrics from configured providers
/// 2. Updates player position
/// 3. Synchronizes playing status
async fn initialize_lyrics_state(
    loop_state: &mut LoopState,
    metadata: &TrackMetadata,
    service: &str,
    config: &LoopConfig,
) {
    let position = event::fetch_and_update_lyrics(
        metadata,
        &mut loop_state.state_bundle,
        config.providers(),
        Some(service),
    )
    .await;
    
    loop_state.state_bundle.player_state.set_position(position);
    loop_state.update_playing_status();
}

/// Spawns a background task to watch for MPRIS events.
///
/// The watcher monitors D-Bus for:
/// - Player state changes (metadata, position, playback status)
/// - Seek events (user scrubbing through track)
///
/// Events are forwarded to the event channel for processing by the main loop.
///
/// # Error Handling
///
/// Initialization and runtime errors are logged (if debug enabled) but don't
/// crash the application. The watcher task will terminate on fatal errors.
fn spawn_mpris_watcher(
    event_tx: mpsc::Sender<Event>,
    config: &LoopConfig,
) {
    let update_tx = event_tx.clone();
    let seek_tx = event_tx;
    let block_list = config.block_list().to_vec();

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
        )
        .await;

        match handler_result {
            Ok(mut handler) => {
                if let Err(e) = handler.handle_events().await {
                    tracing::error!(
                        error = %e,
                        "MPRIS event handler terminated"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "Failed to initialize MPRIS event handler"
                );
            }
        }
    });
}

/// Main event processing loop.
///
/// This is the core loop that processes events until shutdown.
///
/// # Event Sources
///
/// - MPRIS events (from background watcher task)
/// - Shutdown signal (for graceful termination)
///
/// # Termination
///
/// The loop runs indefinitely until a shutdown signal is received.
/// All event handlers are designed to never panic, ensuring graceful degradation.
async fn run_event_loop(
    mut loop_state: LoopState,
    mut event_rx: mpsc::Receiver<Event>,
    update_tx: mpsc::Sender<Update>,
    mut shutdown_rx: mpsc::Receiver<()>,
    config: LoopConfig,
) {
    loop {
        tokio::select! {
            // Shutdown signal received - clean up and terminate
            _ = shutdown_rx.recv() => {
                handle_shutdown(&mut loop_state, &update_tx, &config).await;
                break;
            }

            // MPRIS event received from watcher
            event = event_rx.recv() => {
                handle_event(event, &mut loop_state, &update_tx, &config).await;
            }
        }
    }
}

/// Processes a shutdown event and cleans up state.
///
/// Sends a final update to observers before terminating.
async fn handle_shutdown(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    process_event(
        Event::Shutdown,
        &mut loop_state.state_bundle,
        update_tx,
        config.providers(),
    )
    .await;
}

/// Handles an incoming event from the event channel.
///
/// If the channel is closed (returns `None`), logs a warning and does nothing.
/// This allows graceful degradation if the MPRIS watcher terminates.
async fn handle_event(
    event: Option<Event>,
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    let Some(event) = event else {
        // Event channel closed - MPRIS watcher terminated
        tracing::warn!("MPRIS event channel closed");
        return;
    };

    process_event(
        event,
        &mut loop_state.state_bundle,
        update_tx,
        config.providers(),
    )
    .await;
    
    loop_state.update_playing_status();
}