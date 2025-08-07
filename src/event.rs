
//! Event handling for LyricsMPRIS central loop. Clean, robust, and clear logic.

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};

#[derive(Debug)]
pub enum MprisEvent {
    PlayerUpdate(TrackMetadata, f64, String),
    Seeked(TrackMetadata, f64, String),
}

#[derive(Debug)]
pub enum Event {
    Mpris(MprisEvent),
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

/// Fetch lyrics (DB first, then API) and update state. Resync position and send update.
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    debug_log: bool,
    player_service: &str,
) -> f64 {
    let found = if let Some(db) = db {
        try_db_lyrics(meta, state, db).await
    } else {
        false
    };
    if !found {
        fetch_api_lyrics(meta, state, db, db_path, debug_log).await;
    }
    // After fetching, get the most up-to-date position
    let position = crate::mpris::playback::get_position(player_service).await.unwrap_or(0.0);
    state.update_index(position);
    state.player_state.reset_position_cache(position);
    position
}

/// Process an event and update state accordingly.
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

/// Handle MPRIS events: track changes, seeks, etc.
async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    let (meta, position, service, track_changed) = match event {
        MprisEvent::PlayerUpdate(meta, pos, service) => (meta, pos, service, true),
        MprisEvent::Seeked(meta, pos, service) => (meta, pos, service, false),
    };

    let is_new_track = state.player_state.has_changed(&meta);

    if track_changed && is_new_track {
        state.clear_lyrics();
        *latest_meta = Some((meta.clone(), position, service.clone()));
        state.player_state.reset_position_cache(position);
    }

    let prev_playing = state.player_state.playing;
    let playing = matches!(
        crate::mpris::get_playback_status(&service).await.unwrap_or_default().as_str(),
        "Playing"
    );

    state.player_state.update_playback_dbus(playing, position);
    let updated = state.update_index(state.player_state.estimate_position());

    if prev_playing != playing || (updated && !is_new_track) {
        send_update(state, update_tx, false).await;
    }
}

/// Handle latest meta update: reload lyrics and sync lyric index to position.
async fn handle_latest_meta_update(
    state: &mut StateBundle,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    debug_log: bool,
    update_tx: &mpsc::Sender<Update>,
) -> bool {
    if let Some((meta, _old_position, service)) = latest_meta.take() {
        // Fetch lyrics for the new track
        let position = fetch_and_update_lyrics(&meta, state, db, db_path, debug_log, &service).await;
        state.update_index(position);
        state.player_state.reset_position_cache(position);
        send_update(state, update_tx, true).await;
        true
    } else {
        false
    }
}

/// Handle position sync: update lyric index, resync if needed, handle song end.
async fn handle_position_sync(
    state: &mut StateBundle,
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
    debug_log: bool,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    let mut should_send_update = false;

    if handle_latest_meta_update(state, latest_meta, db, db_path, debug_log, update_tx).await {
        should_send_update = true;
    }

    if state.player_state.playing {
        if let Some(changed) = handle_position_sync(state).await {
            if changed {
                should_send_update = true;
            }
        }
    }

    if state.player_state.err.is_some() {
        should_send_update = true;
    }

    if should_send_update {
        send_update(state, update_tx, false).await;
    }
}