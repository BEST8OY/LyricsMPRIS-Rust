use crate::mpris::TrackMetadata;
use crate::state::{Provider, StateBundle, Update};
use tokio::sync::mpsc;
use std::sync::atomic::{AtomicU64, Ordering};

// ============================================================================
// Event Types
// ============================================================================

/// Events originating from MPRIS player interface
#[derive(Debug)]
pub enum MprisEvent {
    /// Full player state update with metadata, position, and service name
    PlayerUpdate(TrackMetadata, f64, String),
    /// Seek event when user scrubs through track
    Seeked(TrackMetadata, f64, String),
}

/// Top-level events processed by the main event loop
#[derive(Debug)]
pub enum Event {
    Mpris(MprisEvent),
    Shutdown,
}

// ============================================================================
// Update Tracking
// ============================================================================

/// Tracks the last sent state to avoid redundant UI updates.
/// Format: (version << 1) | playing_bit
static LAST_SENT_VERSION: AtomicU64 = AtomicU64::new(0);

/// Computes a unique key for the current state
fn compute_state_key(version: u64, playing: bool) -> u64 {
    let playing_bit = if playing { 1 } else { 0 };
    (version << 1) | playing_bit
}

/// Checks if the state has changed since the last update
fn has_state_changed(version: u64, playing: bool) -> bool {
    let current_key = compute_state_key(version, playing);
    let last_key = LAST_SENT_VERSION.load(Ordering::Relaxed);
    current_key != last_key
}

/// Marks the current state as sent
fn mark_state_sent(version: u64, playing: bool) {
    let key = compute_state_key(version, playing);
    LAST_SENT_VERSION.store(key, Ordering::Relaxed);
}

// ============================================================================
// Update Sending
// ============================================================================

/// Determines if an update should be sent to the UI
fn should_send_update(state: &StateBundle, force: bool) -> bool {
    if force {
        return true;
    }
    
    if !has_state_changed(state.version, state.player_state.playing) {
        return false;
    }
    
    // Send update if there's meaningful content
    !state.lyric_state.lines.is_empty() || state.player_state.err.is_some()
}

/// Builds an Update message from current state.
/// Uses `StateBundle::create_update` to build Update snapshots (keeps
/// the logic colocated with the state container).
///
/// Sends an update to the UI channel when appropriate.
pub async fn send_update(state: &StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
    if !should_send_update(state, force) {
        return;
    }

    let update = state.create_update();
    
    if update_tx.send(update).await.is_ok() {
        mark_state_sent(state.version, state.player_state.playing);
    }
}

// ============================================================================
// Lyrics Fetching
// ============================================================================

/// Result of a lyrics fetch attempt
enum FetchResult {
    Success,
    Transient,
    NonTransient(crate::lyrics::LyricsError),
}

/// Attempts to fetch lyrics from a single provider
async fn try_provider(
    provider: &str,
    meta: &TrackMetadata,
    state: &mut StateBundle,
) -> FetchResult {
    match provider {
        "lrclib" => try_lrclib(meta, state).await,
        "musixmatch" => try_musixmatch(meta, state).await,
        _ => FetchResult::Transient, // Unknown providers are skipped
    }
}

/// Fetches lyrics from LRCLib
async fn try_lrclib(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_lrclib(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
    )
    .await
    {
        Ok((lines, _raw)) if !lines.is_empty() => {
            state.update_lyrics(lines, meta, None, Some(Provider::Lrclib));
            FetchResult::Success
        }
        Ok(_) => FetchResult::Transient,
        Err(crate::lyrics::LyricsError::Network(_)) => FetchResult::Transient,
        Err(e) => FetchResult::NonTransient(e),
    }
}

/// Fetches lyrics from Musixmatch
async fn try_musixmatch(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.spotify_id.as_deref(),
    )
    .await
    {
        Ok((lines, raw)) if !lines.is_empty() => {
            let provider = determine_musixmatch_provider(&lines, &raw);
            state.update_lyrics(lines, meta, None, Some(provider));
            FetchResult::Success
        }
        Ok(_) => FetchResult::Transient,
        Err(crate::lyrics::LyricsError::Network(_)) => FetchResult::Transient,
        Err(e) => FetchResult::NonTransient(e),
    }
}

/// Determines which Musixmatch format was returned
fn determine_musixmatch_provider(lines: &[crate::lyrics::LyricLine], raw: &Option<String>) -> Provider {
    let has_words = lines.iter().any(|l| l.words.is_some());
    let is_richsync = raw
        .as_ref()
        .map(|r| r.starts_with(";;richsync=1"))
        .unwrap_or(false);

    if has_words || is_richsync {
        Provider::MusixmatchRichsync
    } else {
        Provider::MusixmatchSubtitles
    }
}

/// Fetches lyrics from all configured providers
async fn fetch_api_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    debug_log: bool,
    providers: &[String],
) {
    for provider in providers {
        match try_provider(provider, meta, state).await {
            FetchResult::Success => return,
            FetchResult::Transient => continue,
            FetchResult::NonTransient(err) => {
                if debug_log {
                    eprintln!("[LyricsMPRIS] Provider error ({}): {}", provider, err);
                }
                state.update_lyrics(Vec::new(), meta, Some(err.to_string()), None);
                return;
            }
        }
    }

    // No provider succeeded
    state.update_lyrics(Vec::new(), meta, None, None);
}

/// Fetches a fresh position from the player
async fn fetch_fresh_position(
    service: Option<&str>,
    state: &StateBundle,
    debug_log: bool,
) -> f64 {
    let Some(svc) = service else {
        return state.player_state.estimate_position();
    };

    match crate::mpris::playback::get_position(svc).await {
        Ok(pos) => pos,
        Err(e) => {
            if debug_log {
                eprintln!("[LyricsMPRIS] Failed to fetch position: {}", e);
            }
            state.player_state.estimate_position()
        }
    }
}

/// Fetches lyrics and updates position atomically
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    debug_log: bool,
    providers: &[String],
    service: Option<&str>,
) -> f64 {
    fetch_api_lyrics(meta, state, debug_log, providers).await;
    
    let position = fetch_fresh_position(service, state, debug_log).await;
    state.update_index(position);
    state.player_state.set_position(position);
    
    position
}

// ============================================================================
// Event Processing
// ============================================================================

/// Processes a single event from the event loop
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    match event {
        Event::Mpris(mpris_event) => {
            handle_mpris_event(mpris_event, state, update_tx, latest_meta).await;
        }
        Event::Shutdown => {
            send_update(state, update_tx, true).await;
        }
    }
}

/// Handles MPRIS events (player updates and seeks)
async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    let (meta, position, service, is_player_update) = match event {
        MprisEvent::PlayerUpdate(m, p, s) => (m, p, s, true),
        MprisEvent::Seeked(m, p, s) => (m, p, s, false),
    };

    // Handle empty service (no active player)
    if service.is_empty() {
        handle_no_player(state, update_tx).await;
        return;
    }

    let playback_status = get_playback_status(&service).await;

    // Handle stopped player
    if playback_status.as_deref() == Some("Stopped") {
        handle_no_player(state, update_tx).await;
        return;
    }

    // Handle new track
    if is_player_update && state.player_state.has_changed(&meta) {
        handle_new_track(meta, position, service, playback_status, state, update_tx, latest_meta).await;
        return;
    }

    // Handle position/playback state changes
    handle_state_update(position, playback_status, state, update_tx).await;
}

/// Clears state when no player is active
async fn handle_no_player(state: &mut StateBundle, update_tx: &mpsc::Sender<Update>) {
    state.clear_lyrics();
    state.player_state = Default::default();
    send_update(state, update_tx, true).await;
}

/// Handles detection of a new track
async fn handle_new_track(
    meta: TrackMetadata,
    position: f64,
    service: String,
    playback_status: Option<String>,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    state.clear_lyrics();
    *latest_meta = Some((meta, position, service));

    // Update playback state if available
    if let Some(status) = playback_status {
        let playing = status == "Playing";
        state.player_state.update_playback_dbus(playing, position);
    } else {
        state.player_state.set_position(position);
    }

    send_update(state, update_tx, true).await;
}

/// Handles position and playback state updates
async fn handle_state_update(
    position: f64,
    playback_status: Option<String>,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
) {
    let prev_playing = state.player_state.playing;

    // Update playback state
    if let Some(status) = playback_status {
        let playing = status == "Playing";
        state.player_state.update_playback_dbus(playing, position);
    } else {
        state.player_state.set_position(position);
    }

    // Update lyric index
    let position = state.player_state.estimate_position();
    let changed_index = state.update_index(position);

    // Send update if meaningful change occurred
    let playing_changed = prev_playing != state.player_state.playing;
    if playing_changed || changed_index {
        send_update(state, update_tx, false).await;
    }
}

/// Fetches playback status from the player
async fn get_playback_status(service: &str) -> Option<String> {
    crate::mpris::get_playback_status(service)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

// ============================================================================
// Polling
// ============================================================================

/// Handles queued metadata fetches
async fn handle_pending_metadata(
    state: &mut StateBundle,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    debug_log: bool,
    providers: &[String],
    update_tx: &mpsc::Sender<Update>,
) -> bool {
    let Some((meta, _pos, service)) = latest_meta.take() else {
        return false;
    };

    fetch_and_update_lyrics(&meta, state, debug_log, providers, Some(&service)).await;
    send_update(state, update_tx, true).await;
    true
}

/// Syncs position when playing
async fn sync_position(state: &mut StateBundle) -> bool {
    if !state.player_state.playing {
        return false;
    }

    let position = state.player_state.estimate_position();
    state.player_state.set_position(position);
    state.update_index(position)
}

/// Periodic polling handler for background tasks
pub async fn handle_poll(
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    debug_log: bool,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    providers: &[String],
) {
    let mut needs_update = false;

    // Process pending metadata
    if handle_pending_metadata(state, latest_meta, debug_log, providers, update_tx).await {
        needs_update = true;
    }

    // Sync position when playing
    if sync_position(state).await {
        needs_update = true;
    }

    // Always send if there's an error
    if state.player_state.err.is_some() {
        needs_update = true;
    }

    if needs_update {
        send_update(state, update_tx, false).await;
    }
}