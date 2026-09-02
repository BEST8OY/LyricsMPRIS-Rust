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

use crate::event::fetch::FetchSummary;
use crate::event::{self, Event, MprisEvent, RetryDirective, process_event, send_update};
use crate::mpris::{TrackMetadata, events::MprisEventHandler};
use crate::state::{StateBundle, Update};
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::time::Sleep;

/// Delay before the first retry when providers are unreachable.
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
/// Upper bound for the exponential retry backoff.
const RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// A scheduled background retry for a track whose lyrics could not be fetched
/// because all providers were unreachable.
struct PendingRetry {
    /// Track to refetch
    meta: TrackMetadata,
    /// Player service used for fresh position lookups
    service: String,
    /// Current backoff delay (doubles on each failed attempt)
    delay: std::time::Duration,
    /// Timer firing the next attempt
    sleep: Pin<Box<Sleep>>,
}

impl PendingRetry {
    fn new(meta: TrackMetadata, service: String) -> Self {
        Self {
            sleep: Box::pin(tokio::time::sleep(RETRY_BASE_DELAY)),
            meta,
            service,
            delay: RETRY_BASE_DELAY,
        }
    }
}

/// Configuration for the event loop.
///
/// Wraps the main application config and provides convenient accessors
/// for event loop operations.
struct LoopConfig {
    /// Main app config
    inner: crate::Config,
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
            inner: config,
            providers,
        }
    }

    /// Returns the list of blocked player services.
    fn block_list(&self) -> &[String] {
        &self.inner.block
    }

    /// Returns the list of targeted player services
    fn target_list(&self) -> &[String] {
        &self.inner.target
    }

    /// Returns the ordered list of lyrics providers.
    fn providers(&self) -> &[String] {
        &self.providers
    }
}

/// Encapsulates the runtime state needed by the event loop.
///
/// This struct maintains the shared state bundle for event processing.
struct LoopState {
    /// Shared state bundle with lyrics and player state
    state_bundle: StateBundle,
    /// Scheduled retry when providers were unreachable for the current track
    pending_retry: Option<PendingRetry>,
}

impl LoopState {
    /// Creates a new loop state with default values.
    fn new() -> Self {
        Self {
            state_bundle: StateBundle::new(),
            pending_retry: None,
        }
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

    run_event_loop(loop_state, event_rx, update_tx, shutdown_rx, loop_config).await;
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
    tracing::debug!("Initializing event loop");
    let (event_tx, event_rx) = mpsc::channel::<Event>(16);

    let active_service = discover_active_player(config).await;

    if let Some(service) = active_service {
        tracing::debug!(service = %service, "Active player found");
        initialize_with_player(loop_state, &service, config).await;
    } else {
        tracing::debug!("No active player found");
        handle_no_player(loop_state, update_tx).await;
    }

    spawn_mpris_watcher(event_tx, config);

    event_rx
}

/// Initializes state with an active player.
///
/// Fetches initial metadata and lyrics for the current track. If providers
/// were unreachable, schedules a background retry on the loop state.
async fn initialize_with_player(loop_state: &mut LoopState, service: &str, config: &LoopConfig) {
    tracing::debug!(
        service = %service,
        providers = ?config.providers(),
        "Initializing with active player"
    );
    let initial_metadata = fetch_initial_metadata(service, config).await;
    let summary = initialize_lyrics_state(loop_state, &initial_metadata, service, config).await;

    if summary == FetchSummary::Unavailable {
        tracing::info!(
            title = %initial_metadata.title,
            artist = %initial_metadata.artist,
            retry_delay_secs = RETRY_BASE_DELAY.as_secs(),
            "Providers unreachable at startup, scheduling retry"
        );
        loop_state.pending_retry = Some(PendingRetry::new(initial_metadata, service.to_string()));
    }
}

/// Discovers the first active, non-blocked media player service.
///
/// # Returns
///
/// - `Some(service)` if an active, non-blocked player is found
/// - `None` if no players are available, all are blocked, or enumeration failed
///
/// # Error Handling
///
/// D-Bus enumeration errors are logged as warnings and treated as no player.
async fn discover_active_player(config: &LoopConfig) -> Option<String> {
    match crate::mpris::find_active_service(config.block_list(), config.target_list()).await {
        Ok(Some(service)) => {
            tracing::debug!(selected_player = %service, "Selected active player");
            Some(service)
        }
        Ok(None) => {
            tracing::debug!("No active player found");
            None
        }
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
async fn handle_no_player(loop_state: &mut LoopState, update_tx: &mpsc::Sender<Update>) {
    loop_state.state_bundle.clear_lyrics();
    loop_state.state_bundle.player_state = Default::default();
    send_update(&mut loop_state.state_bundle, update_tx, true).await;
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
async fn fetch_initial_metadata(service: &str, _config: &LoopConfig) -> TrackMetadata {
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
/// This function fetches lyrics from configured providers.
/// Position and state updates are handled internally by `fetch_and_update_lyrics`.
///
/// Returns the [`FetchSummary`] of the fetch pass.
async fn initialize_lyrics_state(
    loop_state: &mut LoopState,
    metadata: &TrackMetadata,
    service: &str,
    config: &LoopConfig,
) -> FetchSummary {
    tracing::debug!(
        title = %metadata.title,
        artist = %metadata.artist,
        "Fetching initial lyrics"
    );

    // fetch_and_update_lyrics already sets the position internally
    let (_position, summary) = event::fetch_and_update_lyrics(
        metadata,
        &mut loop_state.state_bundle,
        config.providers(),
        Some(service),
    )
    .await;

    if loop_state.state_bundle.has_lyrics() {
        tracing::debug!(
            provider = ?loop_state.state_bundle.provider,
            lines = loop_state.state_bundle.lyric_state.lines.len(),
            "Successfully loaded lyrics"
        );
    } else {
        tracing::debug!("No lyrics found for track");
    }

    summary
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
/// If initialization fails or the D-Bus connection is lost, the watcher automatically
/// retries with exponential backoff (500 ms up to 30 s). Backoff is reset to 500 ms
/// only after sustained uptime (>= 5 seconds).
fn spawn_mpris_watcher(event_tx: mpsc::Sender<Event>, config: &LoopConfig) {
    tracing::debug!("Spawning MPRIS event watcher");
    let block_list = config.block_list().to_vec();
    let target_list = config.target_list().to_vec();

    tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_millis(500);
        loop {
            let update_tx = event_tx.clone();
            let seek_tx = event_tx.clone();
            let calib_tx = event_tx.clone();
            let handler_result = MprisEventHandler::with_closures(
                move |meta, pos, service, status| {
                    let _ = update_tx.try_send(Event::Mpris(Box::new(MprisEvent::PlayerUpdate(
                        meta, pos, service, status,
                    ))));
                },
                move |meta, pos, service| {
                    let _ = seek_tx.try_send(Event::Mpris(Box::new(MprisEvent::Seeked(
                        meta, pos, service,
                    ))));
                },
                move |pos| {
                    let _ = calib_tx.try_send(Event::Mpris(Box::new(MprisEvent::Calibrated {
                        position: pos,
                    })));
                },
                block_list.clone(),
                target_list.clone(),
            )
            .await;

            match handler_result {
                Ok(mut handler) => {
                    let start = std::time::Instant::now();
                    if let Err(e) = handler.handle_events().await {
                        tracing::warn!(
                            error = %e,
                            "MPRIS event handler terminated, retrying..."
                        );
                    }
                    if start.elapsed() >= std::time::Duration::from_secs(5) {
                        backoff = std::time::Duration::from_millis(500);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backoff = ?backoff,
                        "Failed to initialize MPRIS event handler, retrying with backoff..."
                    );
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
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
/// - Network-retry timer (refetches lyrics when providers were unreachable)
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
                if let Some(ev) = event {
                    let directive =
                        handle_event(ev, &mut loop_state, &update_tx, &config).await;
                    apply_retry_directive(&mut loop_state, directive);
                } else {
                    tracing::warn!("MPRIS event channel closed, terminating event loop");
                    break;
                }
            }

            // Network-retry timer fired
            _ = async {
                match loop_state.pending_retry.as_mut() {
                    Some(pending) => pending.sleep.as_mut().await,
                    None => futures_util::future::pending().await,
                }
            } => {
                handle_retry_tick(&mut loop_state, &update_tx, &config).await;
            }
        }
    }
}

/// Applies a [`RetryDirective`] to the loop state.
fn apply_retry_directive(loop_state: &mut LoopState, directive: RetryDirective) {
    match directive {
        RetryDirective::Keep => {}
        RetryDirective::Clear => {
            if loop_state.pending_retry.take().is_some() {
                tracing::debug!("Cancelled scheduled lyrics retry");
            }
        }
        RetryDirective::Schedule { meta, service } => {
            tracing::info!(
                title = %meta.title,
                artist = %meta.artist,
                retry_delay_secs = RETRY_BASE_DELAY.as_secs(),
                "Scheduling lyrics retry after provider outage"
            );
            loop_state.pending_retry = Some(PendingRetry::new(meta, service));
        }
    }
}

/// Handles a fired network-retry timer.
///
/// Refetches lyrics for the pending track if it is still current and still
/// lyric-less. On continued unavailability, reschedules with doubled backoff
/// (capped at [`RETRY_MAX_DELAY`]); otherwise the retry is retired.
async fn handle_retry_tick(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    let Some(pending) = loop_state.pending_retry.take() else {
        return;
    };

    // Track changed or lyrics arrived meanwhile - drop the stale retry
    let player = &loop_state.state_bundle.player_state;
    if player.title != pending.meta.title
        || player.artist != pending.meta.artist
        || player.album != pending.meta.album
        || loop_state.state_bundle.has_lyrics()
    {
        tracing::debug!("Skipping stale lyrics retry");
        return;
    }

    tracing::debug!(
        title = %pending.meta.title,
        artist = %pending.meta.artist,
        "Retrying lyrics fetch after network failure"
    );

    let (_, summary) = event::fetch_and_update_lyrics(
        &pending.meta,
        &mut loop_state.state_bundle,
        config.providers(),
        Some(&pending.service),
    )
    .await;

    send_update(&mut loop_state.state_bundle, update_tx, true).await;

    match summary {
        FetchSummary::Unavailable => {
            let delay = pending.delay.saturating_mul(2).min(RETRY_MAX_DELAY);
            tracing::debug!(
                next_delay_secs = delay.as_secs(),
                "Providers still unreachable, rescheduling lyrics retry"
            );
            loop_state.pending_retry = Some(PendingRetry {
                meta: pending.meta,
                service: pending.service,
                delay,
                sleep: Box::pin(tokio::time::sleep(delay)),
            });
        }
        FetchSummary::Loaded | FetchSummary::Exhausted => {}
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
    tracing::debug!("Shutting down event loop");
    process_event(
        Event::Shutdown,
        &mut loop_state.state_bundle,
        update_tx,
        config.providers(),
    )
    .await;
}

/// Handles an incoming event from the event channel.
async fn handle_event(
    event: Event,
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) -> RetryDirective {
    process_event(
        event,
        &mut loop_state.state_bundle,
        update_tx,
        config.providers(),
    )
    .await
}
