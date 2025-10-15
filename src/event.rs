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

#[inline]
fn state_key(version: u64, playing: bool) -> u64 {
    (version << 1) | (playing as u64)
}

#[inline]
fn state_changed(version: u64, playing: bool) -> bool {
    state_key(version, playing) != LAST_SENT_VERSION.load(Ordering::Relaxed)
}

#[inline]
fn mark_state_sent(version: u64, playing: bool) {
    LAST_SENT_VERSION.store(state_key(version, playing), Ordering::Relaxed);
}

// ============================================================================
// Update Sending
// ============================================================================

/// Determines if an update should be sent to the UI
fn should_send_update(state: &StateBundle, force: bool) -> bool {
    if force {
        return true;
    }

    if !state_changed(state.version, state.player_state.playing) {
        return false;
    }

    // Only send updates when there's something worth showing to the UI
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
async fn try_provider(provider: &str, meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match provider.as_ref() {
        "lrclib" => try_lrclib(meta, state).await,
        "musixmatch" => try_musixmatch(meta, state).await,
        _ => FetchResult::Transient,
    }
}

/// Fetches lyrics from LRCLib
async fn try_lrclib(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title, &meta.album, meta.length).await {
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
    let is_richsync = raw.as_deref().map(|r| r.starts_with(";;richsync=1")).unwrap_or(false);

    if has_words || is_richsync {
        Provider::MusixmatchRichsync
    } else {
        Provider::MusixmatchSubtitles
    }
}

/// Fetches lyrics from all configured providers
async fn fetch_api_lyrics(meta: &TrackMetadata, state: &mut StateBundle, debug_log: bool, providers: &[String]) {
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
    debug_log: bool,
    providers: &[String],
) {
    match event {
        Event::Mpris(ev) => handle_mpris_event(ev, state, update_tx, debug_log, providers).await,
        Event::Shutdown => send_update(state, update_tx, true).await,
    }
}

/// Handles MPRIS events (player updates and seeks)
async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    debug_log: bool,
    providers: &[String],
) {
    let (meta, position, service, is_full_update) = match event {
        MprisEvent::PlayerUpdate(m, p, s) => (m, p, s, true),
        MprisEvent::Seeked(m, p, s) => (m, p, s, false),
    };

    // No active player: clear state and notify UI
    if service.is_empty() {
        handle_no_player(state, update_tx).await;
        return;
    }

    // Only fetch playback status for full updates
    let playback_status = if is_full_update { get_playback_status(&service).await } else { None };

    // If the player reported 'Stopped' on a full update, treat as no player
    if is_full_update && playback_status.as_deref() == Some("Stopped") {
        handle_no_player(state, update_tx).await;
        return;
    }

    // New track detection on full updates
    if is_full_update && state.player_state.has_changed(&meta) {
        handle_new_track(meta, position, service, playback_status, state, update_tx, debug_log, providers).await;
        return;
    }

    // If this was a Seeked event (not a full PlayerUpdate), force a UI update
    // so the UI immediately reflects the new position/highlight even if the
    // index or playing flag didn't change.
    if !is_full_update {
        send_update(state, update_tx, true).await;
    }

    // Otherwise it's a position/playback update
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
    debug_log: bool,
    providers: &[String],
) {
    state.clear_lyrics();

    // Update playback state if available
    if let Some(status) = playback_status {
        let playing = status == "Playing";
        state.player_state.update_playback_dbus(playing, position);
    } else {
        state.player_state.set_position(position);
    }

    // Notify UI immediately that a new track started (lyrics may follow)
    send_update(state, update_tx, true).await;

    // Fetch lyrics immediately (synchronously) and update state
    // This may perform network IO; it's executed inside the event task.
    let _ = fetch_and_update_lyrics(&meta, state, debug_log, providers, Some(&service)).await;
    // After fetching, send another forced update to refresh UI
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