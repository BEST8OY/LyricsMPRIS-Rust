//! Event types and event handling for the central pool loop.

// --- Imports ---
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};

// --- Event Types ---
/// Events that can be sent to the event loop.
#[derive(Debug)]
pub enum Event {
    /// Player metadata, position, and track change flag
    PlayerUpdate(TrackMetadata, f64, bool),
    /// Shutdown event
    Shutdown,
}

// --- Update Helpers ---
/// Send an update if state has changed or if forced.
pub async fn update_and_maybe_send(
    state: &StateBundle,
    update_tx: &mpsc::Sender<Update>,
    force: bool,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_SENT_VERSION: AtomicU64 = AtomicU64::new(0);
    let version = state.version;
    let last_version = LAST_SENT_VERSION.load(Ordering::Relaxed);
    if !force && version == last_version {
        return;
    }
    let update = Update {
        lines: state.lyric_state.lines.clone(),
        index: state.lyric_state.index,
        err: state.player_state.err.as_ref().map(|e| e.to_string()),
        version,
        playing: state.player_state.playing,
        artist: state.player_state.artist.clone(),
        title: state.player_state.title.clone(),
        album: state.player_state.album.clone(),
    };
    if force || !update.lines.is_empty() || update.err.is_some() {
        let _ = update_tx.send(update).await;
        LAST_SENT_VERSION.store(version, Ordering::Relaxed);
    }
}

// --- Lyrics Fetching & State Update ---
/// Try to load lyrics from the DB and update state. Returns true if found.
async fn try_load_from_db_and_update(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    db: &Arc<Mutex<LyricsDB>>,
) -> bool {
    let guard = db.lock().await;
    if let Some(synced) = guard.get(&meta.artist, &meta.title) {
        state.update_lyrics(crate::lyrics::parse_synced_lyrics(&synced), meta, None);
        return true;
    }
    state.update_lyrics(Vec::new(), meta, None);
    false
}

/// Fetch lyrics from API, update state, and optionally cache in DB.
async fn fetch_and_update_from_api(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    debug_log: bool,
) {
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title).await {
        Ok(synced) if !synced.is_empty() => {
            state.update_lyrics(crate::lyrics::parse_synced_lyrics(&synced), meta, None);
            if let Some((db, path)) = db.zip(db_path) {
                let mut guard = db.lock().await;
                guard.insert(&meta.artist, &meta.title, &synced);
                let _ = guard.save(path);
            }
        }
        Ok(_) => {
            state.update_lyrics(Vec::new(), meta, None);
        }
        Err(e) => {
            if debug_log {
                eprintln!("[LyricsMPRIS] API error: {}", e);
            }
            state.update_lyrics(Vec::new(), meta, Some(e.to_string()));
        }
    }
}

/// Helper: Get the current playback position from MPRIS
async fn get_current_position(config: Option<&crate::Config>) -> f64 {
    match crate::mpris::get_current_player(config).await {
        Ok(Some(player)) => player.position_seconds(),
        _ => 0.0,
    }
}

/// Helper: Update all player state fields
fn update_player_state_fields(state: &mut StateBundle, meta: &TrackMetadata, position: f64, service: Option<String>, err: Option<String>) {
    state.player_state.title = meta.title.clone();
    state.player_state.artist = meta.artist.clone();
    state.player_state.album = meta.album.clone();
    state.player_state.position = position;
    state.player_state.player_service = service;
    state.player_state.err = err;
}

/// Fetch lyrics from DB or API, update state, and send update.
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    position: f64,
    debug_log: bool,
    update_tx: &mpsc::Sender<Update>,
) {
    let mut updated = false;
    if let Some(db) = db {
        if try_load_from_db_and_update(meta, state, db).await {
            updated = true;
        }
    }
    if !updated {
        fetch_and_update_from_api(meta, state, db, db_path, debug_log).await;
    }
    // Re-query the current position after lyrics are loaded
    let fresh_position = get_current_position(None).await;
    state.update_index(fresh_position);
    state.player_state.reset_position_cache(fresh_position);
    update_and_maybe_send(state, update_tx, true).await;
}

/// Process a single event, updating state and sending updates as needed.
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    mpris_config: &crate::Config,
) {
    match event {
        Event::PlayerUpdate(meta, position, is_track_change) => {
            let changed = state.player_state.has_changed(&meta);
            if is_track_change && changed {
                // Clear lyrics immediately on track change
                state.clear_lyrics();
                update_and_maybe_send(state, update_tx, true).await;
                state.player_state.player_service = None;
                let service = state.player_state.player_service.clone().unwrap_or_default();
                *latest_meta = Some((meta.clone(), position, service));
                state.player_state.reset_position_cache(position);
            }
            let prev_playing = state.player_state.playing;
            let playing = matches!(crate::mpris::get_playback_status(Some(mpris_config)).await.unwrap_or_default().as_str(), "Playing");
            state.player_state.update_playback_dbus(playing, position);
            let updated = state.update_index(state.player_state.estimate_position());
            if changed {
                // Don't send another clear here, already sent above
                return;
            }
            if prev_playing != playing {
                update_and_maybe_send(state, update_tx, true).await;
                return;
            }
            if updated {
                update_and_maybe_send(state, update_tx, false).await;
            }
        }
        Event::Shutdown => {
            update_and_maybe_send(state, update_tx, true).await;
        }
    }
}

/// Poll the player state, update lyrics if needed, and send updates.
pub async fn handle_poll(
    state: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    mpris_config: &crate::Config,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    let mut sent = false;
    if let Some((meta, _old_position, service)) = latest_meta.take() {
        // Query the latest position from the player after track change
        let position = get_current_position(Some(mpris_config)).await;
        fetch_and_update_lyrics(&meta, state, db, db_path, position, mpris_config.debug_log, update_tx).await;
        update_player_state_fields(
            state,
            &meta,
            position,
            if !service.is_empty() { Some(service) } else { None },
            None,
        );
        sent = true;
    }
    if state.player_state.playing {
        let position = state.player_state.estimate_position();
        state.player_state.position = position;
        let changed = state.update_index(position);
        if changed || !sent {
            update_and_maybe_send(state, update_tx, false).await;
        }
    }
    if !sent && state.player_state.err.is_some() {
        update_and_maybe_send(state, update_tx, true).await;
    }
}
