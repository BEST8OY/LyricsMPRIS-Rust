// event.rs: Event types and event handling helpers for pool

use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

// --- Event Types ---
/// Events that can be sent to the event loop.
#[derive(Debug)]
pub enum Event {
    PlayerUpdate(TrackMetadata, f64, bool, bool), // (meta, pos, is_track_change, is_playing)
    Shutdown,
}

// --- Update Helpers ---
/// Send an update if state has changed or if forced.
pub async fn update_and_maybe_send(state: &StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
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
    };
    if force || !update.lines.is_empty() || update.err.is_some() {
        let _ = update_tx.send(update).await;
        LAST_SENT_VERSION.store(version, Ordering::Relaxed);
    }
}

// --- Lyrics Fetching & State Update ---
/// Try to load lyrics from the DB and update state. Returns true if found.
pub async fn try_load_from_db_and_update(meta: &TrackMetadata, state: &mut StateBundle, db: &Arc<Mutex<LyricsDB>>) -> bool {
    let guard = db.lock().await;
    if let Some(synced) = guard.get(&meta.artist, &meta.title) {
        state.update_lyrics(crate::lyrics::parse_synced_lyrics(&synced), meta, None);
        return true;
    }
    state.update_lyrics(Vec::new(), meta, None);
    false
}

/// Fetch lyrics from API, update state, and optionally cache in DB.
pub async fn fetch_and_update_from_api(
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
    state.lyric_state.index = state.lyric_state.get_index(position);
    update_and_maybe_send(state, update_tx, true).await;
}

// --- Event Processing ---
/// Process a single event, updating state and sending updates as needed.
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, bool)>,
    _mpris_config: &crate::Config,
) {
    match event {
        Event::PlayerUpdate(meta, position, is_track_change, is_playing) => {
            let changed = state.player_state.has_changed(&meta);
            if is_track_change && changed {
                *latest_meta = Some((meta.clone(), position, is_playing));
            }
            let prev_playing = state.player_state.playing;
            state.update_playback(is_playing, position);
            let updated = state.update_index(state.player_state.position);
            if changed {
                state.clear_lyrics();
                update_and_maybe_send(state, update_tx, true).await;
                return;
            }
            // Force update if playing/paused state changed
            if prev_playing != is_playing {
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
    latest_meta: &mut Option<(TrackMetadata, f64, bool)>,
) {
    // removed unused sent variable
    if let Some((meta, position, playing)) = latest_meta.take() {
        fetch_and_update_lyrics(&meta, state, db, db_path, position, mpris_config.debug_log, update_tx).await;
        state.update_playback(playing, position);
        let changed = state.has_player_changed(&meta);
        let updated = state.update_index(state.player_state.position);
        if changed || updated {
            update_and_maybe_send(state, update_tx, true).await;
        }
        if state.player_state.err.is_some() {
            update_and_maybe_send(state, update_tx, true).await;
        }
        // removed unused sent variable
    } else {
        // Only poll D-Bus if we have no recent event-driven update
        let meta = crate::mpris::get_metadata(Some(mpris_config)).await.unwrap_or_default();
        let playing = matches!(crate::mpris::get_playback_status(Some(mpris_config)).await.unwrap_or_default().as_str(), "Playing");
        let position = crate::mpris::get_position(Some(mpris_config)).await.unwrap_or(0.0);
        state.update_playback(playing, position);
        let changed = state.has_player_changed(&meta);
        let updated = state.update_index(state.player_state.position);
        if changed || updated {
            update_and_maybe_send(state, update_tx, true).await;
        }
        if state.player_state.err.is_some() {
            update_and_maybe_send(state, update_tx, true).await;
        }
    }
}
