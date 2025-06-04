// pool.rs: Central event loop for polling and event-based updates

use crate::lyrics::LyricLine;
use crate::mpris;
use crate::lyricsdb::LyricsDB;
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use tokio::time::{Duration};

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

impl PlayerState {
    fn update_from_metadata(&mut self, meta: &crate::mpris::TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.position = 0.0;
        self.err = None;
    }
    fn update_playback(&mut self, playing: bool, position: f64) {
        self.playing = playing;
        self.position = position;
    }
    fn has_changed(&self, meta: &crate::mpris::TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }
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
    fn update_lines(&mut self, lines: Vec<LyricLine>, index: usize) {
        self.lines = lines;
        self.index = index;
    }
    fn update_index(&mut self, new_index: usize) -> bool {
        if new_index != self.index {
            self.index = new_index;
            true
        } else {
            false
        }
    }
}

// Helper to try loading lyrics from DB
async fn try_load_from_db(
    meta: &crate::mpris::TrackMetadata,
    lyric_state: &mut LyricState,
    last_unsynced: &mut Option<String>,
    state: &mut PlayerState,
    db: &Arc<Mutex<LyricsDB>>,
) -> bool {
    let guard = db.lock().await;
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
        true
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
        false
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
                let mut guard = db.lock().await;
                guard.insert(&meta.artist, &meta.title, &synced);
                let _ = guard.save(path);
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
        if try_load_from_db(meta, lyric_state, last_unsynced, state, db).await {
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
    player_state: &mut PlayerState,
    meta: &crate::mpris::TrackMetadata,
    err: Option<String>,
) {
    lyric_state.update_lines(lines, index);
    *last_unsynced = unsynced;
    player_state.err = err;
    player_state.update_from_metadata(meta);
}

/// Helper to update lyric index and send UI update if needed
async fn update_lyric_index_and_send(
    update_tx: &mpsc::Sender<Update>,
    lyric_state: &mut LyricState,
    player_state: &PlayerState,
    last_unsynced: &Option<String>,
    new_index: usize,
) {
    if lyric_state.update_index(new_index) {
        send_update(update_tx, lyric_state, player_state, last_unsynced).await;
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
    let mut player_state = PlayerState::default();
    let mut lyric_state = LyricState::default();
    let mut last_unsynced: Option<String> = None;
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let latest_meta = Arc::new(Mutex::new(None));
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
            maybe_event = event_rx.recv() => {
                if let Some((meta, position, is_track_change)) = maybe_event {
                    handle_event_rx(
                        &update_tx,
                        &mut lyric_state,
                        &mut player_state,
                        &mut last_unsynced,
                        &latest_meta,
                        meta,
                        position,
                        is_track_change
                    ).await;
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                handle_poll_interval(
                    &update_tx,
                    &mut lyric_state,
                    &mut player_state,
                    &mut last_unsynced,
                    &latest_meta,
                    db.as_ref(),
                    db_path.as_deref()
                ).await;
            }
        }
    }
}

// Handles event_rx branch
async fn handle_event_rx(
    update_tx: &mpsc::Sender<Update>,
    lyric_state: &mut LyricState,
    player_state: &mut PlayerState,
    last_unsynced: &mut Option<String>,
    latest_meta: &Arc<Mutex<Option<(crate::mpris::TrackMetadata, f64)>>>,
    meta: crate::mpris::TrackMetadata,
    position: f64,
    is_track_change: bool,
) {
    let changed = player_state.has_changed(&meta);
    if is_track_change && changed {
        let mut guard = latest_meta.lock().await;
        *guard = Some((meta.clone(), position));
    }
    player_state.update_playback(true, position);
    let new_index = lyric_state.get_index(player_state.position);
    if changed {
        lyric_state.index = new_index;
        send_update(update_tx, lyric_state, player_state, last_unsynced).await;
    } else {
        update_lyric_index_and_send(update_tx, lyric_state, player_state, last_unsynced, new_index).await;
    }
}

// Handles poll_interval branch
async fn handle_poll_interval(
    update_tx: &mpsc::Sender<Update>,
    lyric_state: &mut LyricState,
    player_state: &mut PlayerState,
    last_unsynced: &mut Option<String>,
    latest_meta: &Arc<Mutex<Option<(crate::mpris::TrackMetadata, f64)>>>,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
) {
    let mut guard = latest_meta.lock().await;
    if let Some((meta, position)) = guard.take() {
        fetch_and_update_lyrics(&meta, lyric_state, last_unsynced, player_state, db, db_path, position).await;
        send_update(update_tx, lyric_state, player_state, last_unsynced).await;
    }
    let meta = mpris::get_metadata().await.unwrap_or_default();
    let playing = matches!(mpris::get_playback_status().await.as_deref(), Ok("Playing"));
    let position = mpris::get_position().await.unwrap_or(0.0);
    let changed = player_state.has_changed(&meta);
    player_state.update_playback(playing, position);
    let new_index = lyric_state.get_index(player_state.position);
    if changed {
        lyric_state.index = new_index;
        send_update(update_tx, lyric_state, player_state, last_unsynced).await;
    } else {
        update_lyric_index_and_send(update_tx, lyric_state, player_state, last_unsynced, new_index).await;
    }
    if player_state.err.is_some() || last_unsynced.is_some() {
        send_update(update_tx, lyric_state, player_state, last_unsynced).await;
    }
}
