//! Event handling for LyricsMPRIS central loop.
//!
//! This module receives MPRIS events and drives the `StateBundle` updates.
//! The goals of the overhaul are clarity, safer update sending, and simpler
//! control flow when handling track changes, seeks and polling.

use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update, Provider};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum MprisEvent {
    /// A full player update (new metadata and position); normally emitted on
    /// playback changes or new track starts.
    PlayerUpdate(TrackMetadata, f64, String),
    /// A seek event: metadata is same track, position changed.
    Seeked(TrackMetadata, f64, String),
}

#[derive(Debug)]
pub enum Event {
    Mpris(MprisEvent),
    Shutdown,
}

/// Send an update if state has changed, or if `force` is true.
///
/// We combine the state's `version` with the `playing` bit to produce a
/// monotonic key. The atomically-stored key is updated only when the send
/// actually succeeds to avoid spurious skipping of future updates.
pub async fn send_update(state: &StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_SENT_VERSION: AtomicU64 = AtomicU64::new(0);

    let version = state.version;
    let playing_bit: u64 = if state.player_state.playing { 1 } else { 0 };
    let key = (version << 1) | playing_bit;

    let last_key = LAST_SENT_VERSION.load(Ordering::Relaxed);
    if !force && key == last_key {
        return;
    }

    let update = Update {
        lines: state.lyric_state.lines.clone(),
        index: state.lyric_state.index,
        position: state.player_state.position,
        position_timestamp: Some({
            let now = std::time::SystemTime::now();
            now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
        }),
        err: state.player_state.err.as_ref().map(|e| e.to_string()),
        version,
        playing: state.player_state.playing,
        artist: state.player_state.artist.clone(),
        title: state.player_state.title.clone(),
        album: state.player_state.album.clone(),
        provider: state.provider.clone(),
    };

    // Only send if forced, or we actually have something to show (lines or error).
    if force || !update.lines.is_empty() || update.err.is_some() {
        // Propagate send result; update LAST_SENT_VERSION only on success.
        if update_tx.send(update).await.is_ok() {
            LAST_SENT_VERSION.store(key, Ordering::Relaxed);
        }
    }
}

/// Try the configured providers in order. When a provider returns non-empty
/// lyrics we immediately update the state and return. On non-network errors
/// from providers we treat that as a fatal provider error and update state
/// with the error message.
async fn fetch_api_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    debug_log: bool,
    providers: &[String],
) {
    for prov in providers.iter() {
        match prov.as_str() {
            "lrclib" => {
                match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title, &meta.album, meta.length).await {
                    Ok((lines, _raw)) if !lines.is_empty() => {
                        state.update_lyrics(lines, meta, None, Some(Provider::Lrclib));
                        return;
                    }
                    Ok((_lines, _)) => { /* empty, try next provider */ }
                    Err(e) => {
                        if debug_log {
                            eprintln!("[LyricsMPRIS] lrclib error: {}", e);
                        }
                        match e {
                            crate::lyrics::LyricsError::Network(_) => { /* transient: try next */ }
                            _ => {
                                state.update_lyrics(Vec::new(), meta, Some(e.to_string()), None);
                                return;
                            }
                        }
                    }
                }
            }
            "musixmatch" => {
                match crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
                    &meta.artist,
                    &meta.title,
                    &meta.album,
                    meta.length,
                ).await {
                    Ok((lines, _raw)) if !lines.is_empty() => {
                        let provider_tag = if lines.iter().any(|l| l.words.is_some())
                            || _raw.as_ref().map(|r| r.starts_with(";;richsync=1")).unwrap_or(false)
                        {
                            Some(Provider::MusixmatchRichsync)
                        } else {
                            Some(Provider::MusixmatchSubtitles)
                        };
                        state.update_lyrics(lines, meta, None, provider_tag);
                        return;
                    }
                    Ok((_lines, _)) => { /* empty, try next provider */ }
                    Err(e) => {
                        if debug_log {
                            eprintln!("[LyricsMPRIS] musixmatch error: {}", e);
                        }
                        match e {
                            crate::lyrics::LyricsError::Network(_) => { /* transient: try next */ }
                            _ => {
                                state.update_lyrics(Vec::new(), meta, Some(e.to_string()), None);
                                return;
                            }
                        }
                    }
                }
            }
            other => {
                if debug_log {
                    eprintln!("[LyricsMPRIS] unknown provider: {}", other);
                }
            }
        }
    }

    // No provider found lyrics
    state.update_lyrics(Vec::new(), meta, None, None);
}

/// Fetch lyrics and return the best estimate of current position. We update
/// the state's index and reset the player position cache so playback position
/// estimation continues smoothly after potentially long network calls.
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    debug_log: bool,
    providers: &[String],
) -> f64 {
    fetch_api_lyrics(meta, state, debug_log, providers).await;
    let position = state.player_state.estimate_position();
    state.update_index(position);
    state.player_state.reset_position_cache(position);
    position
}

/// Process a single event.
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    match event {
        Event::Mpris(ev) => handle_mpris_event(ev, state, update_tx, latest_meta).await,
        Event::Shutdown => send_update(state, update_tx, true).await,
    }
}

/// Handle MPRIS events: player updates and seeks.
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

    // If the service string is empty the player probably disappeared.
    if service.is_empty() {
        state.clear_lyrics();
        state.player_state = Default::default();
        send_update(state, update_tx, true).await;
        return;
    }

    let playback_status = crate::mpris::get_playback_status(&service).await.unwrap_or_default();

    if playback_status == "Stopped" {
        // If the player reports stopped, clear and reset state.
        state.clear_lyrics();
        state.player_state = Default::default();
        send_update(state, update_tx, true).await;
        return;
    }

    let is_new_track = state.player_state.has_changed(&meta);

    if is_player_update && is_new_track {
        // New track started: clear old lyrics and defer fetching to poll handler
        // (so we avoid blocking the DBus event path).
        state.clear_lyrics();
        *latest_meta = Some((meta.clone(), position, service.clone()));
        state.player_state.reset_position_cache(position);
        send_update(state, update_tx, true).await; // immediate UI clear
        return;
    }

    // Update playback flags and position
    let prev_playing = state.player_state.playing;
    let playing = playback_status == "Playing";
    state.player_state.update_playback_dbus(playing, position);

    // For seeks we want to resync index immediately. For player updates on the
    // same track we update index and decide whether to notify UI below.
    let changed_index = state.update_index(state.player_state.estimate_position());

    // Send update if playback started/stopped, or index changed while playing.
    if prev_playing != playing || (changed_index && !is_new_track) {
        send_update(state, update_tx, false).await;
    }
}

/// If `latest_meta` contains a pending new-track metadata, fetch lyrics for it
/// and return true if we performed an update.
async fn handle_latest_meta_update(
    state: &mut StateBundle,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    debug_log: bool,
    providers: &[String],
    update_tx: &mpsc::Sender<Update>,
) -> bool {
    if let Some((meta, _old_position, _service)) = latest_meta.take() {
        let position = fetch_and_update_lyrics(&meta, state, debug_log, providers).await;
        state.update_index(position);
        state.player_state.reset_position_cache(position);
        send_update(state, update_tx, true).await;
        return true;
    }
    false
}

/// Update player's cached position and return whether the lyric index changed.
async fn handle_position_sync(state: &mut StateBundle) -> bool {
    let position = state.player_state.estimate_position();
    state.player_state.position = position;
    state.update_index(position)
}

/// Poll handler: called periodically to perform work that should not run in
/// the DBus event path (fetching lyrics, periodic resync, and sending batched updates).
pub async fn handle_poll(
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    debug_log: bool,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    providers: &[String],
) {
    let mut should_send_update = false;

    if handle_latest_meta_update(state, latest_meta, debug_log, providers, update_tx).await {
        should_send_update = true;
    }

    if state.player_state.playing && handle_position_sync(state).await {
        should_send_update = true;
    }

    if state.player_state.err.is_some() {
        should_send_update = true;
    }

    if should_send_update {
        send_update(state, update_tx, false).await;
    }
}
