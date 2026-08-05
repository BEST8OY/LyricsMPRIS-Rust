//! Event processing module for MPRIS player state changes.
//!
//! This module handles all player events (track changes, seeks, playback state updates)
//! and coordinates lyrics fetching with state updates.
//!
//! # Architecture
//!
//! - [`Event`]: Top-level event types (MPRIS, Shutdown)
//! - [`MprisEvent`]: Player-specific events (updates, seeks)
//! - Update tracking: Avoids redundant UI updates using atomic version tracking
//! - Lyrics fetching: Async provider coordination with fallback logic
//!
//! # Flow
//!
//! 1. MPRIS event arrives (new track, seek, position update)
//! 2. State is updated (player metadata, position, lyrics)
//! 3. UI update is sent (if state changed meaningfully)

use crate::mpris::TrackMetadata;
use crate::state::{Provider, StateBundle, Update};
use tokio::sync::mpsc;

// ============================================================================
// Event Types
// ============================================================================

/// Context for handling new track events.
struct NewTrackContext<'a> {
    meta: TrackMetadata,
    position: f64,
    service: String,
    playback_status: Option<String>,
    state: &'a mut StateBundle,
    update_tx: &'a mpsc::Sender<Update>,
    providers: &'a [String],
}

/// Events originating from MPRIS player interface.
///
/// These events represent changes in the media player that require
/// state updates and potentially UI refreshes.
#[derive(Debug, Clone)]
pub enum MprisEvent {
    /// Full player state update with metadata, position, and service name.
    ///
    /// Fired when:
    /// - A new track starts playing
    /// - Player metadata changes
    /// - Periodic polling detects state changes
    PlayerUpdate(TrackMetadata, f64, String),

    /// Seek event when user scrubs through track.
    ///
    /// Fired when:
    /// - User manually seeks to a different position
    /// - Player jumps to a specific timestamp
    Seeked(TrackMetadata, f64, String),
}

/// Top-level events processed by the main event loop.
#[derive(Debug)]
pub enum Event {
    /// MPRIS player event
    Mpris(MprisEvent),
    /// Shutdown signal (graceful termination)
    Shutdown,
}

// ============================================================================
// Update Sending
// ============================================================================

/// Sends an update to the UI channel when appropriate.
///
/// This function:
/// 1. Checks if an update is needed
/// 2. Creates an immutable snapshot
/// 3. Sends to the channel
/// 4. Marks state as sent (on success)
///
/// # Arguments
///
/// * `state` - Current application state
/// * `update_tx` - Channel to send updates through
/// * `force` - If true, bypasses change detection
///
/// # Errors
///
/// If the channel is closed, the update is silently dropped (receiver is gone).
pub async fn send_update(state: &mut StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
    if !state.should_send_update(force) {
        return;
    }

    let update = state.create_update();

    if update_tx.send(update).await.is_ok() {
        state.mark_state_sent();
    }
}

// ============================================================================
// Lyrics Fetching
// ============================================================================

/// Result of a lyrics fetch attempt from a single provider.
///
/// This enum classifies failures as transient (retry with next provider)
/// or non-transient (stop trying and report error).
enum FetchResult {
    /// Lyrics fetched successfully
    Success,
    /// Transient error (no lyrics found, network issue) - try next provider
    Transient,
    /// Non-transient error (API error, parse error) - stop trying
    NonTransient(crate::lyrics::LyricsError),
}

/// Attempts to fetch lyrics from a single provider by name.
///
/// # Returns
///
/// - `Success` if lyrics were fetched and stored
/// - `Transient` if the provider didn't have lyrics or had a recoverable error
/// - `NonTransient` if a fatal error occurred
async fn try_provider(
    provider: &str,
    meta: &TrackMetadata,
    state: &mut StateBundle,
) -> FetchResult {
    match provider {
        "lrclib" => try_lrclib(meta, state).await,
        "musixmatch" => try_musixmatch(meta, state).await,
        _ => {
            // Unknown provider - treat as transient to continue to next
            FetchResult::Transient
        }
    }
}

/// Stores fetched lyrics in the database cache.
///
/// Helper to reduce duplication across provider implementations.
async fn store_lyrics_in_cache(
    meta: &TrackMetadata,
    raw: Option<String>,
    format: crate::lyrics::database::LyricsFormat,
    provider_isrc: Option<&str>,
    provider_spotify_id: Option<&str>,
    provider_itunes_id: Option<&str>,
) {
    if let Some(raw_text) = raw {
        let isrc = provider_isrc.or(meta.isrc.as_deref());
        let spotify_id = provider_spotify_id.or(meta.spotify_id.as_deref());
        let itunes_id = provider_itunes_id.or(meta.itunes_id.as_deref());

        crate::lyrics::database::store_in_database(
            &meta.artist,
            &meta.title,
            &meta.album,
            meta.length,
            format,
            raw_text,
            isrc,
            spotify_id,
            itunes_id,
        )
        .await;
    }
}

/// Fetches lyrics from LRCLIB.
///
/// Network errors are treated as transient to allow fallback to other providers.
async fn try_lrclib(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_lrclib(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
    )
    .await
    {
        Ok((lines, raw, ids)) if !lines.is_empty() => {
            state.update_lyrics(lines, meta, None, Some(Provider::Lrclib));
            store_lyrics_in_cache(
                meta,
                raw,
                crate::lyrics::database::LyricsFormat::Lrclib,
                ids.track_isrc.as_deref(),
                ids.track_spotify_id.as_deref(),
                ids.track_itunes_id.as_deref(),
            )
            .await;
            FetchResult::Success
        }
        Ok(_) => FetchResult::Transient,
        Err(crate::lyrics::LyricsError::Network(_)) => FetchResult::Transient,
        Err(e) => FetchResult::NonTransient(e),
    }
}

/// Maps a Provider enum to the corresponding database LyricsFormat.
fn provider_to_db_format(provider: Provider) -> crate::lyrics::database::LyricsFormat {
    match provider {
        Provider::Lrclib => crate::lyrics::database::LyricsFormat::Lrclib,
        Provider::MusixmatchRichsync => crate::lyrics::database::LyricsFormat::Richsync,
        Provider::MusixmatchSubtitles => crate::lyrics::database::LyricsFormat::Subtitles,
    }
}

/// Fetches lyrics from Musixmatch.
///
/// Automatically detects whether the response is Richsync or Subtitles format.
/// Network errors are treated as transient.
async fn try_musixmatch(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.spotify_id.as_deref(),
        meta.isrc.as_deref(),
        meta.itunes_id.as_deref(),
    )
    .await
    {
        Ok((lines, raw, ids)) if !lines.is_empty() => {
            let provider = determine_musixmatch_provider(&lines, &raw);
            state.update_lyrics(lines, meta, None, Some(provider));

            let format = provider_to_db_format(provider);
            store_lyrics_in_cache(
                meta,
                raw,
                format,
                ids.track_isrc.as_deref(),
                ids.track_spotify_id.as_deref(),
                ids.track_itunes_id.as_deref(),
            )
            .await;

            FetchResult::Success
        }
        Ok(_) => FetchResult::Transient,
        Err(crate::lyrics::LyricsError::Network(_)) => FetchResult::Transient,
        Err(e) => FetchResult::NonTransient(e),
    }
}

/// Determines which Musixmatch format was returned.
///
/// Richsync format includes word-level timestamps, while Subtitles format
/// only has line-level timestamps.
fn determine_musixmatch_provider(
    lines: &[crate::lyrics::LyricLine],
    raw: &Option<String>,
) -> Provider {
    let has_words = lines.iter().any(|l| l.words.is_some());
    let is_richsync = raw
        .as_deref()
        .is_some_and(|r| r.starts_with(";;richsync=1"));

    if has_words || is_richsync {
        Provider::MusixmatchRichsync
    } else {
        Provider::MusixmatchSubtitles
    }
}

/// Determines provider type from raw lyrics format.
///
/// Used when retrieving lyrics from the database cache.
/// Detects based on JSON structure since raw is now the original JSON.
///
/// # Format Detection
///
/// - **Richsync**: `[{"ts":29.26,"te":31.597,"l":[{"c":"Have","o":0}...],"x":"text"}...]`
///   - Has `"ts"`, `"te"`, `"l"`, or `"words"` fields
///   - Contains word-level timing data
///
/// - **Subtitles**: `[{"text":"lyrics","time":{"total":29.26,"minutes":0,"seconds":29,"hundredths":26}}...]`
///   - Has `"time"` object with `"total"`, `"minutes"`, `"seconds"` fields
///   - Line-level timing only
///
/// - **LRC**: `[00:29.26]Have you got colour in your cheeks?`
///   - Plain text with timestamp markers
fn detect_provider_from_raw(raw: &Option<String>) -> Option<Provider> {
    raw.as_deref().map(|text| {
        let trimmed = text.trim_start();
        if trimmed.starts_with("[{") {
            // JSON array - distinguish between richsync and subtitles
            // Richsync has word-level timing: "l":[...] or "words":[...]
            // Subtitles has line-level timing: "time":{"total":...}
            if trimmed.contains("\"ts\":")
                || trimmed.contains("\"l\":[")
                || trimmed.contains("\"words\":[")
            {
                Provider::MusixmatchRichsync
            } else if trimmed.contains("\"time\":{") {
                Provider::MusixmatchSubtitles
            } else {
                // Unknown JSON format, default to subtitles
                Provider::MusixmatchSubtitles
            }
        } else if trimmed.starts_with('[') {
            // LRC format starts with [MM:SS.CC]
            Provider::Lrclib
        } else {
            // Default to LRCLIB
            Provider::Lrclib
        }
    })
}

/// Attempts to fetch lyrics from the database cache.
///
/// Returns `true` if lyrics were found and loaded successfully.
async fn try_database(meta: &TrackMetadata, state: &mut StateBundle) -> bool {
    let Some(db_result) = crate::lyrics::database::fetch_from_database(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.isrc.as_deref(),
        meta.spotify_id.as_deref(),
        meta.itunes_id.as_deref(),
    )
    .await
    else {
        return false;
    };

    match db_result {
        Ok((lines, raw, _ids)) if !lines.is_empty() => {
            let provider = detect_provider_from_raw(&raw);
            let line_count = lines.len();
            state.update_lyrics(lines, meta, None, provider);

            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                lines = line_count,
                "Database cache hit"
            );

            true
        }
        Ok(_) => {
            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                "Empty lyrics in database cache"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                title = %meta.title,
                artist = %meta.artist,
                error = %e,
                "Failed to parse cached lyrics"
            );
            false
        }
    }
}

/// Fetches lyrics from all configured providers in order.
///
/// Stops on the first successful fetch or non-transient error.
///
/// # Behavior
///
/// 1. Check database first
/// 2. Try each provider in order
/// 3. On success: update state and return
/// 4. On transient error: try next provider
/// 5. On non-transient error: log, update state with error, return
/// 6. If all fail: update state with empty lyrics
async fn fetch_api_lyrics(meta: &TrackMetadata, state: &mut StateBundle, providers: &[String]) {
    // Try database cache first
    if try_database(meta, state).await {
        return;
    }

    // Database miss - try external providers
    for provider in providers {
        match try_provider(provider, meta, state).await {
            FetchResult::Success => return,
            FetchResult::Transient => continue,
            FetchResult::NonTransient(err) => {
                tracing::warn!(
                    provider = %provider,
                    error = %err,
                    track = %meta.title,
                    artist = %meta.artist,
                    "Provider failed to fetch lyrics"
                );
                state.update_lyrics(Vec::new(), meta, Some(err.to_string()), None);
                return;
            }
        }
    }

    // No provider succeeded - update with empty lyrics
    state.update_lyrics(Vec::new(), meta, None, None);
}

/// Fetches a fresh position from the player or estimates it.
///
/// Falls back to estimation if D-Bus query fails or no service is provided.
async fn fetch_fresh_position(service: Option<&str>, state: &StateBundle) -> f64 {
    let Some(svc) = service else {
        let estimated = state.player_state.estimate_position();
        tracing::debug!(
            position = estimated,
            "Using estimated position (no service)"
        );
        return estimated;
    };

    match crate::mpris::playback::get_position(svc).await {
        Ok(pos) => {
            tracing::debug!(
                service = %svc,
                position = pos,
                "Fetched fresh position from D-Bus"
            );
            pos
        }
        Err(e) => {
            let estimated = state.player_state.estimate_position();
            tracing::warn!(
                service = %svc,
                error = %e,
                position = estimated,
                "Failed to fetch position, using estimation"
            );
            estimated
        }
    }
}

/// Fetches lyrics and updates position atomically.
///
/// This is the main entry point for lyrics fetching. It:
/// 1. Fetches lyrics from providers
/// 2. Gets fresh position from player
/// 3. Updates lyric index
/// 4. Updates player position
///
/// # Returns
///
/// The fresh position (either from D-Bus or estimated).
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    providers: &[String],
    service: Option<&str>,
) -> f64 {
    let position_before = state.player_state.estimate_position();
    let start_time = std::time::Instant::now();

    fetch_api_lyrics(meta, state, providers).await;

    let fetch_duration = start_time.elapsed();
    let position = fetch_fresh_position(service, state).await;
    let position_change = position - position_before;

    // Note: position_change can be negative if user seeked backward during fetch,
    // or much larger than fetch_duration if user seeked forward.
    // It only represents actual time drift when no seeking occurred.
    tracing::debug!(
        position_before,
        position_after = position,
        position_change,
        fetch_duration = ?fetch_duration,
        "Position updated after lyrics fetch"
    );

    state.update_index(position);
    state.player_state.set_position(position);

    position
}

// ============================================================================
// Event Processing
// ============================================================================

/// Processes a single event from the event loop.
///
/// This is the main entry point for event handling. It dispatches to
/// specialized handlers based on event type.
///
/// # Event Types
///
/// - `Event::Mpris`: Player state change (update, seek)
/// - `Event::Shutdown`: Graceful shutdown signal
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    providers: &[String],
) {
    match event {
        Event::Mpris(ev) => handle_mpris_event(ev, state, update_tx, providers).await,
        Event::Shutdown => send_update(state, update_tx, true).await,
    }
}

/// Handles MPRIS events (player updates and seeks).
///
/// This function orchestrates different behaviors based on:
/// - Event type (PlayerUpdate vs Seeked)
/// - Player availability (empty service = no player)
/// - Player state (Stopped = treat as no player)
/// - Track changes (new track triggers lyrics fetch)
///
/// # Flow
///
/// 1. Extract event data (metadata, position, service)
/// 2. Handle no-player cases (empty service, stopped status)
/// 3. Detect new tracks and fetch lyrics
/// 4. Handle seeks with forced updates
/// 5. Handle position/playback updates
async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
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

    // Only fetch playback status for full updates (optimization)
    let playback_status = if is_full_update {
        get_playback_status(&service).await
    } else {
        None
    };

    // If the player reported 'Stopped' on a full update, treat as no player
    if is_full_update && playback_status.as_deref() == Some("Stopped") {
        handle_no_player(state, update_tx).await;
        return;
    }

    // New track detection on full updates
    if is_full_update && state.player_state.has_changed(&meta) {
        handle_new_track(NewTrackContext {
            meta,
            position,
            service,
            playback_status,
            state,
            update_tx,
            providers,
        })
        .await;
        return;
    }

    // For seek events, filter initialization Seeked signals.
    //
    // When a track starts, Spotify sends a Seeked signal whose position is
    // nearly identical to the current playback position (a "position echo").
    // We ignore it only when BOTH conditions hold:
    //   1. It arrived within 0.5s of lyrics being loaded (start-of-track window)
    //   2. The seek position is within 2.0s of our current estimate
    //      (i.e. not a real user seek that moves position significantly)
    //
    // A real user seek during this window — e.g. immediately jumping to 60s —
    // will have a large position delta and will be processed normally.
    if !is_full_update {
        if state.player_state.title == meta.title
            && state.player_state.artist == meta.artist
            && state.has_lyrics()
            && let Some(loaded_at) = state.lyrics_loaded_at
        {
            let elapsed = loaded_at.elapsed().as_secs_f64();
            let current_position = state.player_state.estimate_position();
            let position_delta = (position - current_position).abs();

            if elapsed < 0.5 && position_delta < 2.0 {
                tracing::debug!(
                    seek_position = position,
                    current_position,
                    position_delta,
                    time_since_load = elapsed,
                    "Ignoring initialization Seeked echo"
                );
                return;
            }
        }

        // Legitimate seek event - update position immediately
        state.player_state.set_position(position);
        state.update_index(position);
        send_update(state, update_tx, true).await;
        return;
    }

    // Position/playback state update (for full updates)
    handle_state_update(position, playback_status, state, update_tx).await;
}

/// Clears state when no player is active.
///
/// Called when:
/// - Player service is empty
/// - Player status is "Stopped"
/// - Player disconnects
async fn handle_no_player(state: &mut StateBundle, update_tx: &mpsc::Sender<Update>) {
    state.clear_lyrics();
    state.player_state = Default::default();
    send_update(state, update_tx, true).await;
}

/// Handles detection of a new track.
///
/// This function orchestrates the multi-step process of responding to a track change:
/// 1. Clear old lyrics
/// 2. Update playback state
/// 3. Notify UI immediately (shows track info even before lyrics load)
/// 4. Fetch lyrics from providers
/// 5. Notify UI again with lyrics
///
/// # Performance Note
///
/// Lyrics fetching is done synchronously within the event handler to ensure
/// state consistency. The UI is updated before and after fetching to provide
/// immediate feedback.
async fn handle_new_track(ctx: NewTrackContext<'_>) {
    let NewTrackContext {
        meta,
        position: _event_position, // Ignored - often stale from previous track
        service,
        playback_status,
        state,
        update_tx,
        providers,
    } = ctx;

    state.clear_lyrics();

    // Update metadata immediately so first update has correct track info
    state.player_state.update_from_metadata(&meta);

    // IMPORTANT: On track changes, the position from the MPRIS event is often stale
    // (still from the previous track). We'll fetch a fresh position after lyrics.
    // Set position to 0 first to establish a clean anchor point.
    state.player_state.set_position(0.0);

    if let Some(status) = playback_status {
        let playing = status == "Playing";
        if playing {
            state.player_state.start_playing();
        } else {
            state.player_state.pause();
        }
    }

    // Notify UI immediately that a new track started (lyrics may follow)
    send_update(state, update_tx, true).await;

    // Fetch lyrics synchronously and update state.
    // This will also fetch a FRESH position from D-Bus, avoiding the stale
    // event position from the previous track.
    let _ = fetch_and_update_lyrics(&meta, state, providers, Some(&service)).await;

    // After fetching, send another forced update to refresh UI with lyrics
    send_update(state, update_tx, true).await;
}

/// Handles position and playback state updates.
///
/// This function:
/// 1. Updates playback state (playing/paused + position)
/// 2. Recalculates active lyric line index
/// 3. Sends UI update if meaningful change occurred
///
/// # Change Detection
///
/// Updates are sent only if:
/// - Playing state changed (play ↔ pause)
/// - Active lyric line changed
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

    // Update lyric index based on current position
    let current_position = state.player_state.estimate_position();
    let changed_index = state.update_index(current_position);

    // Send update if meaningful change occurred
    let playing_changed = prev_playing != state.player_state.playing;
    if playing_changed || changed_index {
        send_update(state, update_tx, false).await;
    }
}

/// Fetches playback status from the player via D-Bus.
///
/// Returns `None` if the query fails or returns an empty string.
async fn get_playback_status(service: &str) -> Option<String> {
    crate::mpris::get_playback_status(service)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}
