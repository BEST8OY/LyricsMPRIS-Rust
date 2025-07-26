
//! Event handling for LyricsMPRIS central loop. Clean, robust, and clear logic.

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};

#[derive(Debug)]
pub enum Event {
    PlayerUpdate {
        meta: TrackMetadata,
        position: f64,
        track_changed: bool,
        player_service: String,
    },
    Shutdown,
}

/// Send an update if state has changed or force is true.
pub async fn send_update(state: &StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
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

/// Try to load lyrics from DB. Returns true if found.
async fn try_db_lyrics(meta: &TrackMetadata, state: &mut StateBundle, db: &Arc<Mutex<LyricsDB>>) -> bool {
    let guard = db.lock().await;
    if let Some(synced) = guard.get(&meta.artist, &meta.title) {
        state.update_lyrics(crate::lyrics::parse_synced_lyrics(&synced), meta, None);
        true
    } else {
        state.update_lyrics(Vec::new(), meta, None);
        false
    }
}

/// Fetch lyrics from API and update state. Save to DB if possible.
async fn fetch_api_lyrics(meta: &TrackMetadata, state: &mut StateBundle, db: Option<&Arc<Mutex<LyricsDB>>>, db_path: Option<&str>, debug_log: bool) {
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

async fn get_position(service: Option<&str>) -> f64 {
    crate::mpris::playback::get_position(service.unwrap_or("")).await.unwrap_or(0.0)
}

fn update_player_state(state: &mut StateBundle, meta: &TrackMetadata, position: f64, service: Option<String>, err: Option<String>) {
    state.player_state.title = meta.title.clone();
    state.player_state.artist = meta.artist.clone();
    state.player_state.album = meta.album.clone();
    state.player_state.length = meta.length;
    state.player_state.position = position;
    state.player_state.player_service = service;
    state.player_state.err = err;
}

/// Fetch lyrics (DB first, then API) and update state. Resync position and send update.
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    debug_log: bool,
    update_tx: &mpsc::Sender<Update>,
) {
    let found = if let Some(db) = db {
        try_db_lyrics(meta, state, db).await
    } else {
        false
    };
    if !found {
        fetch_api_lyrics(meta, state, db, db_path, debug_log).await;
    }
    let position = get_position(None).await;
    state.update_index(position);
    state.player_state.reset_position_cache(position);
    // Removed pending_position_resync logic
    send_update(state, update_tx, true).await;
}

/// Process an event and update state accordingly.
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    match event {
        Event::PlayerUpdate { meta, position, track_changed, player_service } => {
            let changed = state.player_state.has_changed(&meta);
            if track_changed && changed {
                state.clear_lyrics();
                send_update(state, update_tx, true).await;
                state.player_state.player_service = Some(player_service.clone());
                *latest_meta = Some((meta.clone(), position, player_service.clone()));
                state.player_state.reset_position_cache(position);
            }
            let prev_playing = state.player_state.playing;
            let playing = matches!(
                crate::mpris::get_playback_status(&player_service).await.unwrap_or_default().as_str(),
                "Playing"
            );
            state.player_state.update_playback_dbus(playing, position);
            let updated = state.update_index(state.player_state.estimate_position());
            if changed {
                return;
            }
            if prev_playing != playing {
                send_update(state, update_tx, true).await;
                return;
            }
            if updated {
                send_update(state, update_tx, false).await;
            }
        }
        Event::Shutdown => {
            send_update(state, update_tx, true).await;
        }
    }
}

/// Handle latest meta update: reload lyrics and sync lyric index to position.
async fn handle_latest_meta_update(
    state: &mut StateBundle,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    update_tx: &mpsc::Sender<Update>,
) -> bool {
    if let Some((meta, _old_position, service)) = latest_meta.take() {
        let position = crate::mpris::playback::get_position(&service).await.unwrap_or(0.0);
        fetch_and_update_lyrics(&meta, state, db, db_path, false, update_tx).await;
        let lyric_index = state.lyric_state.get_index(position);
        state.lyric_state.update_index(lyric_index);
        state.player_state.reset_position_cache(position);
        // Removed pending_position_resync logic
        send_update(state, update_tx, true).await;
        update_player_state(
            state,
            &meta,
            position,
            if !service.is_empty() { Some(service) } else { None },
            None,
        );
        true
    } else {
        false
    }
}

/// Handle position sync: update lyric index, resync if needed, handle song end.
async fn handle_position_sync(
    state: &mut StateBundle,
    _update_tx: &mpsc::Sender<Update>,
) -> Option<bool> {
    let position = state.player_state.estimate_position();
    state.player_state.position = position;

    // Removed out_of_bounds logic
    Some(state.update_index(position))
}

/// Poll handler: update lyrics, sync position, send updates as needed.
pub async fn handle_poll(
    state: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    let mut sent = handle_latest_meta_update(state, latest_meta, db, db_path, update_tx).await;

    if state.player_state.playing {
        if let Some(changed) = handle_position_sync(state, update_tx).await {
            if changed || !sent {
                send_update(state, update_tx, false).await;
                sent = true;
            }
        }
    }

    if !sent && state.player_state.err.is_some() {
        send_update(state, update_tx, true).await;
    }
}