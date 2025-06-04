// pool.rs: Central event loop for polling and event-based updates

use crate::lyrics::LyricLine;
use crate::mpris;
use crate::lyricsdb::LyricsDB;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Sleep};
use std::pin::Pin;

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
        if position >= self.lines[self.index].time {
            for i in self.index + 1..self.lines.len() {
                if position < self.lines[i].time {
                    return i - 1;
                }
            }
            return self.lines.len() - 1;
        }
        for i in (0..=self.index).rev() {
            if position > self.lines[i].time {
                return i;
            }
        }
        0
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.index = 0;
    }
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
                lyric_state.lines = crate::lyrics::parse_synced_lyrics(&synced);
                lyric_state.index = 0;
                *last_unsynced = None;
                update_player_state(state, meta);
                return true;
            } else {
                lyric_state.clear();
                lyric_state.index = 0;
                *last_unsynced = None;
                update_player_state(state, meta);
                return true;
            }
        }
        Err(e) => {
            lyric_state.clear();
            lyric_state.index = 0;
            *last_unsynced = None;
            state.err = Some(format!("DB error: {}", e));
            update_player_state(state, meta);
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
    db_path: Option<&String>,
) {
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title).await {
        Ok((_, synced)) if !synced.is_empty() => {
            lyric_state.lines = crate::lyrics::parse_synced_lyrics(&synced);
            lyric_state.index = 0;
            *last_unsynced = None;
            if let (Some(db), Some(path)) = (db, db_path) {
                if let Ok(mut guard) = db.lock() {
                    guard.insert(&meta.artist, &meta.title, &synced);
                    let _ = guard.save(path);
                }
            }
            update_player_state(state, meta);
        }
        Ok((plain, _)) => {
            lyric_state.clear();
            lyric_state.index = 0;
            *last_unsynced = Some(plain);
            update_player_state(state, meta);
        }
        Err(e) => {
            lyric_state.clear();
            lyric_state.index = 0;
            *last_unsynced = None;
            state.err = Some(e.to_string());
            update_player_state(state, meta);
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
    db_path: Option<&String>,
) {
    if let Some(db) = db {
        if try_load_from_db(meta, lyric_state, last_unsynced, state, db) {
            return;
        }
    }
    try_fetch_from_api_and_save(meta, lyric_state, last_unsynced, state, db, db_path).await;
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

/// Listens for player and lyric updates, sending them to the update channel.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
) {
    let mut state = PlayerState::default();
    let mut lyric_state = LyricState::default();
    let mut last_unsynced: Option<String> = None;
    let (event_tx, mut event_rx) = mpsc::channel(8);

    // Debounce state for track changes
    let mut pending_meta: Option<crate::mpris::TrackMetadata> = None;
    let mut debounce_sleep: Option<Pin<Box<Sleep>>> = None;
    let debounce_duration = Duration::from_millis(500);

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
            Some((meta, position, is_track_change)) = event_rx.recv() => {
                let changed = meta.title != state.title || meta.artist != state.artist || meta.album != state.album;
                if is_track_change && changed {
                    // Debounce: store meta and reset timer
                    pending_meta = Some(meta.clone());
                    debounce_sleep = Some(Box::pin(sleep(debounce_duration)));
                }
                state.playing = true;
                state.position = position;
                let new_index = lyric_state.get_index(state.position);
                let index_changed = new_index != lyric_state.index;
                if changed {
                    lyric_state.index = new_index;
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                } else if index_changed {
                    lyric_state.index = new_index;
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                }
            }
            // Debounce timer fires: fetch lyrics for last pending meta
            _ = async {
                if let Some(ref mut sleep_fut) = debounce_sleep {
                    sleep_fut.as_mut().await;
                    true
                } else {
                    false
                }
            }, if debounce_sleep.is_some() => {
                if let Some(meta) = pending_meta.take() {
                    fetch_and_update_lyrics(&meta, &mut lyric_state, &mut last_unsynced, &mut state, db.as_ref(), db_path.as_ref()).await;
                    lyric_state.index = lyric_state.get_index(state.position);
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                }
                debounce_sleep = None;
            }
            _ = tokio::time::sleep(poll_interval) => {
                // Fallback polling (for robustness)
                let meta = mpris::get_metadata().await.unwrap_or_default();
                let playing = matches!(mpris::get_playback_status().await.as_deref(), Ok("Playing"));
                let position = mpris::get_position().await.unwrap_or(0.0);
                let changed = meta.title != state.title || meta.artist != state.artist || meta.album != state.album;
                if changed && playing {
                    // Debounce polling as well
                    pending_meta = Some(meta.clone());
                    debounce_sleep = Some(Box::pin(sleep(debounce_duration)));
                }
                state.playing = playing;
                state.position = position;
                let new_index = lyric_state.get_index(state.position);
                let index_changed = new_index != lyric_state.index;
                if changed {
                    lyric_state.index = new_index;
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                } else if index_changed {
                    lyric_state.index = new_index;
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                } else if state.err.is_some() || last_unsynced.is_some() {
                    send_update(&update_tx, &lyric_state, &state, &last_unsynced).await;
                }
            }
        }
    }
}
