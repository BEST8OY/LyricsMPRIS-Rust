// pool.rs: Central event loop for polling and event-based updates

use crate::lyrics::LyricLine;
use crate::mpris;
use crate::lyricsdb::LyricsDB;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

/// Represents a UI update for lyrics and player state.
#[derive(Debug, Clone, Default)]
pub struct Update {
    pub lines: Vec<LyricLine>,
    pub index: usize,
    pub err: Option<String>,
    pub unsynced: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PlayerState {
    title: String,
    artist: String,
    album: String,
    playing: bool,
    position: f64,
    err: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LyricState {
    lines: Vec<LyricLine>,
    index: usize,
}

impl LyricState {
    /// Returns the index of the lyric line for the given playback position.
    fn get_index(&self, position: f64) -> usize {
        if self.lines.len() <= 1 {
            return 0;
        }
        // Use binary search for efficiency
        match self.lines.binary_search_by(|line| line.time.partial_cmp(&position).unwrap_or(std::cmp::Ordering::Less)) {
            Ok(idx) => idx,
            Err(0) => 0,
            Err(idx) => idx - 1,
        }
    }

    // fn clear(&mut self) {
    //     self.lines.clear();
    //     self.index = 0;
    // }
}

// Helper to update player state
fn update_player_state(state: &mut PlayerState, meta: &crate::mpris::TrackMetadata) {
    state.title = meta.title.clone();
    state.artist = meta.artist.clone();
    state.album = meta.album.clone();
    state.position = 0.0;
    state.err = None;
}

// Helper to try loading lyrics from DB
fn try_load_from_db(
    meta: &crate::mpris::TrackMetadata,
    lyric_state: &mut LyricState,
    last_unsynced: &mut Option<String>,
    state: &mut PlayerState,
    db: &Arc<Mutex<LyricsDB>>,
) -> bool {
    match db.lock() {
        Ok(guard) => {
            if let Some(synced) = guard.get(&meta.artist, &meta.title) {
                set_lyric_state(
                    lyric_state,
                    crate::lyrics::parse_synced_lyrics(&synced),
                    0,
                    last_unsynced,
                    None,
                    state,
                    meta,
                    None,
                );
                return true;
            } else {
                // Not found in DB, clear state and return false to trigger API fetch
                set_lyric_state(
                    lyric_state,
                    Vec::new(),
                    0,
                    last_unsynced,
                    None,
                    state,
                    meta,
                    None,
                );
                return false;
            }
        }
        Err(e) => {
            eprintln!("[LyricsMPRIS] DB error: {}", e); // Log DB error
            set_lyric_state(
                lyric_state,
                Vec::new(),
                0,
                last_unsynced,
                None,
                state,
                meta,
                Some(format!("DB error: {}", e)),
            );
            return true;
        }
    }
}

// Helper to fetch from API and optionally save to DB
async fn try_fetch_from_api_and_save(
    meta: &crate::mpris::TrackMetadata,
    lyric_state: &mut LyricState,
    last_unsynced: &mut Option<String>,
    state: &mut PlayerState,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
) {
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title).await {
        Ok((_plain, synced)) if !synced.is_empty() => {
            set_lyric_state(
                lyric_state,
                crate::lyrics::parse_synced_lyrics(&synced),
                0,
                last_unsynced,
                None,
                state,
                meta,
                None,
            );
            if let (Some(db), Some(path)) = (db, db_path) {
                if let Ok(mut guard) = db.lock() {
                    guard.insert(&meta.artist, &meta.title, &synced);
                    let _ = guard.save(path);
                }
            }
        }
        Ok((plain, _synced)) => {
            // No synced lyrics found (plain may be empty or not), clear state, do not set err
            set_lyric_state(
                lyric_state,
                Vec::new(),
                0,
                last_unsynced,
                if plain.is_empty() { None } else { Some(plain) },
                state,
                meta,
                None,
            );
        }
        Err(e) => {
            eprintln!("[LyricsMPRIS] API error: {}", e); // Log API error
            set_lyric_state(
                lyric_state,
                Vec::new(),
                0,
                last_unsynced,
                None,
                state,
                meta,
                Some(e.to_string()),
            );
        }
    }
}

/// Attempts to update the lyric state for the given track metadata.
async fn fetch_and_update_lyrics(
    meta: &crate::mpris::TrackMetadata,
    lyric_state: &mut LyricState,
    last_unsynced: &mut Option<String>,
    state: &mut PlayerState,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    position: f64, // <-- add position argument
) {
    if let Some(db) = db {
        if try_load_from_db(meta, lyric_state, last_unsynced, state, db) {
            // Set correct index immediately after loading from DB
            lyric_state.index = lyric_state.get_index(position);
            return;
        }
    }
    try_fetch_from_api_and_save(meta, lyric_state, last_unsynced, state, db, db_path).await;
    // Set correct index after fetching from API
    lyric_state.index = lyric_state.get_index(position);
}

/// Sends an update to the UI channel.
async fn send_update(
    update_tx: &mpsc::Sender<Update>,
    lyric_state: &LyricState,
    state: &PlayerState,
    last_unsynced: &Option<String>,
) {
    let _ = update_tx.send(Update {
        lines: lyric_state.lines.clone(),
        index: lyric_state.index,
        err: state.err.clone(),
        unsynced: last_unsynced.clone(),
    }).await;
}

/// Helper to update lyric and player state in a consistent way
fn set_lyric_state(
    lyric_state: &mut LyricState,
    lines: Vec<LyricLine>,
    index: usize, // unused, always recalculate
    last_unsynced: &mut Option<String>,
    unsynced: Option<String>,
    state: &mut PlayerState,
    meta: &crate::mpris::TrackMetadata,
    err: Option<String>,
) {
    lyric_state.lines = lines;
    lyric_state.index = index;
    *last_unsynced = unsynced;
    state.err = err;
    update_player_state(state, meta);
}

/// Helper to update lyric index and send UI update if needed
async fn update_lyric_index_and_send(
    update_tx: &mpsc::Sender<Update>,
    lyric_state: &mut LyricState,
    state: &PlayerState,
    last_unsynced: &Option<String>,
    new_index: usize,
) {
    if new_index != lyric_state.index {
        lyric_state.index = new_index;
        send_update(update_tx, lyric_state, state, last_unsynced).await;
    }
}

/// Listens for player and lyric updates, sending them to the update channel.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    let mut state = PlayerState::default();
    let mut lyric_state = LyricState::default();
    let mut last_unsynced: Option<String> = None;
    let (event_tx, mut event_rx) = mpsc::channel(8);

    // Rate limit state
    let rate_limit = Duration::from_secs(2);
    let mut last_api_call = Instant::now() - rate_limit;
    let latest_meta = Arc::new(AsyncMutex::new(None));

    // Spawn event-based listener for MPRIS property changes
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        let _ = crate::mpris::watch_and_handle_events(
            move |meta, pos| {
                let _ = event_tx_clone.try_send((meta, pos, true));
            },
            move |meta, pos| {
                let _ = event_tx.try_send((meta, pos, false));
            },
        ).await;
    });

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            Some((meta, position, is_track_change)) = event_rx.recv() => {
                let changed = meta.title != state.title || meta.artist != state.artist || meta.album != state.album;
                if is_track_change && changed {
                    // Save the latest meta to be fetched
                    let mut guard = latest_meta.lock().await;
                    *guard = Some((meta.clone(), position));
                }
                state.playing = true;
                state.position = position;
                let new_index = lyric_state.get_index(state.position);
                if changed {
                    lyric_state.index = new_index;
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                } else {
                    update_lyric_index_and_send(&update_tx, &mut lyric_state, &state, &last_unsynced, new_index).await;
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                // Check if we need to fetch new lyrics (rate-limited)
                let now = Instant::now();
                if now.duration_since(last_api_call) >= rate_limit {
                    let mut guard = latest_meta.lock().await;
                    if let Some((meta, position)) = guard.take() {
                        fetch_and_update_lyrics(&meta, &mut lyric_state, &mut last_unsynced, &mut state, db.as_ref(), db_path.as_deref(), position).await;
                        send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                        last_api_call = Instant::now();
                    }
                }
                // Always update lyric index and UI
                let meta = mpris::get_metadata().await.unwrap_or_default();
                let playing = matches!(mpris::get_playback_status().await.as_deref(), Ok("Playing"));
                let position = mpris::get_position().await.unwrap_or(0.0);
                let changed = meta.title != state.title || meta.artist != state.artist || meta.album != state.album;
                state.playing = playing;
                state.position = position;
                let new_index = lyric_state.get_index(state.position);
                if changed {
                    lyric_state.index = new_index;
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                } else {
                    update_lyric_index_and_send(&update_tx, &mut lyric_state, &state, &last_unsynced, new_index).await;
                }
                if state.err.is_some() || last_unsynced.is_some() {
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                }
            }
        }
    }
}
