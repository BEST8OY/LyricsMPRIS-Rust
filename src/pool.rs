// pool.rs: Central event loop for polling and event-based updates

use crate::lyrics::LyricLine;
use crate::lyricsdb::LyricsDB;
use crate::mpris::TrackMetadata;
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use tokio::time::Duration;

// =====================
// Data Structures
// =====================

/// Represents a UI update for lyrics and player state.
#[derive(Debug, Clone, Default)]
pub struct Update {
    pub lines: Vec<LyricLine>,
    pub index: usize,
    pub err: Option<String>,
    pub unsynced: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct PlayerState {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position: f64,
    pub err: Option<String>,
}

impl PlayerState {
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.position = 0.0;
        self.err = None;
    }
    pub fn update_playback(&mut self, playing: bool, position: f64) {
        self.playing = playing;
        self.position = position;
    }
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }
}

#[derive(Debug, Default)]
pub struct LyricState {
    pub lines: Vec<LyricLine>,
    pub index: usize,
}

impl LyricState {
    /// Returns the index of the lyric line for the given playback position.
    pub fn get_index(&self, position: f64) -> usize {
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
    pub fn update_lines(&mut self, lines: Vec<LyricLine>) {
        self.index = 0;
        self.lines = lines;
    }
    pub fn update_index(&mut self, new_index: usize) -> bool {
        if new_index != self.index {
            self.index = new_index;
            true
        } else {
            false
        }
    }
}

pub struct StateBundle {
    pub lyric_state: LyricState,
    pub player_state: PlayerState,
    pub last_unsynced: Option<String>,
}

impl StateBundle {
    pub fn new() -> Self {
        Self {
            lyric_state: LyricState::default(),
            player_state: PlayerState::default(),
            last_unsynced: None,
        }
    }

    pub fn update_playback(&mut self, playing: bool, position: f64) {
        self.player_state.update_playback(playing, position);
    }

    pub fn has_player_changed(&self, meta: &TrackMetadata) -> bool {
        self.player_state.has_changed(meta)
    }

    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.lyric_state.index = 0;
    }

    pub fn update_lyrics(&mut self, lines: Vec<LyricLine>, unsynced: Option<String>, meta: &TrackMetadata, err: Option<String>) {
        self.lyric_state.update_lines(lines);
        self.last_unsynced = unsynced;
        self.player_state.err = err;
        self.player_state.update_from_metadata(meta);
    }

    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        self.lyric_state.update_index(new_index)
    }
}

/// Helper function to send an update if state has changed
async fn update_and_maybe_send(state_bundle: &StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
    // This could be improved to check for actual changes if needed
    let update = Update {
        lines: state_bundle.lyric_state.lines.clone(),
        index: state_bundle.lyric_state.index,
        err: state_bundle.player_state.err.as_ref().map(|e| e.to_string()),
        unsynced: state_bundle.last_unsynced.as_ref().map(|u| u.to_string()),
    };
    if force || !update.lines.is_empty() || update.err.is_some() || update.unsynced.is_some() {
        let _ = update_tx.send(update).await;
    }
}

// =====================
// Internal Helpers
// =====================

async fn try_load_from_db_and_update(
    meta: &TrackMetadata,
    state_bundle: &mut StateBundle,
    db: &Arc<Mutex<LyricsDB>>,
) -> bool {
    let guard = db.lock().await;
    if let Some(synced) = guard.get(&meta.artist, &meta.title) {
        state_bundle.update_lyrics(crate::lyrics::parse_synced_lyrics(&synced), None, meta, None);
        return true;
    }
    state_bundle.update_lyrics(Vec::new(), None, meta, None);
    false
}

async fn fetch_and_update_from_api(
    meta: &TrackMetadata,
    state_bundle: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    debug_log: bool,
) {
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title).await {
        Ok((_plain, synced)) if !synced.is_empty() => {
            state_bundle.update_lyrics(crate::lyrics::parse_synced_lyrics(&synced), None, meta, None);
            if let Some((db, path)) = db.zip(db_path) {
                let mut guard = db.lock().await;
                guard.insert(&meta.artist, &meta.title, &synced);
                let _ = guard.save(path);
            }
        }
        Ok((plain, _)) => {
            let unsynced = if plain.is_empty() { None } else { Some(plain) };
            state_bundle.update_lyrics(Vec::new(), unsynced, meta, None);
        }
        Err(e) => {
            if debug_log {
                eprintln!("[LyricsMPRIS] API error: {}", e);
            }
            state_bundle.update_lyrics(Vec::new(), None, meta, Some(e.to_string()));
        }
    }
}

async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state_bundle: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    position: f64,
    debug_log: bool,
    update_tx: &mpsc::Sender<Update>,
) {
    let mut updated = false;
    if let Some(db) = db {
        if try_load_from_db_and_update(meta, state_bundle, db).await {
            updated = true;
        }
    }
    if !updated {
        fetch_and_update_from_api(meta, state_bundle, db, db_path, debug_log).await;
    }
    state_bundle.lyric_state.index = state_bundle.lyric_state.get_index(position);
    update_and_maybe_send(state_bundle, update_tx, true).await;
}

async fn handle_event(
    meta: TrackMetadata,
    position: f64,
    is_track_change: bool,
    state_bundle: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64)>,
) {
    let changed = state_bundle.has_player_changed(&meta);
    if is_track_change && changed {
        *latest_meta = Some((meta, position));
    }
    state_bundle.update_playback(true, position);
    let updated = state_bundle.update_index(state_bundle.player_state.position);
    if changed {
        state_bundle.clear_lyrics();
        update_and_maybe_send(state_bundle, update_tx, true).await;
        return;
    }
    if updated {
        update_and_maybe_send(state_bundle, update_tx, false).await;
    }
}

async fn handle_poll(
    state_bundle: &mut StateBundle,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    mpris_config: &crate::Config,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64)>,
) {
    let mut sent = false;
    if let Some((meta, position)) = latest_meta.take() {
        fetch_and_update_lyrics(&meta, state_bundle, db, db_path, position, mpris_config.debug_log, update_tx).await;
        sent = true;
    }
    let meta = crate::mpris::get_metadata(Some(mpris_config)).await.unwrap_or_default();
    let playing = matches!(crate::mpris::get_playback_status(Some(mpris_config)).await.unwrap_or_default().as_str(), "Playing");
    let position = crate::mpris::get_position(Some(mpris_config)).await.unwrap_or(0.0);
    let changed = state_bundle.has_player_changed(&meta);
    state_bundle.update_playback(playing, position);
    let updated = state_bundle.update_index(state_bundle.player_state.position);
    if (changed || updated) && !sent {
        update_and_maybe_send(state_bundle, update_tx, true).await;
        sent = true;
    }
    if !sent && (state_bundle.player_state.err.is_some() || state_bundle.last_unsynced.is_some()) {
        update_and_maybe_send(state_bundle, update_tx, true).await;
    }
}

// =====================
// Public API
// =====================

/// Listens for player and lyric updates, sending them to the update channel.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mut shutdown_rx: mpsc::Receiver<()>,
    mpris_config: crate::Config,
) {
    let mut state_bundle = StateBundle::new();
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut latest_meta: Option<(TrackMetadata, f64)> = None;
    let event_tx_clone = event_tx.clone();
    let mpris_config_clone = mpris_config.clone();
    tokio::spawn(async move {
        let _ = crate::mpris::watch_and_handle_events(
            move |meta, pos| {
                let _ = event_tx_clone.try_send((meta, pos, true));
            },
            move |meta, pos| {
                let _ = event_tx.try_send((meta, pos, false));
            },
            Some(&mpris_config_clone),
        ).await;
    });
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            maybe_event = event_rx.recv() => {
                if let Some((meta, position, is_track_change)) = maybe_event {
                    handle_event(meta, position, is_track_change, &mut state_bundle, &update_tx, &mut latest_meta).await;
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                handle_poll(
                    &mut state_bundle,
                    db.as_ref(),
                    db_path.as_deref(),
                    &mpris_config,
                    &update_tx,
                    &mut latest_meta,
                ).await;
            }
        }
    }
}