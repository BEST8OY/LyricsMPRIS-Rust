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
//! - **Non-blocking responsiveness**: Lyrics retrieval is fully asynchronous and never blocks the event loop
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐
//! │ MPRIS D-Bus     │
//! │ Event Watcher   │
//! └────────┬────────┘
//!          │ Events (channel cap 64)
//!          ▼
//! ┌─────────────────┐      ┌─────────────────────────┐
//! │ Event Loop      │─────▶│ Background Fetch Tasks  │
//! │ (this module)   │◀─────│ (epoch-tagged outcome)  │
//! └────────┬────────┘      └─────────────────────────┘
//!          │
//!          │ Updates
//!          ▼
//! ┌─────────────────┐
//! │ UI Update       │
//! │ Channel         │
//! └─────────────────┘
//! ```

use crate::event::fetch::{FetchOutcome, spawn_fetch};
use crate::event::{Event, RetryDirective, ScoredCandidate, process_event, send_update};
use crate::lyrics::filter::prepare_lyrics_for_display;
use crate::lyrics::types::{Lyrics, TrackInfo};
use crate::mpris::{TrackMetadata, events::MprisEventHandler};
use crate::state::{Provider, StateBundle, Update};
use std::collections::HashSet;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::time::Sleep;

/// UI commands sent from the TUI to the background event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCommand {
    /// Cycle to the next candidate lyrics result (`'r'` key in TUI).
    NextCandidate,
}

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
    fn new(meta: TrackMetadata, service: String, delay: std::time::Duration) -> Self {
        Self {
            sleep: Box::pin(tokio::time::sleep(delay)),
            meta,
            service,
            delay,
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
    /// Whether `'r'` candidate cycling is active (true in TUI mode, false in `--pipe`)
    candidate_cycling: bool,
}

impl LoopConfig {
    /// Creates a new loop configuration from the main app config.
    #[cfg(test)]
    fn new(config: crate::Config) -> Self {
        Self::with_cycling(config, false)
    }

    fn with_cycling(mut config: crate::Config, candidate_cycling: bool) -> Self {
        let providers = if config.providers.is_empty() {
            crate::config::SUPPORTED_PROVIDERS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        } else {
            std::mem::take(&mut config.providers)
        };

        Self {
            inner: config,
            providers,
            candidate_cycling,
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

    fn word_sync(&self) -> bool {
        !self.inner.no_karaoke
    }

    fn candidate_cycling(&self) -> bool {
        self.candidate_cycling
    }

    fn settings(&self) -> &crate::config::RuntimeSettings {
        &self.inner.settings
    }
}

/// Encapsulates the runtime state needed by the event loop.
///
/// This struct maintains the shared state bundle, generational epoch counter,
/// candidate queue, and background task handles for non-blocking execution.
struct LoopState {
    /// Shared state bundle with lyrics and player state
    state_bundle: StateBundle,
    /// Scheduled retry when providers were unreachable for the current track
    pending_retry: Option<PendingRetry>,
    /// Monotonic epoch counter to reject stale out-of-order background fetches
    current_epoch: u64,
    /// Task handle for the in-flight background lyrics fetch, aborted on track changes
    active_fetch: Option<tokio::task::JoinHandle<()>>,
    /// Ranked candidate queue populated by the 3-stage concurrent fetcher
    candidates: Vec<ScoredCandidate>,
    /// Fingerprints already displayed for the current track (for 'r' cycling)
    shown_fingerprints: HashSet<u64>,
    /// Fingerprint currently displayed
    current_fingerprint: Option<u64>,
}

impl LoopState {
    /// Creates a new loop state with default values.
    fn new() -> Self {
        Self {
            state_bundle: StateBundle::new(),
            pending_retry: None,
            current_epoch: 0,
            active_fetch: None,
            candidates: Vec::new(),
            shown_fingerprints: HashSet::new(),
            current_fingerprint: None,
        }
    }

    /// Aborts any running background lyrics fetch task.
    fn abort_fetch(&mut self) {
        if let Some(handle) = self.active_fetch.take() {
            handle.abort();
        }
    }
}

/// Main event loop entry point (without UI command channel, used by pipe mode).
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    shutdown_rx: mpsc::Receiver<()>,
    config: crate::Config,
) {
    listen_with_commands(update_tx, shutdown_rx, None, config).await;
}

/// Main event loop entry point with optional interactive TUI command receiver.
pub async fn listen_with_commands(
    update_tx: mpsc::Sender<Update>,
    shutdown_rx: mpsc::Receiver<()>,
    ui_cmd_rx: Option<mpsc::Receiver<UiCommand>>,
    config: crate::Config,
) {
    let loop_config = LoopConfig::with_cycling(config, ui_cmd_rx.is_some());
    let mut loop_state = LoopState::new();
    let (watcher_shutdown_tx, watcher_shutdown_rx) = tokio::sync::watch::channel(false);

    let (event_rx, fetch_tx, fetch_rx) = initialize_loop(
        &mut loop_state,
        &update_tx,
        &loop_config,
        watcher_shutdown_rx,
    )
    .await;

    let channels = LoopChannels {
        event_rx,
        fetch_tx,
        fetch_rx,
        update_tx,
        shutdown_rx,
        watcher_shutdown_tx,
        ui_cmd_rx,
    };

    run_event_loop(loop_state, channels, loop_config).await;
}

/// Initializes the event loop infrastructure.
///
/// This function:
/// 1. Creates the event channel and background fetch channel
/// 2. Discovers active player
/// 3. Fetches initial metadata and initializes player state (<5ms)
/// 4. Dispatches immediate UI update for instant startup
/// 5. Spawns isolated background fetch task
/// 6. Spawns MPRIS event watcher
async fn initialize_loop(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
    watcher_shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> (
    mpsc::Receiver<Event>,
    mpsc::Sender<FetchOutcome>,
    mpsc::Receiver<FetchOutcome>,
) {
    tracing::debug!("Initializing event loop");
    let (event_tx, event_rx) = mpsc::channel::<Event>(64);
    let (fetch_tx, fetch_rx) = mpsc::channel::<FetchOutcome>(16);

    let active_service = discover_active_player(config).await;

    if let Some(service) = active_service {
        tracing::debug!(service = %service, "Active player found");
        initialize_with_player(loop_state, &service, config, &fetch_tx).await;
        send_update(&mut loop_state.state_bundle, update_tx, true).await;
    } else {
        tracing::debug!("No active player found");
        crate::event::handle_no_player(&mut loop_state.state_bundle, update_tx).await;
    }

    spawn_mpris_watcher(event_tx, config, watcher_shutdown_rx);

    (event_rx, fetch_tx, fetch_rx)
}

/// Initializes state with an active player.
///
/// Queries metadata, status, and position from D-Bus (<5ms) and immediately
/// schedules non-blocking lyrics retrieval in the background.
async fn initialize_with_player(
    loop_state: &mut LoopState,
    service: &str,
    config: &LoopConfig,
    fetch_tx: &mpsc::Sender<FetchOutcome>,
) {
    tracing::debug!(
        service = %service,
        providers = ?config.providers(),
        "Initializing with active player"
    );
    let initial_metadata = fetch_initial_metadata(service, config).await;
    let playback_status = crate::mpris::get_playback_status(service).await;
    let initial_pos = crate::mpris::playback::get_position(service)
        .await
        .unwrap_or(0.0);

    loop_state
        .state_bundle
        .player_state
        .update_from_metadata(&initial_metadata);
    loop_state
        .state_bundle
        .player_state
        .set_position(initial_pos);
    if playback_status == "Playing" {
        loop_state.state_bundle.player_state.start_playing();
    } else {
        loop_state.state_bundle.player_state.pause();
    }

    // Spawn non-blocking background fetch with epoch 1
    loop_state.current_epoch = 1;
    let handle = spawn_fetch(
        loop_state.current_epoch,
        initial_metadata,
        config.providers().to_vec(),
        Some(service.to_string()),
        config.word_sync(),
        config.candidate_cycling(),
        config.settings().clone(),
        fetch_tx.clone(),
    );
    loop_state.active_fetch = Some(handle);
}

/// Discovers the first active, non-blocked media player service.
///
/// # Returns
///
/// - `Some(service)` if an active, non-blocked player is found
/// - `None` if no players are available, all are blocked, or enumeration failed
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

/// Fetches initial metadata for the discovered player service.
///
/// # Returns
///
/// Track metadata, or default metadata if the fetch fails.
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

/// Spawns a background task to watch for MPRIS events.
///
/// The watcher monitors D-Bus for:
/// - Player state changes (metadata, position, playback status)
/// - Seek events (user scrubbing through track)
///
/// Events are forwarded to the event channel for processing by the main loop.
fn spawn_mpris_watcher(
    event_tx: mpsc::Sender<Event>,
    config: &LoopConfig,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tracing::debug!("Spawning MPRIS event watcher");
    let block_list = config.block_list().to_vec();
    let target_list = config.target_list().to_vec();

    tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_millis(500);
        loop {
            if *shutdown_rx.borrow() || event_tx.is_closed() {
                tracing::debug!("MPRIS event watcher terminating on shutdown");
                break;
            }

            let handler_result =
                MprisEventHandler::new(event_tx.clone(), block_list.clone(), target_list.clone())
                    .await;

            match handler_result {
                Ok(mut handler) => {
                    let start = std::time::Instant::now();
                    tokio::select! {
                        res = handler.handle_events() => {
                            if let Err(e) = res {
                                tracing::warn!(
                                    error = %e,
                                    "MPRIS event handler terminated, retrying..."
                                );
                            }
                        }
                        _ = shutdown_rx.changed() => {
                            tracing::debug!("MPRIS event watcher received shutdown signal");
                            break;
                        }
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

            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown_rx.changed() => {
                    tracing::debug!("MPRIS event watcher received shutdown during backoff");
                    break;
                }
            }
            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
        }
    });
}

/// Channel endpoints used by the event loop.
struct LoopChannels {
    event_rx: mpsc::Receiver<Event>,
    fetch_tx: mpsc::Sender<FetchOutcome>,
    fetch_rx: mpsc::Receiver<FetchOutcome>,
    update_tx: mpsc::Sender<Update>,
    shutdown_rx: mpsc::Receiver<()>,
    watcher_shutdown_tx: tokio::sync::watch::Sender<bool>,
    ui_cmd_rx: Option<mpsc::Receiver<UiCommand>>,
}

async fn recv_ui_cmd(rx: &mut Option<mpsc::Receiver<UiCommand>>) -> Option<UiCommand> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => futures_util::future::pending().await,
    }
}

/// Main event processing loop.
async fn run_event_loop(mut loop_state: LoopState, mut channels: LoopChannels, config: LoopConfig) {
    loop {
        tokio::select! {
            // Shutdown signal received - clean up and terminate
            _ = channels.shutdown_rx.recv() => {
                let _ = channels.watcher_shutdown_tx.send(true);
                loop_state.abort_fetch();
                handle_shutdown(&mut loop_state, &channels.update_tx).await;
                break;
            }

            // Interactive TUI commands ('r' candidate cycling)
            maybe_cmd = recv_ui_cmd(&mut channels.ui_cmd_rx) => {
                match maybe_cmd {
                    Some(UiCommand::NextCandidate) => {
                        handle_next_candidate(&mut loop_state, &channels.update_tx, &config).await;
                    }
                    None => {
                        channels.ui_cmd_rx = None;
                    }
                }
            }

            // Asynchronous lyrics fetch outcome received
            outcome = channels.fetch_rx.recv() => {
                if let Some(outcome) = outcome {
                    handle_fetch_outcome(outcome, &mut loop_state, &channels.update_tx, &config).await;
                }
            }

            // MPRIS event received from watcher
            event = channels.event_rx.recv() => {
                if let Some(ev) = event {
                    let directive = handle_event(ev, &mut loop_state, &channels.update_tx).await;
                    apply_retry_directive(&mut loop_state, directive, &channels.fetch_tx, &config);
                } else {
                    tracing::warn!("MPRIS event channel closed, terminating event loop");
                    let _ = channels.watcher_shutdown_tx.send(true);
                    loop_state.abort_fetch();
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
                handle_retry_tick(&mut loop_state, &channels.fetch_tx, &config).await;
            }
        }
    }
}

/// Applies a [`RetryDirective`] to the loop state.
fn apply_retry_directive(
    loop_state: &mut LoopState,
    directive: RetryDirective,
    fetch_tx: &mpsc::Sender<FetchOutcome>,
    config: &LoopConfig,
) {
    match directive {
        RetryDirective::Keep => {}
        RetryDirective::Clear => {
            loop_state.abort_fetch();
            loop_state.candidates.clear();
            loop_state.shown_fingerprints.clear();
            loop_state.current_fingerprint = None;
            if loop_state.pending_retry.take().is_some() {
                tracing::debug!("Cancelled scheduled lyrics retry");
            }
        }
        RetryDirective::TrackChanged { meta, service } => {
            loop_state.current_epoch = loop_state.current_epoch.wrapping_add(1);
            loop_state.abort_fetch();
            loop_state.pending_retry = None;
            loop_state.candidates.clear();
            loop_state.shown_fingerprints.clear();
            loop_state.current_fingerprint = None;
            let handle = spawn_fetch(
                loop_state.current_epoch,
                *meta,
                config.providers().to_vec(),
                Some(service),
                config.word_sync(),
                config.candidate_cycling(),
                config.settings().clone(),
                fetch_tx.clone(),
            );
            loop_state.active_fetch = Some(handle);
        }
    }
}

fn track_info_from_meta(meta: &TrackMetadata, fallback_len: Option<f64>) -> TrackInfo {
    let mut info = TrackInfo::from_metadata(meta);
    if info.duration_secs <= 0
        && let Some(secs) = fallback_len
        && secs > 0.0
    {
        info.duration_secs = secs.round() as i64;
    }
    info
}

fn apply_lyrics_to_bundle(
    loop_state: &mut LoopState,
    raw_lyrics: Lyrics,
    meta: &TrackMetadata,
    config: &LoopConfig,
) -> Provider {
    let fp = raw_lyrics.fingerprint();
    loop_state.shown_fingerprints.insert(fp);
    loop_state.current_fingerprint = Some(fp);

    let track = track_info_from_meta(meta, loop_state.state_bundle.player_state.length);
    let prepared = prepare_lyrics_for_display(raw_lyrics, &track, &config.settings().filters);
    let provider = Provider::from_lyrics(&prepared);
    loop_state
        .state_bundle
        .update_lyrics(prepared.lines, meta, None, Some(provider.clone()));
    provider
}

/// Cycles to the next candidate lyrics variant for the active track (`'r'` key in TUI).
async fn handle_next_candidate(
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    if loop_state.candidates.is_empty() {
        return;
    }

    let next_idx = loop_state
        .candidates
        .iter()
        .position(|c| !loop_state.shown_fingerprints.contains(&c.fingerprint))
        .or_else(|| {
            if loop_state.candidates.len() > 1 {
                loop_state.shown_fingerprints.clear();
                if let Some(cur) = loop_state.current_fingerprint {
                    loop_state.shown_fingerprints.insert(cur);
                }
                loop_state
                    .candidates
                    .iter()
                    .position(|c| Some(c.fingerprint) != loop_state.current_fingerprint)
            } else {
                None
            }
        });

    let Some(idx) = next_idx else {
        return;
    };

    let cand_lyrics = loop_state.candidates[idx].lyrics.clone();
    let ps = &loop_state.state_bundle.player_state;
    let meta = TrackMetadata {
        title: ps.title.clone(),
        artist: ps.artist.clone(),
        album: ps.album.clone(),
        length: ps.length,
        ..Default::default()
    };

    let provider = apply_lyrics_to_bundle(loop_state, cand_lyrics, &meta, config);
    let current_pos = loop_state.state_bundle.player_state.estimate_position();
    loop_state.state_bundle.update_index(current_pos);
    send_update(&mut loop_state.state_bundle, update_tx, true).await;

    tracing::info!(
        title = %meta.title,
        artist = %meta.artist,
        provider = ?provider,
        "Cycled to alternate lyrics candidate"
    );
}

/// Handles a completed background lyrics fetch.
async fn handle_fetch_outcome(
    outcome: FetchOutcome,
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
    config: &LoopConfig,
) {
    if outcome.epoch() != loop_state.current_epoch {
        tracing::debug!(
            outcome_epoch = outcome.epoch(),
            current_epoch = loop_state.current_epoch,
            "Discarding stale fetch outcome from previous epoch"
        );
        return;
    }

    match outcome {
        FetchOutcome::Success {
            meta,
            lyrics,
            position,
            candidates,
            ..
        } => {
            let current = &loop_state.state_bundle.player_state;
            if current.title != meta.title || current.artist != meta.artist {
                tracing::debug!("Discarding fetch success for mismatched track");
                return;
            }

            loop_state.pending_retry = None;
            if !candidates.is_empty() {
                loop_state.candidates = candidates;
            }

            let provider = apply_lyrics_to_bundle(loop_state, *lyrics, &meta, config);

            let pos_before = loop_state.state_bundle.player_state.estimate_position();
            let effective_pos = if position > 0.05 {
                position
            } else {
                pos_before
            };

            loop_state.state_bundle.update_index(effective_pos);
            loop_state
                .state_bundle
                .player_state
                .set_position(effective_pos);

            send_update(&mut loop_state.state_bundle, update_tx, true).await;

            tracing::info!(
                title = %meta.title,
                artist = %meta.artist,
                provider = ?provider,
                "Lyrics loaded successfully"
            );
        }
        FetchOutcome::Upgraded {
            meta,
            lyrics,
            candidates,
            ..
        } => {
            let current = &loop_state.state_bundle.player_state;
            if current.title != meta.title || current.artist != meta.artist {
                return;
            }

            if !candidates.is_empty() {
                loop_state.candidates = candidates;
            }
            let provider = apply_lyrics_to_bundle(loop_state, *lyrics, &meta, config);
            let current_pos = loop_state.state_bundle.player_state.estimate_position();
            loop_state.state_bundle.update_index(current_pos);
            send_update(&mut loop_state.state_bundle, update_tx, false).await;
            tracing::info!(
                title = %meta.title,
                artist = %meta.artist,
                provider = ?provider,
                "Upgraded active track lyrics"
            );
        }
        FetchOutcome::CandidatesReady {
            meta, candidates, ..
        } => {
            let current = &loop_state.state_bundle.player_state;
            if current.title != meta.title || current.artist != meta.artist {
                return;
            }
            loop_state.active_fetch = None;
            if !candidates.is_empty() {
                loop_state.candidates = candidates;
            }
        }
        FetchOutcome::Exhausted { meta, err, .. } => {
            let current = &loop_state.state_bundle.player_state;
            if current.title != meta.title || current.artist != meta.artist {
                return;
            }

            loop_state.active_fetch = None;
            loop_state.pending_retry = None;

            loop_state
                .state_bundle
                .update_lyrics(Vec::new(), &meta, None, None);
            if let Some(err_msg) = err {
                loop_state.state_bundle.player_state.err = Some(err_msg);
            }
            send_update(&mut loop_state.state_bundle, update_tx, true).await;
            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                "Lyrics exhausted (no lyrics available)"
            );
        }
        FetchOutcome::Unavailable {
            meta,
            service,
            message,
            ..
        } => {
            let current = &loop_state.state_bundle.player_state;
            if current.title != meta.title || current.artist != meta.artist {
                return;
            }

            loop_state.active_fetch = None;
            loop_state
                .state_bundle
                .update_lyrics(Vec::new(), &meta, Some(message), None);
            send_update(&mut loop_state.state_bundle, update_tx, true).await;

            let next_delay = match &loop_state.pending_retry {
                Some(p) => p.delay.saturating_mul(2).min(RETRY_MAX_DELAY),
                None => RETRY_BASE_DELAY,
            };
            tracing::info!(
                title = %meta.title,
                artist = %meta.artist,
                retry_delay_secs = next_delay.as_secs(),
                "Providers unreachable, scheduling lyrics retry"
            );
            loop_state.pending_retry = Some(PendingRetry::new(meta, service, next_delay));
        }
    }
}

/// Handles a fired network-retry timer.
///
/// Spawns an asynchronous fetch for the pending track if it is still current
/// and still lyric-less.
async fn handle_retry_tick(
    loop_state: &mut LoopState,
    fetch_tx: &mpsc::Sender<FetchOutcome>,
    config: &LoopConfig,
) {
    let Some(pending) = loop_state.pending_retry.as_ref() else {
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
        loop_state.pending_retry = None;
        return;
    }

    let meta = pending.meta.clone();
    let service = pending.service.clone();

    tracing::debug!(
        title = %meta.title,
        artist = %meta.artist,
        "Retrying lyrics fetch after network failure"
    );

    loop_state.current_epoch = loop_state.current_epoch.wrapping_add(1);
    loop_state.abort_fetch();
    let handle = spawn_fetch(
        loop_state.current_epoch,
        meta,
        config.providers().to_vec(),
        Some(service),
        config.word_sync(),
        config.candidate_cycling(),
        config.settings().clone(),
        fetch_tx.clone(),
    );
    loop_state.active_fetch = Some(handle);
}

/// Processes a shutdown event and cleans up state.
///
/// Sends a final update to observers before terminating.
async fn handle_shutdown(loop_state: &mut LoopState, update_tx: &mpsc::Sender<Update>) {
    tracing::debug!("Shutting down event loop");
    send_update(&mut loop_state.state_bundle, update_tx, true).await;
}

/// Handles an incoming event from the event channel.
async fn handle_event(
    event: Event,
    loop_state: &mut LoopState,
    update_tx: &mpsc::Sender<Update>,
) -> RetryDirective {
    process_event(event, &mut loop_state.state_bundle, update_tx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_state_initial_values() {
        let state = LoopState::new();
        assert_eq!(state.current_epoch, 0);
        assert!(state.pending_retry.is_none());
        assert!(state.active_fetch.is_none());
        assert!(!state.state_bundle.has_lyrics());
    }

    #[tokio::test]
    async fn test_apply_retry_directive_track_changed() {
        let mut loop_state = LoopState::new();
        let (fetch_tx, _fetch_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());

        let meta = TrackMetadata {
            title: "Test Track".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };

        apply_retry_directive(
            &mut loop_state,
            RetryDirective::TrackChanged {
                meta: Box::new(meta),
                service: "org.mpris.MediaPlayer2.test".into(),
            },
            &fetch_tx,
            &config,
        );

        assert_eq!(loop_state.current_epoch, 1);
        assert!(loop_state.active_fetch.is_some());
        assert!(loop_state.pending_retry.is_none());

        loop_state.abort_fetch();
        assert!(loop_state.active_fetch.is_none());
    }

    #[tokio::test]
    async fn test_apply_retry_directive_clear_aborts_fetch() {
        let mut loop_state = LoopState::new();
        let (fetch_tx, _fetch_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());

        let meta = TrackMetadata {
            title: "Test Track".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };

        apply_retry_directive(
            &mut loop_state,
            RetryDirective::TrackChanged {
                meta: Box::new(meta.clone()),
                service: "org.mpris.MediaPlayer2.test".into(),
            },
            &fetch_tx,
            &config,
        );
        assert!(loop_state.active_fetch.is_some());

        apply_retry_directive(&mut loop_state, RetryDirective::Clear, &fetch_tx, &config);
        assert!(loop_state.active_fetch.is_none());
    }

    #[tokio::test]
    async fn test_handle_fetch_outcome_stale_epoch_ignored() {
        let mut loop_state = LoopState::new();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());
        loop_state.current_epoch = 2;

        let meta = TrackMetadata {
            title: "Track 1".into(),
            artist: "Artist 1".into(),
            ..Default::default()
        };

        let outcome = FetchOutcome::Success {
            epoch: 1, // Stale epoch (current is 2)
            meta,
            lyrics: Box::new(Lyrics::new(
                vec![crate::lyrics::LyricLine::new(0.0, "Hello")],
                "LRCLIB",
                true,
            )),
            position: 0.0,
            candidates: Vec::new(),
        };

        handle_fetch_outcome(outcome, &mut loop_state, &update_tx, &config).await;

        // No lyrics should have been applied and no update sent
        assert!(!loop_state.state_bundle.has_lyrics());
        assert!(update_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_handle_fetch_outcome_matching_epoch_applies_lyrics() {
        let mut loop_state = LoopState::new();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());
        loop_state.current_epoch = 1;

        let meta = TrackMetadata {
            title: "Track 1".into(),
            artist: "Artist 1".into(),
            ..Default::default()
        };
        loop_state
            .state_bundle
            .player_state
            .update_from_metadata(&meta);

        let outcome = FetchOutcome::Success {
            epoch: 1,
            meta: meta.clone(),
            lyrics: Box::new(Lyrics::new(
                vec![crate::lyrics::LyricLine::new(0.0, "Hello")],
                "LRCLIB",
                true,
            )),
            position: 0.0,
            candidates: Vec::new(),
        };

        handle_fetch_outcome(outcome, &mut loop_state, &update_tx, &config).await;

        assert!(loop_state.state_bundle.has_lyrics());
        assert_eq!(loop_state.state_bundle.lyric_state.lines.len(), 1);
        let update = update_rx.try_recv().expect("UI update should be sent");
        assert_eq!(update.title, "Track 1");
        assert_eq!(update.lines.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_fetch_outcome_upgraded_hotswaps_active_track() {
        let mut loop_state = LoopState::new();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());
        loop_state.current_epoch = 1;

        let mut meta = TrackMetadata::default();
        meta.title = "Current Track".into();
        meta.artist = "Current Artist".into();
        loop_state
            .state_bundle
            .player_state
            .update_from_metadata(&meta);
        loop_state.state_bundle.mark_state_sent();

        let rich_word = crate::lyrics::types::WordTiming {
            start: 10.0,
            end: 14.0,
            text: "Karaoke".into(),
            grapheme_boundaries: vec![0, 7],
            grapheme_times: vec![],
            ..Default::default()
        };
        let upgraded_lines = vec![
            crate::lyrics::LyricLine::new(5.0, "Upgraded line 1"),
            crate::lyrics::LyricLine::new(10.0, "Karaoke line 2").with_words(Some(vec![rich_word])),
        ];

        let outcome = FetchOutcome::Upgraded {
            epoch: 1,
            meta: meta.clone(),
            lyrics: Box::new(Lyrics::new(upgraded_lines, "Musixmatch", true)),
            candidates: Vec::new(),
        };

        handle_fetch_outcome(outcome, &mut loop_state, &update_tx, &config).await;

        assert_eq!(loop_state.state_bundle.lyric_state.lines.len(), 2);
        assert_eq!(
            loop_state.state_bundle.lyric_state.lines[1].text,
            "Karaoke line 2"
        );
        assert!(loop_state.state_bundle.lyric_state.lines[1].words.is_some());
        let update = update_rx
            .try_recv()
            .expect("Hot-swap update should be sent");
        assert_eq!(
            update.provider,
            Some(crate::state::Provider::MusixmatchRichsync)
        );
    }

    #[tokio::test]
    async fn test_handle_fetch_outcome_upgraded_ignores_stale_track() {
        let mut loop_state = LoopState::new();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());
        loop_state.current_epoch = 1;

        // Player is currently playing Track B
        let mut current_meta = TrackMetadata::default();
        current_meta.title = "New Song".into();
        current_meta.artist = "New Artist".into();
        loop_state
            .state_bundle
            .player_state
            .update_from_metadata(&current_meta);
        let initial_lines = vec![crate::lyrics::LyricLine::new(0.0, "New track line")];
        loop_state
            .state_bundle
            .lyric_state
            .update_lines(initial_lines);
        loop_state.state_bundle.mark_state_sent();

        // Stale upgrade arrives for Track A
        let mut old_meta = TrackMetadata::default();
        old_meta.title = "Old Song".into();
        old_meta.artist = "Old Artist".into();
        let old_lines = vec![crate::lyrics::LyricLine::new(0.0, "Stale line")];

        let outcome = FetchOutcome::Upgraded {
            epoch: 1,
            meta: old_meta,
            lyrics: Box::new(Lyrics::new(old_lines, "Musixmatch", true)),
            candidates: Vec::new(),
        };

        handle_fetch_outcome(outcome, &mut loop_state, &update_tx, &config).await;

        // No update sent because track didn't match
        assert!(update_rx.try_recv().is_err());
        assert_eq!(
            loop_state.state_bundle.lyric_state.lines[0].text,
            "New track line"
        );
    }

    #[tokio::test]
    async fn test_next_candidate_cycling_wraps_cleanly() {
        let mut loop_state = LoopState::new();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let config = LoopConfig::new(crate::Config::default());
        loop_state.current_epoch = 1;

        let meta = TrackMetadata {
            title: "Song".into(),
            artist: "Artist".into(),
            ..Default::default()
        };
        loop_state
            .state_bundle
            .player_state
            .update_from_metadata(&meta);

        let l1 = Lyrics::new(
            vec![crate::lyrics::LyricLine::new(1.0, "Cand 1")],
            "Apple",
            true,
        );
        let l2 = Lyrics::new(
            vec![crate::lyrics::LyricLine::new(1.0, "Cand 2")],
            "LRCLIB",
            true,
        );
        let c1 = ScoredCandidate::from_lyrics(l1.clone(), 90);
        let c2 = ScoredCandidate::from_lyrics(l2, 70);

        handle_fetch_outcome(
            FetchOutcome::Success {
                epoch: 1,
                meta,
                lyrics: Box::new(l1),
                position: 1.0,
                candidates: vec![c1, c2],
            },
            &mut loop_state,
            &update_tx,
            &config,
        )
        .await;
        let _ = update_rx.try_recv().expect("Initial update");

        handle_next_candidate(&mut loop_state, &update_tx, &config).await;
        let u2 = update_rx.try_recv().expect("Cycle to second candidate");
        assert_eq!(u2.lines[0].text, "Cand 2");
        assert_eq!(
            u2.provider.as_ref().map(Provider::display_name),
            Some("LRCLIB")
        );

        handle_next_candidate(&mut loop_state, &update_tx, &config).await;
        let u3 = update_rx.try_recv().expect("Wrap back to first candidate");
        assert_eq!(u3.lines[0].text, "Cand 1");
    }
}
